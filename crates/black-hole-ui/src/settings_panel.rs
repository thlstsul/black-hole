use black_hole_shared::{SchemeId, Theme};

use crate::settings_manager::SettingsManager;
use crate::theme_visuals;

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

    pub fn settings(&self) -> &black_hole_shared::Settings {
        self.settings_mgr.settings()
    }

    fn show_feedback(&mut self, msg: impl Into<String>, ok: bool) {
        let icon = if ok { "✓" } else { "✗" };
        self.feedback = Some(format!("{} {}", icon, msg.into()));
        self.feedback_timer = 3.0; // 显示 3 秒
    }
}

impl eframe::App for SettingsPanelApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        let current_theme = self.settings_mgr.settings().theme;
        if current_theme != self.last_theme {
            ctx.set_visuals(theme_visuals(current_theme));
            self.last_theme = current_theme;
        }

        // 内容区：窗口固定大小，内容超出时可滚动；content_margin 提供四周内边距
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .content_margin(egui::Margin::symmetric(8, 8))
            .show(ui, |ui| {
                ui.heading("黑洞输入法设置");
                ui.add_space(16.0);

                {
                    let settings = self.settings_mgr.settings_mut();

                    ui.group(|ui| {
                        ui.label("主题");
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut settings.theme, Theme::Light, "浅色");
                            ui.selectable_value(&mut settings.theme, Theme::Dark, "深色");
                            ui.selectable_value(&mut settings.theme, Theme::System, "跟随系统");
                        });
                    });

                    ui.add_space(8.0);

                    ui.group(|ui| {
                        ui.label("默认输入方案");
                        ui.horizontal(|ui| {
                            ui.selectable_value(
                                &mut settings.default_scheme,
                                SchemeId::Pinyin,
                                "全拼",
                            );
                            ui.selectable_value(
                                &mut settings.default_scheme,
                                SchemeId::Shuangpin,
                                "双拼",
                            );
                        });
                    });

                    ui.add_space(8.0);

                    ui.group(|ui| {
                        ui.label("候选窗口");
                        ui.horizontal(|ui| {
                            ui.label("字体大小:");
                            ui.add(
                                egui::DragValue::new(&mut settings.candidate_window.font_size)
                                    .speed(1)
                                    .range(10..=32),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("最大候选数:");
                            ui.add(
                                egui::DragValue::new(&mut settings.candidate_window.max_candidates)
                                    .speed(1)
                                    .range(3..=15),
                            );
                        });
                    });

                    ui.add_space(8.0);

                    ui.group(|ui| {
                        ui.label("按键绑定");
                        ui.horizontal(|ui| {
                            ui.label("下一个:");
                            ui.text_edit_singleline(&mut settings.key_bindings.next_candidate);
                        });
                        ui.horizontal(|ui| {
                            ui.label("上一个:");
                            ui.text_edit_singleline(&mut settings.key_bindings.prev_candidate);
                        });
                        ui.horizontal(|ui| {
                            ui.label("上屏:");
                            ui.text_edit_singleline(&mut settings.key_bindings.commit);
                        });
                        ui.horizontal(|ui| {
                            ui.label("取消:");
                            ui.text_edit_singleline(&mut settings.key_bindings.cancel);
                        });
                        ui.horizontal(|ui| {
                            ui.label("切换方案:");
                            ui.text_edit_singleline(&mut settings.key_bindings.switch_scheme);
                        });
                    });
                }

                ui.add_space(8.0);

                ui.group(|ui| {
                    ui.label("开机自启动");
                    let mut auto_start = self.settings_mgr.settings().auto_start;
                    let resp = ui.checkbox(&mut auto_start, "登录时自动启动黑洞输入法");
                    if resp.changed() {
                        match black_hole_platform::auto_start::set_auto_start(auto_start) {
                            Ok(()) => {
                                self.settings_mgr.settings_mut().auto_start = auto_start;
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
                    if ui.button("保存").clicked() {
                        if self.settings_mgr.save() {
                            self.show_feedback("设置已保存", true);
                        } else {
                            self.show_feedback("保存设置失败(请查看日志)", false);
                        }
                    }
                    if ui.button("恢复默认").clicked() {
                        self.settings_mgr.reset_to_default();
                        self.show_feedback("已恢复默认设置(点击保存生效)", true);
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

        // 关闭窗口时自动保存
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.settings_mgr.save() {
                tracing::info!("Settings saved on window close");
            } else {
                tracing::error!("Failed to save settings on window close");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

/// 运行设置面板（阻塞当前线程）
pub fn run_settings_panel(settings_mgr: SettingsManager) {
    // daemon 在后台线程中调用本函数；Windows 上 winit 默认要求事件循环
    // 在主线程创建，需显式允许任意线程，否则窗口无法创建（与候选窗一致）。
    #[cfg(target_os = "windows")]
    let event_loop_builder = Some(Box::new(|builder: &mut eframe::EventLoopBuilder<_>| {
        use winit::platform::windows::EventLoopBuilderExtWindows;
        builder.with_any_thread(true);
    }) as eframe::EventLoopBuilderHook);

    #[cfg(not(target_os = "windows"))]
    let event_loop_builder: Option<eframe::EventLoopBuilderHook> = None;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([480.0, 360.0])
            .with_title("黑洞输入法设置"),
        event_loop_builder,
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        "Black-Hole Settings",
        options,
        Box::new(|cc| {
            crate::configure_fonts(&cc.egui_ctx);
            cc.egui_ctx
                .set_visuals(theme_visuals(settings_mgr.settings().theme));
            Ok(Box::new(SettingsPanelApp::new(settings_mgr)))
        }),
    ) {
        tracing::error!(error = ?e, "settings panel run_native error");
    }
}
