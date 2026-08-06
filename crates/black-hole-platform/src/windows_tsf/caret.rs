use super::{ServiceInner, send_ui_command_inner};
use black_hole_shared::{InputContext, UiCommand};
use std::mem;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{E_FAIL, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::TextServices::{
    ITfComposition, ITfContext, ITfEditSession, ITfEditSession_Impl, TF_ANCHOR_END,
    TF_DEFAULT_SELECTION, TF_SELECTION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
};
use windows_core::{BOOL, Result};
use windows_core::implement;

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

            let context = InputContext {
                caret_x,
                caret_y,
                caret_h,
            };
            let cmd = UiCommand::UpdatePosition { context };
            send_ui_command_inner(&self.inner_arc, cmd);
        }

        Ok(())
    }
}
