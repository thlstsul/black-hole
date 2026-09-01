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
                            inner.context_version += 1;
                        }
                        !valid
                    }
                }
            };
            if need_start {
                {
                    let mut inner = inner_arc.lock().unwrap();
                    // 合成开始：文本/光标状态不可靠，一并清空光标位置并作废缓存
                    inner.last_caret_pos = None;
                    inner.context_version += 1;
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
                    inner.context_version += 1;
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
                    // 清空合成：文本已变化，一并清空光标位置并作废缓存
                    inner.last_caret_pos = None;
                    inner.context_version += 1;
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
                let context = InputContext::caret(caret_x, caret_y, caret_h);
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
        SchemeResult::Committed {
            text,
            temporary_english,
        } => {
            // 临时英文结束上屏后，以英文为基线锁定自动切换，避免紧随其后的
            // 中→英自动切换把用户拉入全英文模式（临时英文的本意是停留在中文
            // 模式、随时再弹英文）。临时英文上屏文本为纯 ASCII 字母（方案层
            // 只接受 is_ascii_alphabetic），Space 结束路径可带一个尾部空格
            // （suggest_input_mode 的信号扫描会跳过空白），故上屏后语境必为
            // 英文，直接以 Some(true) 作基线，无需回读文档（回读可能取到
            // 上屏前的陈旧文本）。基线为英文：同英文语境的中→英建议被抑制
            // （保持中文），用户移动光标到非英文语境后 evaluate 自动解锁、
            // 恢复自动切换。
            // 与取 composition 共用一次加锁（lock_manual 是纯字段写，AutoModeSwitch 为 Copy）。
            let composition = {
                let mut inner = inner_arc.lock().unwrap();
                if *temporary_english {
                    inner.auto_mode.lock_manual(Some(true));
                }
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
                // 直插文本（无合成）：文档已变化，一并清空光标位置并作废缓存
                let mut inner = inner_arc.lock().unwrap();
                inner.last_caret_pos = None;
                inner.context_version += 1;
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
                inner.context_version += 1;
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
            inner.context_version += 1;
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
            inner.context_version += 1;
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
