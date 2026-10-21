//! "根据光标周围文本自动切换中英模式"的 TSF 侧支持。
//!
//! 英文模式下按键不被消费（OnTestKeyDown 直接放行），`handle_key_event_internal`
//! 不会执行，因此两个方向分别挂钩：
//! - 中文→英文：在 `handle_key_event_internal`（key_event.rs）已读到周围文本后、
//!   发送 KeyEvent 之前评估；命中则切换模式、收尾并放行本次按键给应用；
//! - 英文→中文：在 `OnTestKeyDown`（service.rs）中用本模块的
//!   [`AutoSwitchEditSession`] 请求同步只读会话读取周围文本并评估；命中则切回
//!   中文，本次按键按中文模式继续走原有消费逻辑。
//!
//! 另有聚焦时评估（[`FocusAutoSwitchSession`]，service.rs OnSetFocus 发起）：
//! 把"首键才按语境纠正模式"提前到进入编辑器时，消除"进入时徽标显示英文、
//! 首键才切回中文"的窗口期——徽标从第一帧起即与输入行为一致。
//!
//! 手动 Ctrl 切换（TSF 路径与全局钩子路径）会调用 `AutoModeSwitch::lock_manual`
//! 锁定当时的语境基线：锁定期间同语境的自动建议被抑制，语境变化后自动解锁。

use super::caret::{read_surrounding_text, truncate_for_log};
use super::key_event::context_changed;
use super::service::{apply_auto_input_mode_toggle, caret_context_for_show};
use super::{
    ServiceInner, lock_service, locked, no_active_composition, safe_com, send_ui_command_inner,
};
use black_hole_shared::{ModeSuggestion, UiCommand, suggest_input_mode};
use std::sync::{Arc, Mutex};
use tracing::{debug, info};
use windows::Win32::UI::TextServices::{
    ITfComposition, ITfContext, ITfEditSession, ITfEditSession_Impl,
};
use windows_core::{Interface, Result, implement};

/// 自动切换生效后的收尾：重置引擎、同步系统 compartment、刷新语言栏图标，
/// 并向 daemon 上报临时模式（`SetInputModeTransient` 只更新共享状态、不持久化；
/// 单向，不读响应）。
///
/// 走自动切换专用收尾（[`apply_auto_input_mode_toggle`]）：**不请求编辑
/// 会话**——全部调用点（按键中→英评估、英→中按键评估、聚焦评估）都在
/// `no_active_composition` 门控下，且后两者直接在 `DoEditSession` 内执行，
/// 会话内请求同步编辑会话是 TSF 的禁忌（`TF_E_SYNCHRONOUS`/潜在卡死）。
pub(crate) fn apply_auto_mode_toggle(inner: &Arc<Mutex<ServiceInner>>, target: bool) {
    info!(
        "Input mode auto switched: {}",
        if target { "英文" } else { "中文" }
    );
    apply_auto_input_mode_toggle(inner, target);
    // 先上报模式（同步字样），再显式 Show 徽标（携带本实例焦点光标）：
    // Show 触发的重绘必然呈现新字样
    send_ui_command_inner(inner, UiCommand::SetInputModeTransient(target));
    let context = caret_context_for_show();
    send_ui_command_inner(
        inner,
        UiCommand::ShowModeIndicator {
            context,
            invalidate_caret: false,
            english: Some(target),
        },
    );
}

/// 编辑会话内共用：通过 TSF 文本存储读取光标周围文本并给出语境建议。
/// 返回 (语境建议, 语境建议是否可信)。可信 = 读到真实文本，或证实真空
/// 文档（两方向位移都到达文档边界）；不可信 = 语境未知（无文本存储、
/// 选区存在但内容读不出——虚拟化编辑器等），消费方必须跳过评估与基线
/// 锁定：把未知语境的 Neutral 当"空文档"评估，"中立语境回中文"规则会
/// 撤销用户的手动选择（徽标取反）。
/// 供自动切换评估（[`AutoSwitchEditSession`]）与手动切换的基线采样
/// （[`SuggestionReadSession`]）复用。
pub(crate) fn suggest_from_surrounding_text(
    ec: u32,
    ctx: &ITfContext,
    composition: Option<&ITfComposition>,
) -> (ModeSuggestion, bool) {
    let (preceding, following, context_known) = read_surrounding_text(ec, ctx, composition);
    let suggestion = suggest_input_mode(preceding.as_deref(), following.as_deref());
    debug!(
        "surrounding-text suggestion: preceding={:?} following={:?} suggestion={:?} known={}",
        preceding.as_deref().map(truncate_for_log),
        following.as_deref().map(truncate_for_log),
        suggestion,
        context_known
    );
    (suggestion, context_known)
}

/// 英文→中文自动切换评估用的只读编辑会话（仿 caret.rs 的 LayoutChangeEditSession）。
///
/// 会话内通过 TSF 文本存储读取光标周围文本并评估，结果写入 `result`，
/// 由调用方（OnTestKeyDown）在同步会话返回后读取并完成收尾。
#[implement(ITfEditSession)]
pub(crate) struct AutoSwitchEditSession {
    pub(crate) inner_arc: Arc<Mutex<ServiceInner>>,
    /// 评估结果输出：Some(target) 表示应自动切换（此场景 target 必为 false，即切回中文）。
    pub(crate) result: Arc<Mutex<Option<bool>>>,
}

impl ITfEditSession_Impl for AutoSwitchEditSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        safe_com(|| self.do_edit_session_body(ec))
    }
}

impl AutoSwitchEditSession_Impl {
    fn do_edit_session_body(&self, ec: u32) -> Result<()> {
        let (ctx, composition, entry_version) = {
            let inner = lock_service(&self.inner_arc);
            // 会话执行时再次确认门控（请求发出到会话执行之间状态可能已变化）：
            // 开关开启、当前英文模式、有上下文且无进行中的合成
            // （GetRange 失败视为无合成，与 service.rs is_composing 判定一致）。
            if !inner.auto_switch || !inner.mode_switch.is_english() {
                return Ok(());
            }
            let ctx = match &inner.context {
                Some(c) => c.clone(),
                None => return Ok(()),
            };
            let composition = inner.composition.clone();
            let no_composition = no_active_composition(&composition);
            if !no_composition {
                return Ok(());
            }
            (ctx, composition, inner.context_version)
        };

        // 读文本期间不持有 inner 锁，避免 TSF 回调重入死锁
        // （与 LayoutChangeEditSession 的加锁模式一致）。
        let (suggestion, context_known) =
            suggest_from_surrounding_text(ec, &ctx, composition.as_ref());
        debug!("auto-switch eval (英→中): suggestion={:?}", suggestion);

        let mut inner = lock_service(&self.inner_arc);
        // 读取期间发生语境变更（合成/焦点等，context_version 已 bump）：
        // 本次读取的文本已过期，跳过评估（评估会话不消费按键，直接返回
        // 不会吞键，OnTestKeyDown 按未评估继续）。
        if context_changed(entry_version, inner.context_version) {
            return Ok(());
        }
        // 语境未知（无文本存储的应用——纯 IMM32 无 ITfTextStore；以及
        // 选区存在但内容读不出的虚拟化编辑器——VSCode/WebView2 类）：
        // 建议恒为无信号且不可信，evaluate 的"中立语境默认回中文"规则会
        // 撤销用户的手动选择（徽标取反）——置位"应用不可读"粘性标记
        // （禁用后续自动切换评估，省掉每次按键的探测会话）并跳过本次评估。
        if !context_known {
            inner.auto_switch_no_text = true;
            return Ok(());
        }
        // 消费"延迟手动锁定"：手动切换路径（钩子/TSF 采样失败/语言栏）未能
        // 即时 lock_manual 时，以本次实时采样到的语境建议作基线锁定，保护
        // 手动切换不被随后的英→中自动切换立即撤销。
        if inner.pending_manual_lock {
            inner.auto_mode.lock_manual(suggestion);
            inner.pending_manual_lock = false;
        }
        let current = inner.mode_switch.is_english();
        if let Some(target) = inner.auto_mode.evaluate(suggestion, current) {
            // evaluate 已确认目标与当前不同，set_english 必然产生切换
            inner.mode_switch.set_english(target);
            *locked(&self.result) = Some(target);
        }
        Ok(())
    }
}

/// 聚焦时自动切换评估用的异步只读编辑会话：读取光标周围文本并按与按键路径
/// 完全相同的规则评估（同门控、同采样、同 `evaluate` 规则），命中则直接切换
/// 模式并收尾。由 OnSetFocus 以 `TF_ES_ASYNC | TF_ES_READ` 发起（焦点回调内
/// 禁止同步会话，见 service.rs 注释），把"首键才按语境纠正模式"提前到进入
/// 编辑器时，使徽标从第一帧起即与首键行为一致——不引入新的切换规则。
///
/// 无按键需要放行（区别于 [`AutoSwitchEditSession`] 的"结果交回调用方、由
/// OnTestKeyDown 决定消费"），切换在会话内直接完成（同 key_event.rs 中→英
/// 路径的会话内收尾模式）。
#[implement(ITfEditSession)]
pub(crate) struct FocusAutoSwitchSession {
    pub(crate) inner_arc: Arc<Mutex<ServiceInner>>,
    /// 请求时（OnSetFocus）的聚焦 context：执行时锁内 context 已被覆写说明
    /// 焦点已再次切换，本次评估属于旧窗口，直接丢弃（与
    /// LayoutChangeEditSession 的 context 配对校验一致）。
    pub(crate) context: ITfContext,
}

impl ITfEditSession_Impl for FocusAutoSwitchSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        safe_com(|| self.do_edit_session_body(ec))
    }
}

impl FocusAutoSwitchSession_Impl {
    fn do_edit_session_body(&self, ec: u32) -> Result<()> {
        let ctx = self.context.clone();
        let (composition, entry_version) = {
            let inner = lock_service(&self.inner_arc);
            // 执行时再次确认门控：开关开启、聚焦 context 仍等于会话携带的
            // context、无进行中的合成。None 同样拒绝：OnPopContext 清空
            // context 且不 bump context_version，仅靠版本号检查拦不住
            // "聚焦后文档被弹出"的过期评估——那会基于已失焦窗口的文本
            // 切换模式/锁定手动基线
            if !inner.auto_switch {
                return Ok(());
            }
            match &inner.context {
                Some(c) if c.as_raw() == ctx.as_raw() => (),
                _ => return Ok(()),
            }
            let composition = inner.composition.clone();
            let no_composition = no_active_composition(&composition);
            if !no_composition {
                return Ok(());
            }
            (composition, inner.context_version)
        };

        // 读文本期间不持有 inner 锁，避免 TSF 回调重入死锁
        let (suggestion, context_known) =
            suggest_from_surrounding_text(ec, &ctx, composition.as_ref());

        let mut inner = lock_service(&self.inner_arc);
        // 读取期间焦点/语境可能已变化：锁内 context 仍须等于会话携带的
        // context（OnPopContext 清空 context 且不 bump context_version，
        // 版本检查单独拦不住"聚焦后文档被弹出"），且语境版本未变更——
        // 任一不满足则本次读取的文本已过期，跳过评估（判定见
        // focus_session_fresh，含单元测试）
        let ctx_matches = inner
            .context
            .as_ref()
            .is_some_and(|c| c.as_raw() == ctx.as_raw());
        if !focus_session_fresh(entry_version, inner.context_version, ctx_matches) {
            return Ok(());
        }
        // 语境未知（无文本存储——纯 IMM32；内容读不出——虚拟化编辑器）：
        // 建议不可信，"中立语境回中文"规则会在聚焦时误切、撤销手动选择，
        // 与按键路径一致跳过评估，并置位"应用不可读"粘性标记
        if !context_known {
            inner.auto_switch_no_text = true;
            return Ok(());
        }
        if let Some(target) = focus_evaluate_locked(&mut inner, suggestion) {
            // evaluate 已确认目标与当前不同，set_english 恒返回 Some；
            // 防御：若返回 None（状态未变）则跳过收尾，避免徽标/daemon
            // 上报与实际 mode_switch 不一致
            match inner.mode_switch.set_english(target) {
                Some(actual) => {
                    // 收尾函数会重新获取 inner 锁，先释放再调用（key_event.rs 同款）
                    drop(inner);
                    apply_auto_mode_toggle(&self.inner_arc, actual);
                }
                None => debug!("focus auto-switch: set_english reported no change, skip finalize"),
            }
        }
        Ok(())
    }
}

/// 聚焦评估会话读取后的新鲜度判定（纯函数，便于单测）：context 配对一致
/// 且语境版本未变才视为有效。两者缺一不可——OnPopContext 清空 context
/// 不 bump 版本（版本检查单独拦不住），反之合成等语境变更 bump 版本但
/// context 指针可能恰好相同（配对检查单独拦不住）。
fn focus_session_fresh(entry_version: u64, current_version: u64, ctx_matches: bool) -> bool {
    ctx_matches && !context_changed(entry_version, current_version)
}

/// 聚焦评估会话的锁内决策（纯函数，便于单测）：消费延迟手动锁定 → 评估。
/// 返回 Some(target) 表示应切换到 target。
fn focus_evaluate_locked(inner: &mut ServiceInner, suggestion: ModeSuggestion) -> Option<bool> {
    // 消费"延迟手动锁定"：与按键评估点一致，以本次实时采样的语境建议
    // 锁定基线，保护手动切换不被本次聚焦评估立即撤销
    if inner.pending_manual_lock {
        inner.auto_mode.lock_manual(suggestion);
        inner.pending_manual_lock = false;
    }
    let current = inner.mode_switch.is_english();
    inner.auto_mode.evaluate(suggestion, current)
}

/// 手动 Ctrl 切换时采样语境基线用的只读编辑会话：读取光标周围文本并计算
/// 语境建议写入 `suggestion`，不做模式评估/切换（与 [`AutoSwitchEditSession`]
/// 的评估职责区分）。供 service.rs 的 OnTestKeyUp 在同步会话返回后读取结果。
/// 输出 `None` = 语境未知（无文本存储、虚拟化编辑器读不出内容等），无法
/// 取得可信建议——调用方据此改走延迟锁定；真实的中立语境（证实为空的
/// 文档）仍输出 `Some(Neutral)`。
#[implement(ITfEditSession)]
pub(crate) struct SuggestionReadSession {
    pub(crate) inner_arc: Arc<Mutex<ServiceInner>>,
    /// 采样结果输出：`Some(建议)`=可信采样；`None`=语境未知（不是中立
    /// 语境——锁定 Neutral 基线在任一有效建议下立即解锁、无保护作用，
    /// 手动选择会在首键被自动切换撤销）。
    pub(crate) suggestion: Arc<Mutex<Option<ModeSuggestion>>>,
}

impl ITfEditSession_Impl for SuggestionReadSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        safe_com(|| self.do_edit_session_body(ec))
    }
}

impl SuggestionReadSession_Impl {
    fn do_edit_session_body(&self, ec: u32) -> Result<()> {
        let (ctx, composition) = {
            let inner = lock_service(&self.inner_arc);
            let Some(ctx) = inner.context.clone() else {
                return Ok(());
            };
            (ctx, inner.composition.clone())
        };
        // 读文本期间不持有 inner 锁，避免 TSF 回调重入死锁
        let (suggestion, context_known) =
            suggest_from_surrounding_text(ec, &ctx, composition.as_ref());
        // 语境未知：无可信建议可锁定，输出 None 让调用方改走延迟锁定
        // （锁定 Neutral 基线在任一有效建议下立即解锁，等于没锁——手动
        // 选择会在首键被自动切换撤销，徽标反复取反）。同时置位
        // "应用不可读"粘性标记，与评估路径一致禁用后续自动切换评估。
        if !context_known {
            *locked(&self.suggestion) = None;
            lock_service(&self.inner_arc).auto_switch_no_text = true;
            return Ok(());
        }
        *locked(&self.suggestion) = Some(suggestion);
        // 清空"延迟手动锁定"：TSF 路径（service.rs OnTestKeyUp）会以本次
        // 采样建议即时 lock_manual，钩子路径遗留的 pending 无需再延迟消费
        // 到后续评估点，避免重复锁定。
        lock_service(&self.inner_arc).pending_manual_lock = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_session_fresh_requires_pair_and_version() {
        // 配对一致 + 版本未变：新鲜
        assert!(focus_session_fresh(7, 7, true));
        // context 为 None/指向别处：拒绝（OnPopContext 清空 context 且不
        // bump 版本，版本检查单独拦不住"聚焦后文档被弹出"的过期评估）
        assert!(!focus_session_fresh(7, 7, false));
        // 语境版本已变更（合成/焦点等）：拒绝（配对检查单独拦不住——
        // 变更后 context 指针可能恰好相同）
        assert!(!focus_session_fresh(7, 8, true));
    }

    #[test]
    fn focus_evaluate_consumes_pending_manual_lock_exactly_once() {
        let mut inner = ServiceInner::new();
        inner.pending_manual_lock = true;

        // 手动切换（延迟锁定挂起）：本次采样建议（英文语境）作基线锁定，
        // 同语境评估被抑制、不产生切换
        let target = focus_evaluate_locked(&mut inner, ModeSuggestion::English);
        assert_eq!(target, None, "同语境建议应被手动锁定抑制");
        // 挂起标志恰被消费一次
        assert!(!inner.pending_manual_lock, "挂起标志必须被消费");
        // 基线已锁定：同建议的后续评估仍被抑制（None 而非 Some(true)）
        assert_eq!(
            inner.auto_mode.evaluate(ModeSuggestion::English, false),
            None,
            "手动基线应持续抑制同语境建议"
        );
    }

    #[test]
    fn focus_evaluate_switches_without_pending_lock() {
        let mut inner = ServiceInner::new();
        // 无挂起锁定：英文语境建议 + 当前中文 → 自动切英文（与按键路径
        // 同一 evaluate 规则）
        let target = focus_evaluate_locked(&mut inner, ModeSuggestion::English);
        assert_eq!(target, Some(true));
    }
}
