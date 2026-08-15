use super::auto_switch::apply_auto_mode_toggle;
use super::caret::{
    get_caret_position_via_gui_thread_info, read_surrounding_text, read_surrounding_text_via_uia,
    truncate_for_log,
};
use super::commit::apply_result;
use super::{ServiceInner, try_reconnect_ipc};
use crate::ipc::{IpcRequest, read_response, send_request};
use black_hole_shared::{InputContext, KeyEvent, KeyState, Modifiers, suggest_input_mode};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, error, warn};
use windows::Win32::Foundation::{E_UNEXPECTED, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN,
    VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::TextServices::{ITfEditSession, ITfEditSession_Impl};
use windows_core::{BOOL, Error, Result, implement};

// External Win32 functions not provided by the windows crate
unsafe extern "system" {
    fn MapVirtualKeyW(uCode: u32, uMapType: u32) -> u32;
    fn GetKeyboardState(lpKeyState: *mut u8) -> BOOL;
    fn ToUnicode(
        wVirtKey: u32,
        wScanCode: u32,
        lpKeyState: *const u8,
        pwszBuff: *mut u16,
        cchBuff: i32,
        wFlags: u32,
    ) -> i32;
}

/// Convert a Win32 virtual-key code into our internal `KeyEvent` representation.
pub(crate) fn virtual_key_to_key_event(
    vk: VIRTUAL_KEY,
    _wparam: WPARAM,
    _lparam: LPARAM,
    state: KeyState,
) -> Option<KeyEvent> {
    let vk_val = vk.0 as u32;

    let scan_code = unsafe { MapVirtualKeyW(vk_val, 0) };
    let mut kbd_state = [0u8; 256];
    let mut wch = [0u16; 8];
    let key_char = if unsafe { GetKeyboardState(kbd_state.as_mut_ptr()) }.as_bool() {
        let len = unsafe {
            ToUnicode(
                vk_val,
                scan_code,
                kbd_state.as_ptr(),
                wch.as_mut_ptr(),
                wch.len() as i32,
                0,
            )
        };
        if len > 0 {
            let slice = &wch[..len as usize];
            char::decode_utf16(slice.iter().copied())
                .filter_map(|r| r.ok())
                .next()
        } else {
            None
        }
    } else {
        None
    };

    let key = match vk {
        VK_BACK => "Backspace".to_string(),
        VK_ESCAPE => "Escape".to_string(),
        VK_RETURN => "Enter".to_string(),
        VK_SPACE => "Space".to_string(),
        VK_TAB => "Tab".to_string(),
        VK_LEFT => "ArrowLeft".to_string(),
        VK_RIGHT => "ArrowRight".to_string(),
        VK_UP => "ArrowUp".to_string(),
        VK_DOWN => "ArrowDown".to_string(),
        _ => {
            if let Some(ch) = key_char {
                if !ch.is_ascii_alphanumeric() && !ch.is_ascii_punctuation() {
                    return None;
                }
                ch.to_string()
            } else if (0x30..=0x39).contains(&vk_val) {
                ((vk_val as u8 - 0x30 + b'0') as char).to_string()
            } else if (0x41..=0x5A).contains(&vk_val) {
                ((vk_val as u8 - 0x41 + b'a') as char).to_string()
            } else {
                return None;
            }
        }
    };

    let shift = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0;
    let ctrl = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
    let alt = unsafe { GetAsyncKeyState(0x12i32) } < 0;
    let capslock = (kbd_state[0x14] & 0x01) != 0;

    Some(KeyEvent {
        key,
        modifiers: Modifiers {
            shift,
            ctrl,
            alt,
            meta: false,
            capslock,
        },
        state,
    })
}

// ---------------------------------------------------------------------------
// EditSession for key handling
// ---------------------------------------------------------------------------

#[implement(ITfEditSession)]
pub(crate) struct KeyHandlerEditSession {
    pub(crate) service: Arc<Mutex<ServiceInner>>,
    pub(crate) key_event: KeyEvent,
    /// 中文→英文自动切换标志：会话内发生切换时置位，OnKeyDown 据此放行
    /// 本次按键给应用（英文模式不消费按键，与 Linux 侧语义一致）。
    pub(crate) auto_switched: Arc<AtomicBool>,
}

impl ITfEditSession_Impl for KeyHandlerEditSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        let service = self.service.clone();
        let key_event = self.key_event.clone();
        let auto_switched = self.auto_switched.clone();

        match handle_key_event_with_reconnect(&service, ec, key_event, &auto_switched) {
            Ok(()) => Ok(()),
            Err(e) => {
                error!("DoEditSession: failed with error: {:?}", e);
                Err(e)
            }
        }
    }
}

/// Handle key event with automatic IPC reconnection support.
///
/// 注意：此函数在宿主程序 UI 线程（TSF DoEditSession）上执行，
/// daemon 不可用时不允许 sleep 重试，否则会卡住宿主程序。
/// 重连由 [`try_reconnect_ipc`] 限频，每次失败最多只额外尝试一次，
/// 其余情况直接返回错误，交由下一次按键再试。
pub(crate) fn handle_key_event_with_reconnect(
    service: &Arc<Mutex<ServiceInner>>,
    ec: u32,
    key_event: KeyEvent,
    auto_switched: &AtomicBool,
) -> Result<()> {
    let result = handle_key_event_internal(service, ec, &key_event, auto_switched);
    if result.is_ok() {
        return result;
    }

    warn!("IPC operation failed, clearing connection");

    {
        let mut inner = service.lock().unwrap();
        inner.ipc_conn = None;
    }

    if !try_reconnect_ipc(service) {
        return result;
    }

    handle_key_event_internal(service, ec, &key_event, auto_switched)
}

/// Internal key event handling logic (assumes connection exists).
fn handle_key_event_internal(
    service: &Arc<Mutex<ServiceInner>>,
    ec: u32,
    key_event: &KeyEvent,
    auto_switched: &AtomicBool,
) -> Result<()> {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut inner = service.lock().unwrap();
        let ctx = inner.context.clone().ok_or(E_UNEXPECTED)?;
        let composition = inner.composition.clone();
        let last_caret_pos = inner.last_caret_pos;

        // 读取光标周围文本（整句补全上下文与自动切换评估共用）。
        // TSF 读取不依赖光标坐标，始终尝试；取不到上下文（Zed 等纯 IMM32 应用
        // 不实现 TSF 文本存储）时回退 UIA TextPattern。UIA 优先走 GetSelection，
        // 无需 Win32 光标坐标（自绘光标应用拿不到）；坐标仅作 RangeFromPoint 兜底，
        // 优先用布局事件缓存，缺失时从 GUI 线程信息获取。读取失败时静默跳过，
        // 不影响按键管线。
        let caret_pos = last_caret_pos.or_else(|| get_caret_position_via_gui_thread_info().ok());
        let (mut preceding_text, mut following_text) =
            read_surrounding_text(ec, &ctx, composition.as_ref());
        // UIA 回退仅服务自动切换评估（纯 IMM32 应用 TSF 取不到文本）：
        // 未开启自动切换时跳过，避免中文模式下每键一次的 UIA 跨进程读取开销
        if inner.auto_switch && preceding_text.is_none() && following_text.is_none() {
            let (uia_preceding, uia_following) =
                read_surrounding_text_via_uia(caret_pos.map(|(x, y, _)| (x, y)));
            preceding_text = uia_preceding;
            following_text = uia_following;
        }
        let surrounding = if preceding_text.is_some() || following_text.is_some() {
            Some((preceding_text, following_text))
        } else {
            None
        };

        // 根据光标周围文本自动切换中英模式（中文→英文方向）。
        // 仅当开关开启且无进行中的合成（GetRange 失败视为无合成）时评估；
        // 命中建议则切换并复用手动切换收尾，本次按键不再送入引擎，
        // 由 OnKeyDown 依据 auto_switched 标志放行给应用（英文直输）。
        if inner.auto_switch {
            let no_composition = match &composition {
                None => true,
                Some(c) => unsafe { c.GetRange().is_err() },
            };
            if no_composition && let Some((preceding_text, following_text)) = &surrounding {
                let suggestion =
                    suggest_input_mode(preceding_text.as_deref(), following_text.as_deref());
                debug!(
                    "auto-switch eval (中→英): preceding={:?} following={:?} suggestion={:?}",
                    preceding_text.as_deref().map(truncate_for_log),
                    following_text.as_deref().map(truncate_for_log),
                    suggestion
                );
                let current = inner.mode_switch.is_english();
                if let Some(target) = inner.auto_mode.evaluate(suggestion, current) {
                    // evaluate 已确认目标与当前不同，set_english 必然产生切换
                    inner.mode_switch.set_english(target);
                    auto_switched.store(true, Ordering::SeqCst);
                    // 收尾函数会重新获取 inner 锁，先释放再调用（同 service.rs 既有模式）
                    drop(inner);
                    apply_auto_mode_toggle(service, target);
                    return Ok(());
                }
            }
        }

        let conn = inner.ipc_conn.as_mut().ok_or(E_UNEXPECTED)?;

        // 读取光标周围文本并同步给 daemon（供整句补全提供上下文）。
        // SetContext 为单向请求（daemon 不写响应），随后 KeyEvent 正常请求-响应。
        if let (Some((caret_x, caret_y, caret_h)), Some((preceding_text, following_text))) =
            (caret_pos, surrounding)
        {
            let set_ctx = IpcRequest::SetContext(InputContext {
                caret_x,
                caret_y,
                caret_h,
                preceding_text,
                following_text,
            });
            let _ = send_request(&mut conn.writer, &set_ctx);
        }

        let request = IpcRequest::KeyEvent(key_event.clone());
        send_request(&mut conn.writer, &request).map_err(|_| E_UNEXPECTED)?;

        let response = read_response(&mut conn.reader).map_err(|_| E_UNEXPECTED)?;

        drop(inner);
        apply_result(service.clone(), ec, &ctx, &response.into())?;
        Ok::<(), Error>(())
    }));

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(E_UNEXPECTED.into()),
    }
}
