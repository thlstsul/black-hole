use super::caret::{get_caret_position, get_caret_position_via_gui_thread_info};
use super::service::BlackHoleTextService;
use super::{ServiceInner, send_ui_command_inner};
use black_hole_shared::{InputContext, SchemeResult, UiCommand};
use std::mem;
use std::slice;
use std::sync::{Arc, Mutex};
use windows::Win32::UI::TextServices::{
    ITfCompositionSink, ITfContext, ITfContextComposition, ITfEditSession, ITfEditSession_Impl,
    ITfInsertAtSelection, ITfSource, ITfTextLayoutSink, TF_AE_NONE, TF_ANCHOR_END,
    TF_IAS_QUERYONLY, TF_SELECTION, TF_SELECTIONSTYLE,
};
use windows_core::{BOOL, Interface, Result, implement};

/// Apply the engine result to TSF and the UI.
pub(crate) fn apply_result(
    inner_arc: Arc<Mutex<ServiceInner>>,
    ec: u32,
    ctx: &ITfContext,
    result: &SchemeResult,
) -> Result<()> {
    match result {
        SchemeResult::Composing {
            code,
            candidates,
            selected_index,
            expanded,
        } => {
            let need_start = {
                let mut inner = inner_arc.lock().unwrap();
                match &inner.composition {
                    None => true,
                    Some(c) => {
                        let valid = unsafe { c.GetRange().is_ok() };
                        if !valid {
                            inner.composition = None;
                        }
                        !valid
                    }
                }
            };
            if need_start {
                {
                    let mut inner = inner_arc.lock().unwrap();
                    inner.last_caret_pos = None;
                }

                let insert: ITfInsertAtSelection = ctx.cast()?;
                let range = unsafe { insert.InsertTextAtSelection(ec, TF_IAS_QUERYONLY, &[])? };
                let ctx_comp: ITfContextComposition = ctx.cast()?;
                let sink = BlackHoleTextService::new_for_sink(inner_arc.clone());
                let sink_iface: ITfCompositionSink = sink.into();
                let comp = unsafe { ctx_comp.StartComposition(ec, &range, &sink_iface)? };

                {
                    let mut inner = inner_arc.lock().unwrap();
                    inner.composition = Some(comp);
                }

                {
                    let sink = BlackHoleTextService::new_for_sink(inner_arc.clone());
                    let sink_iface: ITfTextLayoutSink = sink.into();
                    if let Ok(source) = ctx.cast::<ITfSource>()
                        && let Ok(cookie) = unsafe {
                            source.AdviseSink(&<ITfTextLayoutSink as Interface>::IID, &sink_iface)
                        }
                    {
                        let mut inner = inner_arc.lock().unwrap();
                        inner.layout_sink_cookie = Some(cookie);
                    }
                }

                let mut sel = TF_SELECTION {
                    range: mem::ManuallyDrop::new(Some(range)),
                    style: TF_SELECTIONSTYLE {
                        ase: TF_AE_NONE,
                        fInterimChar: BOOL(0),
                    },
                };
                unsafe { ctx.SetSelection(ec, slice::from_ref(&sel))? };
                let _ = unsafe { mem::ManuallyDrop::take(&mut sel.range) };
            }

            if code.is_empty() {
                let composition = {
                    let mut inner = inner_arc.lock().unwrap();
                    inner.composition.take()
                };
                if let Some(composition) = composition {
                    let _ = unsafe { composition.EndComposition(ec) };
                }
                return Ok(());
            }

            let range = {
                let inner = inner_arc.lock().unwrap();
                inner
                    .composition
                    .as_ref()
                    .map(|c| unsafe { c.GetRange() })
                    .transpose()?
            };
            if let Some(range) = range {
                let text: Vec<u16> = code.encode_utf16().collect();
                unsafe { range.SetText(ec, 0, &text)? };
            }

            let caret_pos = if need_start {
                match get_caret_position_via_gui_thread_info() {
                    Ok(pos) => Some(pos),
                    Err(_) => {
                        let inner = inner_arc.lock().unwrap();
                        let comp = inner.composition.as_ref();
                        match get_caret_position(ec, ctx, comp) {
                            Ok(pos) => Some(pos),
                            Err(_) => inner.last_caret_pos,
                        }
                    }
                }
            } else {
                let inner = inner_arc.lock().unwrap();
                let comp = inner.composition.as_ref();
                match get_caret_position(ec, ctx, comp) {
                    Ok(pos) => Some(pos),
                    Err(_) => inner.last_caret_pos,
                }
            };

            if let Some((caret_x, caret_y, caret_h)) = caret_pos {
                let mut inner = inner_arc.lock().unwrap();
                inner.last_caret_pos = Some((caret_x, caret_y, caret_h));
                drop(inner);
                let context = InputContext {
                    caret_x,
                    caret_y,
                    caret_h,
                };
                let cmd = UiCommand::ShowCandidates {
                    code: code.clone(),
                    candidates: candidates.clone(),
                    selected_index: *selected_index,
                    context,
                    expanded: *expanded,
                };
                send_ui_command_inner(&inner_arc, cmd);
            }
        }
        SchemeResult::Committed { text } => {
            let composition = {
                let mut inner = inner_arc.lock().unwrap();
                inner.composition.take()
            };

            let Some(composition) = composition else {
                let _ = (|| -> Result<()> {
                    let insert: ITfInsertAtSelection = ctx.cast()?;
                    let utf16: Vec<u16> = text.encode_utf16().collect();
                    // 使用 QUERYONLY 获取插入点 range（避免 NOQUERY 返回空指针导致 Drop 时崩溃）
                    let range = unsafe { insert.InsertTextAtSelection(ec, TF_IAS_QUERYONLY, &[])? };
                    unsafe { range.SetText(ec, 0, &utf16)? };
                    Ok(())
                })();
                return Ok(());
            };

            let _ = (|| -> Result<()> {
                let range = unsafe { composition.GetRange()? };
                let utf16: Vec<u16> = text.encode_utf16().collect();
                unsafe { range.SetText(ec, 0, &utf16)? };
                let _ = unsafe { composition.EndComposition(ec) };

                let Ok(collapsed) = (unsafe { range.Clone() }) else {
                    return Ok(());
                };
                let _ = unsafe { collapsed.Collapse(ec, TF_ANCHOR_END) };
                let mut sel = TF_SELECTION {
                    range: mem::ManuallyDrop::new(Some(collapsed)),
                    style: TF_SELECTIONSTYLE {
                        ase: TF_AE_NONE,
                        fInterimChar: BOOL(0),
                    },
                };
                let _ = unsafe { ctx.SetSelection(ec, slice::from_ref(&sel)) };
                let _ = unsafe { mem::ManuallyDrop::take(&mut sel.range) };
                Ok(())
            })();
            {
                let mut inner = inner_arc.lock().unwrap();
                inner.last_caret_pos = None;
                if let Some(cookie) = inner.layout_sink_cookie.take()
                    && let Ok(source) = ctx.cast::<ITfSource>()
                {
                    let _ = unsafe { source.UnadviseSink(cookie) };
                }
            }
        }
        SchemeResult::Ignored => {}
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Cancel composition edit session
// ---------------------------------------------------------------------------

/// 结束进行中的 composition，但保留已输入文本（相当于上屏），
/// 用于切换到英文模式时保留输入框中的内容。
#[implement(ITfEditSession)]
pub(crate) struct CommitCompositionEditSession {
    pub(crate) inner_arc: Arc<Mutex<ServiceInner>>,
}

impl ITfEditSession_Impl for CommitCompositionEditSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        let (composition, ctx, layout_cookie) = {
            let inner = self.inner_arc.lock().unwrap();
            let comp = inner.composition.clone();
            let ctx = inner.context.clone();
            let cookie = inner.layout_sink_cookie;
            (comp, ctx, cookie)
        };

        if let Some(comp) = composition {
            let _ = (|| -> Result<()> {
                let Some(ctx) = ctx.as_ref() else {
                    return Ok(());
                };
                let range = unsafe { comp.GetRange()? };
                // 保留当前文本，仅结束 composition（相当于上屏）
                let _ = unsafe { comp.EndComposition(ec) };

                let Ok(collapsed) = (unsafe { range.Clone() }) else {
                    return Ok(());
                };
                let _ = unsafe { collapsed.Collapse(ec, TF_ANCHOR_END) };
                let mut sel = TF_SELECTION {
                    range: mem::ManuallyDrop::new(Some(collapsed)),
                    style: TF_SELECTIONSTYLE {
                        ase: TF_AE_NONE,
                        fInterimChar: BOOL(0),
                    },
                };
                let _ = unsafe { ctx.SetSelection(ec, slice::from_ref(&sel)) };
                let _ = unsafe { mem::ManuallyDrop::take(&mut sel.range) };
                Ok(())
            })();

            let mut inner = self.inner_arc.lock().unwrap();
            inner.composition = None;
            inner.last_caret_pos = None;
            if let Some(cookie) = layout_cookie
                && let Some(ref ctx) = ctx
                && let Ok(source) = ctx.cast::<ITfSource>()
            {
                let _ = unsafe { source.UnadviseSink(cookie) };
            }
            inner.layout_sink_cookie = None;
        }

        Ok(())
    }
}

#[implement(ITfEditSession)]
pub(crate) struct CancelCompositionEditSession {
    pub(crate) inner_arc: Arc<Mutex<ServiceInner>>,
}

impl ITfEditSession_Impl for CancelCompositionEditSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        let (composition, ctx, layout_cookie) = {
            let inner = self.inner_arc.lock().unwrap();
            let comp = inner.composition.clone();
            let ctx = inner.context.clone();
            let cookie = inner.layout_sink_cookie;
            (comp, ctx, cookie)
        };

        if let Some(comp) = composition {
            let _ = (|| -> Result<()> {
                let range = unsafe { comp.GetRange()? };
                unsafe { range.SetText(ec, 0, &[])? };
                let _ = unsafe { comp.EndComposition(ec) };
                Ok(())
            })();

            let mut inner = self.inner_arc.lock().unwrap();
            inner.composition = None;
            inner.last_caret_pos = None;
            if let Some(cookie) = layout_cookie
                && let Some(ref ctx) = ctx
                && let Ok(source) = ctx.cast::<ITfSource>()
            {
                let _ = unsafe { source.UnadviseSink(cookie) };
            }
            inner.layout_sink_cookie = None;
        }

        Ok(())
    }
}
