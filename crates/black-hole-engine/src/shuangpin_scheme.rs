use crate::{
    CandidateRanker, Codec, CodecState, Dictionary, GraphDecoder, InputScheme, RimeDict,
    ShuangpinCodec, SimpleRanker, UserDictionary, global_user_dict,
};
use black_hole_shared::candidate_layout::{
    EXPANDED_AVAILABLE_WIDTH, GridDirection, digit_to_candidate_index_excluding,
    navigate_grid_excluding,
};
use black_hole_shared::{Candidate, InputContext, KeyEvent, KeyState, SchemeId, SchemeResult};
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
    /// 缓存最近一次查询结果（导航期间复用，保证候选顺序稳定）
    last_query: Option<(String, Vec<Candidate>)>,
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
            dictionary: Box::new(RimeDict::from_builtin()),
            ranker: Box::new(SimpleRanker::new()),
            user_dict: None,
            user_freq_cache: HashMap::new(),
            last_query: None,
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
            last_query: None,
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
        // 仅当 text 与当前编码精确匹配时才记录用户词频。
        // 否则用户从前缀匹配结果中选择一个不完全匹配当前编码的词（如输入 shu 选择 shuo 的“说”）
        // 会被错误地学习为 code -> text 映射，导致下次输入同一编码时首选错误的词。
        let exact_match = self.dictionary.lookup(&code).iter().any(|c| c.text == text);
        if !exact_match {
            return;
        }
        if let Some(ref ud) = self.user_dict_ref() {
            ud.lock()
                .unwrap()
                .record_commit(SchemeId::Shuangpin, &code, text);
            self.user_freq_cache.remove(text);
        }
    }

    fn current_candidates(&mut self) -> Vec<Candidate> {
        let started = std::time::Instant::now();
        let full_code = self.codec.full_code();
        let spaced_code = self.codec.spaced_code();
        let has_pending = self.codec.has_pending();

        // 缓存：同一原始输入直接复用结果，避免导航期间重复查询导致候选顺序抖动
        let input_code = self.codec.code().to_string();
        if let Some((cached_code, cached_candidates)) = &self.last_query
            && cached_code == &input_code
        {
            return cached_candidates.clone();
        }

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen_texts = std::collections::HashSet::new();

        // === 核心流水线：音节图 → 词典检索 → 维特比解码 ===
        let graph = self.codec.syllable_graph();
        if graph.total_len() > 0 {
            let decoder =
                GraphDecoder::new(&*self.dictionary).with_user_freqs(&self.user_freq_cache);
            let decode_results = decoder.decode(&graph);
            for result in decode_results {
                if seen_texts.insert(result.text.clone()) {
                    // 有 pending 时音节图只覆盖了部分输入，标记为"组合"
                    let is_partial = result.is_partial || has_pending;
                    candidates.push(Candidate {
                        text: result.text,
                        comment: if is_partial {
                            Some("组合".to_string())
                        } else {
                            Some("整句".to_string())
                        },
                        score: (result.score * 100.0) as i64,
                    });
                }
            }
        }
        let decode_elapsed = started.elapsed();

        // === 前缀匹配（空格分隔 + 连续全拼）===
        // 单音节时 spaced 与 full 相同（如 "niang"），只查一次
        let t = std::time::Instant::now();
        let queries: Vec<&String> = if spaced_code == full_code {
            vec![&spaced_code]
        } else {
            vec![&spaced_code, &full_code]
        };
        for query in queries {
            if query.is_empty() {
                continue;
            }
            for cand in self.dictionary.prefix_lookup(query) {
                if seen_texts.insert(cand.text.clone()) {
                    candidates.push(cand);
                } else if let Some(existing) = candidates.iter_mut().find(|c| c.text == cand.text)
                    && cand.score > existing.score
                {
                    existing.score = cand.score;
                }
            }
        }
        let prefix_elapsed = t.elapsed();

        // === 挂起字符声母前缀匹配 ===
        // 当有 pending 时，用 "[已有音节] [pending声母]" 格式查找双字词。
        // 例如 "uuy" → "shu y" 匹配 "shu yao" / "shu ye" / "shu yu" 等。
        let t = std::time::Instant::now();
        if let Some(pending_query) = self.codec.spaced_code_with_pending_initial() {
            for cand in self.dictionary.prefix_lookup(&pending_query) {
                if seen_texts.insert(cand.text.clone()) {
                    candidates.push(Candidate {
                        text: cand.text.clone(),
                        comment: Some("整句".to_string()),
                        score: cand.score + 5000,
                    });
                } else if let Some(existing) = candidates.iter_mut().find(|c| c.text == cand.text) {
                    existing.score += 5000;
                    existing.comment = Some("整句".to_string());
                }
            }
        }
        let pending_prefix_elapsed = t.elapsed();

        // === 用户词典查询 ===
        let t = std::time::Instant::now();
        if let Some(ref ud) = self.user_dict_ref() {
            let user_cands = ud.lock().unwrap().lookup(SchemeId::Shuangpin, &spaced_code);
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
        let userdb_elapsed = t.elapsed();

        // === 排序 ===
        // 当刚好完整切分时，字数等于音节数的候选优先。
        // 有 pending 时（如 "uuy" 的 'y'→下一字声母），有效音节数含 pending 音节，
        // 使双字词获得字数匹配优先。且此时跳过 ranker（ranker 按分数排序会打乱层序）。
        let t = std::time::Instant::now();
        let syllable_count = spaced_code.split_whitespace().count();
        let is_fully_segmented =
            !spaced_code.is_empty() && full_code == spaced_code.replace(" ", "");
        let eff_syl_count = if has_pending {
            syllable_count + 1
        } else {
            syllable_count
        };

        crate::sort_candidates(
            &mut candidates,
            eff_syl_count,
            is_fully_segmented || has_pending,
        );

        if !candidates.is_empty() && !is_fully_segmented && !has_pending {
            self.ranker.rank(&full_code, &mut candidates);
        }
        let sort_elapsed = t.elapsed();

        tracing::debug!(
            "shuangpin candidates: full='{}', spaced='{}', pending={}, n={}, decode_us={}, prefix_us={}, pending_prefix_us={}, userdb_us={}, sort_us={}, total_us={}",
            full_code,
            spaced_code,
            has_pending,
            candidates.len(),
            decode_elapsed.as_micros(),
            prefix_elapsed.as_micros(),
            pending_prefix_elapsed.as_micros(),
            userdb_elapsed.as_micros(),
            sort_elapsed.as_micros(),
            started.elapsed().as_micros()
        );

        self.last_query = Some((input_code, candidates.clone()));
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
        self.last_query = None;
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
                self.last_query = None;
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
        // 编码为空时分号键直接输出中文分号（避免被当作双拼编码键"ing"）
        if self.codec.code().is_empty() && ch == ';' {
            return SchemeResult::Committed {
                text: "；".to_string(),
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
        self.last_query = None;
        Some(SchemeResult::Committed { text })
    }

    fn reset(&mut self) {
        self.codec.reset();
        self.last_query = None;
        self.expanded = false;
        self.selected_index = 0;
        self.english_buffer = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use black_hole_shared::{InputContext, KeyEvent, KeyState, Modifiers};

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

    /// 从 (code, text, weight) 三元组构建测试词典
    fn build_dict(entries: &[(&str, &str, i64)]) -> RimeDict {
        RimeDict::from_entries(
            entries
                .iter()
                .map(|(code, text, weight)| crate::RawEntry {
                    code: code.to_string(),
                    text: text.to_string(),
                    weight: Some(*weight as f32),
                })
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn test_shuangpin_le_not_leng() {
        // 模拟 RIME 词库：le -> 了，leng -> 冷
        let dict = build_dict(&[("le", "了", 100), ("leng", "冷", 200)]);

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

        let dict = RimeDict::from_rime_dict_cached(dict_path, std::env::temp_dir()).unwrap();
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
        // 只插入 "leng"，不插入 "le"
        let dict = build_dict(&[("leng", "冷", 200), ("lei", "类", 150)]);

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
        use crate::{Engine, PinyinScheme, ShuangpinScheme};
        use black_hole_shared::{EngineCommand, SchemeResult};

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
        let pinyin_dict = std::sync::Arc::new(
            RimeDict::from_rime_dict_cached(dict_path, &cache_dir).expect("加载外部词典失败"),
        );
        let shuangpin_dict =
            RimeDict::from_rime_dict_cached(dict_path, &cache_dir).expect("加载外部词典失败");

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

    #[test]
    fn test_shuangpin_user_dict_no_partial_learn() {
        // 模拟词典：shu -> 书（高频），shuo -> 说（低频）
        let dict = build_dict(&[("shu", "书", 200), ("shuo", "说", 100)]);

        let user_dict = Arc::new(Mutex::new(UserDictionary::open_in_memory()));
        let mut scheme =
            ShuangpinScheme::with_dictionary(Box::new(dict)).with_user_dict(user_dict.clone());
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 第一次输入 uu -> shu
        for ch in ["u", "u"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        // 模拟用户从前缀匹配中选择了“说”（其真实编码是 shuo，不是 shu）
        scheme.record_user_commit("说");
        scheme.codec.reset();

        // 再次输入 uu
        for ch in ["u", "u"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        let candidates = scheme.current_candidates();
        let first = candidates.first().map(|c| c.text.as_str()).unwrap_or("");
        assert_eq!(
            first, "书",
            "不精确匹配的用户选择不应污染当前编码的首选；实际首选为 '{}'，候选: {:?}",
            first, candidates
        );
    }

    #[test]
    fn test_shuangpin_navigation_keeps_candidate_order() {
        // 多个同权重候选：前缀查询去重曾依赖 HashMap 迭代顺序（随机），
        // 且双拼方案此前每次导航都重新查询，导致候选顺序在上下导航时抖动
        let dict = build_dict(&[
            ("ni", "你", 100),
            ("ni", "尼", 100),
            ("ni", "泥", 100),
            ("ni", "逆", 100),
            ("ni", "匿", 100),
            ("ni", "腻", 100),
            ("ni", "妮", 100),
            ("ni", "霓", 100),
            ("ni", "倪", 100),
            ("ni", "坭", 100),
            ("ni", "猊", 100),
            ("ni", "怩", 100),
            ("ni", "拟", 100),
            ("ni", "溺", 100),
            ("ni", "昵", 100),
            ("ni", "鲵", 100),
            ("ni", "旎", 100),
            ("ni", "睨", 100),
            ("ni", "铌", 100),
            ("ni", "嫟", 100),
        ]);
        let mut scheme = ShuangpinScheme::with_dictionary(Box::new(dict));
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        for ch in ["n", "i"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        let SchemeResult::Composing {
            candidates: initial,
            ..
        } = scheme.handle_key(&key_event("ArrowDown"), &ctx)
        else {
            panic!("输入 ni 后按 ArrowDown 应处于 Composing 状态");
        };
        assert!(initial.len() >= 20, "候选数量应覆盖全部同权重字");

        let initial_texts: Vec<&str> = initial.iter().map(|c| c.text.as_str()).collect();
        // 连续多次上下导航，候选列表顺序必须保持冻结
        for key in ["ArrowDown", "ArrowDown", "ArrowUp", "ArrowDown", "ArrowUp"] {
            let SchemeResult::Composing { candidates, .. } =
                scheme.handle_key(&key_event(key), &ctx)
            else {
                panic!("导航 {} 应处于 Composing 状态", key);
            };
            let texts: Vec<&str> = candidates.iter().map(|c| c.text.as_str()).collect();
            assert_eq!(initial_texts, texts, "导航 {} 后候选顺序发生变动", key);
        }
    }
}
