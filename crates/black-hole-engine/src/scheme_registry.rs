use crate::{
    InputScheme, PinyinScheme, RimeDict, ShuangpinScheme, default_user_dict_dir, global_user_dict,
};
use black_hole_shared::SchemeId;
use std::sync::Arc;

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

    /// 加载全局共享的 rime-dict 词典（同一词库路径进程内只编译/加载一次）
    fn load_dictionary(&self) -> Option<Arc<RimeDict>> {
        let path = self.dict_path.as_ref()?;
        let cache_dir = default_user_dict_dir().join("cache");

        tracing::info!("Loading RIME dictionary from: {}", path);
        let dict = RimeDict::shared(path, &cache_dir)?;
        tracing::info!("RIME dictionary loaded successfully.");
        Some(dict)
    }
}

impl Default for SchemeRegistry {
    fn default() -> Self {
        Self::new()
    }
}
