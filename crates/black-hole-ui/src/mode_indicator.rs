// ---------------------------------------------------------------------------
// 中英模式徽标：候选窗进程内的 deferred viewport（"中"/"英"）
// ---------------------------------------------------------------------------

//! 与候选窗同一 eframe 事件循环（winit 0.30 每进程仅允许一个事件循环，
//! 不能再开第二个 run_native），由候选窗在自身 `ui()` 中按需调用
//! `show_viewport_deferred` 挂载本模块的渲染函数。
//!
//! 显隐由焦点与输入事件驱动：
//! - 聚焦输入框（ShowModeIndicator，携带光标位置）→ 显示，输入前即可见；
//! - 开始输入（候选窗收到 ShowCandidates/CommitText）→ 隐藏；
//! - 切换中英（SetInputMode*）→ 仅同步字样；重显由切换实例的
//!   ShowModeIndicator（携带自己的焦点光标）驱动；
//! - 焦点离开（HideModeIndicator / TIP 停用）→ 隐藏。
//!
//! 无已知光标位置（聚焦即显示、光标事件未到）时把徽标停放到屏幕外：
//! Edge 等 Chromium 系应用无系统 caret，居中显示会误导用户。
//!
//! 重绘唤醒：`egui::Context` 由所有 viewport 共享（内部是
//! `Arc<RwLock<ContextImpl>>`，克隆之间不含 viewport 身份），
//! `Context::request_repaint()` 只作用于"当前 pass 所属的 viewport"，在 pass
//! 之外（如候选窗的命令线程）调用会回落到 ROOT，即只重绘主窗口。要定向唤醒
//! 徽标必须用 [`indicator_viewport_id`] + `Context::request_repaint_of`。

use super::*;

/// 徽标尺寸（逻辑像素）：单字 + 内边距
pub(crate) const BADGE_WIDTH: f32 = 34.0;
pub(crate) const BADGE_HEIGHT: f32 = 28.0;
/// 徽标与光标的间距
const GAP: f32 = 4.0;

/// 徽标 deferred viewport 的稳定 id：候选窗据此注册/关闭本 viewport，
/// 命令线程据此定向请求重绘（见文件头注释）。
pub(crate) fn indicator_viewport_id() -> ViewportId {
    ViewportId(Id::new("mode_indicator"))
}

/// 徽标 viewport 命令的下发状态。
///
/// `Context::send_viewport_cmd` 内部会对当前 viewport 无条件请求重绘
/// （egui 0.36 `send_viewport_cmd_to` 首行即 `request_repaint_of`），因此
/// 每帧重发同样的位置/显隐命令会让徽标窗口持续自我触发重绘——只要徽标
/// 可见就永不空闲，白耗 CPU/GPU/电量。仅在期望状态变化时下发。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) enum IndicatorSent {
    /// 尚未下发（收到 Show 时重置为它）：强制下一帧下发一次，保证新建的
    /// viewport 不会停留在系统默认位置
    #[default]
    Unknown,
    /// 已下发"屏外停放 + 不可见"
    Parked,
    /// 已下发"在 (x, y) 显示"（逻辑坐标）
    Shown(f32, f32),
}

/// 徽标运行时状态（由候选窗的命令线程随 `apply_ui_command` 更新）
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct IndicatorState {
    pub english: bool,
    pub visible: bool,
    /// 是否已收到光标位置：未收到时把徽标停放到屏幕外（Edge 等 Chromium
    /// 系应用无系统 caret，居中显示会误导）
    pub has_caret: bool,
    pub caret_x: i32,
    pub caret_y: i32,
    pub caret_h: i32,
    /// 已下发的 viewport 命令状态（去重依据，见 [`IndicatorSent`]）
    pub sent: IndicatorSent,
}

/// 显示器矩形（物理像素全幅边界），用于把徽标夹回光标所在显示器。
/// 由候选窗线程从 winit 枚举并缓存；夹取时统一用全局 `pixels_per_point`
/// 换算为逻辑坐标（与光标坐标同一空间，混合 DPI 下才一致）。
pub(crate) type MonitorRects = Vec<Rect>;

/// 隐藏态的停放位置（屏幕外），与候选窗同款：eframe 在首帧渲染后会
/// 无条件 `set_visible(true)`，仅 Visible(false) 有一帧可见窗口期，
/// 挪出屏幕后即使被短暂强制显示也不可见。
const OFFSCREEN_POS: Pos2 = Pos2::new(-32000.0, -32000.0);

/// 计算徽标应处的逻辑坐标：光标右侧邻接，越界时夹回光标所在显示器。
/// 调用方须保证已收到光标位置。
fn indicator_position(
    indicator: &IndicatorState,
    ctx: &Context,
    monitors: &MonitorRects,
) -> (f32, f32) {
    // OuterPosition 自动乘以 pixels_per_point，需把物理像素坐标转为逻辑坐标
    let ppp = ctx.pixels_per_point();
    let caret_x = indicator.caret_x as f32 / ppp;
    let caret_y = indicator.caret_y as f32 / ppp;
    // 显示在光标右侧、紧贴光标（仅一个小间距），与光标行顶对齐；
    // 越界时夹回光标所在显示器工作区
    let mut pos_x = caret_x + GAP;
    let mut pos_y = caret_y;
    // 夹取矩形统一除以全局 pixels_per_point 换算为逻辑坐标，与光标坐标
    // 同一空间——混合 DPI 下按各屏自身 scale 换算会错位
    let screen = monitors
        .iter()
        .find(|rect| {
            rect.contains(Pos2::new(
                indicator.caret_x as f32,
                indicator.caret_y as f32,
            ))
        })
        .map(|rect| {
            Rect::from_min_size(
                Pos2::new(rect.min.x / ppp, rect.min.y / ppp),
                Vec2::new(rect.width() / ppp, rect.height() / ppp),
            )
        })
        .or_else(|| {
            // 光标不在任何缓存显示器内（显示器缓存未及时刷新：热插拔/
            // 分辨率变更）：退化为对全部显示器并集夹取，避免徽标被放到
            // 屏幕外不可见。枚举失败（空表）时返回 None，跳过夹取保持
            // 光标邻接定位——盲翻左侧的负向回退依赖屏幕宽度信息
            (!monitors.is_empty()).then(|| {
                let (min_x, min_y, max_x, max_y) = monitors.iter().fold(
                    (f32::MAX, f32::MAX, f32::MIN, f32::MIN),
                    |(a, b, c, d), r| {
                        (
                            a.min(r.min.x),
                            b.min(r.min.y),
                            c.max(r.max.x),
                            d.max(r.max.y),
                        )
                    },
                );
                Rect::from_min_max(
                    Pos2::new(min_x / ppp, min_y / ppp),
                    Pos2::new(max_x / ppp, max_y / ppp),
                )
            })
        });
    if let Some(screen) = screen {
        if pos_x + BADGE_WIDTH > screen.max.x {
            pos_x = caret_x - BADGE_WIDTH - GAP;
        }
        pos_x = pos_x.max(screen.min.x);
        if pos_y + BADGE_HEIGHT > screen.max.y {
            pos_y = screen.max.y - BADGE_HEIGHT;
        }
        pos_y = pos_y.max(screen.min.y);
    }
    (pos_x, pos_y)
}

/// 渲染徽标 deferred viewport：定位 + 绘制"中"/"英"。
/// 由该 viewport 自身的 pass 调用，`ui` 为该 viewport 的画布
/// （egui 0.36 的 deferred 回调直接提供 `&mut Ui`）。
pub(crate) fn render_mode_indicator(
    indicator: &mut IndicatorState,
    theme: Theme,
    monitors: &MonitorRects,
    ui: &mut Ui,
) {
    let ctx = ui.ctx().clone();
    // 期望状态：None = 屏外停放并不可见（未请求显示，或光标位置未知）
    let target = (indicator.visible && indicator.has_caret)
        .then(|| indicator_position(indicator, &ctx, monitors));
    // 位置以整数光标坐标按同一 ppp 换算而来，比较是精确的；
    // 显示器拓扑变化会改变夹取结果，从而自动触发重发
    let desired = match target {
        Some((x, y)) => IndicatorSent::Shown(x, y),
        None => IndicatorSent::Parked,
    };
    // 仅在下发状态变化时发命令（每条命令都会请求重绘，见 IndicatorSent）
    if indicator.sent != desired {
        match target {
            Some((x, y)) => {
                ctx.send_viewport_cmd(ViewportCommand::OuterPosition(Pos2::new(x, y)));
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            }
            None => {
                // 停放：挪到屏幕外并置不可见（eframe 首帧后会强制
                // set_visible(true)，不停放会在旧位置闪现）
                ctx.send_viewport_cmd(ViewportCommand::OuterPosition(OFFSCREEN_POS));
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
        }
        indicator.sent = desired;
    }

    // 徽标是否真正显示由 `visible && has_caret` 决定，屏外停放（target 为 None）
    // 是"看不到徽标"的唯一原因。留一行可查的现场（debug 级，避免每次 pass
    // 在渲染路径上构造/分发事件）：徽标不显示时可据此直接定位是"没收到
    // Show/被隐藏"（无本行日志）还是"位置未知被停放"（target=None）
    debug!(
        visible = indicator.visible,
        has_caret = indicator.has_caret,
        english = indicator.english,
        ?target,
        "mode indicator pass"
    );

    if target.is_none() {
        // 屏外停放：无内容可画
        return;
    }

    let colors = theme::palette(theme);
    // 英文模式降低存在感（灰底），中文模式用主题色，一眼可辨
    let (fill, text) = if indicator.english {
        (colors.base_300, colors.base_content)
    } else {
        (colors.primary, colors.primary_content)
    };

    // 视口尺寸在创建时已用 with_inner_size([BADGE_WIDTH, BADGE_HEIGHT])
    // 固定，无需重发 InnerSize（理由同 IndicatorSent）
    EguiFrame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(2))
        .show(ui, |ui| {
            ui.set_min_size(Vec2::new(BADGE_WIDTH - 4.0, BADGE_HEIGHT - 4.0));
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                ui.add(
                    Label::new(
                        RichText::new(if indicator.english { "英" } else { "中" })
                            .size(16.0)
                            .color(text),
                    )
                    .selectable(false),
                );
            });
        });
}
