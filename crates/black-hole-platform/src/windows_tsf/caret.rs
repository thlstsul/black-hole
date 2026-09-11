use super::{ServiceInner, send_ui_command_inner};
use black_hole_shared::{InputContext, UiCommand};
use std::mem;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{E_FAIL, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::TextServices::{
    ITfComposition, ITfContext, ITfContextView, ITfEditSession, ITfEditSession_Impl, ITfRange,
    TF_ANCHOR_END, TF_ANCHOR_START, TF_DEFAULT_SELECTION, TF_SELECTION,
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
) -> (Option<String>, Option<String>, bool) {
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
    // 存储可用性：GetSelection 成功或存在可用的合成 range。消费方据此跳过
    // "中立语境回中文"评估（见 AutoSwitchEditSession：无文本存储的应用每键
    // 评估只会得到无信号建议，会强制切回英文模式）。
    let store_available = caret_range.is_some();
    let Some(range) = caret_range else {
        return (None, None, false);
    };

    // 折叠到起点（光标位置）
    let Ok(caret) = (unsafe { range.Clone() }) else {
        return (None, None, store_available);
    };
    if unsafe { caret.Collapse(ec, TF_ANCHOR_START) }.is_err() {
        return (None, None, store_available);
    }

    // 合成串 range：合成中合成串是文档流的一部分，取上下文时必须排除"正在输入的字母"
    let comp_range = composition.and_then(|c| unsafe { c.GetRange() }.ok());

    // 光标相对合成串的位置：-1 在合成串之前 / 0 在合成串内（含两端）/ 1 在合成串之后
    let caret_side = match comp_range.as_ref() {
        Some(comp) => {
            let before_start = unsafe { caret.CompareStart(ec, comp, TF_ANCHOR_START) };
            if before_start.is_err() || before_start.unwrap_or(0) < 0 {
                -1
            } else {
                let after_end = unsafe { caret.CompareEnd(ec, comp, TF_ANCHOR_END) };
                if after_end.is_err() || after_end.unwrap_or(0) > 0 {
                    1
                } else {
                    0
                }
            }
        }
        None => -1,
    };

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
                // 光标落在合成串内/末尾时，前文会混入正在输入的字母，
                // 把终点收回到合成串起点，只取真实前文。
                if caret_side >= 0
                    && let Some(comp) = comp_range.as_ref()
                {
                    let _ = unsafe { r.ShiftEndToRange(ec, comp, TF_ANCHOR_START) };
                }
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
                // 光标落在合成串内/起点时，后文会混入正在输入的字母，
                // 把起点移到合成串末尾，只取真实后文。
                if caret_side == 0
                    && let Some(comp) = comp_range.as_ref()
                {
                    let _ = unsafe { r.ShiftStartToRange(ec, comp, TF_ANCHOR_END) };
                }
                text = read_range_text(ec, &r);
            }
        }
        (!text.is_empty()).then_some(text)
    };

    (preceding, following, store_available)
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

/// 诊断日志用：截断过长文本（保留末尾，自动切换评估关注光标附近字符）
pub(crate) fn truncate_for_log(text: &str) -> &str {
    const MAX_LOG_CHARS: usize = 16;
    let mut start = text.len();
    for (count, (i, _)) in text.char_indices().rev().enumerate() {
        if count == MAX_LOG_CHARS {
            break;
        }
        start = i;
    }
    &text[start..]
}

/// 取文本范围在屏幕上的矩形（`ITfContextView::GetTextExt`）；
/// 失败（应用无文本视图/范围失效）返回 None。
fn text_ext(ec: u32, view: &ITfContextView, range: &ITfRange) -> Option<RECT> {
    let mut rect = RECT::default();
    let mut clipped = BOOL(0);
    unsafe { view.GetTextExt(ec, range, &mut rect, &mut clipped) }
        .is_ok()
        .then_some(rect)
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
            && let Ok(view) = unsafe { ctx.GetActiveView() }
            && let Some(rect) = text_ext(ec, &view, &range)
        {
            return Ok((rect.left, rect.bottom, rect.bottom - rect.top));
        }
    }

    // Layer 2: Composition range fallback
    if let Some(comp) = composition
        && let Ok(range) = unsafe { comp.GetRange() }
        && let Ok(view) = unsafe { ctx.GetActiveView() }
    {
        if let Ok(collapsed) = unsafe { range.Clone() } {
            let _ = unsafe { collapsed.Collapse(ec, TF_ANCHOR_END) };
            if let Some(rect) = text_ext(ec, &view, &collapsed) {
                return Ok((rect.left, rect.bottom, rect.bottom - rect.top));
            }
        }
        if let Some(rect) = text_ext(ec, &view, &range) {
            return Ok((rect.right, rect.bottom, rect.bottom - rect.top));
        }
    }

    // Layer 3: GetGUIThreadInfo (last resort)
    get_caret_position_via_gui_thread_info()
}

/// 候选窗锚点位置。
///
/// 合成期间固定取合成串**起点**坐标：候选窗应在整个合成期间保持稳定，而
/// `set_caret_to_range_end` 会把选区移到合成串末尾，且每次按键 `SetText` 与
/// `SetSelection` 之间触发的布局回调会让选区处于过渡态，若直接跟随选区，
/// 候选窗会在每次按键时移动/抖动。合成串起点不随编码增长而变化，也与应用
/// 是否跟踪选区无关。无可用合成时退回光标位置。
pub(crate) fn get_candidate_position(
    ec: u32,
    ctx: &ITfContext,
    composition: Option<&ITfComposition>,
) -> Result<(i32, i32, i32)> {
    if let Some(comp) = composition
        && let Ok(range) = unsafe { comp.GetRange() }
        && let Ok(start) = unsafe { range.Clone() }
        && let Ok(view) = unsafe { ctx.GetActiveView() }
    {
        let _ = unsafe { start.Collapse(ec, TF_ANCHOR_START) };
        if let Some(rect) = text_ext(ec, &view, &start) {
            return Ok((rect.left, rect.bottom, rect.bottom - rect.top));
        }
    }

    get_caret_position(ec, ctx, composition)
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

        if let Ok((caret_x, caret_y, caret_h)) = get_candidate_position(ec, &ctx, comp.as_ref()) {
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
