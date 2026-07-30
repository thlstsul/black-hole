use blackhole_shared::{Candidate, EngineCommand, InputContext, KeyEvent, SchemeId, SchemeResult};

pub mod builder;
pub mod graph_decoder;
pub mod language_model;
pub mod pinyin;
pub mod pinyin_preprocessor;
pub mod pinyin_scheme;
pub mod punctuation;
pub mod ranker;
pub mod rime_dict;
pub mod scheme_registry;
pub mod shuangpin;
pub mod shuangpin_scheme;
pub mod syllable_graph;
pub mod user_dict;

pub use builder::EngineBuilder;
pub use graph_decoder::{DecodeResult, GraphDecoder, ScoringConfig, ScoringConfigBuilder};
pub use language_model::LanguageModel;
pub use pinyin::PinyinCodec;
pub use pinyin_preprocessor::PinyinPreprocessor;
pub use pinyin_scheme::PinyinScheme;
pub use punctuation::convert_punctuation;
pub use ranker::{SimpleRanker, UserAwareRanker};
pub use rime_dict::{RawEntry, RimeDict, RimeDictError};
pub use scheme_registry::SchemeRegistry;
pub use shuangpin::ShuangpinCodec;
pub use shuangpin_scheme::ShuangpinScheme;
pub use syllable_graph::SyllableGraph;
pub use user_dict::{
    UserDictionary, default_user_dict_dir, global_user_dict, init_global_user_dict,
};

/// 输入方案顶层 trait
pub trait InputScheme: Send {
    fn name(&self) -> &str;
    fn scheme_id(&self) -> SchemeId;
    fn handle_key(&mut self, key: &KeyEvent, ctx: &InputContext) -> SchemeResult;
    fn select_candidate(&mut self, index: usize) -> Option<SchemeResult>;
    fn reset(&mut self);
}

/// 编解码器：将原始按键序列转换为方案内部编码
pub trait Codec: Send {
    fn push(&mut self, ch: char) -> CodecState;
    fn code(&self) -> &str;
    fn reset(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecState {
    Accepted,
    Rejected,
    Complete,
}

/// 词典后端
pub trait Dictionary: Send {
    /// 精确匹配查询
    fn lookup(&self, code: &str) -> Vec<Candidate>;
    /// 前缀匹配查询
    fn prefix_lookup(&self, code: &str) -> Vec<Candidate>;
    /// 音节匹配查询（支持空格分隔的音节模式匹配）
    fn syllable_match(&self, pattern: &str) -> Vec<Candidate> {
        // 默认实现：使用前缀匹配
        self.prefix_lookup(pattern)
    }
    /// 模糊匹配查询（支持通配符）
    fn fuzzy_match(&self, pattern: &str) -> Vec<Candidate> {
        // 默认实现：返回空，子类可覆盖
        let _ = pattern;
        Vec::new()
    }
}

/// 候选排序器
pub trait CandidateRanker: Send {
    fn rank(&self, code: &str, candidates: &mut [Candidate]);
}

/// 按来源分层和字数优先级排序候选词
///
/// 排序优先级：用户词 > 整句精确匹配 > 组合 > 前缀匹配 > 简拼匹配
/// 输入完整切分时，字数等于音节数的候选额外优先
pub fn sort_candidates(
    candidates: &mut [Candidate],
    syllable_count: usize,
    is_fully_segmented: bool,
) {
    candidates.sort_by(|a, b| {
        let layer = |c: &Candidate| match c.comment.as_deref() {
            Some("用户") => 0,
            Some("整句") => 1,
            Some("组合") => 2,
            Some(s) if s.starts_with("简拼") => 4,
            _ => 3,
        };
        let length_priority = |c: &Candidate| {
            if is_fully_segmented {
                let text_len = c.text.chars().count();
                if text_len == syllable_count {
                    0
                } else if text_len < syllable_count {
                    1
                } else {
                    2
                }
            } else {
                2
            }
        };
        length_priority(a)
            .cmp(&length_priority(b))
            .then_with(|| layer(a).cmp(&layer(b)))
            .then_with(|| b.score.cmp(&a.score))
    });
}

/// 引擎核心,持有当前方案、词典、排序器
pub struct Engine {
    scheme: Box<dyn InputScheme>,
    registry: SchemeRegistry,
}

impl Engine {
    pub fn new(scheme: Box<dyn InputScheme>) -> Self {
        Self {
            scheme,
            registry: SchemeRegistry::new(),
        }
    }

    pub fn with_registry(scheme: Box<dyn InputScheme>, registry: SchemeRegistry) -> Self {
        Self { scheme, registry }
    }

    pub fn process(&mut self, cmd: &EngineCommand, ctx: &InputContext) -> SchemeResult {
        match cmd {
            EngineCommand::Key(key) => self.scheme.handle_key(key, ctx),
            EngineCommand::SelectCandidate(idx) => self
                .scheme
                .select_candidate(*idx)
                .unwrap_or(SchemeResult::Ignored),
            EngineCommand::Reset => {
                self.scheme.reset();
                SchemeResult::Ignored
            }
            EngineCommand::SwitchScheme(id) => {
                self.scheme = self.registry.create_scheme(*id);
                SchemeResult::Ignored
            }
            _ => SchemeResult::Ignored,
        }
    }

    pub fn switch_scheme(&mut self, scheme: Box<dyn InputScheme>) {
        self.scheme = scheme;
    }

    pub fn current_scheme_name(&self) -> &str {
        self.scheme.name()
    }
}
