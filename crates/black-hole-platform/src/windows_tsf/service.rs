use super::super::ipc::{IpcRequest, IpcResponse};
use super::{ServiceInner, send_ui_command_inner};
use black_hole_shared::{KeyState, UiCommand};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{E_UNEXPECTED, LPARAM, WPARAM};
use windows::Win32::System::Variant::{VARIANT, VT_I4};
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
use windows_core::{BOOL, GUID, Interface, Ref, implement};

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
            super::dll_release();
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

    /// Connect to the daemon IPC server with retry mechanism.
    fn connect_ipc(&self) -> windows_core::Result<()> {
        let inner = self.inner.lock().unwrap();
        if inner.ipc_conn.is_some() {
            tracing::info!("connect_ipc: already connected");
            return Ok(());
        }
        drop(inner);

        let max_retries = 3;
        let mut retry_delay_ms = 500;

        for attempt in 1..=max_retries {
            match std::net::TcpStream::connect(super::super::ipc::IPC_SERVER_ADDR) {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    tracing::info!(
                        "connect_ipc: connected to daemon at {} (attempt {})",
                        super::super::ipc::IPC_SERVER_ADDR,
                        attempt
                    );
                    let reader =
                        std::io::BufReader::new(stream.try_clone().map_err(|_| E_UNEXPECTED)?);
                    let mut inner = self.inner.lock().unwrap();
                    inner.ipc_conn = Some(super::IpcConnection {
                        writer: stream,
                        reader,
                    });
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(
                        "connect_ipc: failed to connect to daemon (attempt {}/{}): {}",
                        attempt,
                        max_retries,
                        e
                    );
                    if attempt < max_retries {
                        std::thread::sleep(std::time::Duration::from_millis(retry_delay_ms));
                        retry_delay_ms = (retry_delay_ms * 2).min(3000);
                    }
                }
            }
        }

        tracing::error!(
            "connect_ipc: failed to connect to daemon after {} attempts",
            max_retries
        );
        Err(E_UNEXPECTED.into())
    }

    /// 向 daemon 查询当前的 scheme 和 theme 设置，并更新 ServiceInner，
    /// 使托盘菜单能勾选正确的选项。
    fn sync_settings_from_daemon(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(ref mut conn) = inner.ipc_conn {
            let request = IpcRequest::GetSettings;
            if super::super::ipc::send_request(&mut conn.writer, &request).is_ok()
                && let Ok(IpcResponse::Settings { scheme_id, theme }) =
                    super::super::ipc::read_response(&mut conn.reader)
            {
                inner.current_scheme = scheme_id;
                inner.current_theme = theme;
                tracing::info!(
                    "Synced settings from daemon: scheme={:?}, theme={:?}",
                    scheme_id,
                    theme
                );
            }
        }
    }

    /// Disconnect from the daemon IPC server and clean up state.
    fn disconnect_ipc(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.ipc_conn = None;
        inner.composition = None;
        inner.active = false;
    }

    /// Send a UI command to the daemon via IPC.
    fn send_ui_command(&self, cmd: UiCommand) {
        send_ui_command_inner(&self.inner, cmd);
    }

    /// Send Reset to the engine and cancel any active composition.
    fn send_reset(&self) {
        let (ctx_opt, client_id) = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(ref mut conn) = inner.ipc_conn {
                let request = super::super::ipc::IpcRequest::Reset;
                let _ = super::super::ipc::send_request(&mut conn.writer, &request);
                let _ = super::super::ipc::read_response(&mut conn.reader);
            }
            (inner.context.clone(), inner.client_id)
        };

        if let Some(ctx) = ctx_opt {
            let session = super::commit::CancelCompositionEditSession {
                inner_arc: self.inner.clone(),
            };
            let edit_session: ITfEditSession = session.into();
            let _ = unsafe {
                ctx.RequestEditSession(client_id, &edit_session, TF_ES_SYNC | TF_ES_READWRITE)
            };
        }

        self.send_ui_command(UiCommand::HideCandidates);
    }

    /// Check whether an active composition exists.
    fn is_composing(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        match &inner.composition {
            None => false,
            Some(c) => unsafe { c.GetRange().is_ok() },
        }
    }

    /// 中英文模式切换后的收尾工作：取消进行中的 composition、
    /// 重置引擎状态、同步系统输入法状态并刷新语言栏图标。
    fn on_input_mode_toggled(&self, english: bool) {
        tracing::info!(
            "Input mode toggled: {}",
            if english { "英文" } else { "中文" }
        );
        // 取消未完成的输入并重置引擎（两个方向均为幂等操作）。
        self.send_reset();
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
                tracing::warn!("Failed to set OPENCLOSE compartment: {}", e);
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
                tracing::warn!("Failed to set INPUTMODE_CONVERSION compartment: {}", e);
            }
        }
    }
}

/// 构造 VT_I4 类型的 VARIANT（windows crate 的 Win32 VARIANT 未提供 From<i32>）
fn variant_i32(v: i32) -> VARIANT {
    use std::mem::ManuallyDrop;
    use windows::Win32::System::Variant::{VARIANT_0, VARIANT_0_0, VARIANT_0_0_0};

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
    fn Activate(&self, ptim: Ref<'_, ITfThreadMgr>, tid: u32) -> windows_core::Result<()> {
        tracing::info!("BlackHoleTextService::Activate called tid={}", tid);
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
                let item = super::langbar::BlackHoleLangBarItem::new(self.inner.clone());
                let item_iface: ITfLangBarItem = item.into();
                match unsafe { langbar_mgr.AddItem(&item_iface) } {
                    Ok(()) => {
                        let mut inner = self.inner.lock().unwrap();
                        inner.langbar_item = Some(item_iface);
                        tracing::info!("Language bar item registered");
                    }
                    Err(e) => {
                        tracing::warn!("Failed to add language bar item: {}", e);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to obtain ITfLangBarItemMgr: {}", e);
            }
        }

        self.connect_ipc()?;

        // 连接成功后，向 daemon 查询当前设置，确保托盘菜单勾选正确。
        self.sync_settings_from_daemon();

        // 同步初始中英文模式到系统键盘 compartment（默认中文），
        // 使其它应用在 IME 激活后即可感知输入法状态。
        let english = self.inner.lock().unwrap().mode_switch.is_english();
        self.sync_input_mode_compartments(english);

        Ok(())
    }

    fn Deactivate(&self) -> windows_core::Result<()> {
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
            tracing::info!("Language bar item removed");
        }

        self.send_reset();
        self.disconnect_ipc();
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
    fn OnSetFocus(&self, fforeground: BOOL) -> windows_core::Result<()> {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.last_caret_pos = None;
        }
        if !fforeground.as_bool() {
            self.send_reset();
        }
        Ok(())
    }

    fn OnTestKeyDown(
        &self,
        _pic: Ref<'_, ITfContext>,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> windows_core::Result<BOOL> {
        let vk = VIRTUAL_KEY(wparam.0 as u16);

        // Ctrl 键：标记切换候选，松开时在 OnTestKeyUp 中切换中英文模式。
        // 永远不拦截 Ctrl 本身，保证 Ctrl+C 等组合键正常工作。
        if is_ctrl_key(vk) {
            let mut inner = self.inner.lock().unwrap();
            inner.mode_switch.ctrl_pressed();
            inner.last_key_event = None;
            return Ok(BOOL(0));
        }

        // 按住 Ctrl 期间按下其他键（如 Ctrl+C、Ctrl+Shift），取消切换候选。
        if unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0 {
            let mut inner = self.inner.lock().unwrap();
            inner.mode_switch.other_key_pressed(true);
        }

        // 英文模式下不拦截任何按键。
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.mode_switch.is_english() {
                inner.last_key_event = None;
                return Ok(BOOL(0));
            }
        }

        if let Some(evt) =
            super::key_event::virtual_key_to_key_event(vk, wparam, lparam, KeyState::Press)
        {
            let is_composing = self.is_composing();
            let is_input_char = evt.key.len() == 1 && {
                let ch = evt.key.chars().next().unwrap();
                ch.is_ascii_alphanumeric() || ch.is_ascii_punctuation()
            };
            let intercept = if is_composing {
                matches!(
                    evt.key.as_str(),
                    "Backspace"
                        | "Enter"
                        | "Space"
                        | "ArrowLeft"
                        | "ArrowRight"
                        | "ArrowUp"
                        | "ArrowDown"
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
    ) -> windows_core::Result<BOOL> {
        // Ctrl 松开时切换中英文模式。切换逻辑放在 OnTestKeyUp 而非 OnKeyUp：
        // 部分应用（如控制台宿主）只在 TestKeyUp 返回 TRUE 时才回调 OnKeyUp，
        // 而 OnTestKeyUp 对每次真实松开都会被调用且仅一次。
        let vk = VIRTUAL_KEY(wparam.0 as u16);
        if is_ctrl_key(vk) {
            let toggled = {
                let mut inner = self.inner.lock().unwrap();
                inner.mode_switch.ctrl_released()
            };
            if let Some(english) = toggled {
                self.on_input_mode_toggled(english);
            }
        }
        Ok(BOOL(0))
    }

    fn OnKeyDown(
        &self,
        pic: Ref<'_, ITfContext>,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> windows_core::Result<BOOL> {
        // 英文模式下不处理任何按键。
        {
            let inner = self.inner.lock().unwrap();
            if inner.mode_switch.is_english() {
                return Ok(BOOL(0));
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
                let vk = windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(wparam.0 as u16);
                match super::key_event::virtual_key_to_key_event(
                    vk,
                    wparam,
                    lparam,
                    KeyState::Press,
                ) {
                    Some(evt) => evt,
                    None => return Ok(BOOL(0)),
                }
            }
        };

        let composing = self.is_composing();
        if !composing {
            let is_input_char = key_event.key.len() == 1 && {
                let ch = key_event.key.chars().next().unwrap();
                ch.is_ascii_alphanumeric() || ch.is_ascii_punctuation()
            };
            if !is_input_char {
                return Ok(BOOL(0));
            }
        }

        {
            let mut inner = self.inner.lock().unwrap();
            inner.context = pic.to_owned();
        }

        let session = super::key_event::KeyHandlerEditSession {
            service: self.inner.clone(),
            key_event,
        };
        let edit_session: ITfEditSession = session.into();

        let inner = self.inner.lock().unwrap();
        let client_id = inner.client_id;
        drop(inner);

        let ctx = pic.to_owned().ok_or(E_UNEXPECTED)?;
        let hr = unsafe {
            ctx.RequestEditSession(client_id, &edit_session, TF_ES_SYNC | TF_ES_READWRITE)?
        };

        if hr.is_ok() { Ok(BOOL(1)) } else { Ok(BOOL(0)) }
    }

    fn OnKeyUp(
        &self,
        _pic: Ref<'_, ITfContext>,
        _wparam: WPARAM,
        _lparam: LPARAM,
    ) -> windows_core::Result<BOOL> {
        Ok(BOOL(0))
    }

    fn OnPreservedKey(
        &self,
        _pic: Ref<'_, ITfContext>,
        _rguid: *const GUID,
    ) -> windows_core::Result<BOOL> {
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
    ) -> windows_core::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.composition = None;
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
    ) -> windows_core::Result<()> {
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

        let session = super::caret::LayoutChangeEditSession {
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
    fn OnInitDocumentMgr(&self, _pdocmgr: Ref<'_, ITfDocumentMgr>) -> windows_core::Result<()> {
        Ok(())
    }

    fn OnUninitDocumentMgr(&self, _pdocmgr: Ref<'_, ITfDocumentMgr>) -> windows_core::Result<()> {
        Ok(())
    }

    fn OnSetFocus(
        &self,
        _pdocmgrfocus: Ref<'_, ITfDocumentMgr>,
        _pdocmgrprevfocus: Ref<'_, ITfDocumentMgr>,
    ) -> windows_core::Result<()> {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.last_caret_pos = None;
        }
        self.send_reset();
        Ok(())
    }

    fn OnPushContext(&self, _pic: Ref<'_, ITfContext>) -> windows_core::Result<()> {
        Ok(())
    }

    fn OnPopContext(&self, _pic: Ref<'_, ITfContext>) -> windows_core::Result<()> {
        Ok(())
    }
}
