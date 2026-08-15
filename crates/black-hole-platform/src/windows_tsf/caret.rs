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

/// UIA 回退：TSF 文本存储不可用的应用（如 Zed 等纯 IMM32 应用，不实现
/// ITfTextStore/ITfContextOwner，`ITfContext::GetSelection/GetText` 取不到文本）
/// 无法走 TSF 读取周围文本；此类应用通常通过 UIA（accesskit_windows 等）实现
/// `TextPattern`，这里改从 UIA 读取光标周围文本。
///
/// 取文本范围的两条路径，任一失败则回退到另一条：
/// 1. `GetSelection`：拿到光标/选区 range；
/// 2. `RangeFromPoint`：用已知的光标屏幕坐标定位 range（部分应用不暴露选区）。
///    仅在调用方拿到 Win32 光标坐标时可用；自绘光标的应用（如 Zed）传 None。
///
/// TextPattern 可能挂在焦点元素的祖先节点上（如编辑器容器），
/// 因此沿祖先链逐级查找，最多回溯 [`MAX_UIA_ANCESTORS`] 层。
///
/// 读取失败或无可读内容时对应项为 None。
pub(crate) fn read_surrounding_text_via_uia(
    caret_pos: Option<(i32, i32)>,
) -> (Option<String>, Option<String>) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationTextPattern, TextPatternRangeEndpoint_End,
        TextPatternRangeEndpoint_Start, TextUnit_Character, UIA_TextPatternId,
    };

    const MAX_UIA_ANCESTORS: usize = 8;

    let automation =
        unsafe { CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER) };
    let Ok(automation) = automation else {
        return (None, None);
    };
    let focused = unsafe { automation.GetFocusedElement() };
    let Ok(focused) = focused else {
        return (None, None);
    };

    // 沿祖先链（含自身）查找支持 TextPattern 的元素
    let Ok(raw_cond) = (unsafe { automation.RawViewCondition() }) else {
        return (None, None);
    };
    let Ok(tree_walker) = (unsafe { automation.CreateTreeWalker(&raw_cond) }) else {
        return (None, None);
    };
    let mut element = focused;
    let mut pattern = None;
    for _ in 0..=MAX_UIA_ANCESTORS {
        if let Ok(p) =
            unsafe { element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) }
        {
            pattern = Some(p);
            break;
        }
        let Ok(parent) = (unsafe { tree_walker.GetParentElement(&element) }) else {
            break;
        };
        element = parent;
    }
    let Some(pattern) = pattern else {
        return (None, None);
    };

    // 路径 1：光标选区 range；路径 2：光标坐标定位 range（有坐标时可用）
    let caret_range = {
        let mut range = None;
        if let Ok(selection) = unsafe { pattern.GetSelection() }
            && let Ok(len) = unsafe { selection.Length() }
            && len > 0
            && let Ok(r) = unsafe { selection.GetElement(0) }
        {
            range = Some(r);
        }
        if range.is_none()
            && let Some((caret_x, caret_y)) = caret_pos
        {
            range = unsafe {
                pattern.RangeFromPoint(POINT {
                    x: caret_x,
                    y: caret_y,
                })
            }
            .ok();
        }
        range
    };
    let Some(caret) = caret_range else {
        return (None, None);
    };

    // 前文：把选区起点向前扩展 MAX_PRECEDING_CHARS 字符后读取
    let preceding = {
        let mut text = String::new();
        if let Ok(r) = unsafe { caret.Clone() } {
            let moved = unsafe {
                r.MoveEndpointByUnit(
                    TextPatternRangeEndpoint_Start,
                    TextUnit_Character,
                    -MAX_PRECEDING_CHARS,
                )
            };
            if moved.is_ok()
                && moved.unwrap_or(0) != 0
                && let Ok(bstr) = unsafe { r.GetText(-1) }
            {
                text = String::from_utf16_lossy(&bstr);
            }
        }
        (!text.is_empty()).then_some(text)
    };

    // 后文：把选区终点向后扩展 MAX_FOLLOWING_CHARS 字符后读取
    let following = {
        let mut text = String::new();
        if let Ok(r) = unsafe { caret.Clone() } {
            let moved = unsafe {
                r.MoveEndpointByUnit(
                    TextPatternRangeEndpoint_End,
                    TextUnit_Character,
                    MAX_FOLLOWING_CHARS,
                )
            };
            if moved.is_ok()
                && moved.unwrap_or(0) != 0
                && let Ok(bstr) = unsafe { r.GetText(-1) }
            {
                text = String::from_utf16_lossy(&bstr);
            }
        }
        (!text.is_empty()).then_some(text)
    };

    (preceding, following)
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
