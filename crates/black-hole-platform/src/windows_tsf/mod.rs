//! Windows TSF (Text Services Framework) COM server implementation
#![allow(clippy::not_unsafe_ptr_arg_deref)]
//!
//! This module implements a full in-process TSF text input processor (TIP).
//! When built as a `cdylib`, the resulting DLL exports the required COM entry
//! points (`DllGetClassObject`, `DllCanUnloadNow`, `DllRegisterServer`,
//! `DllUnregisterServer`) so Windows can load the IME into every application
//! process.
//!
//! Reference: Microsoft SampleIME project.

use crate::ipc::{IPC_SERVER_ADDR, IpcRequest, IpcResponse, send_request};
use serde_json::{from_str, to_string};
use std::ffi::c_void;
use std::io::{self, BufRead, BufReader, Write};
use std::mem::discriminant;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use windows::Win32::Foundation::{HINSTANCE, LPARAM, WPARAM};
use windows::Win32::UI::TextServices::{
    ITfComposition, ITfContext, ITfLangBarItem, ITfLangBarItemSink, ITfThreadMgr,
};
use windows_core::{GUID, PCWSTR, w};

use super::{PlatformError, PlatformIme};
use black_hole_shared::{
    EngineCommand, InputModeSwitch, KeyEvent, SchemeId, SchemeResult, Theme, UiCommand,
};

pub mod auto_register;
pub(crate) mod caret;
pub(crate) mod commit;
pub(crate) mod dll;
pub(crate) mod hook;
pub(crate) mod key_event;
pub(crate) mod langbar;
pub(crate) mod registry;
pub(crate) mod service;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// COM class ID for the Black-Hole text service.
pub(crate) const CLSID_BLACKHOLE_TIP: GUID = GUID::from_values(
    0xA1B2C3D4,
    0xE5F6,
    0x7890,
    [0x12, 0x34, 0x56, 0x78, 0x90, 0xAB, 0xCD, 0xEF],
);

/// Language-profile GUID for the Black-Hole text service.
pub(crate) const GUID_PROFILE_BLACKHOLE: GUID = GUID::from_values(
    0xB2C3D4E5,
    0xF678,
    0x9012,
    [0x34, 0x56, 0x78, 0x90, 0xAB, 0xCD, 0xEF, 0x01],
);

/// CLSID for the TSF `ITfInputProcessorProfiles` COM object.
pub(crate) const CLSID_TF_INPUTPROCESSORPROFILES: GUID = GUID::from_values(
    0x33C53A50,
    0xF456,
    0x4884,
    [0xB0, 0x49, 0x85, 0xFD, 0x64, 0x3E, 0xCF, 0xED],
);

/// Human-readable name stored in the registry.
pub(crate) const TIP_DISPLAY_NAME: PCWSTR = w!("Black-Hole IME");
pub(crate) const TIP_PROFILE_NAME: PCWSTR = w!("Black-Hole");

/// Thread-safe global DLL instance handle (stored as usize for Send+Sync).
pub(crate) static DLL_INSTANCE: AtomicUsize = AtomicUsize::new(0);

/// Global COM object reference count (used by `DllCanUnloadNow`).
pub(crate) static GLOBAL_REF_COUNT: Mutex<u32> = Mutex::new(0);

// ---------------------------------------------------------------------------
// DLL reference counting helpers
// ---------------------------------------------------------------------------

pub(crate) fn dll_add_ref() {
    let mut count = GLOBAL_REF_COUNT.lock().unwrap();
    *count += 1;
}

pub(crate) fn dll_release() {
    let mut count = GLOBAL_REF_COUNT.lock().unwrap();
    *count -= 1;
}

pub(crate) fn set_dll_instance(inst: HINSTANCE) {
    DLL_INSTANCE.store(inst.0 as usize, Ordering::SeqCst);
}

pub(crate) fn get_dll_instance() -> Option<HINSTANCE> {
    let ptr = DLL_INSTANCE.load(Ordering::SeqCst);
    if ptr == 0 {
        None
    } else {
        Some(HINSTANCE(ptr as *mut c_void))
    }
}

// ---------------------------------------------------------------------------
// Internal service state (shared between COM threads)
// ---------------------------------------------------------------------------

/// IPC connection wrapper to avoid creating BufReader on every key press.
pub(crate) struct IpcConnection {
    pub(crate) writer: TcpStream,
    pub(crate) reader: BufReader<TcpStream>,
}

pub(crate) struct ServiceInner {
    /// IPC connection to the daemon (out-of-process mode).
    pub(crate) ipc_conn: Option<IpcConnection>,
    /// 上次尝试重连的时间戳，用于限制重连频率（daemon 不可用时避免不断重连）。
    pub(crate) last_reconnect_attempt: Option<Instant>,
    /// Cached thread manager (set on Activate).
    pub(crate) thread_mgr: Option<ITfThreadMgr>,
    /// TSF client ID.
    pub(crate) client_id: u32,
    /// Current context.
    pub(crate) context: Option<ITfContext>,
    /// Active composition object.
    pub(crate) composition: Option<ITfComposition>,
    /// Whether the service is currently active.
    pub(crate) active: bool,
    /// Cached key event from OnTestKeyDown so OnKeyDown can reuse it.
    pub(crate) last_key_event: Option<(WPARAM, LPARAM, KeyEvent)>,
    /// Last known caret position for fallback when GetTextExt fails.
    pub(crate) last_caret_pos: Option<(i32, i32, i32)>,
    /// Cookie returned by ITfSource::AdviseSink for ITfTextLayoutSink.
    pub(crate) layout_sink_cookie: Option<u32>,
    /// Cookie returned by ITfSource::AdviseSink for ITfThreadMgrEventSink.
    pub(crate) thread_mgr_event_sink_cookie: Option<u32>,
    /// Language bar item interface, kept for removal on deactivation.
    pub(crate) langbar_item: Option<ITfLangBarItem>,
    /// Sink installed by the language bar manager via ITfSource::AdviseSink.
    pub(crate) langbar_item_sink: Option<ITfLangBarItemSink>,
    /// Cookie returned for the language bar item sink.
    pub(crate) langbar_item_sink_cookie: u32,
    /// Current input scheme tracked by the language bar menu.
    pub(crate) current_scheme: SchemeId,
    /// Current theme tracked by the language bar menu.
    pub(crate) current_theme: Theme,
    /// 中英文模式切换状态机（Ctrl 键触发，按线程独立维护）。
    pub(crate) mode_switch: InputModeSwitch,
}

impl ServiceInner {
    pub(crate) fn new() -> Self {
        Self {
            ipc_conn: None,
            last_reconnect_attempt: None,
            thread_mgr: None,
            client_id: 0,
            context: None,
            composition: None,
            active: false,
            last_key_event: None,
            last_caret_pos: None,
            layout_sink_cookie: None,
            thread_mgr_event_sink_cookie: None,
            langbar_item: None,
            langbar_item_sink: None,
            langbar_item_sink_cookie: 0,
            current_scheme: SchemeId::Pinyin,
            current_theme: Theme::Light,
            mode_switch: InputModeSwitch::default(),
        }
    }
}

// TSF COM server runs in a single-threaded apartment (STA); all fields are
// accessed from the same thread in practice.
unsafe impl Send for ServiceInner {}
unsafe impl Sync for ServiceInner {}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// 重连最小间隔：daemon 不可用时限制重连频率，避免不断重连、刷日志。
const RECONNECT_MIN_INTERVAL: Duration = Duration::from_millis(1000);
/// 单次连接尝试超时，防止 SYN 被防火墙/杀软丢弃时长时间阻塞调用线程。
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

/// 确保与 daemon 的 IPC 连接存在;若已断开(如 daemon 重启)则自动重连。
/// 返回连接是否可用。供菜单命令、候选窗等非按键路径复用。
/// 重连受 [`try_reconnect_ipc`] 限频，不会在调用线程上 sleep。
pub(crate) fn ensure_ipc_connection(inner_arc: &Arc<Mutex<ServiceInner>>) -> bool {
    if inner_arc.lock().unwrap().ipc_conn.is_some() {
        return true;
    }
    try_reconnect_ipc(inner_arc)
}

/// 尝试重连 daemon（限频、带超时、不阻塞）。
/// 距离上次尝试不足 [`RECONNECT_MIN_INTERVAL`] 时直接放弃，避免不断重连。
/// 供按键路径在失败后调用一次，失败即返回，由下一次按键再试。
pub(crate) fn try_reconnect_ipc(inner_arc: &Arc<Mutex<ServiceInner>>) -> bool {
    {
        let inner = inner_arc.lock().unwrap();
        if inner.ipc_conn.is_some() {
            return true;
        }
        if let Some(last) = inner.last_reconnect_attempt
            && last.elapsed() < RECONNECT_MIN_INTERVAL
        {
            return false;
        }
    }

    // 先记录尝试时间再连接，使连续调用被限频。
    inner_arc.lock().unwrap().last_reconnect_attempt = Some(Instant::now());

    let addr: SocketAddr = match IPC_SERVER_ADDR.parse() {
        Ok(a) => a,
        Err(_) => {
            error!(
                "try_reconnect_ipc: invalid IPC address: {}",
                IPC_SERVER_ADDR
            );
            return false;
        }
    };

    match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(stream) => {
            let _ = stream.set_nodelay(true);
            let reader = match stream.try_clone() {
                Ok(r) => BufReader::new(r),
                Err(_) => return false,
            };
            let mut inner = inner_arc.lock().unwrap();
            inner.ipc_conn = Some(IpcConnection {
                writer: stream,
                reader,
            });
            info!(
                "try_reconnect_ipc: connected to daemon at {}",
                IPC_SERVER_ADDR
            );
            true
        }
        Err(e) => {
            warn!("try_reconnect_ipc: failed to connect to daemon: {}", e);
            false
        }
    }
}

/// Send a UI command to the daemon via IPC (free function for use outside impl).
/// 连接缺失或已断开时自动重连后再发送,失败记录日志,不再静默丢弃。
pub(crate) fn send_ui_command_inner(inner_arc: &Arc<Mutex<ServiceInner>>, cmd: UiCommand) {
    if !ensure_ipc_connection(inner_arc) {
        warn!(
            "send_ui_command_inner: no IPC connection, dropping command {:?}",
            discriminant(&cmd)
        );
        return;
    }

    let request = IpcRequest::UiCommand(cmd);
    let mut inner = inner_arc.lock().unwrap();
    if let Some(ref mut conn) = inner.ipc_conn
        && let Err(e) = send_request(&mut conn.writer, &request)
    {
        warn!("send_ui_command_inner: send failed: {}", e);
        inner.ipc_conn = None;
    }
}

// ---------------------------------------------------------------------------
// PlatformIme implementation (used by the daemon / test harness)
// ---------------------------------------------------------------------------

pub struct WindowsTsfIme {
    /// 运行时方案/主题/中英模式状态，daemon 每次切换时同步更新
    current: Arc<Mutex<(SchemeId, Theme, bool)>>,
}

impl WindowsTsfIme {
    /// 使用共享状态创建。`current` 会被 daemon 在每次方案/主题/中英模式切换时自动更新。
    pub fn new(current: Arc<Mutex<(SchemeId, Theme, bool)>>) -> Self {
        Self { current }
    }

    /// 创建时指定初始值（内部创建共享状态）。
    pub fn new_with_values(default_scheme: SchemeId, default_theme: Theme) -> Self {
        Self {
            current: Arc::new(Mutex::new((default_scheme, default_theme, false))),
        }
    }

    pub fn current(&self) -> &Arc<Mutex<(SchemeId, Theme, bool)>> {
        &self.current
    }
}

impl PlatformIme for WindowsTsfIme {
    fn run(
        &mut self,
        engine_tx: Sender<EngineCommand>,
        platform_rx: Receiver<SchemeResult>,
        ui_tx: Sender<UiCommand>,
    ) -> Result<(), PlatformError> {
        let listener = TcpListener::bind(IPC_SERVER_ADDR)
            .map_err(|e| PlatformError::Other(format!("Failed to bind IPC server: {}", e)))?;

        info!("Windows TSF IPC server listening on {}", IPC_SERVER_ADDR);

        let platform_rx = Arc::new(Mutex::new(platform_rx));

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    info!("TSF DLL connected");
                    let engine_tx = engine_tx.clone();
                    let platform_rx = Arc::clone(&platform_rx);
                    let ui_tx = ui_tx.clone();
                    let current = Arc::clone(&self.current);
                    thread::spawn(move || {
                        if let Err(e) =
                            handle_ipc_client(stream, engine_tx, platform_rx, ui_tx, current)
                        {
                            error!("IPC client error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("IPC accept error: {}", e);
                }
            }
        }

        Ok(())
    }
}

fn handle_ipc_client(
    stream: TcpStream,
    engine_tx: Sender<EngineCommand>,
    platform_rx: Arc<Mutex<Receiver<SchemeResult>>>,
    ui_tx: Sender<UiCommand>,
    current: Arc<Mutex<(SchemeId, Theme, bool)>>,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut line = String::new();

    let send_engine = |cmd: EngineCommand| -> io::Result<SchemeResult> {
        let rx = platform_rx.lock().unwrap();
        engine_tx
            .send(cmd)
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
        rx.recv()
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))
    };

    let write_response = |writer: &mut TcpStream, result: SchemeResult| -> io::Result<()> {
        let response: IpcResponse = result.into();
        let json =
            to_string(&response).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        writeln!(writer, "{}", json)?;
        writer.flush()
    };

    while reader.read_line(&mut line)? > 0 {
        let request: IpcRequest =
            from_str(line.trim()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        debug!("handle_ipc_client start: req={:?}", discriminant(&request));
        match request {
            IpcRequest::KeyEvent(key) => {
                let result = send_engine(EngineCommand::Key(key))?;
                if let SchemeResult::Committed { ref text } = result {
                    let _ = ui_tx.send(UiCommand::CommitText(text.clone()));
                }
                write_response(&mut writer, result)?;
            }
            IpcRequest::SetContext(new_ctx) => {
                let _ = engine_tx.send(EngineCommand::SetContext(new_ctx));
            }
            IpcRequest::Reset => {
                let result = send_engine(EngineCommand::Reset)?;
                let _ = ui_tx.send(UiCommand::HideCandidates);
                write_response(&mut writer, result)?;
            }
            IpcRequest::UiCommand(ui_cmd) => {
                let _ = ui_tx.send(ui_cmd);
            }
            IpcRequest::GetSettings => {
                let (scheme_id, theme, english) = *current.lock().unwrap();
                let response = IpcResponse::Settings {
                    scheme_id,
                    theme,
                    english,
                };
                let json = to_string(&response)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                writeln!(writer, "{}", json)?;
                writer.flush()?;
            }
        }
        debug!("handle_ipc_client end");

        line.clear();
    }

    Ok(())
}
