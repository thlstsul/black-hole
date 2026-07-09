use crate::{
    InputScheme, PinyinScheme, ShuangpinScheme, SqliteDictionary, default_user_dict_path,
    global_user_dict,
};
use blackhole_shared::SchemeId;
use std::path::PathBuf;

/// 方案注册表：根据 SchemeId 创建对应的输入方案实例
pub struct SchemeRegistry {
    dict_path: Option<String>,
}

impl SchemeRegistry {
    pub fn new() -> Self {
        Self { dict_path: None }
    }

    pub fn with_dict_path(path: impl Into<String>) -> Self {
        Self {
            dict_path: Some(path.into()),
        }
    }

    /// 根据方案 ID 创建方案实例
    pub fn create_scheme(&self, id: SchemeId) -> Box<dyn InputScheme> {
        let dict = self.load_dictionary();
        match id {
            SchemeId::Pinyin => {
                let mut scheme = if let Some(dict) = dict {
                    PinyinScheme::with_dictionary(dict)
                } else {
                    PinyinScheme::new()
                };
                if let Some(ud) = global_user_dict() {
                    scheme = scheme.with_user_dict(ud);
                }
                Box::new(scheme)
            }
            SchemeId::Shuangpin => {
                let mut scheme = if let Some(dict) = dict {
                    ShuangpinScheme::with_dictionary(Box::new(dict))
                } else {
                    ShuangpinScheme::new()
                };
                if let Some(ud) = global_user_dict() {
                    scheme = scheme.with_user_dict(ud);
                }
                Box::new(scheme)
            }
        }
    }

    /// 获取所有可用方案的列表
    pub fn list_schemes(&self) -> Vec<(SchemeId, &'static str)> {
        vec![
            (SchemeId::Pinyin, "拼音"),
            (SchemeId::Shuangpin, "小鹤双拼"),
        ]
    }

    fn load_dictionary(&self) -> Option<SqliteDictionary> {
        let path = self.dict_path.as_ref()?;
        let cache_dir = default_user_dict_path()
            .parent()
            .map(|p| p.join("cache"))
            .unwrap_or_else(|| PathBuf::from("."));

        tracing::info!("Loading RIME dictionary from: {}", path);
        match SqliteDictionary::from_rime_dict_cached(path, &cache_dir) {
            Ok(dict) => {
                tracing::info!("RIME dictionary loaded successfully.");
                Some(dict)
            }
            Err(e) => {
                tracing::error!("Failed to load RIME dictionary: {}", e);
                None
            }
        }
    }
}

impl Default for SchemeRegistry {
    fn default() -> Self {
        Self::new()
    }
}
