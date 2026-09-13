use black_hole_shared::candidate_layout::{
    CANDIDATE_WINDOW_WIDTH, EXPANDED_AVAILABLE_WIDTH, ITEM_SPACING,
    layout_candidates_into_rows_excluding,
};
use black_hole_shared::{Candidate, CandidateWindowSettings, Theme, UiCommand};
pub use candidate_window::run_candidate_window;
use eframe::egui::Frame as EguiFrame;
use eframe::egui::{
    Align, Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, Label, Layout,
    Margin, Pos2, Rect, RichText, ScrollArea, Ui, Vec2, ViewportBuilder, ViewportCommand,
};
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
use winit::window::Window;

mod candidate_window;
pub mod settings_manager;
pub mod settings_panel;
pub mod theme;

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

/// 候选窗/设置面板共用的 wgpu 配置。
///
/// Windows 上排除 Vulkan 后端：Intel 核显（如 UHD 630）的 igvk64.dll 驱动在
/// wgpu 初始化/渲染时存在访问冲突崩溃（0xc0000005，事件日志 faulting module
/// 即 igvk64.dll），候选窗随 daemon 常驻、每次按键都可能触发渲染，必须绕开；
/// DX12（含 WARP 兜底）在 Windows 上始终可用。非 Windows 保持原有后端集合。
fn wgpu_configuration() -> WgpuConfiguration {
    #[cfg(target_os = "windows")]
    let backends = Backends::PRIMARY - Backends::VULKAN;
    #[cfg(not(target_os = "windows"))]
    let backends = Backends::PRIMARY;

    WgpuConfiguration {
        wgpu_setup: WgpuSetup::CreateNew(WgpuSetupCreateNew {
            instance_descriptor: InstanceDescriptor {
                flags: InstanceFlags::empty(),
                backends,
                memory_budget_thresholds: MemoryBudgetThresholds::default(),
                backend_options: BackendOptions::default(),
                display: None, // 关键：禁用所有 debug/validation
            },
            device_descriptor: Arc::new(|_adapter| DeviceDescriptor {
                memory_hints: MemoryHints::Manual {
                    suballocated_device_memory_block_size: 4 * 1024 * 1024..16 * 1024 * 1024,
                },
                ..Default::default()
            }),
            display_handle: None,
            power_preference: PowerPreference::None,
            native_adapter_selector: None,
        }),
        ..Default::default()
    }
}
