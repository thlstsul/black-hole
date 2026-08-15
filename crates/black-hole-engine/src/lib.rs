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

use std::borrow::Cow;

use black_hole_shared::{
    Candidate, CompletionHint, EngineCommand, InputContext, KeyBindings, KeyEvent, SchemeId,
    SchemeResult,
};
pub use builder::EngineBuilder;
pub use graph_decoder::{DecodeResult, GraphDecoder, ScoringConfig, ScoringConfigBuilder};
pub use language_model::LanguageModel;
pub use pinyin::PinyinCodec;
pub use pinyin_preprocessor::PinyinPreprocessor;
pub use pinyin_scheme::PinyinScheme;
pub use punctuation::convert_punctuation;
pub use ranker::{SimpleRanker, UserAwareRanker};
pub use rime_dict::{RawEntry, RimeDict, RimeDictConfig, RimeDictError};
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
    /// 接收 LLM 整句补全结果（异步到达），供 Tab 上屏时校验后拼入
    fn update_completion(&mut self, _completion: Option<CompletionHint>) {}
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
    /// 用户自定义按键绑定（设置面板可实时更新）
    key_bindings: KeyBindings,
}

impl Engine {
    pub fn new(scheme: Box<dyn InputScheme>) -> Self {
        Self {
            scheme,
            registry: SchemeRegistry::new(),
            key_bindings: KeyBindings::default(),
        }
    }

    pub fn with_registry(scheme: Box<dyn InputScheme>, registry: SchemeRegistry) -> Self {
        Self {
            scheme,
            registry,
            key_bindings: KeyBindings::default(),
        }
    }

    /// 设置按键绑定（启动时由构建器传入，运行时由 UpdateKeyBindings 命令热更新）
    pub fn set_key_bindings(&mut self, bindings: KeyBindings) {
        self.key_bindings = bindings;
    }

    pub fn process(&mut self, cmd: &EngineCommand, ctx: &InputContext) -> SchemeResult {
        match cmd {
            EngineCommand::Key(key) => {
                // 先把用户绑定键归一化为方案内部标准键名（如 ArrowDown/Space/Escape），
                // 使绑定修改对输入方案透明，无需改动各方案内部的按键处理。
                let normalized = self.normalize_key(key);
                self.scheme.handle_key(&normalized, ctx)
            }
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
            EngineCommand::UpdateKeyBindings(bindings) => {
                self.key_bindings = bindings.clone();
                SchemeResult::Ignored
            }
            EngineCommand::UpdateCompletion(completion) => {
                self.scheme.update_completion(completion.clone());
                SchemeResult::Ignored
            }
            _ => SchemeResult::Ignored,
        }
    }

    /// 将用户自定义按键绑定映射为方案内部使用的标准键名
    ///
    /// 仅当按键处于"纯净"状态（未按住 Ctrl/Alt/Meta）时才改写，
    /// 以免吞掉系统/应用的组合快捷键；对单个字母的绑定，按住 Shift
    /// 或处于 CapsLock 时也不改写，保留方案层的临时英文输入逻辑。
    /// 未命中绑定时不克隆按键，直接借用原事件，避免逐键分配。
    fn normalize_key<'a>(&self, key: &'a KeyEvent) -> Cow<'a, KeyEvent> {
        let m = key.modifiers;
        if m.ctrl || m.alt || m.meta {
            return Cow::Borrowed(key);
        }
        let plain = key.key.as_str();
        // 单个字母的绑定在 Shift/CapsLock 下不改写（临时英文模式由方案层处理）
        let remappable = |binding: &str| {
            let is_letter = {
                let mut chars = binding.chars();
                matches!((chars.next(), chars.next()), (Some(c), None) if c.is_ascii_alphabetic())
            };
            !is_letter || (!m.shift && !m.capslock)
        };
        let canonical = if plain == self.key_bindings.next_candidate
            && remappable(&self.key_bindings.next_candidate)
        {
            Some("ArrowDown")
        } else if plain == self.key_bindings.prev_candidate
            && remappable(&self.key_bindings.prev_candidate)
        {
            Some("ArrowUp")
        } else if plain == self.key_bindings.commit && remappable(&self.key_bindings.commit) {
            Some("Space")
        } else if plain == self.key_bindings.cancel && remappable(&self.key_bindings.cancel) {
            Some("Escape")
        } else if plain == self.key_bindings.commit_sentence
            && remappable(&self.key_bindings.commit_sentence)
        {
            Some("Tab")
        } else {
            None
        };
        if let Some(name) = canonical {
            let mut k = key.clone();
            k.key = name.to_string();
            Cow::Owned(k)
        } else {
            Cow::Borrowed(key)
        }
    }

    pub fn switch_scheme(&mut self, scheme: Box<dyn InputScheme>) {
        self.scheme = scheme;
    }

    pub fn current_scheme_name(&self) -> &str {
        self.scheme.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use black_hole_shared::{KeyState, Modifiers};

    fn plain_key(key: &str) -> KeyEvent {
        KeyEvent {
            key: key.to_string(),
            modifiers: Modifiers {
                shift: false,
                ctrl: false,
                alt: false,
                meta: false,
                capslock: false,
            },
            state: KeyState::Press,
        }
    }

    fn modified_key(key: &str, modifier: fn(&mut Modifiers)) -> KeyEvent {
        let mut e = plain_key(key);
        modifier(&mut e.modifiers);
        e
    }

    fn custom_bindings() -> KeyBindings {
        KeyBindings {
            next_candidate: "j".to_string(),
            prev_candidate: "k".to_string(),
            commit: "f".to_string(),
            cancel: "d".to_string(),
            switch_scheme: "Ctrl+Shift+F12".to_string(),
            commit_sentence: "Tab".to_string(),
        }
    }

    #[test]
    fn default_bindings_remap_to_canonical_keys() {
        let engine = Engine::new(Box::new(PinyinScheme::new()));
        for (input, expected) in [
            ("ArrowDown", "ArrowDown"),
            ("ArrowUp", "ArrowUp"),
            ("Space", "Space"),
            ("Escape", "Escape"),
        ] {
            let key = plain_key(input);
            let normalized = engine.normalize_key(&key);
            assert_eq!(normalized.key, expected, "默认绑定应归一化为 {expected}");
        }
    }

    #[test]
    fn custom_bindings_remap_to_canonical_keys() {
        let mut engine = Engine::new(Box::new(PinyinScheme::new()));
        engine.set_key_bindings(custom_bindings());
        for (input, expected) in [
            ("j", "ArrowDown"),
            ("k", "ArrowUp"),
            ("f", "Space"),
            ("d", "Escape"),
        ] {
            let key = plain_key(input);
            let normalized = engine.normalize_key(&key);
            assert_eq!(normalized.key, expected, "自定义绑定应归一化为 {expected}");
        }
    }

    #[test]
    fn non_bound_keys_pass_through_unchanged() {
        let mut engine = Engine::new(Box::new(PinyinScheme::new()));
        engine.set_key_bindings(custom_bindings());
        for input in ["a", "ArrowLeft", "Tab"] {
            let key = plain_key(input);
            let normalized = engine.normalize_key(&key);
            assert_eq!(normalized.key, input, "未绑定的键应原样通过");
        }
    }

    #[test]
    fn ctrl_alt_meta_held_keys_are_not_remapped() {
        let mut engine = Engine::new(Box::new(PinyinScheme::new()));
        engine.set_key_bindings(custom_bindings());
        // 按住 Ctrl/Alt/Meta 时即使按键匹配绑定也不改写，保留系统快捷键
        for modifier in [
            |m: &mut Modifiers| m.ctrl = true,
            |m: &mut Modifiers| m.alt = true,
            |m: &mut Modifiers| m.meta = true,
        ] {
            let key = modified_key("j", modifier);
            let normalized = engine.normalize_key(&key);
            assert_eq!(normalized.key, "j", "按住修饰键时不应改写绑定键");
        }
    }

    #[test]
    fn letter_binding_not_remapped_under_shift_or_capslock() {
        let mut engine = Engine::new(Box::new(PinyinScheme::new()));
        engine.set_key_bindings(custom_bindings());
        // 字母绑定在 Shift/CapsLock 下不改写，保留方案层的临时英文输入逻辑
        for modifier in [
            |m: &mut Modifiers| m.shift = true,
            |m: &mut Modifiers| m.capslock = true,
        ] {
            let key = modified_key("j", modifier);
            let normalized = engine.normalize_key(&key);
            assert_eq!(normalized.key, "j", "Shift/CapsLock 下的字母绑定不应改写");
        }
    }

    #[test]
    fn update_key_bindings_hot_updates_mapping() {
        let mut engine = Engine::new(Box::new(PinyinScheme::new()));
        let ctx = InputContext::default();
        // 默认绑定下 "j" 不是绑定键，原样通过
        assert_eq!(engine.normalize_key(&plain_key("j")).key, "j");
        // 热更新绑定后，"j" 应归一化为 ArrowDown
        engine.process(&EngineCommand::UpdateKeyBindings(custom_bindings()), &ctx);
        assert_eq!(engine.normalize_key(&plain_key("j")).key, "ArrowDown");
        // 旧的默认绑定 ArrowDown 不再匹配自定义绑定，应原样通过
        assert_eq!(
            engine.normalize_key(&plain_key("ArrowDown")).key,
            "ArrowDown"
        );
    }
}
