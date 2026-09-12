use super::super::ipc::{IpcRequest, IpcResponse, read_response, send_request};
use super::auto_switch::{AutoSwitchEditSession, SuggestionReadSession, apply_auto_mode_toggle};
use super::caret::LayoutChangeEditSession;
use super::commit::{CancelCompositionEditSession, CommitCompositionEditSession};
use super::dll_release;
use super::hook::{
    clear_foreground_thread, register_service, set_foreground_thread, unregister_service,
};
use super::key_event::{KeyHandlerEditSession, virtual_key_to_key_event};
use super::langbar::BlackHoleLangBarItem;
use super::{ServiceInner, ensure_ipc_connection, send_ui_command_inner};
use black_hole_shared::{KeyEvent, KeyState, ModeSuggestion, UiCommand};
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::{E_UNEXPECTED, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_LCONTROL, VK_RCONTROL,
};
use windows::Win32::UI::TextServices::{
    GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION, GUID_COMPARTMENT_KEYBOARD_OPENCLOSE,
    ITfCompartmentMgr, ITfComposition, ITfCompositionSink, ITfCompositionSink_Impl, ITfContext,
    ITfContextView, ITfDocumentMgr, ITfEditSession, ITfKeyEventSink, ITfKeyEventSink_Impl,
    ITfKeystrokeMgr, ITfLangBarItem, ITfLangBarItemMgr, ITfSource, ITfTextInputProcessor,
    ITfTextInputProcessor_Impl, ITfTextLayoutSink, ITfTextLayoutSink_Impl, ITfThreadMgr,
    ITfThreadMgrEventSink, ITfThreadMgrEventSink_Impl, TF_CONVERSIONMODE_ALPHANUMERIC,
    TF_CONVERSIONMODE_NATIVE, TF_ES_READ, TF_ES_READWRITE, TF_ES_SYNC, TF_LBI_ICON, TF_LC_CHANGE,
    TfLayoutCode,
};
use windows_core::{BOOL, GUID, Interface, Ref, Result, implement};

/// 英文模式自动切换评估去重窗口：同一按键在 OnTestKeyDown 评估后，
/// 该时间窗内 OnKeyDown 收到同键视为同一次按下，跳过重复评估
/// （部分应用对同一按键两个回调都会调用）。
const ENG_EVAL_DEDUP_WINDOW: Duration = Duration::from_millis(200);

/// 按键事件是否为可输入字符（ASCII 字母/数字/标点）。
/// OnTestKeyDown/OnKeyDown 的按键拦截判定与英文模式自动切换评估门控共用。
fn is_input_char_event(evt: &KeyEvent) -> bool {
    if evt.key.len() != 1 {
        return false;
    }
    let Some(ch) = evt.key.chars().next() else {
        return false;
    };
    ch.is_ascii_alphanumeric() || ch.is_ascii_punctuation()
}

/// 英文模式自动切换的按键级决策结果（由 [`decide_english_auto_switch`] 产生）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EnglishEvalDecision {
    /// 是否应用新 pic 刷新 context（pic 非空时无条件刷新，
    /// 防焦点切换后的陈旧 ITfContext；空 pic 保留既有有效上下文）。
    pub(crate) refresh_context: bool,
    /// 是否应执行英→中自动切换评估。
    pub(crate) evaluate: bool,
}

/// 英文模式按键是否应执行英→中自动切换评估（可测纯函数）。
///
/// 门控与 OnTestKeyDown/OnKeyDown 两个英文分支共用：
/// - 开关开启（auto_switch）
/// - 无进行中的合成（has_composition=false）
/// - 评估时 context 可用：pic 非空（将刷新）或既有 context 非空（空 pic 保留）
/// - 是字符输入键（is_input_key；Shift/方向键等非输入键直接放行）
/// - 本次按键未在 OnTestKeyDown 已评估过（already_evaluated，去重窗口）
///
/// 将决策从 COM 回调中提取为纯函数，便于单测覆盖刷新/空 pic 保留/去重
/// 三个关键场景；回调仅负责执行动作（刷新 context、请求评估会话）。
pub(crate) fn decide_english_auto_switch(
    auto_switch: bool,
    has_composition: bool,
    context_was_some: bool,
    pic_present: bool,
    is_input_key: bool,
    already_evaluated: bool,
) -> EnglishEvalDecision {
    let context_after = pic_present || context_was_some;
    EnglishEvalDecision {
        refresh_context: pic_present,
        evaluate: auto_switch
            && !has_composition
            && context_after
            && is_input_key
            && !already_evaluated,
    }
}

// ---------------------------------------------------------------------------
// COM object: BlackHoleTextService
// ---------------------------------------------------------------------------

#[implement(
    ITfTextInputProcessor,
    ITfKeyEventSink,
    ITfCompositionSink,
    ITfTextLayoutSink,
    ITfThreadMgrEventSink
)]
pub struct BlackHoleTextService {
    pub(crate) inner: Arc<Mutex<ServiceInner>>,
    /// Only the primary COM object created by the class factory should
    /// influence the DLL global reference count.  Temporary sink objects
    /// reused inside the same DLL must not touch it.
    pub(crate) track_ref_count: bool,
}

impl Default for BlackHoleTextService {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BlackHoleTextService {
    fn drop(&mut self) {
        if self.track_ref_count {
            dll_release();
        }
    }
}

impl BlackHoleTextService {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ServiceInner::new())),
            track_ref_count: true,
        }
    }

    /// Create a lightweight sink wrapper that shares the same inner state
    /// but does not affect the DLL reference count.
    pub(crate) fn new_for_sink(inner: Arc<Mutex<ServiceInner>>) -> Self {
        Self {
            inner,
            track_ref_count: false,
        }
    }

    /// 连接 daemon IPC 服务器。
    /// 在宿主进程线程上执行（Activate），不做 sleep 重试；失败后由按键
    /// 路径经限频重连（try_reconnect_ipc）恢复，避免阻塞宿主程序。
    fn connect_ipc(&self) -> Result<()> {
        if ensure_ipc_connection(&self.inner) {
            Ok(())
        } else {
            error!("connect_ipc: failed to connect to daemon");
            Err(E_UNEXPECTED.into())
        }
    }

    /// 向 daemon 查询当前的 scheme、theme、全局中英模式和自动切换开关，
    /// 并更新 ServiceInner，使托盘菜单能勾选正确的选项、跨进程（管理员/普通）
    /// 保持中英模式一致、自动切换开关设置热生效。
    /// 始终应用 daemon 当前中英模式（含英文）：英文模式下自动切换已由
    /// OnTestKeyDown 补设 context 后正常触发，无需再以"连接默认中文"兜底。
    fn sync_settings_from_daemon(&self) {
        sync_settings_from_daemon_inner(&self.inner);
    }

    /// Disconnect from the daemon IPC server and clean up state.
    fn disconnect_ipc(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.ipc_conn = None;
        inner.composition = None;
        inner.context_version += 1;
        inner.active = false;
    }

    /// Send a UI command to the daemon via IPC.
    fn send_ui_command(&self, cmd: UiCommand) {
        send_ui_command_inner(&self.inner, cmd);
    }

    /// Send Reset to the engine and end any active composition.
    /// `preserve_input` 为 true 时保留输入框中的文本（相当于上屏），否则清空取消。
    fn send_reset(&self, preserve_input: bool) {
        let (ctx_opt, client_id) = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(ref mut conn) = inner.ipc_conn {
                let request = IpcRequest::Reset;
                let _ = send_request(&mut conn.writer, &request);
                let _ = read_response(&mut conn.reader);
            }
            (inner.context.clone(), inner.client_id)
        };

        if let Some(ctx) = ctx_opt {
            let edit_session: ITfEditSession = if preserve_input {
                CommitCompositionEditSession {
                    inner_arc: self.inner.clone(),
                }
                .into()
            } else {
                CancelCompositionEditSession {
                    inner_arc: self.inner.clone(),
                }
                .into()
            };
            let _ = unsafe {
                ctx.RequestEditSession(client_id, &edit_session, TF_ES_SYNC | TF_ES_READWRITE)
            };
        }

        self.send_ui_command(UiCommand::HideCandidates);
    }

    /// Check whether an active composition exists.
    fn is_composing(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner
            .composition
            .as_ref()
            .is_some_and(|c| unsafe { c.GetRange().is_ok() })
    }

    /// 英文→中文自动切换：英文模式的 OnTestKeyDown 中每个按键调用一次。
    /// 通过同步只读编辑会话读取光标周围文本并评估；命中中文语境则切回中文
    /// 并完成收尾（重置引擎、同步 compartment、刷新语言栏、上报临时模式）。
    /// 返回 true 表示已切换到中文，本次按键应按中文模式继续走原有消费逻辑。
    fn try_auto_switch_to_chinese(&self) -> bool {
        let (ctx, client_id) = {
            let inner = self.inner.lock().unwrap();
            // remote/锁屏等场景下 GetFocus 为 null、context 未设置：跳过评估。
            let Some(ctx) = inner.context.clone() else {
                return false;
            };
            (ctx, inner.client_id)
        };

        let result = Arc::new(Mutex::new(None));
        let session = AutoSwitchEditSession {
            inner_arc: self.inner.clone(),
            result: result.clone(),
        };
        let edit_session: ITfEditSession = session.into();
        // 只读同步会话；请求失败（文档不可用等）视为无信号，跳过。
        let hr =
            unsafe { ctx.RequestEditSession(client_id, &edit_session, TF_ES_SYNC | TF_ES_READ) };
        if let Err(e) = hr {
            debug!("auto-switch edit session request failed: {}", e);
            return false;
        }

        let switched = *result.lock().unwrap();
        switched.is_some_and(|target| {
            apply_auto_mode_toggle(&self.inner, target);
            // 此场景 target 必为 false（英文→中文）；!target 即"已切回中文"
            !target
        })
    }

    /// 构造英文→中文自动切换决策，并依据决策刷新 context（若 pic 非空）。
    /// 调用方仍需根据 decision.evaluate 决定是否调用 try_auto_switch_to_chinese。
    fn evaluate_english_auto_switch(
        &self,
        pic: &Ref<'_, ITfContext>,
        wparam: WPARAM,
        lparam: LPARAM,
        vk: VIRTUAL_KEY,
        already_evaluated: bool,
    ) -> EnglishEvalDecision {
        let inner = self.inner.lock().unwrap();
        let pic_present = (*pic).to_owned().is_some();
        let is_input_key = virtual_key_to_key_event(vk, wparam, lparam, KeyState::Press)
            .is_some_and(|evt| is_input_char_event(&evt));
        let decision = decide_english_auto_switch(
            inner.auto_switch,
            inner.composition.is_some(),
            inner.context.is_some(),
            pic_present,
            is_input_key,
            already_evaluated,
        );
        drop(inner);
        if decision.refresh_context {
            self.inner.lock().unwrap().context = (*pic).to_owned();
        }
        decision
    }

    /// 采样当前语境建议：手动 Ctrl 切换时作为自动切换状态机的锁定基线。
    /// 通过同步只读编辑会话读取光标周围文本（TSF 文本存储取不到时按无信号
    /// 处理，UIA 回退已移除），读取失败或无信号返回 Neutral（Neutral 基线下
    /// 任一有效建议即视为语境变化，自动解锁恢复自动切换，不会困住手动选择）。
    fn sample_current_suggestion(&self) -> ModeSuggestion {
        let (ctx, client_id) = {
            let inner = self.inner.lock().unwrap();
            // remote/锁屏等场景下 GetFocus 为 null、context 未设置：无信号
            match inner.context.clone() {
                Some(ctx) => (ctx, inner.client_id),
                None => return ModeSuggestion::Neutral,
            }
        };

        let suggestion = Arc::new(Mutex::new(ModeSuggestion::Neutral));
        let session = SuggestionReadSession {
            inner_arc: self.inner.clone(),
            suggestion: suggestion.clone(),
        };
        let edit_session: ITfEditSession = session.into();
        // 只读同步会话；请求失败（文档不可用等）视为无信号
        let hr =
            unsafe { ctx.RequestEditSession(client_id, &edit_session, TF_ES_SYNC | TF_ES_READ) };
        if let Err(e) = hr {
            debug!("suggestion-read edit session request failed: {}", e);
            return ModeSuggestion::Neutral;
        }
        *suggestion.lock().unwrap()
    }

    /// 中英文模式切换后的收尾工作：结束进行中的 composition、
    /// 重置引擎状态、同步系统输入法状态并刷新语言栏图标。
    /// 切换到英文模式时保留输入框中的内容（上屏），切回中文时清空取消。
    fn on_input_mode_toggled(&self, english: bool) {
        info!(
            "Input mode toggled: {}",
            if english { "英文" } else { "中文" }
        );
        // 结束未完成的输入并重置引擎：切英文时保留输入框内容，切中文时取消。
        self.send_reset(english);
        // 写入系统键盘 compartment，供其它应用感知当前输入法状态。
        self.sync_input_mode_compartments(english);
        let sink = {
            let inner = self.inner.lock().unwrap();
            inner.langbar_item_sink.clone()
        };
        if let Some(sink) = sink {
            let _ = unsafe { sink.OnUpdate(TF_LBI_ICON) };
        }
    }

    /// 将当前中英文模式写入 TSF 键盘 compartment，使系统输入指示器及
    /// 其它应用（通过 ITfCompartmentMgr / IMM 兼容层）能感知输入法状态：
    /// - `GUID_COMPARTMENT_KEYBOARD_OPENCLOSE`：中文=1（打开），英文=0（关闭）
    /// - `GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION`：
    ///   中文=`TF_CONVERSIONMODE_NATIVE`，英文=`TF_CONVERSIONMODE_ALPHANUMERIC`
    ///
    /// compartment 按线程管理器独立维护，与 `mode_switch` 的线程级语义一致。
    fn sync_input_mode_compartments(&self, english: bool) {
        let (thread_mgr, client_id) = {
            let inner = self.inner.lock().unwrap();
            (inner.thread_mgr.clone(), inner.client_id)
        };
        let Some(tm) = thread_mgr else { return };
        let Ok(mgr) = tm.cast::<ITfCompartmentMgr>() else {
            return;
        };

        if let Ok(comp) = unsafe { mgr.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_OPENCLOSE) } {
            let value = variant_i32(if english { 0 } else { 1 });
            if let Err(e) = unsafe { comp.SetValue(client_id, &value) } {
                warn!("Failed to set OPENCLOSE compartment: {}", e);
            }
        }

        if let Ok(comp) =
            unsafe { mgr.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION) }
        {
            let mode = if english {
                TF_CONVERSIONMODE_ALPHANUMERIC
            } else {
                TF_CONVERSIONMODE_NATIVE
            };
            let value = variant_i32(mode as i32);
            if let Err(e) = unsafe { comp.SetValue(client_id, &value) } {
                warn!("Failed to set INPUTMODE_CONVERSION compartment: {}", e);
            }
        }
    }
}

/// sync_settings_from_daemon 的自由函数版本：供 IPC 重连成功等无 service
/// 实例上下文的路径复用（语义与该方法一致）。始终应用 daemon 当前中英模式。
pub(crate) fn sync_settings_from_daemon_inner(inner_arc: &Arc<Mutex<ServiceInner>>) {
    let settings = {
        let mut inner = inner_arc.lock().unwrap();
        let Some(ref mut conn) = inner.ipc_conn else {
            return;
        };
        let request = IpcRequest::GetSettings;
        if send_request(&mut conn.writer, &request).is_err() {
            return;
        }
        let Ok(IpcResponse::Settings {
            scheme_id,
            theme,
            english,
            auto_switch,
        }) = read_response(&mut conn.reader)
        else {
            return;
        };
        inner.current_scheme = scheme_id;
        inner.current_theme = theme;
        inner.auto_switch = auto_switch;
        info!(
            "Synced settings from daemon: scheme={:?}, theme={:?}, english={}, auto_switch={}",
            scheme_id, theme, english, auto_switch
        );
        english
    };
    // 先释放 inner 锁再应用模式切换：apply_input_mode_toggle 内部会重新加锁。
    let changed = {
        let mut inner = inner_arc.lock().unwrap();
        inner.mode_switch.set_english(settings).is_some()
    };
    if changed {
        info!(
            "Input mode synced from daemon: {}",
            if settings { "英文" } else { "中文" }
        );
        apply_input_mode_toggle(inner_arc, settings);
    }
}

/// 语言栏菜单入口触发的模式切换收尾：与快捷键路径共用同一套逻辑
/// （取消 composition、重置引擎、同步 compartment、刷新语言栏图标）。
pub(crate) fn apply_input_mode_toggle(inner: &Arc<Mutex<ServiceInner>>, english: bool) {
    let svc = BlackHoleTextService::new_for_sink(inner.clone());
    svc.on_input_mode_toggled(english);
}

/// 构造 VT_I4 类型的 VARIANT（windows crate 的 Win32 VARIANT 未提供 From<i32>）
fn variant_i32(v: i32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: v },
            }),
        },
    }
}

/// 判断虚拟键是否为 Ctrl（含左右键）。
fn is_ctrl_key(vk: VIRTUAL_KEY) -> bool {
    vk == VK_CONTROL || vk == VK_LCONTROL || vk == VK_RCONTROL
}

// ---------------------------------------------------------------------------
// ITfTextInputProcessor
// ---------------------------------------------------------------------------

impl ITfTextInputProcessor_Impl for BlackHoleTextService_Impl {
    fn Activate(&self, ptim: Ref<'_, ITfThreadMgr>, tid: u32) -> Result<()> {
        info!("BlackHoleTextService::Activate called tid={}", tid);
        let mut inner = self.inner.lock().unwrap();
        inner.thread_mgr = ptim.to_owned();
        inner.client_id = tid;
        inner.active = true;
        drop(inner);

        let tm = ptim.to_owned().ok_or(E_UNEXPECTED)?;
        if let Ok(keystroke_mgr) = tm.cast::<ITfKeystrokeMgr>() {
            let sink = BlackHoleTextService::new_for_sink(self.inner.clone());
            let sink_iface: ITfKeyEventSink = sink.into();
            unsafe { keystroke_mgr.AdviseKeyEventSink(tid, &sink_iface, true) }?;
        }

        if let Ok(source) = tm.cast::<ITfSource>() {
            let sink = BlackHoleTextService::new_for_sink(self.inner.clone());
            let sink_iface: ITfThreadMgrEventSink = sink.into();
            if let Ok(cookie) = unsafe {
                source.AdviseSink(&<ITfThreadMgrEventSink as Interface>::IID, &sink_iface)
            } {
                let mut inner = self.inner.lock().unwrap();
                inner.thread_mgr_event_sink_cookie = Some(cookie);
            }
        }

        // 注册语言栏项（任务栏输入法指示器左侧的专属托盘）
        match tm.cast::<ITfLangBarItemMgr>() {
            Ok(langbar_mgr) => {
                let item = BlackHoleLangBarItem::new(self.inner.clone());
                let item_iface: ITfLangBarItem = item.into();
                match unsafe { langbar_mgr.AddItem(&item_iface) } {
                    Ok(()) => {
                        let mut inner = self.inner.lock().unwrap();
                        inner.langbar_item = Some(item_iface);
                        info!("Language bar item registered");
                    }
                    Err(e) => {
                        warn!("Failed to add language bar item: {}", e);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to obtain ITfLangBarItemMgr: {}", e);
            }
        }

        self.connect_ipc()?;

        // 注册全局键盘钩子（Ctrl 切换用）：Chrome 等应用不把修饰键事件转发给
        // TSF，需通过 WH_KEYBOARD_LL 直接监听物理按键。
        register_service(unsafe { GetCurrentThreadId() }, self.inner.clone());

        // 连接时同步 daemon 当前设置（含中英模式），并同步系统 compartment，
        // 使托盘菜单勾选正确、跨进程（管理员/普通）模式保持一致。
        // 不再以"连接默认中文"兜底：英文模式下自动切换已由 OnTestKeyDown
        // 补设 context 后正常触发，连接时即使处于英文模式也能自动切回中文。
        self.sync_settings_from_daemon();
        let english = self.inner.lock().unwrap().mode_switch.is_english();
        self.sync_input_mode_compartments(english);

        Ok(())
    }

    fn Deactivate(&self) -> Result<()> {
        // 先取出需要注销所需的数据，避免在持有 inner 锁时调用 COM 方法。
        // COM 方法可能回调到同一线程的其它接口方法（如 UnadviseSink），
        // 导致 std::sync::Mutex 重入死锁。
        let (thread_mgr, client_id, thread_mgr_event_cookie, langbar_item) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.thread_mgr.clone(),
                inner.client_id,
                inner.thread_mgr_event_sink_cookie,
                inner.langbar_item.clone(),
            )
        };

        if let Some(ref tm) = thread_mgr
            && let Ok(keystroke_mgr) = tm.cast::<ITfKeystrokeMgr>()
        {
            let _ = unsafe { keystroke_mgr.UnadviseKeyEventSink(client_id) };
        }
        if let Some(ref tm) = thread_mgr
            && let Ok(source) = tm.cast::<ITfSource>()
            && let Some(cookie) = thread_mgr_event_cookie
        {
            let _ = unsafe { source.UnadviseSink(cookie) };
        }
        if let Some(ref tm) = thread_mgr
            && let Ok(langbar_mgr) = tm.cast::<ITfLangBarItemMgr>()
            && let Some(ref item) = langbar_item
        {
            let _ = unsafe { langbar_mgr.RemoveItem(item) };
            info!("Language bar item removed");
        }

        self.send_reset(false);
        self.disconnect_ipc();
        unregister_service(unsafe { GetCurrentThreadId() });
        let mut inner = self.inner.lock().unwrap();
        inner.context = None;
        inner.thread_mgr = None;
        inner.client_id = 0;
        inner.thread_mgr_event_sink_cookie = None;
        inner.langbar_item = None;
        inner.langbar_item_sink = None;
        inner.langbar_item_sink_cookie = 0;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ITfKeyEventSink
// ---------------------------------------------------------------------------

impl ITfKeyEventSink_Impl for BlackHoleTextService_Impl {
    fn OnSetFocus(&self, fforeground: BOOL) -> Result<()> {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.last_caret_pos = None;
            inner.context_version += 1;
        }
        // 记录 TSF 输入焦点：WebView2 等多进程应用的 IME 承载进程与宿主窗口
        // 进程不同，GetForegroundWindow 不可靠，钩子依赖此处的焦点状态判定前台。
        if fforeground.as_bool() {
            set_foreground_thread(unsafe { GetCurrentThreadId() });
            // 获得焦点时从 daemon 同步全局中英模式：管理员/普通进程
            // 各自持有本地状态，切换进程时以此保持一致。始终应用 daemon
            // 当前模式（含英文）：英文模式下自动切换已由 OnTestKeyDown
            // 补设 context 后正常触发，无需以"连接默认中文"跳过。
            self.sync_settings_from_daemon();
        } else {
            clear_foreground_thread();
        }
        if !fforeground.as_bool() {
            self.send_reset(false);
        }
        Ok(())
    }

    fn OnTestKeyDown(
        &self,
        pic: Ref<'_, ITfContext>,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Result<BOOL> {
        let vk = VIRTUAL_KEY(wparam.0 as u16);
        debug!(
            "OnTestKeyDown: vk=0x{:04X} ctrl_held={}",
            vk.0,
            unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0
        );

        // Ctrl 键：标记切换候选（TSF 路径；与全局钩子共用状态机，
        // 防双切由 OnTestKeyUp 中的 hook_toggled 标志协调）。
        // 永远不拦截 Ctrl 本身，保证 Ctrl+C 等组合键正常工作。
        if is_ctrl_key(vk) {
            let mut inner = self.inner.lock().unwrap();
            // 新一轮 Ctrl 按下：重置抑制标志，避免上次残留（如钩子切换后
            // 本路径从未收到 keyup）抑制本次合法切换。
            inner.hook_toggled = false;
            inner.mode_switch.ctrl_pressed();
            inner.last_key_event = None;
            return Ok(BOOL(0));
        }

        // 按住 Ctrl 期间按下其他键（如 Ctrl+C、Ctrl+Shift），取消切换候选。
        if unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0 {
            let mut inner = self.inner.lock().unwrap();
            inner.mode_switch.other_key_pressed(true);
        }

        // 英文模式下不拦截任何按键；若开启"根据光标周围文本自动切换"，
        // 先评估一次：命中中文语境则切回中文，本次按键继续走下方原有中文模式
        // 的消费逻辑（与 Linux 侧 maybe_auto_switch_mode 后再判英文的语义一致）。
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.mode_switch.is_english() {
                inner.last_key_event = None;
                drop(inner);
                let decision = self.evaluate_english_auto_switch(&pic, wparam, lparam, vk, false);
                if !decision.evaluate {
                    return Ok(BOOL(0));
                }
                // 记录本次评估的按键与时间，供 OnKeyDown 去重：
                // 部分应用对同一按键两个回调都会调用，避免同一按键双重评估
                self.inner.lock().unwrap().last_eng_eval_key = Some((wparam.0, Instant::now()));
                if !self.try_auto_switch_to_chinese() {
                    return Ok(BOOL(0));
                }
            }
        }

        if let Some(evt) = virtual_key_to_key_event(vk, wparam, lparam, KeyState::Press) {
            let is_composing = self.is_composing();
            let is_input_char = is_input_char_event(&evt);
            let intercept = if is_composing {
                matches!(
                    evt.key.as_str(),
                    "Backspace"
                        | "Enter"
                        | "Space"
                        | "Tab"
                        | "ArrowLeft"
                        | "ArrowRight"
                        | "ArrowUp"
                        | "ArrowDown"
                        // Esc 仅合成中拦截：取消输入（结束合成、隐藏候选窗），
                        // 不透传给宿主应用——透传会导致焦点转移、候选窗被系统收起
                        | "Escape"
                ) || is_input_char
            } else {
                is_input_char
            };

            if intercept {
                let mut inner = self.inner.lock().unwrap();
                inner.last_key_event = Some((wparam, lparam, evt));
                return Ok(BOOL(1));
            }
        }
        let mut inner = self.inner.lock().unwrap();
        inner.last_key_event = None;
        Ok(BOOL(0))
    }

    fn OnTestKeyUp(
        &self,
        _pic: Ref<'_, ITfContext>,
        wparam: WPARAM,
        _lparam: LPARAM,
    ) -> Result<BOOL> {
        let vk = VIRTUAL_KEY(wparam.0 as u16);
        debug!(
            "OnTestKeyUp: vk=0x{:04X} ctrl_held={}",
            vk.0,
            unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0
        );

        // Ctrl 松开：若全局钩子已抢先完成切换（hook_toggled=true）则跳过，
        // 避免与钩子路径重复切换；否则由 TSF 路径执行切换。
        // - Chrome 等不把修饰键事件转发给 TSF 的应用收不到此回调，由钩子切换；
        // - 设置面板等钩子回调不可靠（安装线程无消息循环）的应用走本路径。
        if is_ctrl_key(vk) {
            let toggled = {
                let mut inner = self.inner.lock().unwrap();
                if inner.hook_toggled {
                    inner.hook_toggled = false;
                    None
                } else {
                    inner.mode_switch.ctrl_released()
                }
            };
            if let Some(english) = toggled {
                // 用户手动切换：即时采样当前语境建议作为锁定基线，
                // 同语境的自动建议不再撤销本次选择（陈旧基线会导致
                // 下个按键即被自动切换撤销，故必须即时采样）
                let suggestion = self.sample_current_suggestion();
                self.inner.lock().unwrap().auto_mode.lock_manual(suggestion);
                self.on_input_mode_toggled(english);
                // 上报 daemon 持久化并更新全局状态，供其它进程同步
                self.send_ui_command(UiCommand::SetInputMode(english));
            }
        }
        Ok(BOOL(0))
    }

    fn OnKeyDown(&self, pic: Ref<'_, ITfContext>, wparam: WPARAM, lparam: LPARAM) -> Result<BOOL> {
        // 英文模式下不处理任何按键；若开启自动切换，先评估一次英→中。
        // 部分应用（如 WebView2）不调用 OnTestKeyDown 而直接调用 OnKeyDown，
        // 评估必须在此补做，否则该类应用中英→中永远不触发；命中切回中文后
        // 继续走下方中文模式流程，消费本次按键（与 OnTestKeyDown 路径语义一致）。
        {
            let inner = self.inner.lock().unwrap();
            if inner.mode_switch.is_english() {
                // 同一按键已在 OnTestKeyDown 评估过（时间窗内同键视为同一次按下），
                // 直接放行：部分应用对同一按键两个回调都会调用，避免双重评估
                let already_evaluated = match &inner.last_eng_eval_key {
                    Some((w, t)) => *w == wparam.0 && t.elapsed() < ENG_EVAL_DEDUP_WINDOW,
                    None => false,
                };
                drop(inner);
                let decision = self.evaluate_english_auto_switch(
                    &pic,
                    wparam,
                    lparam,
                    VIRTUAL_KEY(wparam.0 as u16),
                    already_evaluated,
                );
                if !decision.evaluate || !self.try_auto_switch_to_chinese() {
                    return Ok(BOOL(0));
                }
            }
        }

        let cached_event = {
            let mut inner = self.inner.lock().unwrap();
            inner.last_key_event.take().and_then(|(cw, cl, evt)| {
                if cw.0 == wparam.0 && cl.0 == lparam.0 {
                    Some(evt)
                } else {
                    None
                }
            })
        };

        let key_event = match cached_event {
            Some(evt) => evt,
            None => {
                let vk = VIRTUAL_KEY(wparam.0 as u16);
                match virtual_key_to_key_event(vk, wparam, lparam, KeyState::Press) {
                    Some(evt) => evt,
                    None => return Ok(BOOL(0)),
                }
            }
        };

        // 非合成态且非字符键时放行给应用（BOOL(0)）。合成中的 Esc 不在此放行：
        // 它会继续走编辑会话送引擎处理，引擎返回 Cancelled 后由 apply_result 取消
        // 合成并隐藏候选窗；因 Esc 已被 OnTestKeyDown 拦截而未透传给应用，焦点不转移。
        // 非合成态的 Esc 仍放行给应用（不吞掉应用自身的 Esc）。
        let composing = self.is_composing();
        if !composing && !is_input_char_event(&key_event) {
            return Ok(BOOL(0));
        }

        {
            let mut inner = self.inner.lock().unwrap();
            // 无条件保持 context 最新：TSF 不保证跨按键返回同一接口指针，
            // 以指针比较门控刷新会在指针不稳定时保留过期 context（导致提交/
            // 插入作用到错误上下文）。
            inner.context = pic.to_owned();
        }

        // 中文→英文自动切换标志：编辑会话内发生切换时置位（见 key_event.rs）
        let auto_switched = Arc::new(AtomicBool::new(false));
        let session = KeyHandlerEditSession {
            service: self.inner.clone(),
            key_event,
            auto_switched: auto_switched.clone(),
        };
        let edit_session: ITfEditSession = session.into();

        let inner = self.inner.lock().unwrap();
        let client_id = inner.client_id;
        drop(inner);

        let ctx = pic.to_owned().ok_or(E_UNEXPECTED)?;
        let hr = unsafe {
            ctx.RequestEditSession(client_id, &edit_session, TF_ES_SYNC | TF_ES_READWRITE)?
        };

        // 会话内触发了中文→英文自动切换：该按键未送入引擎，
        // 返回不消费将其放行给应用，按英文直输处理。
        if auto_switched.load(Ordering::SeqCst) {
            return Ok(BOOL(0));
        }

        if hr.is_ok() { Ok(BOOL(1)) } else { Ok(BOOL(0)) }
    }

    fn OnKeyUp(&self, _pic: Ref<'_, ITfContext>, _wparam: WPARAM, _lparam: LPARAM) -> Result<BOOL> {
        Ok(BOOL(0))
    }

    fn OnPreservedKey(&self, _pic: Ref<'_, ITfContext>, _rguid: *const GUID) -> Result<BOOL> {
        Ok(BOOL(0))
    }
}

// ---------------------------------------------------------------------------
// ITfCompositionSink
// ---------------------------------------------------------------------------

impl ITfCompositionSink_Impl for BlackHoleTextService_Impl {
    fn OnCompositionTerminated(
        &self,
        _ecwrite: u32,
        _pcomposition: Ref<'_, ITfComposition>,
    ) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.composition = None;
        // 应用主动终止合成（Esc/点击等绕过 Commit/Cancel 编辑会话）：
        // 与 commit.rs 的合成结束路径保持一致，同步清空光标位置并作废缓存
        inner.last_caret_pos = None;
        inner.context_version += 1;
        if let Some(cookie) = inner.layout_sink_cookie.take()
            && let Some(ref ctx) = inner.context
            && let Ok(source) = ctx.cast::<ITfSource>()
        {
            let _ = unsafe { source.UnadviseSink(cookie) };
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ITfTextLayoutSink
// ---------------------------------------------------------------------------

impl ITfTextLayoutSink_Impl for BlackHoleTextService_Impl {
    fn OnLayoutChange(
        &self,
        pic: Ref<'_, ITfContext>,
        lcode: TfLayoutCode,
        _pview: Ref<'_, ITfContextView>,
    ) -> Result<()> {
        if lcode != TF_LC_CHANGE {
            return Ok(());
        }

        {
            let inner = self.inner.lock().unwrap();
            let current_ctx = match &inner.context {
                Some(c) => c,
                None => return Ok(()),
            };
            if let Some(pic_ctx) = pic.to_owned() {
                if pic_ctx.as_raw() != current_ctx.as_raw() {
                    return Ok(());
                }
            } else {
                return Ok(());
            }
        }

        let session = LayoutChangeEditSession {
            inner_arc: self.inner.clone(),
        };
        let edit_session: ITfEditSession = session.into();

        let inner = self.inner.lock().unwrap();
        let client_id = inner.client_id;
        drop(inner);

        let ctx = pic.to_owned().ok_or(E_UNEXPECTED)?;
        let _ =
            unsafe { ctx.RequestEditSession(client_id, &edit_session, TF_ES_SYNC | TF_ES_READ) };

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ITfThreadMgrEventSink
// ---------------------------------------------------------------------------

impl ITfThreadMgrEventSink_Impl for BlackHoleTextService_Impl {
    fn OnInitDocumentMgr(&self, _pdocmgr: Ref<'_, ITfDocumentMgr>) -> Result<()> {
        Ok(())
    }

    fn OnUninitDocumentMgr(&self, _pdocmgr: Ref<'_, ITfDocumentMgr>) -> Result<()> {
        Ok(())
    }

    fn OnSetFocus(
        &self,
        _pdocmgrfocus: Ref<'_, ITfDocumentMgr>,
        _pdocmgrprevfocus: Ref<'_, ITfDocumentMgr>,
    ) -> Result<()> {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.last_caret_pos = None;
            inner.context_version += 1;
        }
        self.send_reset(false);
        Ok(())
    }

    fn OnPushContext(&self, _pic: Ref<'_, ITfContext>) -> Result<()> {
        Ok(())
    }

    fn OnPopContext(&self, _pic: Ref<'_, ITfContext>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::IpcConnection;
    use super::*;
    use black_hole_shared::{SchemeId, Theme};
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    /// 启动一个 mock daemon：接受连接后循环响应请求。
    /// GetSettings 固定返回 english=true 的 Settings，其余请求返回 Ignored。
    fn spawn_mock_daemon() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let resp = match serde_json::from_str::<IpcRequest>(line.trim()) {
                        Ok(IpcRequest::GetSettings) => IpcResponse::Settings {
                            scheme_id: SchemeId::Pinyin,
                            theme: Theme::Light,
                            english: true,
                            auto_switch: false,
                        },
                        _ => IpcResponse::Ignored,
                    };
                    let json = serde_json::to_string(&resp).unwrap();
                    writeln!(stream, "{}", json).unwrap();
                    stream.flush().unwrap();
                }
            }
        });
        addr
    }

    /// 构造连接到 mock daemon 的 ServiceInner（模拟"连接已建立"状态）。
    fn connect_to_mock(addr: std::net::SocketAddr) -> Arc<Mutex<ServiceInner>> {
        let stream = TcpStream::connect(addr).unwrap();
        let mut inner = ServiceInner::new();
        inner.ipc_conn = Some(IpcConnection {
            writer: stream.try_clone().unwrap(),
            reader: BufReader::new(stream),
        });
        Arc::new(Mutex::new(inner))
    }

    /// 模拟 try_reconnect_ipc 建立连接：仅写入 ipc_conn。
    /// 连接/重连后由调用方执行 sync_settings_from_daemon_inner——
    /// 该函数始终应用 daemon 当前中英模式，无门控。
    fn establish_connection(inner_arc: &Arc<Mutex<ServiceInner>>, addr: std::net::SocketAddr) {
        let stream = TcpStream::connect(addr).unwrap();
        let mut inner = inner_arc.lock().unwrap();
        inner.ipc_conn = Some(IpcConnection {
            writer: stream.try_clone().unwrap(),
            reader: BufReader::new(stream),
        });
    }

    #[test]
    fn sync_settings_applies_daemon_english() {
        // 同步始终应用 daemon 当前模式：mock 返回 english=true → 本地英文
        let addr = spawn_mock_daemon();
        let inner_arc = connect_to_mock(addr);
        sync_settings_from_daemon_inner(&inner_arc);
        assert!(inner_arc.lock().unwrap().mode_switch.is_english());
    }

    #[test]
    fn connect_always_applies_daemon_mode() {
        // 全新连接也始终应用 daemon 当前模式（含英文），不再以"连接默认
        // 中文"兜底：英文模式下自动切换已由 OnTestKeyDown 补设 context
        // 后正常触发，连接时处于英文模式也能自动切回中文。
        let addr = spawn_mock_daemon();
        let inner_arc = Arc::new(Mutex::new(ServiceInner::new()));
        establish_connection(&inner_arc, addr);
        sync_settings_from_daemon_inner(&inner_arc);
        assert!(inner_arc.lock().unwrap().mode_switch.is_english());
    }

    #[test]
    fn reconnect_applies_daemon_current_mode() {
        // 会话中重连同样应用 daemon 当前模式（含英文），跨进程保持一致
        let addr = spawn_mock_daemon();
        let inner_arc = connect_to_mock(addr);
        sync_settings_from_daemon_inner(&inner_arc);
        assert!(inner_arc.lock().unwrap().mode_switch.is_english());
    }

    #[test]
    fn first_activate_failure_then_reconnect_applies_daemon_mode() {
        // 首次 Activate 连接失败（daemon 未就绪）不建立连接；之后按键路径
        // 重连成功并同步——始终应用 daemon 当前模式（mock 返回 english=true），
        // 无"门控被无连接同步提前消费"的问题（门控已随连接默认中文回退）。
        let addr = spawn_mock_daemon();
        let inner_arc = Arc::new(Mutex::new(ServiceInner::new()));

        // 首次 Activate 失败：无连接、未同步
        assert!(inner_arc.lock().unwrap().ipc_conn.is_none());

        // 按键路径 try_reconnect_ipc 重连成功：建立连接
        establish_connection(&inner_arc, addr);
        sync_settings_from_daemon_inner(&inner_arc);
        assert!(inner_arc.lock().unwrap().mode_switch.is_english());
    }

    #[test]
    fn decide_english_auto_switch_refreshes_context_when_pic_present() {
        // 场景一：pic 非空（有文档上下文的按键）→ 无条件刷新 context，
        // 且开关开启、无合成、字符输入键时评估通过
        let d = decide_english_auto_switch(
            true,  // auto_switch
            false, // has_composition
            false, // context_was_some（新实例 context 为 None）
            true,  // pic_present
            true,  // is_input_key
            false, // already_evaluated
        );
        assert!(d.refresh_context, "pic 非空应刷新 context");
        assert!(d.evaluate, "全门控满足应触发评估");
    }

    #[test]
    fn decide_english_auto_switch_keeps_context_when_pic_null() {
        // 场景二：pic 为空（无文档上下文事件）→ 不刷新（保留既有有效
        // 上下文），既有 context 非空时评估仍可通过——空指针不得覆盖
        // 导致自动切换失联
        let d = decide_english_auto_switch(
            true,  // auto_switch
            false, // has_composition
            true,  // context_was_some（既有有效上下文）
            false, // pic_present（空 pic）
            true,  // is_input_key
            false, // already_evaluated
        );
        assert!(!d.refresh_context, "空 pic 应保留既有 context，不刷新");
        assert!(d.evaluate, "空 pic 但既有 context 可用时仍应评估");

        // 空 pic 且无既有 context：无法评估
        let d2 = decide_english_auto_switch(true, false, false, false, true, false);
        assert!(!d2.evaluate, "空 pic 且无既有 context 时不得评估");
    }

    #[test]
    fn decide_english_auto_switch_dedup_window_suppresses_reeval() {
        // 场景三：同一按键已在 OnTestKeyDown 评估过（去重窗口内）→
        // OnKeyDown 收到同键不得重复评估
        let d = decide_english_auto_switch(true, false, true, true, true, true);
        assert!(!d.evaluate, "去重窗口内已评估的按键不得重复评估");
        // 仍应刷新 context（与 evaluate 解耦）
        assert!(d.refresh_context, "去重只抑制评估，不影响 context 刷新");

        // 去重窗口外（already_evaluated=false）恢复评估
        let d2 = decide_english_auto_switch(true, false, true, true, true, false);
        assert!(d2.evaluate, "去重窗口外应恢复评估");
    }

    #[test]
    fn decide_english_auto_switch_gate_conditions() {
        // 门控各条件单独不满足时均不评估：
        // 开关关闭 / 合成中 / 非字符输入键
        assert!(
            !decide_english_auto_switch(false, false, true, true, true, false).evaluate,
            "开关关闭不评估"
        );
        assert!(
            !decide_english_auto_switch(true, true, true, true, true, false).evaluate,
            "合成中不评估"
        );
        assert!(
            !decide_english_auto_switch(true, false, true, true, false, false).evaluate,
            "非字符输入键不评估"
        );
    }
}
