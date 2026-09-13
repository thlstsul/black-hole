use black_hole_platform::auto_start::set_auto_start;
use black_hole_shared::{KeyBindings, SchemeId, Settings, Theme};

use crate::configure_fonts;
use crate::settings_manager::SettingsManager;
use crate::theme;
use crate::wgpu_configuration;
use eframe::egui::emath::Numeric;
use eframe::egui::{
    Context, DragValue, Margin, RichText, ScrollArea, Ui, ViewportBuilder, ViewportCommand, Visuals,
};
use eframe::{App, EventLoopBuilder, EventLoopBuilderHook, Frame, NativeOptions, run_native};
#[cfg(target_os = "windows")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(target_os = "windows")]
use std::ffi::c_void;
use tracing::{error, info};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HWND;
#[cfg(target_os = "windows")]
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
};
#[cfg(target_os = "windows")]
use winit::platform::windows::EventLoopBuilderExtWindows;

pub struct SettingsPanelApp {
    settings_mgr: SettingsManager,
    /// 已应用全局样式的主题（含 `System` 解析后的明暗）
    applied_theme: Option<(Theme, bool)>,
    /// 反馈文字与是否成功（成功用调色板的 success，失败用 error）
    feedback: Option<(String, bool)>,
    feedback_timer: f64,
    /// 打开面板后强制窗口前台聚焦（Windows 前台锁需逐帧重试）
    focus_retries: u32,
}

impl SettingsPanelApp {
    pub fn new(settings_mgr: SettingsManager) -> Self {
        Self {
            settings_mgr,
            applied_theme: None,
            feedback: None,
            feedback_timer: 0.0,
            focus_retries: 0,
        }
    }

    pub fn settings(&self) -> &Settings {
        self.settings_mgr.settings()
    }

    fn show_feedback(&mut self, msg: impl Into<String>, ok: bool) {
        let icon = if ok { "✓" } else { "✗" };
        self.feedback = Some((format!("{} {}", icon, msg.into()), ok));
        self.feedback_timer = 3.0; // 显示 3 秒
    }

    /// 持久化当前设置到磁盘；失败时给出反馈（成功时静默，避免频繁打扰）
    fn save_settings(&mut self) {
        if !self.settings_mgr.save() {
            self.show_feedback("保存设置失败(请查看日志)", false);
        }
    }

    /// 按键绑定校验：空字符串回退为默认值（返回是否有回退），
    /// 避免半输入/清空状态被持久化后永久破坏该键。
    fn normalize_key_bindings(&mut self) -> bool {
        let defaults = KeyBindings::default();
        let b = &mut self.settings_mgr.settings_mut().key_bindings;
        let mut restored = false;
        restored |= normalize_binding(&mut b.next_candidate, &defaults.next_candidate);
        restored |= normalize_binding(&mut b.prev_candidate, &defaults.prev_candidate);
        restored |= normalize_binding(&mut b.commit, &defaults.commit);
        restored |= normalize_binding(&mut b.cancel, &defaults.cancel);
        restored |= normalize_binding(&mut b.switch_scheme, &defaults.switch_scheme);
        restored |= normalize_binding(&mut b.commit_sentence, &defaults.commit_sentence);
        restored
    }

    /// 绘制所有设置分组；返回 (是否有修改，按键绑定是否需要提交)
    fn settings_groups(&mut self, ui: &mut Ui) -> (bool, bool) {
        let settings = self.settings_mgr.settings_mut();

        let mut changed = theme_group(ui, settings);
        ui.add_space(8.0);
        changed |= scheme_group(ui, settings);
        ui.add_space(8.0);
        changed |= candidate_window_group(ui, &mut settings.candidate_window);
        ui.add_space(8.0);
        let bindings_commit = key_bindings_group(ui, &mut settings.key_bindings);
        ui.add_space(8.0);
        changed |= auto_switch_group(ui, settings);
        ui.add_space(8.0);
        changed |= llm_group(ui, &mut settings.llm_completion);
        (changed, bindings_commit)
    }

    /// 开机自启动：切换时同步系统侧并立即落盘
    fn auto_start_group(&mut self, ui: &mut Ui) {
        ui.group(|ui| {
            ui.label("开机自启动");
            let mut auto_start = self.settings_mgr.settings().auto_start;
            let resp = ui.checkbox(&mut auto_start, "登录时自动启动黑洞输入法");
            if resp.changed() {
                match set_auto_start(auto_start) {
                    Ok(()) => {
                        self.settings_mgr.settings_mut().auto_start = auto_start;
                        // 与其它设置项一致：实时生效并立即落盘
                        self.save_settings();
                        let msg = if auto_start {
                            "已开启开机自启动"
                        } else {
                            "已关闭开机自启动"
                        };
                        self.show_feedback(msg, true);
                    }
                    Err(e) => {
                        self.show_feedback(format!("更新开机自启动失败: {}", e), false);
                    }
                }
            }
        });
    }

    /// 恢复默认设置；恢复前若自启动为开启，需同步关闭系统侧
    fn restore_defaults(&mut self) {
        let was_auto_start = self.settings_mgr.settings().auto_start;
        self.settings_mgr.reset_to_default();
        // 立即落盘并生效
        self.save_settings();
        if was_auto_start {
            match set_auto_start(false) {
                Ok(()) => self.show_feedback("已恢复默认设置", true),
                Err(e) => {
                    self.show_feedback(
                        format!("已恢复默认设置，但关闭开机自启动失败: {}", e),
                        false,
                    );
                }
            }
        } else {
            self.show_feedback("已恢复默认设置", true);
        }
    }

    /// 关闭窗口时自动保存；先回退空的按键绑定，避免半输入值被持久化
    fn handle_close(&mut self, ctx: &Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        self.normalize_key_bindings();
        if self.settings_mgr.save() {
            info!("Settings saved on window close");
        } else {
            error!("Failed to save settings on window close");
        }
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }
}

impl App for SettingsPanelApp {
    fn ui(&mut self, ui: &mut Ui, frame: &mut Frame) {
        let ctx = ui.ctx().clone();

        // 强制前台聚焦：daemon 为后台进程，拉起的设置面板默认拿不到焦点
        #[cfg(target_os = "windows")]
        if self.focus_retries < 30 {
            self.focus_retries += 1;
            if !force_foreground(frame) {
                ctx.request_repaint(); // 窗口尚未就绪/聚焦，下一帧继续
            }
        }

        let current_theme = self.settings_mgr.settings().theme;
        theme::sync_visuals(&ctx, current_theme, &mut self.applied_theme);

        // 内容区：窗口固定大小，内容超出时可滚动；content_margin 提供四周内边距
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .content_margin(Margin::symmetric(8, 8))
            .show(ui, |ui| {
                ui.heading("黑洞输入法设置");
                ui.add_space(16.0);

                // 收集本帧是否有设置被修改；修改后立即落盘（实时生效 + 自动保存）
                let (changed, bindings_commit) = self.settings_groups(ui);
                // 设置被修改后立即落盘（实时生效 + 自动保存）
                if changed {
                    self.save_settings();
                }
                // 按键绑定为文本输入：不随逐键保存，仅在输入框失焦时提交；
                // 跳过空值（回退默认），避免半输入被持久化
                if bindings_commit {
                    if self.normalize_key_bindings() {
                        self.show_feedback("按键绑定不能为空，已恢复默认", true);
                    }
                    self.save_settings();
                }

                ui.add_space(8.0);

                self.auto_start_group(ui);

                ui.add_space(16.0);

                ui.horizontal(|ui| {
                    if ui.button("恢复默认").clicked() {
                        self.restore_defaults();
                    }
                });

                // 显示反馈信息（3 秒自动消失）
                if let Some((msg, ok)) = &self.feedback {
                    ui.add_space(8.0);
                    let palette = theme::palette(current_theme);
                    let color = if *ok {
                        palette.success_text
                    } else {
                        palette.error_text
                    };
                    ui.label(RichText::new(msg).color(color));
                    // egui 的 RequestRepaint 确保动画持续刷新
                    ctx.request_repaint();
                }
            });

        self.feedback_timer -= ui.input(|i| i.unstable_dt) as f64;
        if self.feedback_timer <= 0.0 {
            self.feedback = None;
        }

        self.handle_close(&ctx);
    }

    /// 根 Ui 自身没有背景色，窗口底色完全由清屏色决定。
    /// 取全局样式里的浮层底色（调色板的 base-100）；若沿用 eframe 的默认清屏色
    /// （透明），在非透明视口上会被合成为黑色，浅色主题下正文几乎不可读。
    ///
    /// 返回值需为 gamma 空间的 0-1 值（egui 对 `App::clear_color` 的要求）。
    /// 候选窗不能照搬，它靠透明清屏色呈现圆角，见 `candidate_window`。
    fn clear_color(&self, visuals: &Visuals) -> [f32; 4] {
        visuals.window_fill.to_normalized_gamma_f32()
    }
}

/// 空绑定回退为默认值（返回是否有回退）
fn normalize_binding(value: &mut String, default: &str) -> bool {
    if value.trim().is_empty() {
        *value = default.to_string();
        true
    } else {
        false
    }
}

/// 单行「标签 + DragValue」；返回是否被修改
fn drag_row<Num: Numeric>(
    ui: &mut Ui,
    label: &str,
    value: &mut Num,
    speed: f64,
    range: (Num, Num),
) -> bool {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(DragValue::new(value).speed(speed).range(range.0..=range.1))
            .changed()
    })
    .inner
}

/// 单行「标签 + 文本输入」；返回输入框是否失焦
fn text_row(ui: &mut Ui, label: &str, value: &mut String) -> bool {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.text_edit_singleline(value).lost_focus()
    })
    .inner
}

fn theme_group(ui: &mut Ui, settings: &mut Settings) -> bool {
    ui.group(|ui| {
        ui.label("主题");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut settings.theme, Theme::Light, "浅色")
                .changed()
                | ui.selectable_value(&mut settings.theme, Theme::Dark, "深色")
                    .changed()
                | ui.selectable_value(&mut settings.theme, Theme::System, "跟随系统")
                    .changed()
        })
        .inner
    })
    .inner
}

fn scheme_group(ui: &mut Ui, settings: &mut Settings) -> bool {
    ui.group(|ui| {
        ui.label("默认输入方案");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut settings.default_scheme, SchemeId::Pinyin, "全拼")
                .changed()
                | ui.selectable_value(&mut settings.default_scheme, SchemeId::Shuangpin, "双拼")
                    .changed()
        })
        .inner
    })
    .inner
}

fn candidate_window_group(
    ui: &mut Ui,
    cw: &mut black_hole_shared::CandidateWindowSettings,
) -> bool {
    ui.group(|ui| {
        ui.label("候选窗口");
        drag_row(ui, "字体大小:", &mut cw.font_size, 1.0, (10, 32))
            | drag_row(ui, "最大候选数:", &mut cw.max_candidates, 1.0, (3, 15))
    })
    .inner
}

fn key_bindings_group(ui: &mut Ui, b: &mut KeyBindings) -> bool {
    ui.group(|ui| {
        ui.label("按键绑定");
        let mut commit = text_row(ui, "下一个:", &mut b.next_candidate);
        commit |= text_row(ui, "上一个:", &mut b.prev_candidate);
        commit |= text_row(ui, "上屏:", &mut b.commit);
        commit |= text_row(ui, "整句上屏:", &mut b.commit_sentence);
        commit |= text_row(ui, "取消:", &mut b.cancel);
        // 说明：切换方案组合键（默认 Ctrl+Shift+F12）当前未被引擎消费，
        // 平台层也会丢弃功能键事件，故不提供编辑器，避免无效配置项。
        ui.label("提示：切换方案快捷键暂不支持自定义");
        commit
    })
    .inner
}

fn auto_switch_group(ui: &mut Ui, settings: &mut Settings) -> bool {
    ui.group(|ui| {
        ui.label("中英文自动切换");
        let changed = ui
            .checkbox(
                &mut settings.auto_switch_mode,
                "根据光标周围文本自动切换中英文",
            )
            .changed();
        ui.label("移动光标后按语境自动切换中/英文模式。");
        changed
    })
    .inner
}

fn llm_group(ui: &mut Ui, llm: &mut black_hole_shared::LlmCompletionSettings) -> bool {
    ui.group(|ui| {
        ui.label("整句补全（LLM）");
        let mut changed = ui.checkbox(&mut llm.enabled, "启用 LLM 整句补全").changed();
        ui.label("补全请求会将当前选中词发送到以下端点，请确认隐私策略后再开启。");
        changed |= ui.text_edit_singleline(&mut llm.endpoint).changed();
        changed |= ui.text_edit_singleline(&mut llm.model).changed();
        changed |= ui.text_edit_singleline(&mut llm.api_key).changed();
        changed |= drag_row(ui, "最大 Tokens:", &mut llm.max_tokens, 8.0, (16, 1024));
        changed |= drag_row(ui, "温度:", &mut llm.temperature, 0.05, (0.0, 1.5));
        changed |= drag_row(ui, "超时(ms):", &mut llm.timeout_ms, 500.0, (500, 60000));
        changed
    })
    .inner
}

/// 强制窗口置为前台并获得键盘焦点。
/// 返回是否已成功聚焦（前台窗口即本窗口）。
#[cfg(target_os = "windows")]
fn force_foreground(frame: &Frame) -> bool {
    let Some(window) = frame.winit_window() else {
        return false;
    };
    let Ok(handle) = window.window_handle() else {
        return false;
    };
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return false;
    };

    let hwnd = HWND(h.hwnd.get() as *mut c_void);
    unsafe {
        // 前台锁：仅前台进程（或其直接启动的进程）可调用 SetForegroundWindow。
        // daemon 为后台进程，其拉起的设置面板默认拿不到焦点；经典解法是
        // AttachThreadInput 将本线程输入队列挂到前台窗口线程后即可抢占前台。
        let foreground = GetForegroundWindow();
        let foreground_thread = GetWindowThreadProcessId(foreground, None);
        let current_thread = GetCurrentThreadId();
        if foreground_thread != 0 && foreground_thread != current_thread {
            let _ = AttachThreadInput(current_thread, foreground_thread, true);
            let _ = SetForegroundWindow(hwnd);
            let _ = AttachThreadInput(current_thread, foreground_thread, false);
        } else {
            let _ = SetForegroundWindow(hwnd);
        }
        let _ = BringWindowToTop(hwnd);
        GetForegroundWindow() == hwnd
    }
}

#[cfg(not(target_os = "windows"))]
fn force_foreground(_frame: &Frame) -> bool {
    true
}

/// 运行设置面板（阻塞当前线程）
pub fn run_settings_panel(settings_mgr: SettingsManager) {
    // daemon 在后台线程中调用本函数；Windows 上 winit 默认要求事件循环
    // 在主线程创建，需显式允许任意线程，否则窗口无法创建（与候选窗一致）。
    #[cfg(target_os = "windows")]
    let event_loop_builder = Some(Box::new(|builder: &mut EventLoopBuilder<_>| {
        builder.with_any_thread(true);
    }) as EventLoopBuilderHook);

    #[cfg(not(target_os = "windows"))]
    let event_loop_builder: Option<EventLoopBuilderHook> = None;

    let options = NativeOptions {
        viewport: ViewportBuilder::default()
            .with_inner_size([480.0, 360.0])
            .with_title("黑洞输入法设置")
            .with_active(true),
        event_loop_builder,
        wgpu_options: wgpu_configuration(),
        ..Default::default()
    };

    if let Err(e) = run_native(
        "Black-Hole Settings",
        options,
        Box::new(|cc| {
            configure_fonts(&cc.egui_ctx);
            cc.egui_ctx.set_visuals(theme::visuals(theme::palette(
                settings_mgr.settings().theme,
            )));
            Ok(Box::new(SettingsPanelApp::new(settings_mgr)))
        }),
    ) {
        error!(error = ?e, "settings panel run_native error");
    }
}
