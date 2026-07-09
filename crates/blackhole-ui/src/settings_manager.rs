use blackhole_shared::Settings;
use std::fs;
use std::path::PathBuf;

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

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.settings)?;
        fs::write(&self.config_path, json)?;
        Ok(())
    }

    pub fn reset_to_default(&mut self) {
        self.settings = Settings::default();
    }

    fn load_from_disk(path: &PathBuf) -> Settings {
        if let Ok(content) = fs::read_to_string(path)
            && let Ok(settings) = serde_json::from_str::<Settings>(&content)
        {
            return settings;
        }
        Settings::default()
    }

    fn config_dir() -> PathBuf {
        directories::ProjectDirs::from("com", "blackhole", "ime")
            .map(|dirs| dirs.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

impl Default for SettingsManager {
    fn default() -> Self {
        Self::new()
    }
}
