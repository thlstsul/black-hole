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

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{HINSTANCE, LPARAM, WPARAM};
use windows::Win32::UI::TextServices::{
    ITfComposition, ITfContext, ITfLangBarItem, ITfLangBarItemSink, ITfThreadMgr,
};
use windows_core::{GUID, PCWSTR, w};

use blackhole_shared::{
    EngineCommand, InputModeSwitch, KeyEvent, SchemeId, SchemeResult, Theme, UiCommand,
};

pub mod auto_register;
pub(crate) mod caret;
pub(crate) mod commit;
pub(crate) mod dll;
pub(crate) mod key_event;
pub(crate) mod langbar;
pub(crate) mod registry;
pub(crate) mod service;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// COM class ID for the Blackhole text service.
pub(crate) const CLSID_BLACKHOLE_TIP: GUID = GUID::from_values(
    0xA1B2C3D4,
    0xE5F6,
    0x7890,
    [0x12, 0x34, 0x56, 0x78, 0x90, 0xAB, 0xCD, 0xEF],
);

/// Language-profile GUID for the Blackhole text service.
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
pub(crate) const TIP_DISPLAY_NAME: PCWSTR = w!("Blackhole IME");
pub(crate) const TIP_PROFILE_NAME: PCWSTR = w!("Blackhole");

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
    pub(crate) writer: std::net::TcpStream,
    pub(crate) reader: std::io::BufReader<std::net::TcpStream>,
}

pub(crate) struct ServiceInner {
    /// IPC connection to the daemon (out-of-process mode).
    pub(crate) ipc_conn: Option<IpcConnection>,
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

/// Send a UI command to the daemon via IPC (free function for use outside impl).
pub(crate) fn send_ui_command_inner(inner_arc: &Arc<Mutex<ServiceInner>>, cmd: UiCommand) {
    let mut inner = inner_arc.lock().unwrap();
    if let Some(ref mut conn) = inner.ipc_conn {
        let request = super::ipc::IpcRequest::UiCommand(cmd);
        let _ = super::ipc::send_request(&mut conn.writer, &request);
    }
}

// ---------------------------------------------------------------------------
// PlatformIme implementation (used by the daemon / test harness)
// ---------------------------------------------------------------------------

use super::{PlatformError, PlatformIme};

pub struct WindowsTsfIme {
    /// 运行时方案/主题状态，daemon 每次切换方案/主题时同步更新
    current: Arc<Mutex<(SchemeId, Theme)>>,
}

impl WindowsTsfIme {
    /// 使用共享状态创建。`current` 会被 daemon 在每次方案/主题切换时自动更新。
    pub fn new(current: Arc<Mutex<(SchemeId, Theme)>>) -> Self {
        Self { current }
    }

    /// 创建时指定初始值（内部创建共享状态）。
    pub fn new_with_values(default_scheme: SchemeId, default_theme: Theme) -> Self {
        Self {
            current: Arc::new(Mutex::new((default_scheme, default_theme))),
        }
    }

    pub fn current(&self) -> &Arc<Mutex<(SchemeId, Theme)>> {
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
        use std::net::TcpListener;

        let listener = TcpListener::bind(super::ipc::IPC_SERVER_ADDR)
            .map_err(|e| PlatformError::Other(format!("Failed to bind IPC server: {}", e)))?;

        tracing::info!(
            "Windows TSF IPC server listening on {}",
            super::ipc::IPC_SERVER_ADDR
        );

        let platform_rx = Arc::new(Mutex::new(platform_rx));

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    tracing::info!("TSF DLL connected");
                    let engine_tx = engine_tx.clone();
                    let platform_rx = Arc::clone(&platform_rx);
                    let ui_tx = ui_tx.clone();
                    let current = Arc::clone(&self.current);
                    std::thread::spawn(move || {
                        if let Err(e) =
                            handle_ipc_client(stream, engine_tx, platform_rx, ui_tx, current)
                        {
                            tracing::error!("IPC client error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("IPC accept error: {}", e);
                }
            }
        }

        Ok(())
    }
}

fn handle_ipc_client(
    stream: std::net::TcpStream,
    engine_tx: Sender<EngineCommand>,
    platform_rx: Arc<Mutex<Receiver<SchemeResult>>>,
    ui_tx: Sender<UiCommand>,
    current: Arc<Mutex<(SchemeId, Theme)>>,
) -> std::io::Result<()> {
    use std::io::{BufRead, Write};

    let mut reader = std::io::BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut line = String::new();

    let send_engine = |cmd: EngineCommand| -> std::io::Result<SchemeResult> {
        let rx = platform_rx.lock().unwrap();
        engine_tx
            .send(cmd)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;
        rx.recv()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))
    };

    let write_response =
        |writer: &mut std::net::TcpStream, result: SchemeResult| -> std::io::Result<()> {
            let response: super::ipc::IpcResponse = result.into();
            let json = serde_json::to_string(&response)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            writeln!(writer, "{}", json)?;
            writer.flush()
        };

    while reader.read_line(&mut line)? > 0 {
        let request: super::ipc::IpcRequest = serde_json::from_str(line.trim())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        tracing::debug!(
            "handle_ipc_client start: req={:?}",
            std::mem::discriminant(&request)
        );
        match request {
            super::ipc::IpcRequest::KeyEvent(key) => {
                let result = send_engine(EngineCommand::Key(key))?;
                if let SchemeResult::Committed { ref text } = result {
                    let _ = ui_tx.send(UiCommand::CommitText(text.clone()));
                }
                write_response(&mut writer, result)?;
            }
            super::ipc::IpcRequest::SetContext(new_ctx) => {
                let _ = engine_tx.send(EngineCommand::SetContext(new_ctx));
            }
            super::ipc::IpcRequest::Reset => {
                let result = send_engine(EngineCommand::Reset)?;
                let _ = ui_tx.send(UiCommand::HideCandidates);
                write_response(&mut writer, result)?;
            }
            super::ipc::IpcRequest::UiCommand(ui_cmd) => {
                let _ = ui_tx.send(ui_cmd);
            }
            super::ipc::IpcRequest::GetSettings => {
                let (scheme_id, theme) = *current.lock().unwrap();
                let response = super::ipc::IpcResponse::Settings { scheme_id, theme };
                let json = serde_json::to_string(&response)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                writeln!(writer, "{}", json)?;
                writer.flush()?;
            }
        }
        tracing::debug!("handle_ipc_client end");

        line.clear();
    }

    Ok(())
}
