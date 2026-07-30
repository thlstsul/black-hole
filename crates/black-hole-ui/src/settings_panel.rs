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

        ui.heading("Black-Hole Settings");
        ui.add_space(16.0);

        {
            let settings = self.settings_mgr.settings_mut();

            ui.group(|ui| {
                ui.label("Theme");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut settings.theme, Theme::Light, "Light");
                    ui.selectable_value(&mut settings.theme, Theme::Dark, "Dark");
                    ui.selectable_value(&mut settings.theme, Theme::System, "System");
                });
            });

            ui.add_space(8.0);

            ui.group(|ui| {
                ui.label("Default Scheme");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut settings.default_scheme, SchemeId::Pinyin, "Pinyin");
                    ui.selectable_value(
                        &mut settings.default_scheme,
                        SchemeId::Shuangpin,
                        "Shuangpin",
                    );
                });
            });

            ui.add_space(8.0);

            ui.group(|ui| {
                ui.label("Candidate Window");
                ui.horizontal(|ui| {
                    ui.label("Font Size:");
                    ui.add(
                        egui::DragValue::new(&mut settings.candidate_window.font_size)
                            .speed(1)
                            .range(10..=32),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Max Candidates:");
                    ui.add(
                        egui::DragValue::new(&mut settings.candidate_window.max_candidates)
                            .speed(1)
                            .range(3..=15),
                    );
                });
            });

            ui.add_space(8.0);

            ui.group(|ui| {
                ui.label("Key Bindings");
                ui.horizontal(|ui| {
                    ui.label("Next:");
                    ui.text_edit_singleline(&mut settings.key_bindings.next_candidate);
                });
                ui.horizontal(|ui| {
                    ui.label("Prev:");
                    ui.text_edit_singleline(&mut settings.key_bindings.prev_candidate);
                });
                ui.horizontal(|ui| {
                    ui.label("Commit:");
                    ui.text_edit_singleline(&mut settings.key_bindings.commit);
                });
                ui.horizontal(|ui| {
                    ui.label("Cancel:");
                    ui.text_edit_singleline(&mut settings.key_bindings.cancel);
                });
                ui.horizontal(|ui| {
                    ui.label("Switch:");
                    ui.text_edit_singleline(&mut settings.key_bindings.switch_scheme);
                });
            });
        }

        ui.add_space(16.0);

        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                if self.settings_mgr.save() {
                    self.show_feedback("Settings saved", true);
                } else {
                    self.show_feedback("Failed to save settings (see log)", false);
                }
            }
            if ui.button("Reset to Default").clicked() {
                self.settings_mgr.reset_to_default();
                self.show_feedback("Reset to defaults (click Save to persist)", true);
            }
        });

        // 显示反馈信息（3 秒自动消失）
        if let Some(msg) = &self.feedback {
            ui.add_space(8.0);
            ui.label(msg);
            // egui 的 RequestRepaint 确保动画持续刷新
            ctx.request_repaint();
        }
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
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([480.0, 360.0])
            .with_title("Black-Hole Settings"),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "Black-Hole Settings",
        options,
        Box::new(|cc| {
            crate::configure_fonts(&cc.egui_ctx);
            cc.egui_ctx
                .set_visuals(theme_visuals(settings_mgr.settings().theme));
            Ok(Box::new(SettingsPanelApp::new(settings_mgr)))
        }),
    );
}
