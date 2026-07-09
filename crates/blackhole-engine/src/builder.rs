use crate::{Engine, SchemeRegistry, default_user_dict_path, init_global_user_dict};
use blackhole_shared::SchemeId;

/// 引擎构建器
///
/// 使用 Builder 模式简化引擎初始化，隐藏词典加载和方案选择的细节。
///
/// # 示例
/// ```
/// use blackhole_engine::EngineBuilder;
/// use blackhole_shared::SchemeId;
///
/// let engine = EngineBuilder::new()
///     .scheme(SchemeId::Pinyin)
///     .build();
/// ```
pub struct EngineBuilder {
    dict_path: Option<String>,
    scheme_id: SchemeId,
}

impl Default for EngineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineBuilder {
    /// 创建默认构建器（拼音方案，无外部词典）
    pub fn new() -> Self {
        Self {
            dict_path: None,
            scheme_id: SchemeId::Pinyin,
        }
    }

    /// 指定外部 RIME 词典路径
    pub fn dictionary(mut self, path: impl Into<String>) -> Self {
        self.dict_path = Some(path.into());
        self
    }

    /// 可选地指定外部 RIME 词典路径
    pub fn dictionary_opt(mut self, path: Option<impl Into<String>>) -> Self {
        self.dict_path = path.map(|p| p.into());
        self
    }

    /// 指定输入方案
    pub fn scheme(mut self, scheme: SchemeId) -> Self {
        self.scheme_id = scheme;
        self
    }

    /// 构建引擎实例
    pub fn build(self) -> Engine {
        // 初始化全局用户词典
        let user_dict_path = default_user_dict_path();
        if let Some(parent) = user_dict_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = init_global_user_dict(&user_dict_path) {
            tracing::warn!(
                "Failed to initialize user dictionary at {:?}: {}",
                user_dict_path,
                e
            );
        }

        let registry = if let Some(path) = self.dict_path {
            SchemeRegistry::with_dict_path(path)
        } else {
            SchemeRegistry::new()
        };

        let scheme = registry.create_scheme(self.scheme_id);
        Engine::with_registry(scheme, registry)
    }
}
