use black_hole_platform::auto_start::set_auto_start;
use black_hole_shared::{KeyBindings, SchemeId, Settings, Theme};

use crate::configure_fonts;
use crate::settings_manager::SettingsManager;
use crate::theme_visuals;
use eframe::egui::{DragValue, Margin, ScrollArea, Ui, ViewportBuilder, ViewportCommand};
use eframe::{App, EventLoopBuilder, EventLoopBuilderHook, Frame, NativeOptions, run_native};
use tracing::{error, info};
#[cfg(target_os = "windows")]
use winit::platform::windows::EventLoopBuilderExtWindows;

pub struct SettingsPanelApp {
    settings_mgr: SettingsManager,
    last_theme: Theme,
    feedback: Option<String>,
    feedback_timer: f64,
}

impl SettingsPanelApp {
    pub fn new(settings_mgr: SettingsManager) -> Self {
        let last_theme = settings_mgr.settings().theme;
        Self {
            settings_mgr,
            last_theme,
            feedback: None,
            feedback_timer: 0.0,
        }
    }

    pub fn settings(&self) -> &Settings {
        self.settings_mgr.settings()
    }

    fn show_feedback(&mut self, msg: impl Into<String>, ok: bool) {
        let icon = if ok { "✓" } else { "✗" };
        self.feedback = Some(format!("{} {}", icon, msg.into()));
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
        let mut restored = false;
        {
            let b = &mut self.settings_mgr.settings_mut().key_bindings;
            if b.next_candidate.trim().is_empty() {
                b.next_candidate = defaults.next_candidate;
                restored = true;
            }
            if b.prev_candidate.trim().is_empty() {
                b.prev_candidate = defaults.prev_candidate;
                restored = true;
            }
            if b.commit.trim().is_empty() {
                b.commit = defaults.commit;
                restored = true;
            }
            if b.cancel.trim().is_empty() {
                b.cancel = defaults.cancel;
                restored = true;
            }
            if b.switch_scheme.trim().is_empty() {
                b.switch_scheme = defaults.switch_scheme;
                restored = true;
            }
        }
        restored
    }
}

impl App for SettingsPanelApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut Frame) {
        let ctx = ui.ctx().clone();

        let current_theme = self.settings_mgr.settings().theme;
        if current_theme != self.last_theme {
            ctx.set_visuals(theme_visuals(current_theme));
            self.last_theme = current_theme;
        }

        // 内容区：窗口固定大小，内容超出时可滚动；content_margin 提供四周内边距
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .content_margin(Margin::symmetric(8, 8))
            .show(ui, |ui| {
                ui.heading("黑洞输入法设置");
                ui.add_space(16.0);

                // 收集本帧是否有设置被修改；修改后立即落盘（实时生效 + 自动保存）
                let mut changed = false;
                // 按键绑定为文本输入：不随逐键保存，仅在输入框失焦时提交
                let mut bindings_commit = false;
                {
                    let settings = self.settings_mgr.settings_mut();

                    ui.group(|ui| {
                        ui.label("主题");
                        ui.horizontal(|ui| {
                            changed |= ui
                                .selectable_value(&mut settings.theme, Theme::Light, "浅色")
                                .changed();
                            changed |= ui
                                .selectable_value(&mut settings.theme, Theme::Dark, "深色")
                                .changed();
                            changed |= ui
                                .selectable_value(&mut settings.theme, Theme::System, "跟随系统")
                                .changed();
                        });
                    });

                    ui.add_space(8.0);

                    ui.group(|ui| {
                        ui.label("默认输入方案");
                        ui.horizontal(|ui| {
                            changed |= ui
                                .selectable_value(
                                    &mut settings.default_scheme,
                                    SchemeId::Pinyin,
                                    "全拼",
                                )
                                .changed();
                            changed |= ui
                                .selectable_value(
                                    &mut settings.default_scheme,
                                    SchemeId::Shuangpin,
                                    "双拼",
                                )
                                .changed();
                        });
                    });

                    ui.add_space(8.0);

                    ui.group(|ui| {
                        ui.label("候选窗口");
                        ui.horizontal(|ui| {
                            ui.label("字体大小:");
                            changed |= ui
                                .add(
                                    DragValue::new(&mut settings.candidate_window.font_size)
                                        .speed(1)
                                        .range(10..=32),
                                )
                                .changed();
                        });
                        ui.horizontal(|ui| {
                            ui.label("最大候选数:");
                            changed |= ui
                                .add(
                                    DragValue::new(&mut settings.candidate_window.max_candidates)
                                        .speed(1)
                                        .range(3..=15),
                                )
                                .changed();
                        });
                    });

                    ui.add_space(8.0);

                    ui.group(|ui| {
                        ui.label("按键绑定");
                        ui.horizontal(|ui| {
                            ui.label("下一个:");
                            let resp =
                                ui.text_edit_singleline(&mut settings.key_bindings.next_candidate);
                            // 文本输入不随逐键保存，避免半输入值立即生效；失焦时再提交
                            bindings_commit |= resp.lost_focus();
                        });
                        ui.horizontal(|ui| {
                            ui.label("上一个:");
                            let resp =
                                ui.text_edit_singleline(&mut settings.key_bindings.prev_candidate);
                            bindings_commit |= resp.lost_focus();
                        });
                        ui.horizontal(|ui| {
                            ui.label("上屏:");
                            let resp = ui.text_edit_singleline(&mut settings.key_bindings.commit);
                            bindings_commit |= resp.lost_focus();
                        });
                        ui.horizontal(|ui| {
                            ui.label("取消:");
                            let resp = ui.text_edit_singleline(&mut settings.key_bindings.cancel);
                            bindings_commit |= resp.lost_focus();
                        });
                        // 说明：切换方案组合键（默认 Ctrl+Shift+F12）当前未被引擎消费，
                        // 平台层也会丢弃功能键事件，故不提供编辑器，避免无效配置项。
                        ui.label("提示：切换方案快捷键暂不支持自定义");
                    });
                }

                // 设置被修改后立即落盘（实时生效 + 自动保存）
                if changed {
                    self.save_settings();
                }
                // 绑定输入框失焦时提交：跳过空值（回退默认），避免半输入被持久化
                if bindings_commit {
                    if self.normalize_key_bindings() {
                        self.show_feedback("按键绑定不能为空，已恢复默认", true);
                    }
                    self.save_settings();
                }

                ui.add_space(8.0);

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

                ui.add_space(16.0);

                ui.horizontal(|ui| {
                    if ui.button("恢复默认").clicked() {
                        // 记录恢复前自启动状态；恢复后需同步系统侧（默认关闭）
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
                });

                // 显示反馈信息（3 秒自动消失）
                if let Some(msg) = &self.feedback {
                    ui.add_space(8.0);
                    ui.label(msg);
                    // egui 的 RequestRepaint 确保动画持续刷新
                    ctx.request_repaint();
                }
            });

        self.feedback_timer -= ui.input(|i| i.unstable_dt) as f64;
        if self.feedback_timer <= 0.0 {
            self.feedback = None;
        }

        // 关闭窗口时自动保存；先回退空的按键绑定，避免半输入值被持久化
        if ctx.input(|i| i.viewport().close_requested()) {
            self.normalize_key_bindings();
            if self.settings_mgr.save() {
                info!("Settings saved on window close");
            } else {
                error!("Failed to save settings on window close");
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
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
            .with_title("黑洞输入法设置"),
        event_loop_builder,
        ..Default::default()
    };

    if let Err(e) = run_native(
        "Black-Hole Settings",
        options,
        Box::new(|cc| {
            configure_fonts(&cc.egui_ctx);
            cc.egui_ctx
                .set_visuals(theme_visuals(settings_mgr.settings().theme));
            Ok(Box::new(SettingsPanelApp::new(settings_mgr)))
        }),
    ) {
        error!(error = ?e, "settings panel run_native error");
    }
}
