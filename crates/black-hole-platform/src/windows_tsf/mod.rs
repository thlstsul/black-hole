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

use crate::ipc::{
    IPC_PIPE_NAME, IpcListener, IpcRequest, IpcResponse, IpcStream, read_request, send_request_buf,
    send_response_buf,
};
use std::ffi::c_void;
use std::io::{self};
use std::mem::discriminant;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use windows::Win32::Foundation::{E_UNEXPECTED, HINSTANCE, LPARAM, WPARAM};
use windows::Win32::UI::TextServices::{
    ITfComposition, ITfContext, ITfLangBarItem, ITfLangBarItemSink, ITfSource, ITfThreadMgr,
};
use windows_core::{Error, GUID, Interface, PCWSTR, Result as CoreResult, w};

use super::{PlatformError, PlatformIme};
use black_hole_shared::{
    AutoModeSwitch, EngineCommand, InputModeSwitch, KeyEvent, RuntimeSettings, SchemeId,
    SchemeResult, Theme, UiCommand, panic_payload_text,
};

pub mod auto_register;
pub(crate) mod auto_switch;
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

/// COM 回调 panic 防护：捕获 panic 并转为 COM 错误码，避免 unwind 跨越
/// FFI 边界导致进程 abort（退出码 0xc0000409，无 WER 记录、无日志）。
///
/// TSF 宿主直接调用这些 `extern "system"` 接口方法，任何 panic 都不允许
/// 向外传播。注意：本 DLL 加载在宿主应用进程内，daemon 安装的全局 panic
/// hook 在这里不存在，因此 panic 现场必须在 catch 分支就地记录（payload
/// 消息 + 线程名）。宿主进程内 tracing subscriber 通常未初始化，日志可能
/// 无输出，但记录本身保持尽力而为。
pub(crate) fn safe_com<T>(op: impl FnOnce() -> CoreResult<T>) -> CoreResult<T> {
    catch_unwind(AssertUnwindSafe(op)).unwrap_or_else(|payload| {
        let message = panic_payload_text(&payload);
        let thread = thread::current();
        error!(
            "COM callback panicked in thread '{}': {} — returning E_UNEXPECTED",
            thread.name().unwrap_or("<unnamed>"),
            message
        );
        Err(Error::from_hresult(E_UNEXPECTED))
    })
}

/// 普通 Mutex 的 poison 恢复加锁（`ServiceInner` 用 [`lock_service`]）。
pub(crate) fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn dll_add_ref() {
    *locked(&GLOBAL_REF_COUNT) += 1;
}

pub(crate) fn dll_release() {
    *locked(&GLOBAL_REF_COUNT) -= 1;
}

/// 加锁 `ServiceInner`，poison 恢复时重置突变敏感字段。
///
/// 任一线程持锁 panic 会使 Mutex poison。直接 `into_inner()` 会带着
/// 中途崩溃留下的不一致状态继续跑（如 composition 已清但 context_version
/// 未 bump，合成提交/取消会作用到过期范围）。本函数在恢复路径上复用
/// [`ServiceInner::clear_composition`] 收尾（结束合成态、反注册 layout
/// sink、bump 版本号），并清空按键缓存，让下游的陈旧门控重新采样。
pub(crate) fn lock_service(inner: &Mutex<ServiceInner>) -> MutexGuard<'_, ServiceInner> {
    match inner.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            warn!("ServiceInner lock poisoned, resetting mutation-sensitive fields");
            let mut guard = poisoned.into_inner();
            // 毒化恢复路径必须在返回 guard 前完成收尾，因此无法在锁外反注册
            // sink（已知例外；正常路径一律走 unadvise_sink_pair，见其注释）
            unadvise_sink_pair(guard.clear_composition());
            guard.last_key_event = None;
            guard.last_eng_eval_key = None;
            guard
        }
    }
}

/// 非阻塞获取 ServiceInner：锁被占用时返回 None，调用方跳过本次咨询性更新。
///
/// OnPushContext/OnPopContext 在窗口销毁（DestroyWindow → ShutdownTextServices）
/// 的 teardown 路径上由 TSF 回调，回调线程持有 TSF 内部锁；若在此同步阻塞等待
/// 本 Mutex，而持锁方（按键/编辑会话路径）又在等待 TSF 锁，即构成锁序倒置
/// 死锁（Notepad 关窗卡死的根因）。这两处仅做咨询性记录，且按键路径会无条件
/// 刷新 context，竞争时跳过是安全的。
pub(crate) fn try_lock_service(
    inner: &Mutex<ServiceInner>,
) -> Option<MutexGuard<'_, ServiceInner>> {
    match inner.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(p)) => {
            warn!("ServiceInner lock poisoned, resetting mutation-sensitive fields");
            let mut guard = p.into_inner();
            unadvise_sink_pair(guard.clear_composition());
            guard.last_key_event = None;
            guard.last_eng_eval_key = None;
            Some(guard)
        }
        Err(TryLockError::WouldBlock) => None,
    }
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

/// IPC connection wrapper to avoid allocating on every key press.
/// `resp_buf`/`send_buf` 为帧载荷复用缓冲（读写热路径零分配）。
pub(crate) struct IpcConnection {
    pub(crate) writer: IpcStream,
    pub(crate) reader: IpcStream,
    pub(crate) resp_buf: Vec<u8>,
    pub(crate) send_buf: Vec<u8>,
}

pub(crate) struct ServiceInner {
    /// IPC connection to the daemon (out-of-process mode).
    pub(crate) ipc_conn: Option<IpcConnection>,
    /// 上次尝试重连的时间戳，用于限制重连频率（daemon 不可用时避免不断重连）。
    pub(crate) last_reconnect_attempt: Option<Instant>,
    /// 徽标按发送端视角是否可见：Show 置真，任何 Hide 置假
    /// （SetInputMode* 仅同步字样不改显隐，不改标志）。供英文直输 Hide
    /// 的源侧去重（已隐藏时逐键发 Hide 是纯 IPC 开销）
    pub(crate) indicator_visible: bool,
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
    /// Layout sink registration：ITfTextLayoutSink 的 (所属 context, cookie) 配对。
    /// 配对存储保证 UnadviseSink 打到当初 AdviseSink 的同一 context 的
    /// ITfSource 上——cookie 跨 context 无效，错配会泄漏 sink 并持续派发
    /// 布局事件（评审 P1 修复）。
    pub(crate) layout_sink: Option<(ITfContext, u32)>,
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
    /// 中英文模式切换状态机（模式本体按实例维护；Ctrl 手势的 pending
    /// 状态是进程级的，见 [`CTRL_GESTURE`]）。
    pub(crate) mode_switch: InputModeSwitch,
    /// "根据光标周围文本自动切换中英模式"开关（由 daemon 的 GetSettings 同步，支持热更新）。
    pub(crate) auto_switch: bool,
    /// 自动切换状态机：评估语境建议、维护手动切换锁定的语境基线。
    pub(crate) auto_mode: AutoModeSwitch,
    /// 语境变更版本：合成开始/结束、焦点切换、Deactivate 时 +1。
    pub(crate) context_version: u64,
    /// "延迟手动锁定"标志：手动 Ctrl 切换（hook.rs on_ctrl_released；TSF
    /// Ctrl 路径采样失败时同理）无法即时取得可信语境建议时置位，下个实时
    /// 语境采样点（key_event.rs 中→英评估 / auto_switch.rs 英→中评估与聚焦
    /// 评估）在 evaluate 前以本次新鲜读取的建议消费该标志锁定基线，保护
    /// 手动切换不被随后的自动切换撤销。
    ///
    /// 仅在自动切换开启时置位（关闭时没有评估点会消费它，残留会在下次开启
    /// 后被误当成新切换的基线；开关同步路径也会清除残留）。在无文本存储的
    /// 应用里可能长期挂起，直到同一实例的后续可信采样才消费——届时以那次
    /// 语境的建议锁定基线，效果只是可能少一次自动纠正，**不会撤销手动选择**，
    /// 因此不在焦点切换时清除（那会让"切走再切回同一文档"的首键被自动切换
    /// 拉回，正是本次要修的病症）。
    pub(crate) pending_manual_lock: bool,
    /// "当前应用无法取得光标周围文本"粘性标记（VSCode/Zed 等虚拟化编辑器、
    /// 纯 IMM32 无文本存储的应用）。任一自动切换评估路径的语境读取报告
    /// context_known=false 时置位：本实例内禁用后续自动切换评估——英→中按键
    /// 路径不再发起每次按键一次的同步只读会话（反复探测注定失败，纯开销），
    /// 中→英与聚焦评估路径照常跳过。焦点进入新文档时复位，新应用重新探测。
    pub(crate) auto_switch_no_text: bool,
    /// 英文模式自动切换评估去重：最近一次在 OnTestKeyDown 评估过的按键及时间。
    /// 部分应用（如 WebView2）对同一按键既调 OnTestKeyDown 又调 OnKeyDown，
    /// OnKeyDown 据此在时间窗内跳过重复评估。
    pub(crate) last_eng_eval_key: Option<(usize, Instant)>,
}

impl ServiceInner {
    pub(crate) fn new() -> Self {
        Self {
            ipc_conn: None,
            last_reconnect_attempt: None,
            indicator_visible: false,
            thread_mgr: None,
            client_id: 0,
            context: None,
            composition: None,
            active: false,
            last_key_event: None,
            last_caret_pos: None,
            layout_sink: None,
            thread_mgr_event_sink_cookie: None,
            langbar_item: None,
            langbar_item_sink: None,
            langbar_item_sink_cookie: 0,
            current_scheme: SchemeId::Pinyin,
            current_theme: Theme::Light,
            mode_switch: InputModeSwitch::default(),
            auto_switch: false,
            auto_mode: AutoModeSwitch::default(),
            context_version: 0,
            pending_manual_lock: false,
            auto_switch_no_text: false,
            last_eng_eval_key: None,
        }
    }

    /// 取出布局 sink 配对，不做 COM 调用。
    ///
    /// 调用方必须在**释放 inner 锁之后**用 [`unadvise_sink_pair`] 反注册：
    /// UnadviseSink 是 TSF COM 调用，与 TSF 内部锁存在锁序，持 inner 锁调用
    /// 会与"持 TSF 锁回调、又等 inner 锁"的路径构成锁序倒置死锁（既定模式）。
    pub(crate) fn take_layout_sink(&mut self) -> Option<(ITfContext, u32)> {
        self.layout_sink.take()
    }

    /// 清除 composition 状态（两处 edit session 共用收尾逻辑），返回待反注册
    /// 的 layout sink 配对——反注册见 [`unadvise_sink_pair`]，须在锁外调用。
    pub(crate) fn clear_composition(&mut self) -> Option<(ITfContext, u32)> {
        self.composition = None;
        self.last_caret_pos = None;
        self.context_version += 1;
        self.take_layout_sink()
    }
}

/// 反注册布局 sink（须在释放 inner 锁后调用）：以配对存储的
/// (context, cookie) 打回当初 AdviseSink 的同一 context 的 ITfSource——
/// cookie 跨 context 无效，错配会泄漏 sink 并持续派发布局事件。
pub(crate) fn unadvise_sink_pair(pair: Option<(ITfContext, u32)>) {
    if let Some((ctx, cookie)) = pair
        && let Ok(source) = ctx.cast::<ITfSource>()
    {
        let _ = unsafe { source.UnadviseSink(cookie) };
    }
}

/// 是否无有效合成（None 或 GetRange 已失效的死合成）。各自动切换评估点
/// 共用（key_event.rs 中→英 / auto_switch.rs 英→中与聚焦评估）：合成进行中
/// 不做语境评估，避免把正在输入的拼音字母当作文档文本。
pub(crate) fn no_active_composition(composition: &Option<ITfComposition>) -> bool {
    match composition {
        None => true,
        Some(c) => unsafe { c.GetRange().is_err() },
    }
}

// ---------------------------------------------------------------------------
// 进程级 Ctrl 切换手势状态
// ---------------------------------------------------------------------------

/// "按下 Ctrl 标记候选、松开单独 Ctrl 切换"手势的 pending 状态，**进程级
/// 共享**（钩子路径与 TSF 按键路径消费同一份）。
///
/// 为什么必须进程级：一次物理 Ctrl 手势属于键盘而非任何单个实例，但本进程
/// 内可能同时存在多个活跃实例（多线程宿主、重复 Activate 残留），且钩子路径
/// 与 TSF 按键路径可能命中**不同**的实例——若 pending 状态挂在实例上
/// （原 `InputModeSwitch.ctrl_pending` + `hook_toggled` 标志），两条路径会各
/// 自翻转各自实例的模式（同一次 Ctrl 双重切换），相位错开的两实例随后上报
/// 相反的模式，daemon "最后 Show 胜出"的徽标仲裁就会呈现非聚焦实例的字样
/// （徽标显示反了）。改为进程级后，同一次手势在进程内**恰好被消费一次**，
/// 只有赢家的实例翻转并上报。
static CTRL_GESTURE_PENDING: Mutex<bool> = Mutex::new(false);

/// 标记 Ctrl 切换手势候选（按下 Ctrl；自动重复的按下是幂等的）。
pub(crate) fn gesture_ctrl_pressed() {
    if let Ok(mut pending) = CTRL_GESTURE_PENDING.lock() {
        *pending = true;
    }
}

/// 按住 Ctrl 期间按下其他键（Ctrl+C、Ctrl+Shift 等）：取消切换候选。
pub(crate) fn gesture_other_key_pressed(ctrl_held: bool) {
    if ctrl_held && let Ok(mut pending) = CTRL_GESTURE_PENDING.lock() {
        *pending = false;
    }
}

/// 消费 Ctrl 松开手势：返回 `true` 表示本次松开对应一次完整的
/// "单独按下→松开"手势且**首次**被消费；调用方据此翻转自身实例的模式。
/// 并发路径（钩子线程 / 聚焦实例的 TSF 线程）只有一个能拿到 `true`。
pub(crate) fn gesture_consume_release() -> bool {
    CTRL_GESTURE_PENDING
        .lock()
        .map(|mut pending| {
            let armed = *pending;
            *pending = false;
            armed
        })
        .unwrap_or(false)
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

/// 确保与 daemon 的 IPC 连接存在;若已断开(如 daemon 重启)则自动重连。
/// 返回连接是否可用。供菜单命令、候选窗等非按键路径复用。
/// 重连受 [`try_reconnect_ipc`] 限频，不会在调用线程上 sleep。
pub(crate) fn ensure_ipc_connection(inner_arc: &Arc<Mutex<ServiceInner>>) -> bool {
    if lock_service(inner_arc).ipc_conn.is_some() {
        return true;
    }
    try_reconnect_ipc(inner_arc)
}

/// 尝试重连 daemon（限频、带超时、不阻塞）。
/// 距离上次尝试不足 [`RECONNECT_MIN_INTERVAL`] 时直接放弃，避免不断重连。
/// 供按键路径在失败后调用一次，失败即返回，由下一次按键再试。
pub(crate) fn try_reconnect_ipc(inner_arc: &Arc<Mutex<ServiceInner>>) -> bool {
    {
        let inner = lock_service(inner_arc);
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
    lock_service(inner_arc).last_reconnect_attempt = Some(Instant::now());

    match IpcStream::connect(IPC_PIPE_NAME) {
        Ok(stream) => {
            let reader = match stream.try_clone() {
                Ok(r) => r,
                Err(_) => return false,
            };
            let mut inner = lock_service(inner_arc);
            inner.ipc_conn = Some(IpcConnection {
                writer: stream,
                reader,
                resp_buf: Vec::with_capacity(4096),
                send_buf: Vec::with_capacity(4096),
            });
            info!(
                "try_reconnect_ipc: connected to daemon at {}",
                IPC_PIPE_NAME
            );
            // 释放锁后重新同步设置：daemon 可能已重启且方案/主题/中英模式/
            // 自动切换开关发生变化，避免本进程残留旧状态。连接/重连始终
            // 应用 daemon 当前中英模式（含英文）：英文模式下按键路径
            // （OnTestKeyDown 补设 context）已能正常触发自动切换，无需
            // 再以"连接默认中文"兜底，跨进程模式保持一致。
            drop(inner);
            service::sync_settings_from_daemon_inner(inner_arc);
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
    // 源侧徽标可见性维护：仅 Show/Hide 两个变体涉及标志（取锁），其余
    // 命令（逐键热路径：候选窗更新、光标位置等）直接透传不加锁
    enum IndicatorUpdate {
        Show,
        Hide(bool),
    }
    let indicator = match &cmd {
        UiCommand::ShowModeIndicator { .. } => Some(IndicatorUpdate::Show),
        UiCommand::HideModeIndicator { invalidate_caret } => {
            Some(IndicatorUpdate::Hide(*invalidate_caret))
        }
        _ => None,
    };

    // Hide 的去重判定在发送前完成（已隐藏的英文直输 Hide 不发）：焦点失效
    // 语义（invalidate_caret=true）必须送达 UI 作废 has_caret，即使已隐藏
    // 也不去重（失效与显隐是正交语义）。
    //
    // 此处只读不写：标志的更新与 Show 对称，一律在**确认发出之后**完成。
    // 若提前置假，连接缺失/发送失败时 UI 仍显示着徽标，后续英文直输的 Hide
    // 会被去重吞掉，徽标滞留在屏幕上直到下次 Show 或候选窗更新（去重宁可
    // 漏发不可错吞）
    let mut send = true;
    if let Some(IndicatorUpdate::Hide(invalidate_caret)) = &indicator {
        let was_visible = lock_service(inner_arc).indicator_visible;
        send = *invalidate_caret || was_visible;
    }
    if !send {
        return;
    }

    if !ensure_ipc_connection(inner_arc) {
        warn!(
            "send_ui_command_inner: no IPC connection, dropping command {:?}",
            discriminant(&cmd)
        );
        return;
    }

    let request = IpcRequest::UiCommand(cmd);
    let mut inner = lock_service(inner_arc);
    let sent_ok = if let Some(ref mut conn) = inner.ipc_conn {
        match send_request_buf(&mut conn.writer, &request, &mut conn.send_buf) {
            Ok(()) => true,
            Err(e) => {
                warn!("send_ui_command_inner: send failed: {}", e);
                inner.ipc_conn = None;
                false
            }
        }
    } else {
        false
    };
    // 可见标志只在发送成功后更新：连接缺失/发送失败时标志保持原值，
    // 随后的 Show/Hide 不会被去重误吞（去重宁可漏发不可错吞——UI 侧幂等）
    if sent_ok {
        match indicator {
            Some(IndicatorUpdate::Show) => inner.indicator_visible = true,
            Some(IndicatorUpdate::Hide(_)) => inner.indicator_visible = false,
            None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// PlatformIme implementation (used by the daemon / test harness)
// ---------------------------------------------------------------------------

pub struct WindowsTsfIme {
    /// 运行时方案/主题/中英模式/自动切换开关状态，daemon 每次切换时同步更新
    current: Arc<Mutex<RuntimeSettings>>,
    /// 当前聚焦（最近一次徽标 Show）所属的连接编号：跨进程 Show/Hide
    /// 乱序仲裁依据（连接间 IPC 到达顺序无保证）
    focus_pipe_id: Arc<AtomicU64>,
}

impl WindowsTsfIme {
    /// 使用共享状态创建。`current` 会被 daemon 在每次方案/主题/中英模式切换时自动更新。
    pub fn new(current: Arc<Mutex<RuntimeSettings>>) -> Self {
        Self {
            current,
            focus_pipe_id: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn current(&self) -> &Arc<Mutex<RuntimeSettings>> {
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
        let listener = IpcListener::bind(IPC_PIPE_NAME)
            .map_err(|e| PlatformError::Other(format!("Failed to bind IPC pipe: {}", e)))?;

        info!("Windows TSF IPC server listening on {}", IPC_PIPE_NAME);

        let platform_rx = Arc::new(Mutex::new(platform_rx));
        // 连接编号：为每个 TSF DLL 连接分配递增 id，供跨进程 Show/Hide
        // 乱序仲裁（仅最新连接的徽标 Show 视为聚焦方）
        let mut next_pipe_id: u64 = 0;

        loop {
            match listener.accept() {
                Ok(stream) => {
                    info!("TSF DLL connected");
                    next_pipe_id += 1;
                    let pipe_id = next_pipe_id;
                    let engine_tx = engine_tx.clone();
                    let platform_rx = Arc::clone(&platform_rx);
                    let ui_tx = ui_tx.clone();
                    let current = Arc::clone(&self.current);
                    let focus_pipe_id = Arc::clone(&self.focus_pipe_id);
                    thread::spawn(move || {
                        if let Err(e) = handle_ipc_client(
                            stream,
                            engine_tx,
                            platform_rx,
                            ui_tx,
                            current,
                            pipe_id,
                            focus_pipe_id,
                        ) {
                            error!("IPC client error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    // 名称冲突等不可重试错误（ERROR_ACCESS_DENIED：另一 daemon
                    // 已服务同名管道）：fail-fast 退出，避免无限刷错误日志。
                    // bind 的试探实例已拦截绝大多数冲突，此处是兜底。
                    if e.kind() == io::ErrorKind::PermissionDenied {
                        return Err(PlatformError::Other(format!(
                            "IPC pipe {} unavailable (another daemon running?): {}",
                            IPC_PIPE_NAME, e
                        )));
                    }
                    error!("IPC accept error: {}", e);
                }
            }
        }
    }
}

// 评审历史（第十轮 P1×3 撤销）：此处曾有"Show 前台进程校验+重试"与
// "单调 pipe_id 认领（claim_focus）"两层仲裁，实测引入三类回归——①
// 同连接后续 Show 被 `>=` 拒绝吞掉；②连接断开与重试线程竞态永久占用
// 聚焦标记；③pipe_id 只反映连接建立顺序、不代表焦点新近度，单调拒绝会
// 冻结切回旧连接的徽标与候选窗光标跟踪。最终语义见 handle_ipc_client
// 的 Show 分支注释：Show 即焦点认领（最后 Show 胜出），Hide/UpdatePosition
// 按聚焦连接过滤

/// 处理单个 TSF DLL 连接：按帧协议循环读取请求并分发。
///
/// 并发说明：`PIPE_UNLIMITED_INSTANCES` 允许多个宿主进程中的 TSF DLL 同时
/// 连接，但所有连接共享同一个 `platform_rx`（Mutex<Receiver>）——
/// 引擎是单实例，多客户端的按键请求在 `send_engine` 的锁上自然串行化，
/// 这是当前设计的有意取舍（引擎状态本身不并发）。若未来出现引擎响应
/// 阻塞，会放大为所有客户端按键延迟，届时应改为按连接分队列。
fn handle_ipc_client(
    stream: IpcStream,
    engine_tx: Sender<EngineCommand>,
    platform_rx: Arc<Mutex<Receiver<SchemeResult>>>,
    ui_tx: Sender<UiCommand>,
    current: Arc<Mutex<RuntimeSettings>>,
    pipe_id: u64,
    focus_pipe_id: Arc<AtomicU64>,
) -> io::Result<()> {
    let mut reader = stream.try_clone()?;
    let mut writer = stream;
    // 帧载荷缓冲：循环外分配一次，循环内复用（热路径零分配）
    let mut buf = Vec::with_capacity(4096);

    let send_engine = |cmd: EngineCommand| -> io::Result<SchemeResult> {
        let rx = locked(&platform_rx);
        engine_tx
            .send(cmd)
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
        rx.recv()
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))
    };

    let write_response =
        |writer: &mut IpcStream, buf: &mut Vec<u8>, result: SchemeResult| -> io::Result<()> {
            let response: IpcResponse = result.into();
            send_response_buf(writer, &response, buf)
        };

    // 断开清理守卫：handle_ipc_client 的所有退出路径（含 `?` 写失败提前
    // 返回）都会经过 Drop。CAS 限定"仅本连接持有聚焦标记时清除"，避免与
    // 另一连接的并发 Show/断开交错时清掉对方的标记
    struct FocusGuard<'a> {
        focus_pipe_id: &'a AtomicU64,
        pipe_id: u64,
    }
    impl Drop for FocusGuard<'_> {
        fn drop(&mut self) {
            let _ = self.focus_pipe_id.compare_exchange(
                self.pipe_id,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
    let _focus_guard = FocusGuard {
        focus_pipe_id: &focus_pipe_id,
        pipe_id,
    };

    loop {
        let request = match read_request(&mut reader, &mut buf) {
            Ok(req) => req,
            // 客户端正常断开：命名管道上干净关闭表现为 BrokenPipe
            // （ReadFile 失败于 ERROR_BROKEN_PIPE/ERROR_NO_DATA），
            // UnexpectedEof 覆盖长度前缀读到 0 字节、ConnectionReset 覆盖帧中断开
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionReset
                ) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        debug!("handle_ipc_client start: req={:?}", discriminant(&request));
        match request {
            IpcRequest::KeyEvent { key, context } => {
                // 合并请求：先应用按键附带的上下文（整句补全），再处理按键，
                // 单次往返完成原先 SetContext + KeyEvent 两次写。
                if let Some(ctx) = context {
                    let _ = engine_tx.send(EngineCommand::SetContext(ctx));
                }
                let result = send_engine(EngineCommand::Key(key))?;
                if let SchemeResult::Committed { ref text, .. } = result {
                    let _ = ui_tx.send(UiCommand::CommitText(text.clone()));
                }
                write_response(&mut writer, &mut buf, result)?;
            }
            IpcRequest::SetContext(new_ctx) => {
                let _ = engine_tx.send(EngineCommand::SetContext(new_ctx));
            }
            IpcRequest::Reset => {
                let result = send_engine(EngineCommand::Reset)?;
                let _ = ui_tx.send(UiCommand::HideCandidates);
                write_response(&mut writer, &mut buf, result)?;
            }
            IpcRequest::UiCommand(ui_cmd) => {
                // 跨进程 Show/Hide 仲裁：连接间 IPC 到达顺序无保证。
                //
                // Show 即焦点认领（最后 Show 胜出）：ShowModeIndicator 仅由
                // 获得焦点的实例（OnSetFocus）或持有焦点的切换实例（模式
                // 切换收尾）发出，最后一次 Show 就是最新焦点归属，任何连接
                // 的 Show 都可认领。不按 pipe_id 大小设限（见上方说明）。
                // 残留竞态：近同时的"旧连接模式切换 Show"与"新连接焦点
                // Show"跨管道乱序时后到者胜出——最坏一次 transient 错认领，
                // 下一次任一实例的 Show 即纠正。
                //
                // 焦点失效/英文直输 Hide 与光标位置更新仅聚焦连接转发：
                // 旧窗口（已失焦）迟到的同类命令会隐藏/移走用户正输入窗口
                // 的徽标——这正是仲裁要防的乱序（评审 P2 修复）
                match &ui_cmd {
                    UiCommand::ShowModeIndicator { .. } => {
                        focus_pipe_id.store(pipe_id, Ordering::Release);
                        let _ = ui_tx.send(ui_cmd);
                    }
                    // 焦点失效隐藏 / 英文直输隐藏 / 光标位置更新都按聚焦
                    // 连接过滤：旧窗口（已失焦）迟到的同类命令会隐藏新
                    // 窗口正在使用的徽标、或把徽标写回旧窗口的过期光标
                    // 位置（跨管道到达顺序无保证）
                    UiCommand::HideModeIndicator { .. } | UiCommand::UpdatePosition { .. } => {
                        if focus_pipe_id.load(Ordering::Acquire) == pipe_id {
                            let _ = ui_tx.send(ui_cmd);
                        }
                    }
                    _ => {
                        let _ = ui_tx.send(ui_cmd);
                    }
                }
            }
            IpcRequest::GetSettings => {
                let settings = *locked(&current);
                let response = IpcResponse::Settings {
                    scheme_id: settings.scheme_id,
                    theme: settings.theme,
                    english: settings.english,
                    auto_switch: settings.auto_switch,
                };
                send_response_buf(&mut writer, &response, &mut buf)?;
            }
        }
        debug!("handle_ipc_client end");
    }
}
