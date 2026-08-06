use black_hole_shared::Settings;
use serde_json::{from_str, to_string_pretty};
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use tracing::{error, info, warn};

/// 设置管理器：负责设置的加载、保存和默认值
pub struct SettingsManager {
    settings: Settings,
    config_path: PathBuf,
}

impl SettingsManager {
    pub fn new() -> Self {
        let config_dir = Self::config_dir();
        let config_path = config_dir.join("settings.json");
        let settings = Self::load_from_disk(&config_path);
        Self {
            settings,
            config_path,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    /// 保存当前设置到磁盘，失败时记录错误
    pub fn save(&mut self) -> bool {
        let path = &self.config_path;
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            error!("Failed to create config directory {:?}: {}", parent, e);
            return false;
        }

        let json = match to_string_pretty(&self.settings) {
            Ok(j) => j,
            Err(e) => {
                error!("Failed to serialize settings: {}", e);
                return false;
            }
        };
        if let Err(e) = fs::write(path, json) {
            error!("Failed to write settings to {:?}: {}", path, e);
            return false;
        }
        info!("Settings saved to {:?}", path);
        true
    }

    pub fn reset_to_default(&mut self) {
        self.settings = Settings::default();
    }

    fn load_from_disk(path: &PathBuf) -> Settings {
        match fs::read_to_string(path) {
            Ok(content) => match from_str::<Settings>(&content) {
                Ok(settings) => {
                    info!("Loaded settings from {:?}", path);
                    return settings;
                }
                Err(e) => {
                    warn!(
                        "Failed to parse settings file {:?}: {}, using defaults",
                        path,
                        e
                    );
                }
            },
            Err(e) if e.kind() == ErrorKind::NotFound => {
                info!("No settings file found at {:?}, using defaults", path);
            }
            Err(e) => {
                warn!(
                    "Failed to read settings file {:?}: {}, using defaults",
                    path,
                    e
                );
            }
        }
        Settings::default()
    }

    fn config_dir() -> PathBuf {
        directories::ProjectDirs::from("com", "black-hole", "ime")
            .map(|dirs| dirs.config_dir().to_path_buf())
            .expect("Unable to determine config directory")
    }
}

impl Default for SettingsManager {
    fn default() -> Self {
        Self::new()
    }
}
