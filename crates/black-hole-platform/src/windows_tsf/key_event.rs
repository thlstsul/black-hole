use super::auto_switch::apply_auto_mode_toggle;
use super::caret::{
    get_caret_position_via_gui_thread_info, read_surrounding_text, truncate_for_log,
};
use super::commit::apply_result;
use super::hook::focused_thread_id;
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

/// 语境变更判定：读取窗口（不持锁执行 TSF 读取）前后 context_version 不等即
/// 视为读取期间发生了合成/焦点等语境变更，本次读取的快照已过期。
/// 供 handle_key_event_internal（置 key_dropped 放行按键）与 auto_switch.rs
/// 的评估会话（跳过评估）共用；纯函数便于单测固定该判定。
pub(crate) fn context_changed(entry: u64, current: u64) -> bool {
    entry != current
}

/// 读取窗口内语境变更后的重试判定：版本变化且尚未重试 → 重试一次（以新快照
/// 重新读取）；版本一致或已重试过 → 不再重试，接受当前快照继续处理（放行
/// BOOL(0) 会与 OnTestKeyDown 的消费承诺矛盾——应用会重新投递按键导致原始
/// 字符泄漏/丢键，宁接受窄竞态窗口内的近似快照继续消费）。
pub(crate) fn should_retry_read(entry: u64, current: u64, retried: bool) -> bool {
    !retried && context_changed(entry, current)
}

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
        // 读取窗口（不持锁 TSF 读取）内可能发生合成/焦点等语境变更，导致快照
        // 过期。有界重试一次：以新快照重新读取；重试后版本仍变化时接受第二次
        // 快照继续处理——OnTestKeyDown 已对同一按键返回"IME 将消费"，放行
        // （BOOL(0)）会让应用重新投递按键导致原始字符泄漏/丢键，宁接受窄竞态
        // 窗口内的近似快照继续消费（见 should_retry_read）。
        let mut retried = false;
        'read: loop {
            let inner = service.lock().unwrap();
            let ctx = inner.context.clone().ok_or(E_UNEXPECTED)?;
            let composition = inner.composition.clone();
            let last_caret_pos = inner.last_caret_pos;
            let entry_version = inner.context_version;
            // TSF 读取（GetSelection/GetText）可能触发回调重入，不持锁执行，
            // 避免阻塞键盘钩子/语言栏/daemon IPC 等同样竞争 ServiceInner 的路径
            drop(inner);

            // 读取光标周围文本（整句补全上下文与自动切换评估共用），供 SetContext
            // 与自动切换评估消费；TSF 文本存储不可用的应用（VSCode 虚拟化编辑器、
            // 纯 IMM32 应用）取不到时为 None，消费方按无信号处理。
            let (preceding_text, following_text, _) =
                read_surrounding_text(ec, &ctx, composition.as_ref());

            // 重新加锁：继续走消费 surrounding 的路径。
            let mut inner = service.lock().unwrap();
            if should_retry_read(entry_version, inner.context_version, retried) {
                retried = true;
                continue 'read;
            }
            // 重试后仍变化（版本不一致且已重试过）：接受第二次快照继续正常
            // 流程——OnTestKeyDown 已对同一按键返回"IME 将消费"，放行
            // （BOOL(0)）会让应用重新投递按键导致原始字符泄漏/丢键；窄竞态
            // 窗口内第二次快照近似当前状态，继续处理比放行更安全。
            // 前文/后文任一非空即视为有上下文（供 SetContext 与自动切换评估共用）
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
                    // 消费"延迟手动锁定"：钩子路径手动切换（Ctrl）因缓存门控
                    // 失败未能即时 lock_manual 时，以本次实时采样到的语境建议
                    // 作基线锁定，保护手动切换不被随后的中→英自动切换立即撤销。
                    if inner.pending_manual_lock {
                        inner.auto_mode.lock_manual(suggestion);
                        inner.pending_manual_lock = false;
                    }
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
            // 注：TSF 文本存储不可用的应用（VSCode 虚拟化编辑器、纯 IMM32 如 Zed）
            // 周围文本不可得，此处不发送 SetContext，整句补全按无上下文处理——与
            // 自动切换一致"宁缺毋滥"，不再走 UIA 屏幕阅读文本通道（污染不可靠，
            // 若需恢复补全上下文应走独立的干净文档文本通道）。
            if let Some((preceding_text, following_text)) = &surrounding {
                // 惰性查询光标坐标：仅 SetContext 消费方需要，且仅当布局缓存缺失时
                // 才实时查询（or_else 惰性求值，避免每键一次 GetGUIThreadInfo）。
                // 布局缓存 last_caret_pos（ITfContextView::GetTextExt，最可靠）优先，
                // 实时查询基于 GetForegroundWindow，跨进程场景可能返回过期/错误窗口坐标
                if let Some((caret_x, caret_y, caret_h)) =
                    last_caret_pos.or_else(|| get_caret_position_via_gui_thread_info().ok())
                {
                    let set_ctx = IpcRequest::SetContext(InputContext {
                        caret_x,
                        caret_y,
                        caret_h,
                        preceding_text: preceding_text.clone(),
                        following_text: following_text.clone(),
                    });
                    let _ = send_request(&mut conn.writer, &set_ctx);
                }
            }

            let request = IpcRequest::KeyEvent(key_event.clone());
            send_request(&mut conn.writer, &request).map_err(|_| E_UNEXPECTED)?;

            let response = read_response(&mut conn.reader).map_err(|_| E_UNEXPECTED)?;

            drop(inner);
            apply_result(service.clone(), ec, &ctx, &response.into())?;
            // 缓存本次采样（含焦点线程、光标位置与语境版本）：内容与位置都锚定
            // 读取前的快照（entry_version / last_caret_pos），版本盖章必须与
            // 采样文本同源（详见 record_context_sample 的文档）。
            service.lock().unwrap().record_context_sample(
                surrounding
                    .as_ref()
                    .and_then(|(p, f)| suggest_input_mode(p.as_deref(), f.as_deref())),
                focused_thread_id(),
                last_caret_pos,
                entry_version,
            );
            return Ok::<(), Error>(());
        }
    }));

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(E_UNEXPECTED.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{context_changed, should_retry_read};

    #[test]
    fn context_changed_only_on_version_difference() {
        // 版本一致：读取窗口内无语境变更，继续处理按键/评估
        assert!(!context_changed(3, 3));
        // 版本不一致：合成/焦点等语境变更
        assert!(context_changed(2, 3));
        assert!(context_changed(0, 1));
    }

    #[test]
    fn retry_read_once_then_accept_snapshot() {
        // 版本一致：不重试（直接继续处理）
        assert!(!should_retry_read(3, 3, false));
        assert!(!should_retry_read(3, 3, true));
        // 版本变化且未重试：重试一次（以新快照重新读取）
        assert!(should_retry_read(2, 3, false));
        // 版本变化且已重试：不再重试——接受快照继续消费（OnTestKeyDown
        // 已承诺消费，放行 BOOL(0) 会让应用重投按键导致字符泄漏）
        assert!(!should_retry_read(2, 3, true));
    }
}
