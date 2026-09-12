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
//! 手动 Ctrl 切换（TSF 路径与全局钩子路径）会调用 `AutoModeSwitch::lock_manual`
//! 锁定当时的语境基线：锁定期间同语境的自动建议被抑制，语境变化后自动解锁。

use super::caret::{read_surrounding_text, truncate_for_log};
use super::hook::focused_thread_id;
use super::key_event::context_changed;
use super::service::apply_input_mode_toggle;
use super::{ServiceInner, send_ui_command_inner};
use black_hole_shared::{ModeSuggestion, UiCommand, suggest_input_mode};
use std::sync::{Arc, Mutex};
use tracing::{debug, info};
use windows::Win32::UI::TextServices::{
    ITfComposition, ITfContext, ITfEditSession, ITfEditSession_Impl,
};
use windows_core::{Result, implement};

/// 自动切换生效后的收尾：复用手动切换的收尾路径（结束进行中的 composition、
/// 重置引擎、同步系统 compartment、刷新语言栏图标），并向 daemon 上报临时模式
/// （`SetInputModeTransient` 只更新共享状态、不持久化；单向，不读响应）。
pub(crate) fn apply_auto_mode_toggle(inner: &Arc<Mutex<ServiceInner>>, target: bool) {
    info!(
        "Input mode auto switched: {}",
        if target { "英文" } else { "中文" }
    );
    apply_input_mode_toggle(inner, target);
    send_ui_command_inner(inner, UiCommand::SetInputModeTransient(target));
}

/// 编辑会话内共用：通过 TSF 文本存储读取光标周围文本并给出语境建议。
/// 返回 (语境建议, TSF 文本存储是否可用)。存储可用性供英→中评估在
/// 无文本存储的应用（纯 IMM32）上跳过"中立语境回中文"评估。
/// 供自动切换评估（[`AutoSwitchEditSession`]）与手动切换的基线采样
/// （[`SuggestionReadSession`]）复用。
pub(crate) fn suggest_from_surrounding_text(
    ec: u32,
    ctx: &ITfContext,
    composition: Option<&ITfComposition>,
) -> (ModeSuggestion, bool) {
    let (preceding, following, store_available) = read_surrounding_text(ec, ctx, composition);
    let suggestion = suggest_input_mode(preceding.as_deref(), following.as_deref());
    debug!(
        "surrounding-text suggestion: preceding={:?} following={:?} suggestion={:?}",
        preceding.as_deref().map(truncate_for_log),
        following.as_deref().map(truncate_for_log),
        suggestion
    );
    (suggestion, store_available)
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
        let (ctx, composition, entry_version, last_caret_pos) = {
            let inner = self.inner_arc.lock().unwrap();
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
            let no_composition = match &composition {
                None => true,
                Some(c) => unsafe { c.GetRange().is_err() },
            };
            if !no_composition {
                return Ok(());
            }
            (
                ctx,
                composition,
                inner.context_version,
                inner.last_caret_pos,
            )
        };

        // 读文本期间不持有 inner 锁，避免 TSF 回调重入死锁
        // （与 LayoutChangeEditSession 的加锁模式一致）。
        let (suggestion, store_available) =
            suggest_from_surrounding_text(ec, &ctx, composition.as_ref());
        debug!("auto-switch eval (英→中): suggestion={:?}", suggestion);
        // TSF 文本存储不可用的应用（纯 IMM32 无 ITfTextStore）：每键评估都只会
        // 得到无信号建议，evaluate 的"中立语境默认回中文"规则会强制切回英文
        // 模式——存储不可用（GetSelection 失败且无 composition range 可回退，
        // 含 GetRange 已失效的死合成）时跳过评估，保持当前模式。可用性直接
        // 来自本次读取（read_surrounding_text），无需额外探测。
        if !store_available {
            return Ok(());
        }

        let mut inner = self.inner_arc.lock().unwrap();
        // 读取期间发生语境变更（合成/焦点等，context_version 已 bump）：
        // 本次读取的文本已过期，跳过评估（评估会话不消费按键，直接返回
        // 不会吞键，OnTestKeyDown 按未评估继续）。
        if context_changed(entry_version, inner.context_version) {
            return Ok(());
        }
        // 缓存本次采样（含焦点线程、光标位置与语境版本）：钩子路径手动切换
        // 仅在焦点线程/位置一致且缓存未被后续合成/焦点变更作废时据此取锁定基线
        inner.record_context_sample(
            suggestion,
            focused_thread_id(),
            last_caret_pos,
            entry_version,
        );
        // 消费"延迟手动锁定"：钩子路径手动切换（Ctrl）因缓存门控失败未能
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
            *self.result.lock().unwrap() = Some(target);
        }
        Ok(())
    }
}

/// 手动 Ctrl 切换时采样语境基线用的只读编辑会话：读取光标周围文本并计算
/// 语境建议写入 `suggestion`，不做模式评估/切换（与 [`AutoSwitchEditSession`]
/// 的评估职责区分）。供 service.rs 的 OnTestKeyUp 在同步会话返回后读取结果。
#[implement(ITfEditSession)]
pub(crate) struct SuggestionReadSession {
    pub(crate) inner_arc: Arc<Mutex<ServiceInner>>,
    /// 采样结果输出：当前语境建议（Neutral=无信号或读取失败）。
    pub(crate) suggestion: Arc<Mutex<ModeSuggestion>>,
}

impl ITfEditSession_Impl for SuggestionReadSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        let (ctx, composition, entry_version, last_caret_pos) = {
            let inner = self.inner_arc.lock().unwrap();
            let Some(ctx) = inner.context.clone() else {
                return Ok(());
            };
            (
                ctx,
                inner.composition.clone(),
                inner.context_version,
                inner.last_caret_pos,
            )
        };
        // 读文本期间不持有 inner 锁，避免 TSF 回调重入死锁
        let (suggestion, _) = suggest_from_surrounding_text(ec, &ctx, composition.as_ref());
        *self.suggestion.lock().unwrap() = suggestion;
        // 缓存本次采样（含焦点线程、光标位置与语境版本）：钩子路径手动切换
        // 仅在焦点线程/位置一致且缓存未被后续合成/焦点变更作废时据此取锁定基线
        {
            let mut inner = self.inner_arc.lock().unwrap();
            inner.record_context_sample(
                suggestion,
                focused_thread_id(),
                last_caret_pos,
                entry_version,
            );
            // 清空"延迟手动锁定"：TSF 路径（service.rs OnTestKeyUp）会以本次
            // 采样建议即时 lock_manual，钩子路径遗留的 pending 无需再延迟消费
            // 到后续评估点，避免重复锁定。
            inner.pending_manual_lock = false;
        }
        Ok(())
    }
}
