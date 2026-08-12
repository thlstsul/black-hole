use super::{ServiceInner, send_ui_command_inner};
use black_hole_shared::{InputContext, UiCommand};
use std::mem;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{E_FAIL, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::TextServices::{
    ITfComposition, ITfContext, ITfEditSession, ITfEditSession_Impl, ITfRange, TF_ANCHOR_END,
    TF_ANCHOR_START, TF_DEFAULT_SELECTION, TF_SELECTION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
};
use windows_core::implement;
use windows_core::{BOOL, Result};

/// 前文最大读取字符数（补全上下文足够即可，避免逐键开销）
const MAX_PRECEDING_CHARS: i32 = 32;
/// 后文最大读取字符数
const MAX_FOLLOWING_CHARS: i32 = 16;

/// 读取光标周围的文本，返回 (前文, 后文)，供整句补全提供上下文。
///
/// 基于 TSF 编辑会话内的选区 range（合成中即光标位置）：
/// - 前文：克隆光标 range，用 `ShiftStart` 把起点向前扩展后 `GetText`；
/// - 后文：克隆光标 range，用 `ShiftEnd` 把终点向后扩展后 `GetText`。
///
/// 读取失败或无可读内容时对应项为 None。
pub(crate) fn read_surrounding_text(
    ec: u32,
    ctx: &ITfContext,
    composition: Option<&ITfComposition>,
) -> (Option<String>, Option<String>) {
    // 获取光标/选区 range；GetSelection 失败时回退到合成串 range
    let caret_range = {
        let mut sel_buf = [TF_SELECTION::default()];
        let mut fetched = 0u32;
        let hr = unsafe {
            ctx.GetSelection(
                ec,
                TF_DEFAULT_SELECTION,
                &mut sel_buf,
                &mut fetched as *mut _,
            )
        };
        if hr.is_ok() && fetched > 0 {
            unsafe { mem::ManuallyDrop::take(&mut sel_buf[0].range) }
        } else {
            composition.and_then(|c| unsafe { c.GetRange() }.ok())
        }
    };
    let Some(range) = caret_range else {
        return (None, None);
    };

    // 折叠到起点（光标位置）
    let Ok(caret) = (unsafe { range.Clone() }) else {
        return (None, None);
    };
    if unsafe { caret.Collapse(ec, TF_ANCHOR_START) }.is_err() {
        return (None, None);
    }

    // 前文：起点向前扩展 MAX_PRECEDING_CHARS 字符
    let preceding = {
        let mut text = String::new();
        if let Ok(r) = unsafe { caret.Clone() } {
            let mut shifted = 0i32;
            let ok =
                unsafe { r.ShiftStart(ec, -MAX_PRECEDING_CHARS, &mut shifted, std::ptr::null()) }
                    .is_ok()
                    && shifted != 0;
            if ok {
                text = read_range_text(ec, &r);
            }
        }
        (!text.is_empty()).then_some(text)
    };

    // 后文：终点向后扩展 MAX_FOLLOWING_CHARS 字符
    let following = {
        let mut text = String::new();
        if let Ok(r) = unsafe { caret.Clone() } {
            let mut shifted = 0i32;
            let ok = unsafe { r.ShiftEnd(ec, MAX_FOLLOWING_CHARS, &mut shifted, std::ptr::null()) }
                .is_ok()
                && shifted != 0;
            if ok {
                text = read_range_text(ec, &r);
            }
        }
        (!text.is_empty()).then_some(text)
    };

    (preceding, following)
}

/// 读取 range 内纯文本（UTF-16 → String）
fn read_range_text(ec: u32, range: &ITfRange) -> String {
    // 预分配两倍字符数的缓冲（UTF-16 代理对占 2 个 u16，按上限分配即可）
    let cap = (MAX_PRECEDING_CHARS.max(MAX_FOLLOWING_CHARS)) as usize;
    let mut buf = vec![0u16; cap * 2];
    let mut len = 0u32;
    let hr = unsafe { range.GetText(ec, 0, &mut buf, &mut len as *mut _) };
    if hr.is_ok() && len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    }
}

/// Get the screen coordinates of the current caret position.
///
/// Uses a three-layer fallback strategy:
/// 1. `ITfContext::GetSelection` + `GetTextExt` — official TSF method, most reliable.
/// 2. `ITfComposition::GetRange` + `GetTextExt` — fallback when selection is unavailable.
/// 3. `GetGUIThreadInfo` — last resort for apps (e.g. Chromium / Electron) where
///    TSF `GetTextExt` may fail.
pub(crate) fn get_caret_position(
    ec: u32,
    ctx: &ITfContext,
    composition: Option<&ITfComposition>,
) -> Result<(i32, i32, i32)> {
    // Layer 1: TSF GetSelection + ITfContextView::GetTextExt
    let mut sel_buf = [TF_SELECTION::default()];
    let mut fetched = 0u32;
    let hr = unsafe {
        ctx.GetSelection(
            ec,
            TF_DEFAULT_SELECTION,
            &mut sel_buf,
            &mut fetched as *mut _,
        )
    };
    if hr.is_ok() && fetched > 0 {
        let range_opt = unsafe { mem::ManuallyDrop::take(&mut sel_buf[0].range) };
        if let Some(range) = range_opt
            && let Ok(context_view) = unsafe { ctx.GetActiveView() }
        {
            let mut rect = RECT::default();
            let mut clipped = BOOL(0);
            let hr = unsafe { context_view.GetTextExt(ec, &range, &mut rect, &mut clipped) };
            if hr.is_ok() {
                return Ok((rect.left, rect.bottom, rect.bottom - rect.top));
            }
        }
    }

    // Layer 2: Composition range fallback
    if let Some(comp) = composition
        && let Ok(range) = unsafe { comp.GetRange() }
        && let Ok(context_view) = unsafe { ctx.GetActiveView() }
    {
        if let Ok(collapsed) = unsafe { range.Clone() } {
            unsafe {
                let _ = collapsed.Collapse(ec, TF_ANCHOR_END);
                let mut rect = RECT::default();
                let mut clipped = BOOL(0);
                let hr = context_view.GetTextExt(ec, &collapsed, &mut rect, &mut clipped);
                if hr.is_ok() {
                    return Ok((rect.left, rect.bottom, rect.bottom - rect.top));
                }
            }
        }

        let mut rect = RECT::default();
        let mut clipped = BOOL(0);
        let hr = unsafe { context_view.GetTextExt(ec, &range, &mut rect, &mut clipped) };
        if hr.is_ok() {
            return Ok((rect.right, rect.bottom, rect.bottom - rect.top));
        }
    }

    // Layer 3: GetGUIThreadInfo (last resort)
    get_caret_position_via_gui_thread_info()
}

/// Get caret position via `GetGUIThreadInfo` Windows API.
pub(crate) fn get_caret_position_via_gui_thread_info() -> Result<(i32, i32, i32)> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return Err(E_FAIL.into());
        }

        let mut gui_thread_info = GUITHREADINFO {
            cbSize: mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };

        let fore_thread_id = GetWindowThreadProcessId(hwnd, None);
        if GetGUIThreadInfo(fore_thread_id, &mut gui_thread_info).is_err() {
            return Err(E_FAIL.into());
        }

        if gui_thread_info.hwndCaret.0.is_null() {
            return Err(E_FAIL.into());
        }

        let mut point = POINT {
            x: gui_thread_info.rcCaret.right,
            y: gui_thread_info.rcCaret.bottom,
        };
        let _ = ClientToScreen(gui_thread_info.hwndCaret, &mut point);

        let height = gui_thread_info.rcCaret.bottom - gui_thread_info.rcCaret.top;
        Ok((point.x, point.y, height.max(16)))
    }
}

// ---------------------------------------------------------------------------
// Edit session used by ITfTextLayoutSink to query updated caret position
// ---------------------------------------------------------------------------

#[implement(ITfEditSession)]
pub(crate) struct LayoutChangeEditSession {
    pub(crate) inner_arc: Arc<Mutex<ServiceInner>>,
}

impl ITfEditSession_Impl for LayoutChangeEditSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        let inner = self.inner_arc.lock().unwrap();
        let ctx = match &inner.context {
            Some(c) => c.clone(),
            None => return Ok(()),
        };
        let comp = inner.composition.as_ref().cloned();
        drop(inner);

        if let Ok((caret_x, caret_y, caret_h)) = get_caret_position(ec, &ctx, comp.as_ref()) {
            let mut inner = self.inner_arc.lock().unwrap();
            inner.last_caret_pos = Some((caret_x, caret_y, caret_h));
            drop(inner);

            let context = InputContext::caret(caret_x, caret_y, caret_h);
            let cmd = UiCommand::UpdatePosition { context };
            send_ui_command_inner(&self.inner_arc, cmd);
        }

        Ok(())
    }
}
