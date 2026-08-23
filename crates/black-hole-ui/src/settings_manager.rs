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
    /// 磁盘加载是否成功：false 表示文件读取/解析失败（如设置面板写入时
    /// 的半写文件），此时 settings 为默认值回退，调用方应改用其它基线
    /// （如 daemon 的 last_applied）。文件不存在按默认值处理，不算失败。
    load_ok: bool,
}

impl SettingsManager {
    pub fn new() -> Self {
        let config_dir = Self::config_dir();
        let config_path = config_dir.join("settings.json");
        Self::from_config_path(config_path)
    }

    /// 以显式配置文件路径构造（供测试注入临时目录，避免读写真实用户配置）。
    /// 加载语义与 [`SettingsManager::new`] 完全一致（含 `load_ok` 判定）。
    pub fn from_config_path(config_path: PathBuf) -> Self {
        let (settings, load_ok) = Self::load_from_disk(&config_path);
        Self {
            settings,
            config_path,
            load_ok,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    /// 磁盘加载是否成功。
    ///
    /// `false` 表示文件存在但读取/解析失败（如半写文件或损坏），
    /// 此时 `settings()` 是默认值回退而非文件内容；调用方应避免把
    /// 默认值写回磁盘覆盖真实设置。文件不存在视为成功（默认值合法）。
    pub fn load_ok(&self) -> bool {
        self.load_ok
    }

    /// 配置文件完整路径（供 daemon 监听变更时使用）
    pub fn config_path(&self) -> &PathBuf {
        &self.config_path
    }

    /// 重新从磁盘加载设置（供 daemon 热更新轮询使用）。
    ///
    /// 读取或解析失败时返回 `None`，调用方应保留当前内存中的值，
    /// 避免把损坏/半写的文件当成有效设置覆盖运行时状态。
    pub fn try_reload(&self) -> Option<Settings> {
        let content = fs::read_to_string(&self.config_path).ok()?;
        match from_str::<Settings>(&content) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(
                    "Failed to parse settings file {:?}: {}, keeping current",
                    self.config_path, e
                );
                None
            }
        }
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

    /// 从磁盘加载设置，返回 (设置, 加载是否成功)。
    ///
    /// 文件不存在时按默认值处理且视为成功（首次运行）；文件存在但读取或
    /// 解析失败（如设置面板写入的半写文件）时回退默认值并标记失败，
    /// 供调用方决定是否改用其它基线，避免把默认值写回覆盖真实设置。
    fn load_from_disk(path: &PathBuf) -> (Settings, bool) {
        match fs::read_to_string(path) {
            Ok(content) => match from_str::<Settings>(&content) {
                Ok(settings) => {
                    info!("Loaded settings from {:?}", path);
                    (settings, true)
                }
                Err(e) => {
                    warn!(
                        "Failed to parse settings file {:?}: {}, using defaults",
                        path, e
                    );
                    (Settings::default(), false)
                }
            },
            Err(e) if e.kind() == ErrorKind::NotFound => {
                info!("No settings file found at {:?}, using defaults", path);
                (Settings::default(), true)
            }
            Err(e) => {
                warn!(
                    "Failed to read settings file {:?}: {}, using defaults",
                    path, e
                );
                (Settings::default(), false)
            }
        }
    }

    fn config_dir() -> PathBuf {
        directories::ProjectDirs::from("com", "black-hole", "ime")
            .map(|dirs| dirs.config_dir().to_path_buf())
            .unwrap_or_else(|| {
                // 极少数环境无法解析用户目录时回退到临时目录，避免 panic
                warn!("Unable to determine config directory, falling back to temp dir");
                std::env::temp_dir().join("black-hole-ime")
            })
    }
}

impl Default for SettingsManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::process;

    /// 每个测试使用独立的临时目录，避免并行测试互相干扰
    fn temp_config_path(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "black-hole-settings-test-{}-{}",
            process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("settings.json")
    }

    #[test]
    fn load_ok_true_for_valid_default_file() {
        // 文件内容合法且为默认值：加载成功（不能被误判为解析失败）
        let path = temp_config_path("defaults");
        let json = serde_json::to_string(&Settings::default()).unwrap();
        fs::write(&path, json).unwrap();

        let (settings, ok) = SettingsManager::load_from_disk(&path);
        assert!(ok, "合法默认值文件应加载成功");
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn load_ok_false_for_corrupt_file() {
        // 文件存在但内容损坏（如设置面板写入中的半写文件）：
        // 加载失败，调用方应改用其它基线，避免默认值覆盖真实设置
        let path = temp_config_path("corrupt");
        fs::write(&path, "{not valid json").unwrap();

        let (_, ok) = SettingsManager::load_from_disk(&path);
        assert!(!ok, "损坏文件应标记加载失败");
    }

    #[test]
    fn load_ok_true_for_missing_file() {
        // 文件不存在（首次运行）：按默认值处理且视为成功
        let path = temp_config_path("missing");
        // 不创建文件
        let (settings, ok) = SettingsManager::load_from_disk(&path);
        assert!(ok, "文件不存在应按默认值处理且视为成功");
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn load_ok_true_for_valid_non_default_file() {
        // 文件内容合法且含非默认值：加载成功并保留磁盘值
        let path = temp_config_path("nondefault");
        let expected = Settings {
            auto_switch_mode: true,
            ..Settings::default()
        };
        fs::write(&path, serde_json::to_string(&expected).unwrap()).unwrap();

        let (settings, ok) = SettingsManager::load_from_disk(&path);
        assert!(ok);
        assert_eq!(settings, expected);
    }
}
