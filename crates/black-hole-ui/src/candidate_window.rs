// ---------------------------------------------------------------------------
// 跨平台候选窗口: eframe + egui
// ---------------------------------------------------------------------------

use super::mode_indicator::{
    BADGE_HEIGHT, BADGE_WIDTH, IndicatorSent, IndicatorState, MonitorRects, indicator_viewport_id,
    render_mode_indicator,
};
use super::*;
use eframe::egui::InnerResponse;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SCROLL_AREA_MAX_HEIGHT: f32 = 200.0;

/// 隐藏态的停放位置（屏幕外）。eframe 在首帧渲染后会无条件
/// `set_visible(true)`（见 `epi_integration::post_rendering`），而我们通过
/// `ViewportCommand::Visible(false)` 发出的隐藏要等同一轮事件循环的
/// `handle_viewport_output` 才生效，中间留有一帧的可见窗口期——启动时
/// 表现为候选窗"一闪而过"。把窗口停到屏幕外后，即使被短暂强制显示也不可见。
const OFFSCREEN_POS: Pos2 = Pos2::new(-32000.0, -32000.0);

pub(crate) struct AppState {
    visible: bool,
    code: String,
    candidates: Vec<Candidate>,
    selected_index: usize,
    caret_x: i32,
    caret_y: i32,
    caret_h: i32,
    should_exit: bool,
    theme: Theme,
    expanded: bool,
    /// 候选文字字号（设置项，热更新生效）
    font_size: u32,
    /// 最大候选数（设置项，热更新生效）
    max_candidates: usize,
    /// LLM 整句补全文本（首行选中词后的灰色 ghost text）
    completion: Option<String>,
    /// 补全对应的编码串，与当前 code 比对避免异步结果错位显示
    completion_code: String,
    /// 补全对应的选中候选索引，与当前 selected_index 比对：
    /// 导航到其它选项时旧补全自动失效，新选中项由新请求的补全接管
    completion_index: usize,
    /// "整句上屏"实际绑定的按键名（daemon 同步，默认 Tab），首行提示用
    commit_sentence: String,
    /// 中英模式徽标状态：聚焦显示、输入隐藏、切换再现（见 mode_indicator）
    indicator: IndicatorState,
    /// 显示器矩形缓存（物理像素 + 缩放），供徽标夹取光标所在工作区
    monitors: MonitorRects,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            visible: false,
            code: String::new(),
            candidates: Vec::new(),
            selected_index: 0,
            caret_x: 0,
            caret_y: 0,
            caret_h: 0,
            should_exit: false,
            theme: Theme::Light,
            expanded: false,
            font_size: 14,
            max_candidates: 9,
            completion: None,
            completion_code: String::new(),
            completion_index: 0,
            commit_sentence: "Tab".to_string(),
            indicator: IndicatorState::default(),
            monitors: Vec::new(),
        }
    }
}

pub struct ImeUiApp {
    state: Arc<Mutex<AppState>>,
    win_style_applied: bool,
    /// 已应用全局样式的主题（含 `System` 解析后的明暗），仅在变化时重设：
    /// 候选窗每次按键都会重绘，逐帧 `set_visuals` 会白白丢弃样式缓存。
    applied_theme: Option<(Theme, bool)>,
    /// 上次刷新显示器缓存（存于 AppState.monitors）的时间
    monitors_refreshed_at: Option<Instant>,
}

/// 显示器拓扑仅在热插拔/显示设置变化时改变，周期刷新兜底即可
const MONITORS_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

impl ImeUiApp {
    pub fn new(_cc: &CreationContext<'_>, state: Arc<Mutex<AppState>>) -> Self {
        Self {
            state,
            win_style_applied: false,
            applied_theme: None,
            monitors_refreshed_at: None,
        }
    }
}

impl App for ImeUiApp {
    fn ui(&mut self, ui: &mut Ui, frame: &mut Frame) {
        let ctx = ui.ctx().clone();
        #[cfg(target_os = "windows")]
        if !self.win_style_applied {
            apply_windows_style(frame);
            self.win_style_applied = true;
        }

        // 显示器缓存刷新与徽标显隐读取的锁窗口保持短小，随后即释放：
        // deferred viewport 的回调与父 pass 共用同一把 state 锁，若在持锁
        // 期间注册 viewport，而某后端（egui 的 embed_viewports，如 wasm）
        // 同步调用该回调，就会自死锁。桌面端 eframe 的 embed_viewports 为
        // false（回调走独立 pass），但这里不依赖该前提。
        //
        // 显示器枚举（winit 系统调用）不持锁执行：命令线程处理每条
        // UiCommand 都要取同一把锁，持锁枚举会把进行中的命令阻塞整个
        // 枚举时长（缓存每 ~5s 过期一次即触发）。
        let monitors_maybe = {
            let state = self.state.lock().unwrap();
            if state.should_exit {
                drop(state);
                ctx.send_viewport_cmd(ViewportCommand::Close);
                return;
            }
            // 显示器矩形缓存（物理像素 + 缩放），供徽标定位夹取使用；
            // 仅在首次或超期时重新枚举，避免每帧 EnumDisplayMonitors 开销
            if self
                .monitors_refreshed_at
                .is_none_or(|t| t.elapsed() >= MONITORS_REFRESH_INTERVAL)
            {
                Some(collect_monitors(frame))
            } else {
                None
            }
        };

        let indicator_visible = {
            let mut state = self.state.lock().unwrap();

            if state.should_exit {
                drop(state);
                ctx.send_viewport_cmd(ViewportCommand::Close);
                return;
            }

            if let Some(monitors) = monitors_maybe {
                state.monitors = monitors;
                self.monitors_refreshed_at = Some(Instant::now());
            }

            state.indicator.visible
        };

        // 中英模式徽标：与候选窗同一事件循环的 deferred viewport。
        // 必须在候选窗早退（隐藏）之前注册，保证候选窗隐藏时徽标仍能显示。
        // 隐藏时不再注册，egui 会自动关闭该 viewport。
        if indicator_visible {
            let state_arc = Arc::clone(&self.state);
            ctx.show_viewport_deferred(
                indicator_viewport_id(),
                ViewportBuilder::default()
                    .with_decorations(false)
                    .with_transparent(true)
                    .with_always_on_top()
                    .with_active(false)
                    .with_resizable(false)
                    .with_visible(false)
                    .with_taskbar(false)
                    .with_inner_size([BADGE_WIDTH, BADGE_HEIGHT]),
                move |vui, _class| {
                    let mut guard = state_arc.lock().unwrap();
                    // 先降级为 &mut AppState：MutexGuard 的字段访问都经过
                    // Deref/DerefMut（借用整个 guard），无法拆分字段借用
                    let st = &mut *guard;
                    // 传 &mut：渲染时顺便记录已下发的 viewport 命令状态，
                    // 避免每帧重发无变化命令而自我触发重绘（见 IndicatorSent）
                    render_mode_indicator(&mut st.indicator, st.theme, &st.monitors, vui);
                },
            );
        }

        let state = self.state.lock().unwrap();

        // 不可见或无候选时隐藏窗口：停放到屏幕外并置不可见。
        // 仅 Visible(false) 不够——eframe 每帧 post_rendering 都会强制
        // set_visible(true)，故必须同时挪出屏幕兜底。
        if !(state.visible && !state.candidates.is_empty()) {
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(OFFSCREEN_POS));
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            return;
        }

        // 显示窗口：完整列表保留在 state 中，渲染仅取包含选中项的窗口，
        // 使高亮与引擎实际选择保持一致
        let (win_start, win_end) = display_window(&state);
        let win = &state.candidates[win_start..win_end];
        let win_selected = state.selected_index - win_start;
        let (desired_width, desired_height) = estimate_window_size(&state);
        position_window(&ctx, &state, frame, desired_width, desired_height);

        let colors = theme::palette(state.theme);
        theme::sync_visuals(&ctx, state.theme, &mut self.applied_theme);

        EguiFrame::new()
            .fill(colors.base_100)
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.set_min_size(Vec2::new(desired_width, desired_height));
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    render_first_row(ui, &state, win, win_selected, &colors);
                    ui.add_space(6.0);
                    render_candidate_rows(ui, &state, win, win_selected, &colors);
                });
            });
    }
}

#[cfg(target_os = "windows")]
fn apply_windows_style(frame: &mut Frame) {
    let Some(window) = frame.winit_window() else {
        return;
    };
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return;
    };

    let hwnd = HWND(h.hwnd.get() as *mut c_void);
    unsafe {
        // 设置 DWM 圆角窗口
        let corner_pref: u32 = 2; // DWMWCP_ROUND
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(33),
            &corner_pref as *const _ as *const c_void,
            mem::size_of::<u32>() as u32,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn apply_windows_style(_frame: &mut Frame) {}

/// 获取当前显示器工作区逻辑坐标矩形（排除任务栏），用于边界防越
pub(crate) fn get_screen_rect(frame: &Frame) -> Option<Rect> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let window = frame.winit_window()?;
        let monitor = window
            .current_monitor()
            .or_else(|| window.primary_monitor())?;
        let scale = monitor.scale_factor() as f32;

        if let Some(rect) = windows_work_area(window, scale) {
            return Some(rect);
        }

        // 非 Windows 或获取工作区失败时回退到完整监视器尺寸
        let size = monitor.size();
        let pos = monitor.position();
        Some(Rect::from_min_size(
            Pos2::new(pos.x as f32 / scale, pos.y as f32 / scale),
            Vec2::new(size.width as f32 / scale, size.height as f32 / scale),
        ))
    }
    #[cfg(target_arch = "wasm32")]
    {
        None
    }
}

/// Windows 下通过 Win32 API 获取工作区逻辑矩形；失败返回 None。
#[cfg(target_os = "windows")]
fn windows_work_area(window: &Window, scale: f32) -> Option<Rect> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return None;
    };
    let hwnd = HWND(h.hwnd.get() as *mut c_void);
    unsafe {
        let hmonitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: RECT::default(),
            rcWork: RECT::default(),
            dwFlags: 0,
        };
        if !GetMonitorInfoW(hmonitor, &mut info).as_bool() {
            return None;
        }
        Some(Rect::from_min_max(
            Pos2::new(
                info.rcWork.left as f32 / scale,
                info.rcWork.top as f32 / scale,
            ),
            Pos2::new(
                info.rcWork.right as f32 / scale,
                info.rcWork.bottom as f32 / scale,
            ),
        ))
    }
}

/// 非 Windows 平台没有 Win32 工作区可查，始终回退到监视器尺寸。
#[cfg(not(target_os = "windows"))]
fn windows_work_area(_window: &Window, _scale: f32) -> Option<Rect> {
    None
}

/// 选中候选（首行）字号：在基础字号上放大，保持与候选项的视觉层级
fn selected_font_size(font_size: u32) -> f32 {
    font_size as f32 + 10.0
}

/// 候选项字号：基础字号 + 少量补偿（与历史硬编码 16px 对齐）
fn item_font_size(font_size: u32) -> f32 {
    font_size as f32 + 2.0
}

fn render_candidate_item(
    ui: &mut Ui,
    display_number: usize,
    candidate: &Candidate,
    text_color: Color32,
    label_color: Color32,
    show_label: bool,
    font_size: u32,
) {
    // 无论是否显示序号，均使用固定尺寸，确保布局完全一致
    let label_text = if show_label {
        format!("{}.", display_number)
    } else {
        String::new()
    };
    let item_font = item_font_size(font_size);
    ui.spacing_mut().item_spacing.x = 0.0;
    ui.add_sized(
        [20.0, 14.0],
        Label::new(RichText::new(label_text).size(14.0).color(label_color)).selectable(false),
    );
    ui.add(
        Label::new(
            RichText::new(&candidate.text)
                .size(item_font)
                .color(text_color),
        )
        .selectable(false),
    );
}

/// 计算当前应渲染的候选下标窗口 [start, end)：
/// - 展开状态：返回完整列表，由 ScrollArea 负责滚动，避免候选被截断
/// - 折叠状态：最多 max_candidates 项，且始终包含引擎的选中索引，
///   保证 UI 高亮与引擎实际选择一致。
fn display_window(state: &AppState) -> (usize, usize) {
    let len = state.candidates.len();
    if state.expanded {
        return (0, len);
    }
    let max = state.max_candidates.max(1);
    if len <= max {
        return (0, len);
    }
    // 折叠状态：选中索引超出首屏窗口时，窗口跟随滚动以包含选中项
    let end = (state.selected_index + 1).max(max).min(len);
    let start = end - max;
    (start, end)
}

fn estimate_window_size(state: &AppState) -> (f32, f32) {
    let frame_padding = 20.0; // 外层 Frame inner_margin 10 * 2
    let row_spacing = 12.0; // 行间距
    // 候选项行高：字号 + Frame inner_margin 4+4 + 余量
    let candidate_row_height = item_font_size(state.font_size) + 14.0;

    // 只按当前显示窗口估算尺寸，避免隐藏候选撑高窗口
    let (win_start, win_end) = display_window(state);
    let win = &state.candidates[win_start..win_end];

    let first_row_height = if win.is_empty() {
        0.0
    } else {
        // 选中项行高：字号 + inner_margin 6+6 + 少量余量
        selected_font_size(state.font_size) + 18.0
    };

    let total_width = CANDIDATE_WINDOW_WIDTH;

    let total_height = if win.is_empty() {
        0.0
    } else if state.expanded {
        // 展开状态：根据实际行数计算内容高度，避免固定留白（排除首选词）
        let rows = layout_candidates_into_rows_excluding(
            win,
            EXPANDED_AVAILABLE_WIDTH,
            ITEM_SPACING,
            Some(0),
        );
        // ScrollArea 内部每行之间有 item_spacing.y = 6.0，需计入
        let inter_row_spacing = 6.0f32;
        let content_height = rows.len() as f32 * candidate_row_height
            + rows.len().saturating_sub(1) as f32 * inter_row_spacing;
        let scroll_height = content_height.min(SCROLL_AREA_MAX_HEIGHT);
        (first_row_height + scroll_height + row_spacing + frame_padding).max(64.0)
    } else {
        // 折叠状态：第一行 + 第二行（如果有）+ padding
        let second_row_height = if win.len() <= 1 {
            0.0
        } else {
            // 候选项行高：字号 + 上下余量
            item_font_size(state.font_size) + 16.0
        };
        (first_row_height + second_row_height + row_spacing + frame_padding).max(64.0)
    };

    (total_width, total_height.min(320.0))
}

/// 启动候选窗口事件循环（阻塞当前线程）
pub fn run_candidate_window(
    ui_rx: Receiver<UiCommand>,
    initial_theme: Theme,
    initial_cw: CandidateWindowSettings,
) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        run_candidate_window_inner(ui_rx, initial_theme, initial_cw);
    }));
    let Err(e) = result else {
        return;
    };
    let msg = e
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| e.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    error!(msg, "run_candidate_window panicked");
}

fn run_candidate_window_inner(
    ui_rx: Receiver<UiCommand>,
    initial_theme: Theme,
    initial_cw: CandidateWindowSettings,
) {
    info!("run_candidate_window: starting");

    let state = Arc::new(Mutex::new(AppState {
        theme: initial_theme,
        font_size: initial_cw.font_size,
        max_candidates: initial_cw.max_candidates,
        ..AppState::default()
    }));

    #[cfg(target_os = "windows")]
    let event_loop_builder = Some(Box::new(|builder: &mut EventLoopBuilder<_>| {
        builder.with_any_thread(true);
    }) as EventLoopBuilderHook);

    #[cfg(not(target_os = "windows"))]
    let event_loop_builder: Option<EventLoopBuilderHook> = None;

    // 注意：候选窗不要实现 `App::clear_color`（保持默认的透明），最终画面
    // 靠应用透明视口 + `EguiFrame::corner_radius` 得到真正的圆角窗口；
    // 一旦把清屏色改成不透明底色，圆角外区域会被填成方块。
    let options = NativeOptions {
        viewport: ViewportBuilder::default()
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_active(false)
            .with_resizable(false)
            .with_visible(false)
            .with_taskbar(false)
            .with_inner_size([320.0, 40.0]),
        event_loop_builder,
        wgpu_options: wgpu_configuration(),
        ..Default::default()
    };

    let result = run_native(
        "Black-Hole Candidate",
        options,
        Box::new(|cc| {
            configure_fonts(&cc.egui_ctx);
            spawn_command_thread(cc.egui_ctx.clone(), ui_rx, Arc::clone(&state));
            Ok(Box::new(ImeUiApp::new(cc, state)))
        }),
    );

    if let Err(e) = result {
        error!(error = ?e, "eframe run_native error");
    }
}

/// 启动后台线程：从 channel 接收 UI 命令并应用到共享状态，
/// 需要重绘时通知 egui。
fn spawn_command_thread(ctx: Context, ui_rx: Receiver<UiCommand>, state: Arc<Mutex<AppState>>) {
    thread::spawn(move || {
        info!("channel thread started");
        while let Ok(cmd) = ui_rx.recv() {
            info!(
                cmd = ?mem::discriminant(&cmd),
                "channel thread recv"
            );
            // 单次加锁：apply_ui_command 与徽标可见性读取共用一把锁，
            // 该路径是每条命令的必经热路径（锁被渲染线程整帧持有）
            let (should_repaint, indicator_visible) = {
                let mut s = state.lock().unwrap();
                let repaint = apply_ui_command(&mut s, cmd);
                (repaint, s.indicator.visible)
            };
            if should_repaint {
                ctx.request_repaint();
                // 徽标是独立的 deferred viewport，但（egui 0.36）所有 viewport
                // 共享同一个 Context：pass 之外调用的 request_repaint() 只作用
                // 于 ROOT（主窗口），必须用 request_repaint_of 定向徽标，否则
                // 字样/位置变化后画面停留在旧状态
                if indicator_visible {
                    ctx.request_repaint_of(indicator_viewport_id());
                }
            }
        }
        info!("channel thread exited");
    });
}

/// 根据光标位置与屏幕工作区计算窗口位置并下发视口命令
fn position_window(
    ctx: &Context,
    state: &AppState,
    frame: &Frame,
    desired_width: f32,
    desired_height: f32,
) {
    ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(
        desired_width,
        desired_height,
    )));
    // 当前 egui 版本仅支持 OuterPosition；OuterPosition 会自动乘以
    // pixels_per_point 做 DPI 缩放，因此需将物理像素坐标先转为逻辑坐标
    let ppp = ctx.pixels_per_point();
    let caret_x = state.caret_x as f32 / ppp;
    let caret_y = state.caret_y as f32 / ppp;
    // 部分应用（如 Chromium）的 collapsed range 高度可能不准确，
    // 因此至少按一行文本高度预留净空
    let caret_h = (state.caret_h as f32 / ppp).max(16.0);
    let gap = 6.0;

    let (pos_x, pos_y) = clamp_window_position(
        get_screen_rect(frame),
        caret_x,
        caret_y,
        caret_h,
        desired_width,
        desired_height,
        gap,
    );

    ctx.send_viewport_cmd(ViewportCommand::OuterPosition(Pos2::new(pos_x, pos_y)));
    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
}

/// 防止候选窗越界：y 方向优先下方，放不下则放上方，仍放不下时选空间
/// 更大的一侧；x 方向夹回工作区内。
#[allow(clippy::too_many_arguments)]
fn clamp_window_position(
    screen: Option<Rect>,
    caret_x: f32,
    caret_y: f32,
    caret_h: f32,
    logical_w: f32,
    logical_h: f32,
    gap: f32,
) -> (f32, f32) {
    // 默认显示在光标下方
    let mut pos_y = caret_y + gap;
    let Some(screen) = screen else {
        return (caret_x, pos_y);
    };

    if pos_y + logical_h > screen.max.y {
        pos_y = caret_y - caret_h - logical_h - gap;
        if pos_y < screen.min.y {
            // 上下都放不下，选择空间更大的一侧
            let space_below = screen.max.y - caret_y;
            let space_above = caret_y - caret_h - screen.min.y;
            pos_y = if space_below >= space_above {
                screen.max.y - logical_h
            } else {
                screen.min.y
            };
        }
    }

    let mut pos_x = caret_x;
    if pos_x + logical_w > screen.max.x {
        pos_x = screen.max.x - logical_w;
    }
    pos_x = pos_x.max(screen.min.x);
    (pos_x, pos_y)
}

/// 第一行：选中词 + LLM 整句补全 ghost text。
/// 选中词单独使用高亮块（仅首选未导航时）；补全部分放在高亮块之外的
/// 普通背景上、以灰色显示，绝不呈现高亮。
fn render_first_row(
    ui: &mut Ui,
    state: &AppState,
    win: &[Candidate],
    win_selected: usize,
    colors: &theme::Palette,
) {
    if win.is_empty() {
        return;
    }
    let selected_font = selected_font_size(state.font_size);
    // 仅当首选被选中（未导航到其它行）时保持高亮样式
    let is_first = win_selected == 0;

    let selected = &win[win_selected];
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        // 选中词：独立高亮块（白字）或普通块（正文色）
        render_selected_block(ui, selected, selected_font, is_first, colors);
        render_completion(ui, state, selected_font, colors);
    });
}

/// 首行的选中词块：首选被选中时使用高亮（白字），否则普通正文色。
fn render_selected_block(
    ui: &mut Ui,
    selected: &Candidate,
    selected_font: f32,
    is_first: bool,
    colors: &theme::Palette,
) {
    let (fill, text_color) = if is_first {
        (colors.primary, colors.primary_content)
    } else {
        (colors.base_100, colors.base_content)
    };
    EguiFrame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            ui.add(
                Label::new(
                    RichText::new(&selected.text)
                        .size(selected_font)
                        .color(text_color),
                )
                .selectable(false),
            );
        });
}

/// LLM 补全：仅当与当前编码及当前选中项一致时展示，避免异步错位；
/// 始终以普通灰色显示（不在高亮块内）。
fn render_completion(ui: &mut Ui, state: &AppState, selected_font: f32, colors: &theme::Palette) {
    if state.completion_code != state.code
        || state.completion_index != state.selected_index
        || state.completion.is_none()
    {
        return;
    }
    let completion = state.completion.as_deref().unwrap_or_default();
    ui.add(
        Label::new(
            RichText::new(completion)
                .size(selected_font)
                // 比序号色（base_content_weak，0.6）更淡一档，避免补全文字被误读为候选
                .color(colors.base_content.gamma_multiply(0.45)),
        )
        .selectable(false)
        .truncate(),
    );
    ui.add(
        Label::new(
            RichText::new(&state.commit_sentence)
                .size(11.0)
                .color(colors.base_content_weak),
        )
        .selectable(false),
    );
}

/// 候选项网格：展开状态渲染可滚动的多行不规则网格，折叠状态仅渲染
/// 第一行（两者均排除首选词，第一行已单独展示）。
fn render_candidate_rows(
    ui: &mut Ui,
    state: &AppState,
    win: &[Candidate],
    win_selected: usize,
    colors: &theme::Palette,
) {
    if win.len() <= 1 {
        return;
    }
    let rows =
        layout_candidates_into_rows_excluding(win, EXPANDED_AVAILABLE_WIDTH, ITEM_SPACING, Some(0));
    if state.expanded {
        render_expanded_rows(ui, &rows, win, win_selected, state.font_size, colors);
    } else if let Some(first_row) = rows.first() {
        render_collapsed_row(ui, first_row, win, win_selected, colors, state.font_size);
    }
}

/// 一条候选项的高亮/普通配色：(文字色, 序号色, 背景色)
fn item_colors(is_selected: bool, colors: &theme::Palette) -> (Color32, Color32, Option<Color32>) {
    if is_selected {
        (
            colors.primary_content,
            colors.primary_content,
            Some(colors.primary),
        )
    } else {
        (colors.base_content, colors.base_content_weak, None)
    }
}

fn render_expanded_rows(
    ui: &mut Ui,
    rows: &[Vec<usize>],
    win: &[Candidate],
    win_selected: usize,
    font_size: u32,
    colors: &theme::Palette,
) {
    ScrollArea::vertical()
        .max_height(SCROLL_AREA_MAX_HEIGHT)
        .show(ui, |ui| {
            for row in rows {
                render_expanded_row(ui, row, win, win_selected, font_size, colors);
            }
        });
}

/// 展开状态的一行候选项（遍历渲染 + 选中项滚动跟随）。
fn render_expanded_row(
    ui: &mut Ui,
    row: &[usize],
    win: &[Candidate],
    win_selected: usize,
    font_size: u32,
    colors: &theme::Palette,
) {
    let is_selected_row = row.contains(&win_selected);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = ITEM_SPACING;
        for (col, i) in row.iter().enumerate() {
            let response = render_candidate_cell(
                ui,
                col,
                &win[*i],
                *i == win_selected,
                is_selected_row,
                font_size,
                colors,
            );
            if *i == win_selected {
                // 仅在选中项滚出可视区时做最小滚动，避免上下
                // 导航时整个表格反复居中跳动
                response.response.scroll_to_me(None);
            }
        }
    });
}

/// 渲染单个候选项单元格，返回其响应（供滚动跟随使用）。
fn render_candidate_cell(
    ui: &mut Ui,
    col: usize,
    candidate: &Candidate,
    is_selected: bool,
    show_label: bool,
    font_size: u32,
    colors: &theme::Palette,
) -> InnerResponse<()> {
    let (tc, lc, bg) = item_colors(is_selected, colors);
    candidate_item_frame(ui, bg.unwrap_or(colors.base_100), |ui| {
        render_candidate_item(ui, col + 1, candidate, tc, lc, show_label, font_size);
    })
}

fn render_collapsed_row(
    ui: &mut Ui,
    row: &[usize],
    win: &[Candidate],
    win_selected: usize,
    colors: &theme::Palette,
    font_size: u32,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = ITEM_SPACING;
        for (col, i) in row.iter().enumerate() {
            render_candidate_cell(
                ui,
                col,
                &win[*i],
                *i == win_selected,
                true,
                font_size,
                colors,
            );
        }
    });
}

fn candidate_item_frame(
    ui: &mut Ui,
    fill: Color32,
    content: impl FnOnce(&mut Ui),
) -> InnerResponse<()> {
    EguiFrame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::same(4))
        .show(ui, |ui| {
            ui.horizontal(|ui| content(ui));
        })
}

/// 枚举所有显示器（物理像素全幅矩形），供徽标定位夹取。
/// 注意是全幅边界而非工作区（未排除任务栏）：徽标夹取用，任务栏遮挡的
/// 概率与影响都很小。不存各显示器自身 scale：徽标坐标以全局
/// `pixels_per_point` 换算，夹取矩形须用同一全局缩放换算才一致
/// （混合 DPI 下按各屏自身 scale 换算会错位）；获取失败返回空列表。
fn collect_monitors(frame: &Frame) -> MonitorRects {
    let Some(window) = frame.winit_window() else {
        return Vec::new();
    };
    window
        .available_monitors()
        .map(|m| {
            let pos = m.position();
            let size = m.size();
            Rect::from_min_size(
                Pos2::new(pos.x as f32, pos.y as f32),
                Vec2::new(size.width as f32, size.height as f32),
            )
        })
        .collect()
}

/// 应用一条 UI 命令到共享状态，返回是否需要重绘。
/// 命令仅在窗口可见（或刚显示/退出）时触发重绘，避免隐藏时频繁刷新。
fn apply_ui_command(s: &mut AppState, cmd: UiCommand) -> bool {
    let mut should_repaint = s.visible;
    match cmd {
        UiCommand::ShowCandidates {
            code,
            candidates,
            selected_index,
            context,
            expanded,
        } => {
            // 只在首次展示时固定位置，避免后续更新候选列表时闪动
            if !s.visible {
                s.caret_x = context.caret_x;
                s.caret_y = context.caret_y;
                s.caret_h = context.caret_h;
            }
            s.visible = true;
            s.code = code;
            // 保留引擎的完整候选列表与真实选中索引；渲染时按
            // max_candidates 仅显示一个包含选中项的窗口，避免截断
            // 导致 UI 高亮与引擎实际提交的候选不一致。
            s.candidates = candidates;
            s.selected_index = selected_index.min(s.candidates.len().saturating_sub(1));
            s.expanded = expanded;
            // 开始输入：隐藏中英模式徽标（切换时再现）
            s.indicator.visible = false;
            should_repaint = true;
        }
        UiCommand::SetCandidateWindowSettings(cw) => {
            s.font_size = cw.font_size;
            s.max_candidates = cw.max_candidates.max(1);
            // 完整列表与选中索引均保留，显示窗口在渲染时按
            // 新的 max_candidates 即时重算，无需在此截断
            should_repaint = true;
        }
        UiCommand::SetCommitSentenceKey(key) => {
            s.commit_sentence = key;
            should_repaint = true;
        }
        UiCommand::UpdatePosition { context } => {
            if s.visible {
                s.caret_x = context.caret_x;
                s.caret_y = context.caret_y;
                s.caret_h = context.caret_h;
            }
            // 徽标始终记录最新光标（不受候选窗显隐影响），聚焦后
            // 输入前即可就位；仅可见时才需要重绘
            let moved = s.indicator.caret_x != context.caret_x
                || s.indicator.caret_y != context.caret_y
                || s.indicator.caret_h != context.caret_h;
            let first = !s.indicator.has_caret;
            s.indicator.caret_x = context.caret_x;
            s.indicator.caret_y = context.caret_y;
            s.indicator.caret_h = context.caret_h;
            s.indicator.has_caret = true;
            if s.indicator.visible && (moved || first) {
                should_repaint = true;
            }
        }
        UiCommand::HideCandidates | UiCommand::CommitText(_) => {
            s.visible = false;
            // 上屏同样属于"开始输入"：隐藏中英模式徽标。
            // 徽标可见时必须重绘（其 deferred viewport 的画面需消失）；
            // 徽标本就不可见时恢复旧的"候选窗隐藏不重绘"语义，避免
            // 每次整句上屏/收起都多一次全量重绘
            let hid_indicator = s.indicator.visible;
            s.indicator.visible = false;
            should_repaint = hid_indicator;
            clear_completion(s);
        }
        UiCommand::ShowModeIndicator {
            context,
            invalidate_caret,
        } => {
            match context {
                Some(ctx) => {
                    s.indicator.caret_x = ctx.caret_x;
                    s.indicator.caret_y = ctx.caret_y;
                    s.indicator.caret_h = ctx.caret_h;
                    s.indicator.has_caret = true;
                }
                // 本次没取到光标位置：作废与否取决于焦点会话是否变更。
                // - 焦点已切到别的窗口（invalidate_caret=true）：旧坐标对本窗口
                //   无效，作废并屏外停放（见 mode_indicator），待新位置上报后
                //   立即就位，避免在旧窗口的位置闪现；
                // - 同一会话内重显（切换中英，invalidate_caret=false）：沿用已
                //   记录的位置。此处若一律作废，Chromium 等无系统 caret 的应用
                //   在切换中英时徽标会屏外停放且再无唤醒机会——模式切换不产生
                //   文本变化，等不到后续 TF_LC_CHANGE 上报位置
                None => {
                    if invalidate_caret {
                        s.indicator.has_caret = false;
                    }
                }
            }
            s.indicator.visible = true;
            // 强制下一帧重新下发一次 viewport 命令：Show 是新焦点会话/新
            // viewport 的起点，此前的下发状态对它无效（否则新建窗口可能
            // 停在系统默认位置）
            s.indicator.sent = IndicatorSent::Unknown;
            should_repaint = true;
        }
        UiCommand::HideModeIndicator { invalidate_caret } => {
            // 焦点失效时同步作废光标位置：has_caret 必须忠实表示"当前焦点
            // 的光标有效"，否则模式同步命令会在旧窗口的过期位置重显徽标。
            // 英文直输隐藏（invalidate_caret=false）保留位置，切换中英时
            // 仍可在原光标处重显。已隐藏时不再置位重绘：英文逐键下发本命令，
            // 避免无谓重绘
            if invalidate_caret {
                // 焦点已失效，同步作废光标位置。跨进程乱序的 Hide 已在
                // daemon 平台层按"聚焦连接"仲裁丢弃（见 windows_tsf/mod.rs）
                s.indicator.has_caret = false;
            }
            if s.indicator.visible {
                s.indicator.visible = false;
                should_repaint = true;
            }
        }
        UiCommand::SetInputMode(english) | UiCommand::SetInputModeTransient(english) => {
            // 仅同步徽标字样，不改显隐：重显由切换实例自身的
            // ShowModeIndicator（携带自己必然持有的焦点光标位置）驱动。
            // 跨进程模式同步不带光标信息，若在此基于可能过期的 has_caret
            // 重显，徽标会在旧窗口的过期光标位置闪现/停留（评审 P2 修复）
            s.indicator.english = english;
            // 徽标当前可见时字样变化必须重绘：deferred viewport 仅在对自身
            // 请求重绘时才刷新，否则旧"中/英"字样一直停留在屏幕上
            should_repaint = s.indicator.visible;
        }
        UiCommand::Completion {
            code,
            selected_index,
            text,
        } => {
            s.completion_code = code;
            s.completion_index = selected_index;
            s.completion = text;
            should_repaint = true;
        }
        UiCommand::SetTheme(theme) => {
            s.theme = theme;
            should_repaint = true;
        }
        UiCommand::Exit => {
            s.should_exit = true;
            should_repaint = true;
        }
        _ => {}
    }
    should_repaint
}

/// 清空补全状态：防止重打相同编码时旧 ghost text 重现（隐藏与上屏
/// 后都需清空，旧补全与已提交内容分离）。
fn clear_completion(s: &mut AppState) {
    s.completion = None;
    s.completion_code.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use black_hole_shared::InputContext;
    use eframe::egui::RawInput;

    /// 构造仅含定位信息的上下文
    fn ctx_at(x: i32, y: i32) -> InputContext {
        InputContext::caret(x, y, 20)
    }

    /// 跑一次徽标 viewport 的 pass，返回该 pass 下发的 viewport 命令。
    /// 测试在 ROOT pass 内渲染，命令因此记在 ROOT 上——去重行为与命令落在
    /// 哪个 viewport 无关，故此处断言有效（egui 自身也可能为 ROOT 追加
    /// 命令，故断言只看本模块关心的两类命令）。
    fn badge_commands_per_pass(
        indicator: &mut IndicatorState,
        monitors: &MonitorRects,
    ) -> Vec<ViewportCommand> {
        let ctx = Context::default();
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            render_mode_indicator(indicator, Theme::Light, monitors, ui);
        });
        let commands = output
            .viewport_output
            .get(&ViewportId::ROOT)
            .map_or_else(Vec::new, |v| v.commands.clone());
        // 绘制文字会产生字体纹理增量；epaint 要求 drop 前消费或清空它，
        // 否则 panic（真实运行时由 eframe 上传后清空）
        output.textures_delta.clear();
        commands
    }

    /// 本次 pass 下发的位置命令
    fn positions(cmds: &[ViewportCommand]) -> Vec<Pos2> {
        cmds.iter()
            .filter_map(|c| match c {
                ViewportCommand::OuterPosition(p) => Some(*p),
                _ => None,
            })
            .collect()
    }

    /// 本次 pass 下发的显隐命令
    fn visibilities(cmds: &[ViewportCommand]) -> Vec<bool> {
        cmds.iter()
            .filter_map(|c| match c {
                ViewportCommand::Visible(v) => Some(*v),
                _ => None,
            })
            .collect()
    }

    /// 断言徽标状态。真实显示条件是 `visible && has_caret`（渲染端任一
    /// 不满足即屏外停放）：`visible` 单独为 true 只表示"已请求显示"，
    /// 无光标位置时不会实际出现在屏幕上。
    fn assert_indicator(s: &AppState, want_displayed: bool, want_has_caret: bool, line: u32) {
        let displayed = s.indicator.visible && s.indicator.has_caret;
        assert_eq!(displayed, want_displayed, "line {line}: displayed");
        assert_eq!(
            s.indicator.has_caret, want_has_caret,
            "line {line}: has_caret"
        );
    }

    #[test]
    fn show_without_caret_then_set_input_mode_stays_offscreen() {
        // Show(None)（无光标位置，如 Chromium 聚焦）→ SetInputMode：
        // 无光标位置时徽标屏外停放，模式同步不得使其出现在屏幕上
        let mut s = AppState::default();
        apply_ui_command(
            &mut s,
            UiCommand::ShowModeIndicator {
                context: None,
                invalidate_caret: true,
            },
        );
        apply_ui_command(&mut s, UiCommand::SetInputMode(true));
        assert_indicator(&s, false, false, line!());
    }

    #[test]
    fn focus_loss_hide_invalidates_caret() {
        // Show(Some) → Hide(invalidate=true)（焦点失效）→ SetInputMode：
        // 光标已作废，模式同步（仅同步字样）不得在旧窗口位置重显
        let mut s = AppState::default();
        apply_ui_command(
            &mut s,
            UiCommand::ShowModeIndicator {
                context: Some(ctx_at(100, 200)),
                invalidate_caret: true,
            },
        );
        apply_ui_command(
            &mut s,
            UiCommand::HideModeIndicator {
                invalidate_caret: true,
            },
        );
        assert_indicator(&s, false, false, line!());
        apply_ui_command(&mut s, UiCommand::SetInputMode(false));
        assert_indicator(&s, false, false, line!());
    }

    #[test]
    fn english_input_hide_keeps_caret_and_setinputmode_only_updates_label() {
        // Show(Some) → Hide(invalidate=false)（英文直输隐藏）→ SetInputMode：
        // 焦点仍在输入，光标保留；SetInputMode 仅同步字样不改显隐（重显由
        // 切换实例显式 Show 驱动），显式 Show 在原光标处重显
        let mut s = AppState::default();
        apply_ui_command(
            &mut s,
            UiCommand::ShowModeIndicator {
                context: Some(ctx_at(100, 200)),
                invalidate_caret: true,
            },
        );
        apply_ui_command(
            &mut s,
            UiCommand::HideModeIndicator {
                invalidate_caret: false,
            },
        );
        assert_indicator(&s, false, true, line!());
        // 徽标不可见时仅同步字样，不触发重绘
        let repainted = apply_ui_command(&mut s, UiCommand::SetInputMode(true));
        assert!(
            !repainted,
            "line {}: 隐藏时 SetInputMode 不应触发重绘",
            line!()
        );
        assert_indicator(&s, false, true, line!());
        apply_ui_command(
            &mut s,
            UiCommand::ShowModeIndicator {
                context: Some(ctx_at(100, 200)),
                // 模式切换后的重显走生产路径的 false（同一焦点会话沿用已记录位置）
                invalidate_caret: false,
            },
        );
        assert_indicator(&s, true, true, line!());
        // 徽标可见时字样变化必须触发重绘，否则 deferred viewport 停留旧字样
        let repainted = apply_ui_command(&mut s, UiCommand::SetInputMode(false));
        assert!(
            repainted,
            "line {}: 可见时 SetInputMode 必须触发重绘",
            line!()
        );
    }

    #[test]
    fn start_input_hides_badge() {
        // 打字（ShowCandidates）与上屏（CommitText）均属于"开始输入"：
        // 徽标隐藏；CommitText 不作废光标（焦点仍在输入）
        let mut s = AppState::default();
        apply_ui_command(
            &mut s,
            UiCommand::ShowModeIndicator {
                context: Some(ctx_at(100, 200)),
                invalidate_caret: true,
            },
        );
        apply_ui_command(
            &mut s,
            UiCommand::ShowCandidates {
                code: "nh".into(),
                candidates: vec![Candidate {
                    text: "你好".into(),
                    comment: None,
                    score: 0,
                }],
                selected_index: 0,
                context: ctx_at(100, 200),
                expanded: false,
            },
        );
        assert_indicator(&s, false, true, line!());
        apply_ui_command(&mut s, UiCommand::CommitText("你好".into()));
        assert_indicator(&s, false, true, line!());
    }

    #[test]
    fn badge_sends_viewport_commands_only_on_change() {
        // Show 后首帧必须下发（新建 viewport 不能停在系统默认位置），此后
        // 状态未变时不得再发：send_viewport_cmd 会对当前 viewport 无条件
        // 请求重绘，每帧重发会让徽标窗口持续自我触发重绘而永不空闲
        let mut s = AppState::default();
        apply_ui_command(
            &mut s,
            UiCommand::ShowModeIndicator {
                context: Some(ctx_at(100, 200)),
                invalidate_caret: true,
            },
        );
        assert_eq!(s.indicator.sent, IndicatorSent::Unknown, "line {}", line!());

        let first = badge_commands_per_pass(&mut s.indicator, &Vec::new());
        assert!(
            matches!(s.indicator.sent, IndicatorSent::Shown(..)),
            "line {}",
            line!()
        );
        assert_eq!(visibilities(&first), vec![true], "line {}", line!());
        let first_pos = positions(&first);
        assert_eq!(first_pos.len(), 1, "line {}", line!());

        // 状态未变：不得再下发任何位置/显隐命令
        let idle = badge_commands_per_pass(&mut s.indicator, &Vec::new());
        assert!(positions(&idle).is_empty(), "line {}", line!());
        assert!(visibilities(&idle).is_empty(), "line {}", line!());

        // 光标下移 → 位置变化 → 重新下发一次
        apply_ui_command(
            &mut s,
            UiCommand::UpdatePosition {
                context: ctx_at(300, 400),
            },
        );
        let moved = badge_commands_per_pass(&mut s.indicator, &Vec::new());
        let moved_pos = positions(&moved);
        assert_eq!(moved_pos.len(), 1, "line {}", line!());
        assert!(moved_pos[0].y > first_pos[0].y, "line {}", line!());
        assert_eq!(visibilities(&moved), vec![true], "line {}", line!());

        let idle = badge_commands_per_pass(&mut s.indicator, &Vec::new());
        assert!(positions(&idle).is_empty(), "line {}", line!());
    }

    #[test]
    fn badge_reshows_after_hide_even_at_same_position() {
        // 隐藏后 viewport 会被关闭（下一次 Show 是新建窗口），因此再次 Show
        // 必须重新下发一次——即使光标位置与上次完全相同，去重状态也不能
        // 让它停在系统默认位置
        let mut s = AppState::default();
        apply_ui_command(
            &mut s,
            UiCommand::ShowModeIndicator {
                context: Some(ctx_at(100, 200)),
                invalidate_caret: true,
            },
        );
        assert!(!positions(&badge_commands_per_pass(&mut s.indicator, &Vec::new())).is_empty());

        apply_ui_command(
            &mut s,
            UiCommand::HideModeIndicator {
                invalidate_caret: true,
            },
        );
        // Hide 不改写下发状态（重发由 Show 触发）
        assert!(
            matches!(s.indicator.sent, IndicatorSent::Shown(..)),
            "line {}",
            line!()
        );

        // 与上次完全相同的位置：仍必须重发
        apply_ui_command(
            &mut s,
            UiCommand::ShowModeIndicator {
                context: Some(ctx_at(100, 200)),
                invalidate_caret: true,
            },
        );
        assert_eq!(s.indicator.sent, IndicatorSent::Unknown, "line {}", line!());
        let again = badge_commands_per_pass(&mut s.indicator, &Vec::new());
        assert_eq!(positions(&again).len(), 1, "line {}", line!());
        assert_eq!(visibilities(&again), vec![true], "line {}", line!());
    }

    #[test]
    fn badge_parks_offscreen_without_caret_and_only_once() {
        // 无光标位置（Chromium 等无系统 caret 的应用）：停放到屏幕外，
        // 且同样只下发一次
        let mut s = AppState::default();
        apply_ui_command(
            &mut s,
            UiCommand::ShowModeIndicator {
                context: None,
                invalidate_caret: true,
            },
        );

        let parked = badge_commands_per_pass(&mut s.indicator, &Vec::new());
        assert_eq!(s.indicator.sent, IndicatorSent::Parked, "line {}", line!());
        assert_eq!(visibilities(&parked), vec![false], "line {}", line!());
        let parked_pos = positions(&parked);
        assert_eq!(parked_pos.len(), 1, "line {}", line!());
        assert!(parked_pos[0].x < -1000.0, "line {}", line!());

        let idle = badge_commands_per_pass(&mut s.indicator, &Vec::new());
        assert!(positions(&idle).is_empty(), "line {}", line!());
        assert!(visibilities(&idle).is_empty(), "line {}", line!());
    }

    #[test]
    fn mode_switch_without_caret_keeps_last_known_position() {
        // 回归：Chromium 等无系统 caret 的应用只有 TF_LC_CHANGE → UpdatePosition
        // 这一条位置来源。此后切换中英的 Show 取不到实时位置（context=None）时，
        // 同一焦点会话内必须沿用已记录的位置；若一律作废，徽标会屏外停放且再
        // 无唤醒机会——模式切换不产生文本变化，等不到后续位置上报，用户切换
        // 中英时看不到任何反馈（本次缺陷）
        let mut s = AppState::default();
        apply_ui_command(
            &mut s,
            UiCommand::UpdatePosition {
                context: ctx_at(100, 200),
            },
        );
        apply_ui_command(
            &mut s,
            UiCommand::ShowModeIndicator {
                context: None,
                invalidate_caret: false,
            },
        );
        assert_indicator(&s, true, true, line!());
        let cmds = badge_commands_per_pass(&mut s.indicator, &Vec::new());
        assert_eq!(visibilities(&cmds), vec![true], "line {}", line!());
        let pos = positions(&cmds);
        assert_eq!(pos.len(), 1, "line {}", line!());
        // 沿用 UpdatePosition 报来的位置（ppp=1 且无显示器缓存时不夹取）
        assert!(pos[0].x > 100.0 && pos[0].y >= 200.0, "line {}", line!());
    }

    #[test]
    fn focus_change_without_caret_parks_offscreen() {
        // 另一侧语义：焦点会话已变更（新窗口）且未取到位置时，旧窗口的坐标
        // 必须作废并屏外停放，避免在新窗口的旧位置闪现
        let mut s = AppState::default();
        apply_ui_command(
            &mut s,
            UiCommand::UpdatePosition {
                context: ctx_at(100, 200),
            },
        );
        apply_ui_command(
            &mut s,
            UiCommand::ShowModeIndicator {
                context: None,
                invalidate_caret: true,
            },
        );
        assert_indicator(&s, false, false, line!());
        let cmds = badge_commands_per_pass(&mut s.indicator, &Vec::new());
        assert_eq!(visibilities(&cmds), vec![false], "line {}", line!());
    }
}
