use black_hole_shared::candidate_layout::{
    CANDIDATE_WINDOW_WIDTH, EXPANDED_AVAILABLE_WIDTH, ITEM_SPACING,
    layout_candidates_into_rows_excluding,
};
use black_hole_shared::{Candidate, InputContext, Theme, UiCommand};

pub mod settings_manager;

pub use settings_manager::SettingsManager;

pub mod settings_panel;
pub use settings_panel::run_settings_panel;

pub fn theme_visuals(theme: Theme) -> egui::Visuals {
    match theme {
        Theme::Dark | Theme::System => egui::Visuals::dark(),
        _ => egui::Visuals::light(),
    }
}

/// 配置 egui 中文字体（加载系统字体作为 fallback）
pub fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    if let Some(font_data) = load_system_font_data() {
        let name = "system_font".to_owned();
        fonts
            .font_data
            .insert(name.clone(), std::sync::Arc::new(font_data));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push(name.clone());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push(name);
        ctx.set_fonts(fonts);
    }
}

fn load_system_font_data() -> Option<egui::FontData> {
    #[cfg(target_os = "windows")]
    let paths = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyhbd.ttc",
        r"C:\Windows\Fonts\simsun.ttc",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\segoeui.ttf",
    ];

    #[cfg(target_os = "linux")]
    let paths = [
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    ];

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let paths: [&str; 0] = [];

    for path in &paths {
        if let Ok(data) = std::fs::read(path) {
            return Some(egui::FontData::from_owned(data));
        }
    }
    None
}

/// 候选窗口接口
pub trait CandidateWindow {
    fn show(&mut self, code: &str, candidates: &[Candidate], selected: usize, ctx: &InputContext);
    fn hide(&mut self);
    fn update_selection(&mut self, selected: usize);
}

/// 设置面板接口
pub trait SettingsPanel {
    fn show(&mut self);
    fn hide(&mut self);
}

// ---------------------------------------------------------------------------
// 跨平台候选窗口: eframe + egui
// ---------------------------------------------------------------------------
mod candidate_window_cross {
    use super::*;
    use eframe::WgpuConfiguration;
    use eframe::egui_wgpu::{WgpuSetup, WgpuSetupCreateNew};
    use eframe::wgpu::wgt::DeviceDescriptor;
    use eframe::wgpu::{
        BackendOptions, Backends, InstanceDescriptor, InstanceFlags, MemoryBudgetThresholds,
        MemoryHints, PowerPreference,
    };
    use egui::{Color32, Margin, Pos2, Vec2};
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
        pub fn new(_cc: &eframe::CreationContext<'_>, state: Arc<Mutex<AppState>>) -> Self {
            Self {
                state,
                win_style_applied: false,
            }
        }
    }

    impl eframe::App for ImeUiApp {
        fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
            let ctx = ui.ctx().clone();
            #[cfg(target_os = "windows")]
            if !self.win_style_applied {
                apply_windows_style(frame);
                self.win_style_applied = true;
            }

            let state = self.state.lock().unwrap();

            if state.should_exit {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }

            if state.visible && !state.candidates.is_empty() {
                let (desired_width, desired_height) = estimate_window_size(&state);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(
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

                let mut pos_x = caret_x;
                // 默认显示在光标下方
                let mut pos_y = caret_y + gap;

                // 防止候选窗越界，同时避免挡住输入位置
                if let Some(screen) = get_screen_rect(frame) {
                    let logical_w = desired_width;
                    let logical_h = desired_height;

                    // y 方向：优先下方，下方放不下则放上方
                    if pos_y + logical_h > screen.max.y {
                        pos_y = caret_y - caret_h - logical_h - gap;
                        if pos_y < screen.min.y {
                            // 上下都放不下，选择空间更大的一侧
                            let space_below = screen.max.y - caret_y;
                            let space_above = caret_y - caret_h - screen.min.y;
                            if space_below >= space_above {
                                pos_y = screen.max.y - logical_h;
                            } else {
                                pos_y = screen.min.y;
                            }
                        }
                    }

                    // x 方向防越界
                    if pos_x + logical_w > screen.max.x {
                        pos_x = screen.max.x - logical_w;
                    }
                    pos_x = pos_x.max(screen.min.x);
                }

                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(
                    pos_x, pos_y,
                )));
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));

                let ThemeColors {
                    text_color,
                    bg_color,
                    highlight_color,
                    label_color,
                } = theme_colors(state.theme);
                ctx.set_visuals(crate::theme_visuals(state.theme));

                egui::Frame::new()
                    .fill(bg_color)
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_min_size(Vec2::new(desired_width, desired_height));
                        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                            ui.spacing_mut().item_spacing.y = 6.0;

                            // 第一行：选中候选
                            if !state.candidates.is_empty()
                                && state.selected_index < state.candidates.len()
                            {
                                let selected = &state.candidates[state.selected_index];
                                let text = selected.text.clone();

                                egui::Frame::new()
                                    .fill(highlight_color)
                                    .corner_radius(egui::CornerRadius::same(6))
                                    .inner_margin(Margin::same(8))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(&text)
                                                        .size(24.0)
                                                        .color(Color32::WHITE),
                                                )
                                                .selectable(false),
                                            );
                                        });
                                    });

                                ui.add_space(6.0);
                            }

                            if state.expanded {
                                // 展开状态：可滚动的多行不规则网格（排除首选词，第一行已单独展示）
                                if state.candidates.len() > 1 {
                                    let rows = layout_candidates_into_rows_excluding(
                                        &state.candidates,
                                        EXPANDED_AVAILABLE_WIDTH,
                                        ITEM_SPACING,
                                        Some(0),
                                    );
                                    egui::ScrollArea::vertical()
                                        .max_height(SCROLL_AREA_MAX_HEIGHT)
                                        .show(ui, |ui| {
                                            for row in rows {
                                                let is_selected_row =
                                                    row.contains(&state.selected_index);
                                                ui.horizontal(|ui| {
                                                    ui.spacing_mut().item_spacing.x = ITEM_SPACING;
                                                    for (col, i) in row.iter().enumerate() {
                                                        let is_selected =
                                                            *i == state.selected_index;
                                                        let (lc, tc, bg) = if is_selected {
                                                            (
                                                                Color32::WHITE,
                                                                Color32::WHITE,
                                                                Some(highlight_color),
                                                            )
                                                        } else {
                                                            (label_color, text_color, None)
                                                        };
                                                        let response = egui::Frame::new()
                                                            .fill(bg.unwrap_or(bg_color))
                                                            .corner_radius(
                                                                egui::CornerRadius::same(4),
                                                            )
                                                            .inner_margin(Margin::same(4))
                                                            .show(ui, |ui| {
                                                                ui.horizontal(|ui| {
                                                                    render_candidate_item(
                                                                        ui,
                                                                        col + 1,
                                                                        &state.candidates[*i],
                                                                        tc,
                                                                        lc,
                                                                        is_selected_row,
                                                                    );
                                                                });
                                                            });
                                                        if is_selected {
                                                            // 仅在选中项滚出可视区时做
                                                            // 最小滚动，避免上下导航时
                                                            // 整个表格反复居中跳动
                                                            response.response.scroll_to_me(None);
                                                        }
                                                    }
                                                });
                                            }
                                        });
                                }
                            } else {
                                // 折叠状态：第二行显示网格布局的第一行（排除首选词）
                                if state.candidates.len() > 1 {
                                    let rows = layout_candidates_into_rows_excluding(
                                        &state.candidates,
                                        EXPANDED_AVAILABLE_WIDTH,
                                        ITEM_SPACING,
                                        Some(0),
                                    );
                                    if let Some(first_row) = rows.first() {
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing.x = ITEM_SPACING;
                                            for (col, i) in first_row.iter().enumerate() {
                                                let is_selected = *i == state.selected_index;
                                                let (tc, lc) = if is_selected {
                                                    (Color32::WHITE, Color32::WHITE)
                                                } else {
                                                    (text_color, label_color)
                                                };
                                                let bg = if is_selected {
                                                    Some(highlight_color)
                                                } else {
                                                    None
                                                };
                                                egui::Frame::new()
                                                    .fill(bg.unwrap_or(bg_color))
                                                    .corner_radius(egui::CornerRadius::same(4))
                                                    .inner_margin(Margin::same(4))
                                                    .show(ui, |ui| {
                                                        ui.horizontal(|ui| {
                                                            render_candidate_item(
                                                                ui,
                                                                col + 1,
                                                                &state.candidates[*i],
                                                                tc,
                                                                lc,
                                                                true,
                                                            );
                                                        });
                                                    });
                                            }
                                        });
                                    }
                                }
                            }
                        });
                    });
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn apply_windows_style(frame: &mut eframe::Frame) {
        let Some(window) = frame.winit_window() else {
            return;
        };
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(h) = handle.as_raw() else {
            return;
        };

        let hwnd = windows::Win32::Foundation::HWND(h.hwnd.get() as *mut std::ffi::c_void);
        unsafe {
            // 设置 DWM 圆角窗口
            let corner_pref: u32 = 2; // DWMWCP_ROUND
            let _ = windows::Win32::Graphics::Dwm::DwmSetWindowAttribute(
                hwnd,
                windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(33),
                &corner_pref as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            );
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn apply_windows_style(_frame: &mut eframe::Frame) {}

    /// 获取当前显示器工作区逻辑坐标矩形（排除任务栏），用于边界防越
    fn get_screen_rect(frame: &eframe::Frame) -> Option<egui::Rect> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let window = frame.winit_window()?;
            let monitor = window
                .current_monitor()
                .or_else(|| window.primary_monitor())?;
            let scale = monitor.scale_factor() as f32;

            #[cfg(target_os = "windows")]
            {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                let handle = window.window_handle().ok()?;
                let RawWindowHandle::Win32(h) = handle.as_raw() else {
                    return None;
                };
                let hwnd = windows::Win32::Foundation::HWND(h.hwnd.get() as *mut std::ffi::c_void);
                unsafe {
                    use windows::Win32::Graphics::Gdi::{
                        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
                    };
                    let hmonitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                    let mut info = MONITORINFO {
                        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                        rcMonitor: windows::Win32::Foundation::RECT::default(),
                        rcWork: windows::Win32::Foundation::RECT::default(),
                        dwFlags: 0,
                    };
                    if GetMonitorInfoW(hmonitor, &mut info).as_bool() {
                        return Some(egui::Rect::from_min_max(
                            egui::Pos2::new(
                                info.rcWork.left as f32 / scale,
                                info.rcWork.top as f32 / scale,
                            ),
                            egui::Pos2::new(
                                info.rcWork.right as f32 / scale,
                                info.rcWork.bottom as f32 / scale,
                            ),
                        ));
                    }
                }
            }

            // 非 Windows 或获取工作区失败时回退到完整监视器尺寸
            let size = monitor.size();
            let pos = monitor.position();
            Some(egui::Rect::from_min_size(
                egui::Pos2::new(pos.x as f32 / scale, pos.y as f32 / scale),
                egui::Vec2::new(size.width as f32 / scale, size.height as f32 / scale),
            ))
        }
        #[cfg(target_arch = "wasm32")]
        {
            None
        }
    }

    fn render_candidate_item(
        ui: &mut egui::Ui,
        display_number: usize,
        candidate: &Candidate,
        text_color: Color32,
        label_color: Color32,
        show_label: bool,
    ) {
        // 无论是否显示序号，均使用固定尺寸，确保布局完全一致
        let label_text = if show_label {
            format!("{}.", display_number)
        } else {
            String::new()
        };
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_sized(
            [20.0, 14.0],
            egui::Label::new(
                egui::RichText::new(label_text)
                    .size(14.0)
                    .color(label_color),
            )
            .selectable(false),
        );
        ui.add(
            egui::Label::new(
                egui::RichText::new(&candidate.text)
                    .size(16.0)
                    .color(text_color),
            )
            .selectable(false),
        );
    }

    fn estimate_window_size(state: &AppState) -> (f32, f32) {
        let frame_padding = 20.0; // 外层 Frame inner_margin 10 * 2
        let row_spacing = 12.0; // 行间距
        let candidate_row_height = 30.0; // 16px 字体 + Frame inner_margin 4+4 + 余量

        let first_row_height = if state.candidates.is_empty() {
            0.0
        } else {
            42.0 // 24px 字体 + inner_margin 6+6 + 少量余量
        };

        let total_width = CANDIDATE_WINDOW_WIDTH;

        let total_height = if state.candidates.is_empty() {
            0.0
        } else if state.expanded {
            // 展开状态：根据实际行数计算内容高度，避免固定留白（排除首选词）
            let rows = layout_candidates_into_rows_excluding(
                &state.candidates,
                EXPANDED_AVAILABLE_WIDTH,
                ITEM_SPACING,
                Some(0),
            );
            // ScrollArea 内部每行之间有 item_spacing.y = 6.0，需计入
            let inter_row_spacing = 6.0f32;
            let content_height = if rows.len() <= 1 {
                rows.len() as f32 * candidate_row_height
            } else {
                rows.len() as f32 * candidate_row_height
                    + (rows.len() - 1) as f32 * inter_row_spacing
            };
            let scroll_height = content_height.min(SCROLL_AREA_MAX_HEIGHT);
            (first_row_height + scroll_height + row_spacing + frame_padding).max(64.0)
        } else {
            // 折叠状态：第一行 + 第二行（如果有）+ padding
            let second_row_height = if state.candidates.len() <= 1 {
                0.0
            } else {
                32.0 // 16px 字体 + 上下余量
            };
            (first_row_height + second_row_height + row_spacing + frame_padding).max(64.0)
        };

        (total_width, total_height.min(320.0))
    }

    /// 启动候选窗口事件循环（阻塞当前线程）
    pub fn run_candidate_window(ui_rx: Receiver<UiCommand>, initial_theme: Theme) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_candidate_window_inner(ui_rx, initial_theme);
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
        tracing::error!(msg, "run_candidate_window panicked");
    }

    fn run_candidate_window_inner(ui_rx: Receiver<UiCommand>, initial_theme: Theme) {
        tracing::info!("run_candidate_window: starting");

        let state = Arc::new(Mutex::new(AppState {
            theme: initial_theme,
            ..AppState::default()
        }));
        let state_for_thread = Arc::clone(&state);

        #[cfg(target_os = "windows")]
        let event_loop_builder = Some(Box::new(|builder: &mut eframe::EventLoopBuilder<_>| {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            builder.with_any_thread(true);
        }) as eframe::EventLoopBuilderHook);

        #[cfg(not(target_os = "windows"))]
        let event_loop_builder: Option<eframe::EventLoopBuilderHook> = None;

        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_decorations(false)
                .with_transparent(true)
                .with_always_on_top()
                .with_active(false)
                .with_resizable(false)
                .with_visible(false)
                .with_taskbar(false)
                .with_inner_size([320.0, 40.0]),
            event_loop_builder,
            wgpu_options: WgpuConfiguration {
                wgpu_setup: WgpuSetup::CreateNew(WgpuSetupCreateNew {
                    instance_descriptor: InstanceDescriptor {
                        flags: InstanceFlags::empty(),
                        backends: Backends::PRIMARY,
                        memory_budget_thresholds: MemoryBudgetThresholds::default(),
                        backend_options: BackendOptions::default(),
                        display: None, // 关键：禁用所有 debug/validation
                    },
                    device_descriptor: Arc::new(|_adapter| DeviceDescriptor {
                        memory_hints: MemoryHints::Manual {
                            suballocated_device_memory_block_size: 4 * 1024 * 1024
                                ..16 * 1024 * 1024,
                        },
                        ..Default::default()
                    }),
                    display_handle: None,
                    power_preference: PowerPreference::None,
                    native_adapter_selector: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let result = eframe::run_native(
            "Black-Hole Candidate",
            options,
            Box::new(|cc| {
                crate::configure_fonts(&cc.egui_ctx);
                let ctx = cc.egui_ctx.clone();

                std::thread::spawn(move || {
                    tracing::info!("channel thread started");
                    while let Ok(cmd) = ui_rx.recv() {
                        tracing::info!(
                            cmd = ?std::mem::discriminant(&cmd),
                            "channel thread recv"
                        );
                        let mut s = state_for_thread.lock().unwrap();
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
                                s.candidates = candidates;
                                s.selected_index = selected_index;
                                s.expanded = expanded;
                                should_repaint = true;
                            }
                            UiCommand::UpdatePosition { context } => {
                                if s.visible {
                                    s.caret_x = context.caret_x;
                                    s.caret_y = context.caret_y;
                                    s.caret_h = context.caret_h;
                                }
                            }
                            UiCommand::HideCandidates => {
                                s.visible = false;
                                should_repaint = false;
                            }
                            UiCommand::CommitText(_) => {
                                s.visible = false;
                                should_repaint = false;
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
                        drop(s);
                        if should_repaint {
                            ctx.request_repaint();
                        }
                    }
                    tracing::info!("channel thread exited");
                });

                Ok(Box::new(ImeUiApp::new(cc, state)))
            }),
        );

        if let Err(e) = result {
            tracing::error!(error = ?e, "eframe run_native error");
        }
    }
}

pub use candidate_window_cross::run_candidate_window;

/// 显示候选窗口原型（测试用）
pub fn show_candidate_prototype(code: &str, candidates: &[Candidate], selected: usize) {
    tracing::info!(code, "show candidate window");
    for (i, c) in candidates.iter().enumerate() {
        let marker = if i == selected { ">" } else { " " };
        tracing::info!(
            marker,
            index = i + 1,
            text = c.text,
            comment = c.comment.as_deref().unwrap_or(""),
            "candidate"
        );
    }
}
