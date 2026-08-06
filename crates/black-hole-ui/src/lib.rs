use black_hole_shared::candidate_layout::{
    CANDIDATE_WINDOW_WIDTH, EXPANDED_AVAILABLE_WIDTH, ITEM_SPACING,
    layout_candidates_into_rows_excluding,
};
use black_hole_shared::{Candidate, InputContext, Theme, UiCommand};
pub use candidate_window_cross::run_candidate_window;
use eframe::egui_wgpu::{WgpuSetup, WgpuSetupCreateNew};
use eframe::run_native;
use eframe::wgpu::{
    BackendOptions, Backends, InstanceDescriptor, InstanceFlags, MemoryBudgetThresholds,
    MemoryHints, PowerPreference, wgt::DeviceDescriptor,
};
use eframe::{
    App, CreationContext, EventLoopBuilder, EventLoopBuilderHook, Frame, NativeOptions,
    WgpuConfiguration,
};
use egui::Frame as EguiFrame;
use egui::{
    Align, Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, Label, Layout,
    Margin, Pos2, Rect, RichText, ScrollArea, Ui, Vec2, ViewportBuilder, ViewportCommand, Visuals,
};
#[cfg(target_os = "windows")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
pub use settings_manager::SettingsManager;
pub use settings_panel::run_settings_panel;
use std::ffi::c_void;
use std::fs;
use std::mem;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread;
use tracing::{error, info};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{HWND, RECT};
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
#[cfg(target_os = "windows")]
use winit::platform::windows::EventLoopBuilderExtWindows;

mod candidate_window_cross;
pub mod settings_manager;
pub mod settings_panel;

pub fn theme_visuals(theme: Theme) -> Visuals {
    match theme {
        Theme::Dark | Theme::System => Visuals::dark(),
        _ => Visuals::light(),
    }
}

/// 配置 egui 中文字体（加载系统字体作为 fallback）
pub fn configure_fonts(ctx: &Context) {
    let mut fonts = FontDefinitions::default();

    if let Some(font_data) = load_system_font_data() {
        let name = "system_font".to_owned();
        fonts.font_data.insert(name.clone(), Arc::new(font_data));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .push(name.clone());
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .push(name);
        ctx.set_fonts(fonts);
    }
}

fn load_system_font_data() -> Option<FontData> {
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
        if let Ok(data) = fs::read(path) {
            return Some(FontData::from_owned(data));
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

/// 显示候选窗口原型（测试用）
pub fn show_candidate_prototype(code: &str, candidates: &[Candidate], selected: usize) {
    info!(code, "show candidate window");
    for (i, c) in candidates.iter().enumerate() {
        let marker = if i == selected { ">" } else { " " };
        info!(
            marker,
            index = i + 1,
            text = c.text,
            comment = c.comment.as_deref().unwrap_or(""),
            "candidate"
        );
    }
}
