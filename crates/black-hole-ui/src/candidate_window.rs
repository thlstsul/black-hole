// ---------------------------------------------------------------------------
// 跨平台候选窗口: eframe + egui
// ---------------------------------------------------------------------------

use super::*;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

const SCROLL_AREA_MAX_HEIGHT: f32 = 200.0;

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
        }
    }
}

struct ThemeColors {
    text_color: Color32,
    bg_color: Color32,
    highlight_color: Color32,
    label_color: Color32,
}

fn theme_colors(theme: Theme) -> ThemeColors {
    match theme {
        Theme::Dark | Theme::System => ThemeColors {
            text_color: Color32::from_rgb(240, 240, 240),
            bg_color: Color32::from_rgb(40, 40, 40),
            highlight_color: Color32::from_rgb(0, 120, 215),
            label_color: Color32::from_rgb(160, 160, 160),
        },
        _ => ThemeColors {
            text_color: Color32::from_rgb(26, 26, 26),
            bg_color: Color32::from_rgb(245, 245, 245),
            highlight_color: Color32::from_rgb(0, 120, 215),
            label_color: Color32::from_rgb(120, 120, 120),
        },
    }
}

pub struct ImeUiApp {
    state: Arc<Mutex<AppState>>,
    win_style_applied: bool,
}

impl ImeUiApp {
    pub fn new(_cc: &CreationContext<'_>, state: Arc<Mutex<AppState>>) -> Self {
        Self {
            state,
            win_style_applied: false,
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

        let state = self.state.lock().unwrap();

        if state.should_exit {
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }

        // 不可见或无候选时隐藏窗口
        if !(state.visible && !state.candidates.is_empty()) {
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

        let colors = theme_colors(state.theme);
        ctx.set_visuals(theme_visuals(state.theme));

        EguiFrame::new()
            .fill(colors.bg_color)
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
fn get_screen_rect(frame: &Frame) -> Option<Rect> {
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
        let content_height = if rows.len() <= 1 {
            rows.len() as f32 * candidate_row_height
        } else {
            rows.len() as f32 * candidate_row_height + (rows.len() - 1) as f32 * inter_row_spacing
        };
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
    let msg = match e.downcast_ref::<&str>() {
        Some(s) => s.to_string(),
        None => match e.downcast_ref::<String>() {
            Some(s) => s.clone(),
            None => "unknown panic".to_string(),
        },
    };
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
            let should_repaint = apply_ui_command(&mut state.lock().unwrap(), cmd);
            if should_repaint {
                ctx.request_repaint();
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
    colors: &ThemeColors,
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
    colors: &ThemeColors,
) {
    let (fill, text_color) = if is_first {
        (colors.highlight_color, Color32::WHITE)
    } else {
        (colors.bg_color, colors.text_color)
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
fn render_completion(ui: &mut Ui, state: &AppState, selected_font: f32, colors: &ThemeColors) {
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
                .color(colors.text_color.gamma_multiply(0.45)),
        )
        .selectable(false)
        .truncate(),
    );
    ui.add(
        Label::new(
            RichText::new(&state.commit_sentence)
                .size(11.0)
                .color(colors.label_color),
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
    colors: &ThemeColors,
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
fn item_colors(is_selected: bool, colors: &ThemeColors) -> (Color32, Color32, Option<Color32>) {
    if is_selected {
        (Color32::WHITE, Color32::WHITE, Some(colors.highlight_color))
    } else {
        (colors.text_color, colors.label_color, None)
    }
}

fn render_expanded_rows(
    ui: &mut Ui,
    rows: &[Vec<usize>],
    win: &[Candidate],
    win_selected: usize,
    font_size: u32,
    colors: &ThemeColors,
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
    colors: &ThemeColors,
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
    colors: &ThemeColors,
) -> eframe::egui::InnerResponse<()> {
    let (tc, lc, bg) = item_colors(is_selected, colors);
    candidate_item_frame(ui, bg.unwrap_or(colors.bg_color), |ui| {
        render_candidate_item(ui, col + 1, candidate, tc, lc, show_label, font_size);
    })
}

fn render_collapsed_row(
    ui: &mut Ui,
    row: &[usize],
    win: &[Candidate],
    win_selected: usize,
    colors: &ThemeColors,
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
) -> eframe::egui::InnerResponse<()> {
    EguiFrame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::same(4))
        .show(ui, |ui| {
            ui.horizontal(|ui| content(ui));
        })
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
        }
        UiCommand::HideCandidates | UiCommand::CommitText(_) => {
            s.visible = false;
            clear_completion(s);
            should_repaint = false;
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
