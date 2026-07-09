use crate::{
    CandidateRanker, Codec, CodecState, Dictionary, GraphDecoder, InputScheme, ShuangpinCodec,
    SimpleRanker, SqliteDictionary, UserDictionary, global_user_dict,
};
use blackhole_shared::candidate_layout::{
    EXPANDED_AVAILABLE_WIDTH, GridDirection, digit_to_candidate_index_excluding,
    navigate_grid_excluding,
};
use blackhole_shared::{Candidate, InputContext, KeyEvent, KeyState, SchemeId, SchemeResult};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 小鹤双拼输入方案
pub struct ShuangpinScheme {
    codec: ShuangpinCodec,
    dictionary: Box<dyn Dictionary>,
    ranker: Box<dyn CandidateRanker>,
    user_dict: Option<Arc<Mutex<UserDictionary>>>,
    /// 缓存用户词频（避免每次查询 SQLite）
    user_freq_cache: HashMap<String, i64>,
    expanded: bool,
    selected_index: usize,
    /// 临时英文输入缓冲（大写字母开头时进入）
    english_buffer: Option<String>,
}

impl Default for ShuangpinScheme {
    fn default() -> Self {
        Self::new()
    }
}

impl ShuangpinScheme {
    pub fn new() -> Self {
        Self {
            codec: ShuangpinCodec::new(),
            dictionary: Box::new(SqliteDictionary::from_builtin()),
            ranker: Box::new(SimpleRanker::new()),
            user_dict: None,
            user_freq_cache: HashMap::new(),
            expanded: false,
            selected_index: 0,
            english_buffer: None,
        }
    }

    pub fn with_dictionary(dictionary: Box<dyn Dictionary>) -> Self {
        Self {
            codec: ShuangpinCodec::new(),
            dictionary,
            ranker: Box::new(SimpleRanker::new()),
            user_dict: None,
            user_freq_cache: HashMap::new(),
            expanded: false,
            selected_index: 0,
            english_buffer: None,
        }
    }

    pub fn with_user_dict(mut self, user_dict: Arc<Mutex<UserDictionary>>) -> Self {
        self.user_dict = Some(user_dict);
        self
    }

    /// 获取用户词典引用（优先实例字段，回退全局单例）
    fn user_dict_ref(&self) -> Option<Arc<Mutex<UserDictionary>>> {
        self.user_dict.clone().or_else(global_user_dict)
    }

    /// 记录用户上屏，更新词频缓存和持久化存储
    fn record_user_commit(&mut self, text: &str) {
        let code = self.codec.spaced_code();
        if code.is_empty() || text == code {
            return;
        }
        if let Some(ref ud) = self.user_dict_ref() {
            let _ = ud
                .lock()
                .unwrap()
                .record_commit(SchemeId::Shuangpin, &code, text);
            self.user_freq_cache.remove(text);
        }
    }

    fn current_candidates(&mut self) -> Vec<Candidate> {
        let full_code = self.codec.full_code();
        let spaced_code = self.codec.spaced_code();

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen_texts = std::collections::HashSet::new();

        // === 核心流水线：音节图 → 词典检索 → 维特比解码 ===
        let graph = self.codec.syllable_graph();
        if graph.total_len() > 0 {
            let decoder =
                GraphDecoder::new(&*self.dictionary).with_user_freqs(&self.user_freq_cache);
            let decode_results = decoder.decode(&graph);
            tracing::debug!(
                "shuangpin decode_results: count={}, top5={:?}",
                decode_results.len(),
                decode_results
                    .iter()
                    .take(5)
                    .map(|r| (&r.text, r.score, r.is_partial))
                    .collect::<Vec<_>>()
            );

            for result in decode_results {
                if seen_texts.insert(result.text.clone()) {
                    let comment = if result.is_partial {
                        Some("组合".to_string())
                    } else {
                        Some("整句".to_string())
                    };
                    candidates.push(Candidate {
                        text: result.text,
                        comment,
                        score: (result.score * 100.0) as i64,
                    });
                }
            }
        }

        // === 补充：前缀匹配（空格分隔 + 连续全拼）===
        for query in [&spaced_code, &full_code] {
            if !query.is_empty() {
                for cand in self.dictionary.prefix_lookup(query) {
                    if seen_texts.insert(cand.text.clone()) {
                        candidates.push(cand);
                    } else if let Some(existing) =
                        candidates.iter_mut().find(|c| c.text == cand.text)
                        && cand.score > existing.score
                    {
                        existing.score = cand.score;
                    }
                }
            }
        }

        // === 用户词典查询 ===
        if let Some(ref ud) = self.user_dict_ref()
            && let Ok(user_cands) = ud.lock().unwrap().lookup(SchemeId::Shuangpin, &spaced_code)
        {
            for cand in user_cands {
                self.user_freq_cache.insert(cand.text.clone(), cand.score);
                let user_boost = (cand.score * 50).min(3000) + 500;
                if seen_texts.insert(cand.text.clone()) {
                    candidates.push(Candidate {
                        text: cand.text,
                        comment: Some("用户".to_string()),
                        score: user_boost,
                    });
                } else if let Some(existing) = candidates.iter_mut().find(|c| c.text == cand.text) {
                    existing.score += user_boost;
                    existing.comment = Some("用户".to_string());
                }
            }
        }

        // 当输入恰好被完整切分为音节时，优先将字数等于音节数的候选排在前面
        let syllable_count = spaced_code.split_whitespace().count();
        let is_fully_segmented =
            !spaced_code.is_empty() && full_code == spaced_code.replace(" ", "");

        // 排序：用户词 > 整句精确匹配 > 组合 > 前缀匹配 > 简拼匹配
        // 输入完整切分时，字数等于音节数的候选额外优先
        crate::sort_candidates(&mut candidates, syllable_count, is_fully_segmented);

        // 输入完整切分时已按字数优先排序，不再用 ranker 覆盖
        if !candidates.is_empty() && !is_fully_segmented {
            self.ranker.rank(&full_code, &mut candidates);
        }
        candidates
    }

    /// 提交当前编码：优先返回当前选中的候选词，否则返回编码本身。
    fn commit_current_input(&mut self) -> String {
        let candidates = self.current_candidates();
        let text = if candidates.is_empty() {
            self.codec.code().to_string()
        } else {
            let idx = self.selected_index.min(candidates.len().saturating_sub(1));
            candidates[idx].text.clone()
        };
        self.record_user_commit(&text);
        self.codec.reset();
        self.expanded = false;
        self.selected_index = 0;
        text
    }
}

impl InputScheme for ShuangpinScheme {
    fn name(&self) -> &str {
        "小鹤双拼"
    }

    fn scheme_id(&self) -> SchemeId {
        SchemeId::Shuangpin
    }

    fn handle_key(&mut self, key: &KeyEvent, _ctx: &InputContext) -> SchemeResult {
        if key.state != KeyState::Press {
            return SchemeResult::Ignored;
        }

        // 临时英文模式优先处理
        if let Some(ref mut buffer) = self.english_buffer {
            return match key.key.as_str() {
                "Backspace" => {
                    buffer.pop();
                    if buffer.is_empty() {
                        self.english_buffer = None;
                        SchemeResult::Committed {
                            text: "".to_string(),
                        }
                    } else {
                        SchemeResult::Composing {
                            code: buffer.clone(),
                            candidates: vec![],
                            selected_index: 0,
                            expanded: false,
                        }
                    }
                }
                "Escape" => {
                    self.english_buffer = None;
                    SchemeResult::Ignored
                }
                "Space" => {
                    let text = format!("{} ", buffer);
                    self.english_buffer = None;
                    SchemeResult::Committed { text }
                }
                "Enter" => {
                    let text = buffer.clone();
                    self.english_buffer = None;
                    SchemeResult::Committed { text }
                }
                _ => {
                    let ch = match key.key.chars().next() {
                        Some(c) if key.key.len() == 1 && c.is_ascii_alphabetic() => c,
                        _ => return SchemeResult::Ignored,
                    };
                    buffer.push(ch);
                    SchemeResult::Composing {
                        code: buffer.clone(),
                        candidates: vec![],
                        selected_index: 0,
                        expanded: false,
                    }
                }
            };
        }

        match key.key.as_str() {
            "Backspace" => {
                if self.codec.pop() {
                    let code = self.codec.code().to_string();
                    let candidates = self.current_candidates();
                    self.selected_index = 0;
                    return SchemeResult::Composing {
                        code,
                        candidates,
                        selected_index: self.selected_index,
                        expanded: self.expanded,
                    };
                }
                self.codec.reset();
                self.expanded = false;
                self.selected_index = 0;
                return SchemeResult::Committed {
                    text: "".to_string(),
                };
            }
            "Escape" => {
                self.codec.reset();
                self.expanded = false;
                self.selected_index = 0;
                return SchemeResult::Ignored;
            }
            "Space" => {
                let candidates = self.current_candidates();
                let text = if candidates.is_empty() {
                    format!("{} ", self.codec.code())
                } else {
                    let idx = self.selected_index.min(candidates.len().saturating_sub(1));
                    candidates[idx].text.clone()
                };
                if !candidates.is_empty() {
                    self.record_user_commit(&text);
                }
                self.codec.reset();
                self.expanded = false;
                self.selected_index = 0;
                return SchemeResult::Committed { text };
            }
            "Enter" => {
                let text = self.codec.code().to_string();
                self.codec.reset();
                self.expanded = false;
                self.selected_index = 0;
                return SchemeResult::Committed { text };
            }
            "ArrowLeft" => {
                let candidates = self.current_candidates();
                if candidates.is_empty() || !self.expanded {
                    return SchemeResult::Ignored;
                }
                let Some(new_index) = navigate_grid_excluding(
                    &candidates,
                    self.selected_index,
                    EXPANDED_AVAILABLE_WIDTH,
                    GridDirection::Left,
                    Some(0),
                ) else {
                    return SchemeResult::Ignored;
                };
                self.selected_index = new_index;
                return SchemeResult::Composing {
                    code: self.codec.code().to_string(),
                    candidates,
                    selected_index: self.selected_index,
                    expanded: self.expanded,
                };
            }
            "ArrowRight" => {
                let candidates = self.current_candidates();
                if candidates.is_empty() || !self.expanded {
                    return SchemeResult::Ignored;
                }
                let Some(new_index) = navigate_grid_excluding(
                    &candidates,
                    self.selected_index,
                    EXPANDED_AVAILABLE_WIDTH,
                    GridDirection::Right,
                    Some(0),
                ) else {
                    return SchemeResult::Ignored;
                };
                self.selected_index = new_index;
                return SchemeResult::Composing {
                    code: self.codec.code().to_string(),
                    candidates,
                    selected_index: self.selected_index,
                    expanded: self.expanded,
                };
            }
            "ArrowDown" => {
                let candidates = self.current_candidates();
                if candidates.is_empty() {
                    return SchemeResult::Ignored;
                }
                if !self.expanded {
                    self.expanded = true;
                    if self.selected_index == 0 && candidates.len() > 1 {
                        self.selected_index = 1;
                    }
                    return SchemeResult::Composing {
                        code: self.codec.code().to_string(),
                        candidates,
                        selected_index: self.selected_index,
                        expanded: self.expanded,
                    };
                }
                let Some(new_index) = navigate_grid_excluding(
                    &candidates,
                    self.selected_index,
                    EXPANDED_AVAILABLE_WIDTH,
                    GridDirection::Down,
                    Some(0),
                ) else {
                    return SchemeResult::Ignored;
                };
                self.selected_index = new_index;
                return SchemeResult::Composing {
                    code: self.codec.code().to_string(),
                    candidates,
                    selected_index: self.selected_index,
                    expanded: self.expanded,
                };
            }
            "ArrowUp" => {
                let candidates = self.current_candidates();
                if candidates.is_empty() || !self.expanded {
                    return SchemeResult::Ignored;
                }
                let Some(new_index) = navigate_grid_excluding(
                    &candidates,
                    self.selected_index,
                    EXPANDED_AVAILABLE_WIDTH,
                    GridDirection::Up,
                    Some(0),
                ) else {
                    self.expanded = false;
                    if self.selected_index != 0 {
                        self.selected_index = 0;
                    }
                    return SchemeResult::Composing {
                        code: self.codec.code().to_string(),
                        candidates,
                        selected_index: self.selected_index,
                        expanded: self.expanded,
                    };
                };
                self.selected_index = new_index;
                return SchemeResult::Composing {
                    code: self.codec.code().to_string(),
                    candidates,
                    selected_index: self.selected_index,
                    expanded: self.expanded,
                };
            }
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                // 无编码输入时直接输出数字
                if self.codec.code().is_empty() {
                    return SchemeResult::Committed {
                        text: key.key.clone(),
                    };
                }
                let Ok(digit) = key.key.parse::<usize>() else {
                    return SchemeResult::Ignored;
                };
                // 数字 0 不用于选择候选
                if digit == 0 {
                    return SchemeResult::Ignored;
                }
                let candidates = self.current_candidates();
                let Some(index) = digit_to_candidate_index_excluding(
                    &candidates,
                    self.selected_index,
                    self.expanded,
                    digit,
                    Some(0),
                ) else {
                    return SchemeResult::Ignored;
                };
                self.expanded = false;
                return self
                    .select_candidate(index)
                    .unwrap_or(SchemeResult::Ignored);
            }
            _ => {}
        }

        if key.key.len() != 1 {
            return SchemeResult::Ignored;
        }
        let ch = key.key.chars().next().unwrap();
        // 开始输入时，如果按住 Shift 或 CapsLock，进入临时英文模式
        if self.codec.code().is_empty()
            && ch.is_ascii_alphabetic()
            && (key.modifiers.shift || key.modifiers.capslock)
        {
            self.english_buffer = Some(ch.to_string());
            return SchemeResult::Composing {
                code: ch.to_string(),
                candidates: vec![],
                selected_index: 0,
                expanded: false,
            };
        }
        match self.codec.push(ch) {
            CodecState::Accepted | CodecState::Complete => {
                let code = self.codec.code().to_string();
                let candidates = self.current_candidates();
                self.selected_index = 0;
                self.expanded = false;
                SchemeResult::Composing {
                    code,
                    candidates,
                    selected_index: self.selected_index,
                    expanded: self.expanded,
                }
            }
            CodecState::Rejected => {
                let Some(cn) = crate::punctuation::convert_punctuation(ch) else {
                    return SchemeResult::Ignored;
                };
                let committed = if self.codec.code().is_empty() {
                    String::new()
                } else {
                    self.commit_current_input()
                };
                let text = if committed.is_empty() {
                    cn.to_string()
                } else {
                    format!("{}{}", committed, cn)
                };
                SchemeResult::Committed { text }
            }
        }
    }

    fn select_candidate(&mut self, index: usize) -> Option<SchemeResult> {
        let candidates = self.current_candidates();
        if index >= candidates.len() {
            return None;
        }
        let text = candidates[index].text.clone();
        self.record_user_commit(&text);
        self.codec.reset();
        self.expanded = false;
        self.selected_index = 0;
        Some(SchemeResult::Committed { text })
    }

    fn reset(&mut self) {
        self.codec.reset();
        self.expanded = false;
        self.selected_index = 0;
        self.english_buffer = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blackhole_shared::{InputContext, KeyEvent, KeyState, Modifiers};

    fn key_event(key: &str) -> KeyEvent {
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

    #[test]
    fn test_shuangpin_le_not_leng() {
        let mut dict = SqliteDictionary::in_memory();
        // 模拟 RIME 词库：le -> 了，leng -> 冷
        dict.insert("le", "了", 100);
        dict.insert("leng", "冷", 200);

        let mut scheme = ShuangpinScheme::with_dictionary(Box::new(dict));
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 双拼输入 "le" -> 全拼 "le"
        for ch in ["l", "e"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        let candidates = scheme.current_candidates();
        println!("shuangpin 'le' candidates: {:?}", candidates);

        assert!(!candidates.is_empty(), "输入 'le' 后应出现候选词");

        let first = &candidates[0];
        assert_eq!(
            first.text, "了",
            "双拼 'le' 首个候选应为 '了'，实际为 '{}'，完整候选: {:?}",
            first.text, candidates
        );
    }

    #[test]
    fn test_shuangpin_le_builtin_dict() {
        // 使用内置词典测试
        let mut scheme = ShuangpinScheme::new();
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 双拼输入 "le" -> 全拼 "le"
        for ch in ["l", "e"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        let candidates = scheme.current_candidates();
        println!("shuangpin 'le' builtin candidates: {:?}", candidates);

        assert!(!candidates.is_empty(), "输入 'le' 后应出现候选词");

        let first = &candidates[0];
        println!("first candidate: {:?}", first);
        // 记录首个候选，用于调试（不断言，只是观察）
    }

    #[test]
    fn test_shuangpin_le_real_dict() {
        // 使用实际 RIME 词库测试
        let dict_path = std::path::Path::new("../../temp/dicts/rime_ice.dict.yaml");
        if !dict_path.exists() {
            println!("跳过测试：实际词库文件不存在");
            return;
        }

        let dict =
            SqliteDictionary::from_rime_dict_cached(dict_path, std::env::temp_dir()).unwrap();
        let mut scheme = ShuangpinScheme::with_dictionary(Box::new(dict));
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 双拼输入 "le" -> 全拼 "le"
        for ch in ["l", "e"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        let candidates = scheme.current_candidates();
        println!("shuangpin 'le' real dict candidates: {:?}", candidates);

        assert!(!candidates.is_empty(), "输入 'le' 后应出现候选词");

        let first = &candidates[0];
        println!("first candidate: {:?}", first);
        // 记录首个候选，用于调试
    }

    #[test]
    fn test_shuangpin_le_no_exact_match() {
        // 模拟词典中没有 code="le" 精确匹配的情况
        let mut dict = SqliteDictionary::in_memory();
        // 只插入 "leng"，不插入 "le"
        dict.insert("leng", "冷", 200);
        dict.insert("lei", "类", 150);

        let mut scheme = ShuangpinScheme::with_dictionary(Box::new(dict));
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        for ch in ["l", "e"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        let candidates = scheme.current_candidates();
        println!("shuangpin 'le' no exact match candidates: {:?}", candidates);

        assert!(!candidates.is_empty(), "输入 'le' 后应出现候选词");

        // 此时 "冷" 可能因为 score 最高而排在前面
        let first = &candidates[0];
        println!("first candidate (no exact match): {:?}", first);
    }

    #[test]
    fn test_engine_switch_scheme_shares_dict() {
        use crate::{Engine, PinyinScheme, ShuangpinScheme, SqliteDictionary};
        use blackhole_shared::{EngineCommand, SchemeResult};

        let dict_path = std::path::Path::new("../../temp/dicts/rime_ice.dict.yaml");
        if !dict_path.exists() {
            println!("跳过测试：实际词库文件不存在");
            return;
        }

        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 手动加载外部词典，分别构建拼音和双拼引擎（避免用户词典干扰）
        let cache_dir = std::env::temp_dir();
        let pinyin_dict = SqliteDictionary::from_rime_dict_cached(dict_path, &cache_dir)
            .expect("加载外部词典失败");
        let shuangpin_dict = SqliteDictionary::from_rime_dict_cached(dict_path, &cache_dir)
            .expect("加载外部词典失败");

        // 拼音模式下输入 "le"
        let mut pinyin_engine = Engine::new(Box::new(PinyinScheme::with_dictionary(pinyin_dict)));
        for ch in ["l", "e"] {
            let _ = pinyin_engine.process(&EngineCommand::Key(key_event(ch)), &ctx);
        }
        let pinyin_first =
            match pinyin_engine.process(&EngineCommand::Key(key_event("ArrowDown")), &ctx) {
                SchemeResult::Composing { candidates, .. } => {
                    candidates.first().map(|c| c.text.clone())
                }
                _ => None,
            };

        // 双拼模式下输入 "le"
        let mut shuangpin_engine = Engine::new(Box::new(ShuangpinScheme::with_dictionary(
            Box::new(shuangpin_dict),
        )));
        for ch in ["l", "e"] {
            let _ = shuangpin_engine.process(&EngineCommand::Key(key_event(ch)), &ctx);
        }
        let shuangpin_first =
            match shuangpin_engine.process(&EngineCommand::Key(key_event("ArrowDown")), &ctx) {
                SchemeResult::Composing { candidates, .. } => {
                    candidates.first().map(|c| c.text.clone())
                }
                _ => None,
            };

        // 验证拼音和双拼使用同一外部词典时，'le' 的首个候选一致
        assert!(shuangpin_first.is_some(), "双拼应能获取候选词");
        assert_eq!(
            pinyin_first, shuangpin_first,
            "拼音和双拼使用同一外部词典时，'le' 的首个候选应一致: pinyin={:?}, shuangpin={:?}",
            pinyin_first, shuangpin_first
        );
    }
}
