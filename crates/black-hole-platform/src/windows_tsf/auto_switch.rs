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

use super::caret::{
    get_caret_position_via_gui_thread_info, read_surrounding_text, read_surrounding_text_via_uia,
    truncate_for_log,
};
use super::service::apply_input_mode_toggle;
use super::{ServiceInner, send_ui_command_inner};
use black_hole_shared::{UiCommand, suggest_input_mode};
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

/// 编辑会话内共用：读取光标周围文本（TSF 文本存储优先，取不到时回退 UIA
/// TextPattern——仅在纯 IMM32 应用上产生 UIA 开销）并给出语境建议。
/// 供自动切换评估（[`AutoSwitchEditSession`]）与手动切换的基线采样
/// （[`SuggestionReadSession`]）复用。
pub(crate) fn suggest_from_surrounding_text(
    ec: u32,
    ctx: &ITfContext,
    composition: Option<&ITfComposition>,
) -> Option<bool> {
    let (mut preceding, mut following) = read_surrounding_text(ec, ctx, composition);
    // 纯 IMM32 应用（如 Zed）无 TSF 文本存储：回退 UIA TextPattern。
    // UIA 优先走 GetSelection，无需 Win32 光标坐标（自绘光标应用拿不到）；
    // 坐标仅作 RangeFromPoint 兜底。
    if preceding.is_none() && following.is_none() {
        let caret_pos = get_caret_position_via_gui_thread_info().ok();
        let (uia_preceding, uia_following) =
            read_surrounding_text_via_uia(caret_pos.map(|(x, y, _)| (x, y)));
        preceding = uia_preceding;
        following = uia_following;
    }
    let suggestion = suggest_input_mode(preceding.as_deref(), following.as_deref());
    debug!(
        "surrounding-text suggestion: preceding={:?} following={:?} suggestion={:?}",
        preceding.as_deref().map(truncate_for_log),
        following.as_deref().map(truncate_for_log),
        suggestion
    );
    suggestion
}

/// 英文→中文自动切换评估用的只读编辑会话（仿 caret.rs 的 LayoutChangeEditSession）。
///
/// 会话内通过 TSF 文本存储读取光标周围文本并评估（TSF 取不到时回退 UIA
/// TextPattern，仅在纯 IMM32 应用上产生每键一次的 UIA 开销），结果写入
/// `result`，由调用方（OnTestKeyDown）在同步会话返回后读取并完成收尾。
#[implement(ITfEditSession)]
pub(crate) struct AutoSwitchEditSession {
    pub(crate) inner_arc: Arc<Mutex<ServiceInner>>,
    /// 评估结果输出：Some(target) 表示应自动切换（此场景 target 必为 false，即切回中文）。
    pub(crate) result: Arc<Mutex<Option<bool>>>,
}

impl ITfEditSession_Impl for AutoSwitchEditSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        let (ctx, composition) = {
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
            (ctx, composition)
        };

        // 读文本期间不持有 inner 锁，避免 TSF 回调重入死锁
        // （与 LayoutChangeEditSession 的加锁模式一致）。
        let suggestion = suggest_from_surrounding_text(ec, &ctx, composition.as_ref());
        debug!("auto-switch eval (英→中): suggestion={:?}", suggestion);

        let mut inner = self.inner_arc.lock().unwrap();
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
    /// 采样结果输出：当前语境建议（None=无信号或读取失败）。
    pub(crate) suggestion: Arc<Mutex<Option<bool>>>,
}

impl ITfEditSession_Impl for SuggestionReadSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        let (ctx, composition) = {
            let inner = self.inner_arc.lock().unwrap();
            let Some(ctx) = inner.context.clone() else {
                return Ok(());
            };
            (ctx, inner.composition.clone())
        };
        // 读文本期间不持有 inner 锁，避免 TSF 回调重入死锁
        let suggestion = suggest_from_surrounding_text(ec, &ctx, composition.as_ref());
        *self.suggestion.lock().unwrap() = suggestion;
        Ok(())
    }
}
