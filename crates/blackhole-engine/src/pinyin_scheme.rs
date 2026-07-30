use crate::{
    Codec, CodecState, Dictionary, GraphDecoder, InputScheme, LanguageModel, PinyinCodec,
    PinyinPreprocessor, RimeDict, UserDictionary, global_user_dict,
};
use blackhole_shared::candidate_layout::{
    EXPANDED_AVAILABLE_WIDTH, GridDirection, digit_to_candidate_index_excluding,
    navigate_grid_excluding,
};
use blackhole_shared::{Candidate, InputContext, KeyEvent, KeyState, SchemeId, SchemeResult};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 拼音输入方案
pub struct PinyinScheme {
    codec: PinyinCodec,
    dictionary: Box<dyn Dictionary>,
    lm: LanguageModel,
    #[allow(dead_code)]
    preprocessor: PinyinPreprocessor,
    user_dict: Option<Arc<Mutex<UserDictionary>>>,
    /// 缓存用户词频（避免每次查询 SQLite）
    user_freq_cache: HashMap<String, i64>,
    /// 优化：缓存最近一次查询结果，避免重复查询
    last_query: Option<(String, Vec<Candidate>)>,
    /// 候选窗是否展开为完整列表
    expanded: bool,
    /// 当前选中的候选索引
    selected_index: usize,
    /// 临时英文输入缓冲（大写字母开头时进入）
    english_buffer: Option<String>,
}

impl Default for PinyinScheme {
    fn default() -> Self {
        Self::new()
    }
}

impl PinyinScheme {
    pub fn new() -> Self {
        let dict = RimeDict::from_builtin();
        let lm = dict.build_language_model();
        Self {
            codec: PinyinCodec::new(),
            dictionary: Box::new(dict),
            lm,
            preprocessor: PinyinPreprocessor::new(),
            user_dict: None,
            user_freq_cache: HashMap::new(),
            last_query: None,
            expanded: false,
            selected_index: 0,
            english_buffer: None,
        }
    }

    pub fn with_dictionary(dict: Arc<RimeDict>) -> Self {
        let lm = dict.build_language_model();
        Self {
            codec: PinyinCodec::new(),
            dictionary: Box::new(dict),
            lm,
            preprocessor: PinyinPreprocessor::new(),
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
                .record_commit(SchemeId::Pinyin, &code, text);
            // 刷新缓存，下次查询时重新加载
            self.user_freq_cache.remove(text);
        }
    }

    fn current_candidates(&mut self) -> Vec<Candidate> {
        tracing::debug!(
            "current_candidates start: code='{}'",
            self.codec.full_code()
        );

        // 防御性清理：防止用户词频缓存无限增长
        if self.user_freq_cache.len() > 500 {
            self.user_freq_cache.clear();
        }

        let full_code = self.codec.full_code();
        let spaced_code = self.codec.spaced_code();
        let abbreviated = self.codec.abbreviated_code();

        // 优化：使用缓存避免重复查询
        if let Some((cached_code, cached_candidates)) = &self.last_query
            && cached_code == &full_code
        {
            return cached_candidates.clone();
        }

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen_texts = std::collections::HashSet::new();

        // === 核心流水线：音节图 → 词典检索 → 维特比解码 ===
        let graph = self.codec.syllable_graph();
        if graph.total_len() > 0 {
            let decoder = GraphDecoder::new(&*self.dictionary)
                .with_lm(&self.lm)
                .with_user_freqs(&self.user_freq_cache);
            let decode_results = decoder.decode(&graph);
            tracing::debug!(
                "pinyin decode_results: count={}, top5={:?}",
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

        // === 补充：空格分隔编码的前缀匹配 ===
        let prefix_query = if !spaced_code.is_empty() {
            &spaced_code
        } else {
            // 当输入无法完成音节切分时（如第一个字符），直接用 full_code 回退查询
            &full_code
        };
        if !prefix_query.is_empty() {
            let prefix_results = self.dictionary.prefix_lookup(prefix_query);
            for cand in prefix_results {
                if seen_texts.insert(cand.text.clone()) {
                    candidates.push(Candidate {
                        text: cand.text.clone(),
                        comment: Some(prefix_query.clone()),
                        score: cand.score,
                    });
                } else if let Some(existing) = candidates.iter_mut().find(|c| c.text == cand.text)
                    && cand.score > existing.score
                {
                    existing.score = cand.score;
                }
            }
        }

        // === 补充：简拼匹配 ===
        if !abbreviated.is_empty() && abbreviated.len() >= 2 {
            let abbr_results = self.dictionary.prefix_lookup(&abbreviated);
            for cand in abbr_results {
                if seen_texts.insert(cand.text.clone()) {
                    candidates.push(Candidate {
                        text: cand.text.clone(),
                        comment: Some(format!("简拼 {}", abbreviated)),
                        score: cand.score / 2, // 简拼降权
                    });
                } else if let Some(existing) = candidates.iter_mut().find(|c| c.text == cand.text)
                    && cand.score / 2 > existing.score
                {
                    existing.score = cand.score / 2;
                }
            }
        }

        // === 用户词典查询：将用户高频词合并到候选列表 ===
        if let Some(ref ud) = self.user_dict_ref() {
            let user_cands = ud.lock().unwrap().lookup(SchemeId::Pinyin, &spaced_code);
            for cand in user_cands {
                // 更新词频缓存，供 GraphDecoder 的 user_bonus 使用
                self.user_freq_cache.insert(cand.text.clone(), cand.score);
                let user_boost = (cand.score * 50).min(3000) + 500;
                if seen_texts.insert(cand.text.clone()) {
                    candidates.push(Candidate {
                        text: cand.text,
                        comment: Some("用户".to_string()),
                        score: user_boost,
                    });
                } else if let Some(existing) = candidates.iter_mut().find(|c| c.text == cand.text) {
                    // 已存在则提升分数并标记为用户词
                    existing.score += user_boost;
                    existing.comment = Some("用户".to_string());
                }
            }
        }

        // 当输入恰好被完整切分为音节时，优先将字数等于音节数的候选排在前面
        let syllable_count = spaced_code.split_whitespace().count();
        let is_fully_segmented =
            !spaced_code.is_empty() && full_code == spaced_code.replace(" ", "");

        // 排序：按来源分层，用户词 > 整句精确匹配 > 组合 > 前缀匹配 > 简拼匹配
        // 输入完整切分时，字数等于音节数的候选额外优先
        crate::sort_candidates(&mut candidates, syllable_count, is_fully_segmented);

        // 更新缓存
        self.last_query = Some((full_code.clone(), candidates.clone()));

        tracing::debug!(
            "current_candidates end: code='{}', candidates={}",
            full_code,
            candidates.len()
        );

        candidates
    }

    /// 提交当前编码：优先返回当前选中的候选词，否则返回编码本身。
    fn commit_current_input(&mut self) -> String {
        let candidates = self.current_candidates();
        let text = if candidates.is_empty() {
            self.codec.full_code()
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

impl InputScheme for PinyinScheme {
    fn name(&self) -> &str {
        "拼音"
    }

    fn scheme_id(&self) -> SchemeId {
        SchemeId::Pinyin
    }

    fn handle_key(&mut self, key: &KeyEvent, _ctx: &InputContext) -> SchemeResult {
        tracing::debug!("handle_key start: key='{}'", key.key);
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

        let result = match key.key.as_str() {
            "Backspace" => {
                if !self.codec.pop() {
                    self.codec.reset();
                    self.expanded = false;
                    self.selected_index = 0;
                    return SchemeResult::Committed {
                        text: "".to_string(),
                    };
                }
                let code = self.codec.full_code();
                let candidates = self.current_candidates();
                self.selected_index = 0;
                SchemeResult::Composing {
                    code,
                    candidates,
                    selected_index: self.selected_index,
                    expanded: self.expanded,
                }
            }
            "Escape" => {
                self.codec.reset();
                self.expanded = false;
                self.selected_index = 0;
                SchemeResult::Ignored
            }
            "Space" => {
                let candidates = self.current_candidates();
                let text = if !candidates.is_empty() {
                    let idx = self.selected_index.min(candidates.len().saturating_sub(1));
                    candidates[idx].text.clone()
                } else {
                    format!("{} ", self.codec.full_code())
                };
                if !candidates.is_empty() {
                    self.record_user_commit(&text);
                }
                self.codec.reset();
                self.expanded = false;
                self.selected_index = 0;
                self.last_query = None;
                SchemeResult::Committed { text }
            }
            "Enter" => {
                let text = self.codec.full_code();
                self.codec.reset();
                self.expanded = false;
                self.selected_index = 0;
                SchemeResult::Committed { text }
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
                SchemeResult::Composing {
                    code: self.codec.full_code(),
                    candidates,
                    selected_index: self.selected_index,
                    expanded: self.expanded,
                }
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
                SchemeResult::Composing {
                    code: self.codec.full_code(),
                    candidates,
                    selected_index: self.selected_index,
                    expanded: self.expanded,
                }
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
                        code: self.codec.full_code(),
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
                SchemeResult::Composing {
                    code: self.codec.full_code(),
                    candidates,
                    selected_index: self.selected_index,
                    expanded: self.expanded,
                }
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
                        code: self.codec.full_code(),
                        candidates,
                        selected_index: self.selected_index,
                        expanded: self.expanded,
                    };
                };
                self.selected_index = new_index;
                SchemeResult::Composing {
                    code: self.codec.full_code(),
                    candidates,
                    selected_index: self.selected_index,
                    expanded: self.expanded,
                }
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
                self.select_candidate(index)
                    .unwrap_or(SchemeResult::Ignored)
            }
            _ => {
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
                        let code = self.codec.full_code();
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
                        let committed = if self.codec.full_code().is_empty() {
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
        };

        tracing::debug!(
            "handle_key end: key='{}', code='{}'",
            key.key,
            self.codec.full_code()
        );

        result
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
        self.last_query = None; // 清除缓存
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

    /// 从 (code, text, weight) 三元组构建测试词典
    fn build_dict(entries: &[(&str, &str, i64)]) -> Arc<RimeDict> {
        Arc::new(
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
            .unwrap(),
        )
    }

    fn build_dict_with_spaced_code() -> Arc<RimeDict> {
        // RIME 词库格式：text, code, weight，code 带空格分隔
        build_dict(&[
            ("zhong", "中", 100),
            ("zhong wen", "中文", 100),
            ("zhong guo", "中国", 90),
            ("a", "啊", 100),
            ("a ba fu", "阿爸父", 100),
        ])
    }

    #[test]
    fn test_pinyin_scheme_spaced_code_lookup() {
        let dict = build_dict_with_spaced_code();
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 输入 "zhongwen"
        for ch in ["z", "h", "o", "n", "g", "w", "e", "n"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        let result = scheme.handle_key(&key_event("n"), &ctx);
        if let SchemeResult::Composing { candidates, .. } = result {
            assert!(
                candidates.iter().any(|c| c.text == "中文"),
                "应能匹配到带空格编码 'zhong wen' 的候选 '中文'，实际候选: {:?}",
                candidates
            );
        } else {
            panic!("输入 zhongwen 后应处于 Composing 状态");
        }
    }

    #[test]
    fn test_pinyin_scheme_single_syllable() {
        let dict = build_dict_with_spaced_code();
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        let result = scheme.handle_key(&key_event("a"), &ctx);
        if let SchemeResult::Composing { candidates, .. } = result {
            assert!(candidates.iter().any(|c| c.text == "啊"));
        } else {
            panic!("输入 a 后应处于 Composing 状态");
        }
    }

    #[test]
    fn test_first_char_shows_candidates() {
        let dict = build_dict_with_spaced_code();
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 输入第一个字符 "z"，应能匹配到 "zhong" 前缀的候选 "中"
        let result = scheme.handle_key(&key_event("z"), &ctx);
        if let SchemeResult::Composing { candidates, .. } = result {
            assert!(
                !candidates.is_empty(),
                "输入第一个字符 'z' 后应出现候选词，实际候选: {:?}",
                candidates
            );
            assert!(
                candidates.iter().any(|c| c.text == "中"),
                "应能匹配到 '中'，实际候选: {:?}",
                candidates
            );
        } else {
            panic!("输入 z 后应处于 Composing 状态");
        }
    }

    #[test]
    fn test_pinyin_scheme_multi_syllable() {
        let dict = build_dict_with_spaced_code();
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 输入 "abafu"
        for ch in ["a", "b", "a", "f", "u"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        let result = scheme.handle_key(&key_event("u"), &ctx);
        if let SchemeResult::Composing { candidates, .. } = result {
            assert!(
                candidates.iter().any(|c| c.text == "阿爸父"),
                "应能匹配到带空格编码 'a ba fu' 的候选 '阿爸父'，实际候选: {:?}",
                candidates
            );
        } else {
            panic!("输入 abafu 后应处于 Composing 状态");
        }
    }

    #[test]
    fn test_pinyin_scheme_multiple_segmentations() {
        // 添加多种切分可能的词条
        let dict = build_dict(&[
            ("zhong wen", "中文", 100),
            ("zhong", "中", 90),
            ("wen", "文", 80),
            ("zhu ang", " Zhuang", 70),
        ]);

        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 输入 "zhuang"
        for ch in ["z", "h", "u", "a", "n", "g"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        // 应该能匹配到多种切分结果
        let result = scheme.handle_key(&key_event("g"), &ctx);
        if let SchemeResult::Composing { candidates, .. } = result {
            // 至少应该有一些候选
            assert!(!candidates.is_empty(), "应该有候选词");
        } else {
            panic!("输入 zhuang 后应处于 Composing 状态");
        }
    }

    #[test]
    fn test_pinyin_scheme_abbreviated_match() {
        // 添加简拼匹配的词条
        let dict = build_dict(&[("zw", "中文", 50), ("zhong wen", "中文", 100)]);

        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 输入 "zhongwen" 会生成简拼 "zw"
        for ch in ["z", "h", "o", "n", "g", "w", "e", "n"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        let result = scheme.handle_key(&key_event("n"), &ctx);
        if let SchemeResult::Composing { candidates, .. } = result {
            // 应该能通过简拼匹配到 "中文"
            assert!(
                candidates.iter().any(|c| c.text == "中文"),
                "应能匹配到候选 '中文'，实际候选: {:?}",
                candidates
            );
        } else {
            panic!("输入 zhongwen 后应处于 Composing 状态");
        }
    }

    fn build_dict_for_sentence_tests() -> Arc<RimeDict> {
        build_dict(&[
            // 单字
            ("zhong", "中", 50),
            ("guo", "国", 50),
            ("ren", "人", 50),
            ("min", "民", 50),
            ("wen", "文", 50),
            // 词组（score 更高，模拟常用词）
            ("zhong guo", "中国", 120),
            ("ren min", "人民", 120),
            ("zhong wen", "中文", 120),
        ])
    }

    #[test]
    fn test_sentence_building_two_syllables() {
        let dict = build_dict_for_sentence_tests();
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 输入 "zhongguo"
        for ch in ["z", "h", "o", "n", "g", "g", "u"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let result = scheme.handle_key(&key_event("o"), &ctx);

        if let SchemeResult::Composing { candidates, .. } = result {
            assert!(
                candidates.iter().any(|c| c.text == "中国"),
                "应能生成整句候选 '中国'，实际候选: {:?}",
                candidates
            );
            // 整句候选应带有 comment
            let sentence = candidates.iter().find(|c| c.text == "中国");
            assert!(
                sentence.is_some_and(|c| c.comment.as_deref() == Some("整句")),
                "整句候选应带有 '整句' 标注"
            );
        } else {
            panic!("输入 zhongguo 后应处于 Composing 状态");
        }
    }

    #[test]
    fn test_sentence_building_four_syllables() {
        let dict = build_dict_for_sentence_tests();
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 输入 "zhongguorenmin"
        for ch in [
            "z", "h", "o", "n", "g", "g", "u", "o", "r", "e", "n", "m", "i",
        ] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let result = scheme.handle_key(&key_event("n"), &ctx);

        if let SchemeResult::Composing { candidates, .. } = result {
            assert!(
                candidates.iter().any(|c| c.text == "中国人民"),
                "应能生成整句候选 '中国人民'，实际候选: {:?}",
                candidates
            );
        } else {
            panic!("输入 zhongguorenmin 后应处于 Composing 状态");
        }
    }

    #[test]
    fn test_sentence_building_four_syllables_direct() {
        let dict = build_dict_for_sentence_tests();
        let mut scheme = PinyinScheme::with_dictionary(dict);

        // 直接通过 codec 输入完整拼音
        for ch in "zhongguorenmin".chars() {
            scheme.codec.push(ch);
        }

        let candidates = scheme.current_candidates();
        println!("direct candidates: {:?}", candidates);
        assert!(
            candidates.iter().any(|c| c.text == "中国人民"),
            "直接调用应能生成整句候选 '中国人民'，实际候选: {:?}",
            candidates
        );
    }

    #[test]
    fn test_single_syllable_single_char_first() {
        // 单字 score 低于多字词，模拟实际词库场景
        let dict = build_dict(&[
            ("zhong", "中", 50),
            ("zhong", "种", 40),
            ("zhong guo", "中国", 100),
            ("zhong wen", "中文", 90),
        ]);

        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 输入 "zhong"
        for ch in ["z", "h", "o", "n", "g"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        let result = scheme.handle_key(&key_event("g"), &ctx);
        if let SchemeResult::Composing { candidates, .. } = result {
            println!("single syllable candidates: {:?}", candidates);
            assert!(!candidates.is_empty(), "输入 zhong 后应出现候选词");
            // 单音节输入时，单字应排在多字词前面
            let first_char = &candidates[0];
            assert_eq!(
                first_char.text.chars().count(),
                1,
                "单音节输入时第一个候选应为单字，实际是 '{}'，完整候选: {:?}",
                first_char.text,
                candidates
            );
        } else {
            panic!("输入 zhong 后应处于 Composing 状态");
        }
    }

    #[test]
    fn test_single_syllable_no_decode_single_char_first() {
        // 模拟 decode 结果为空的情况：词库中没有 code="zhong" 的单字，
        // 只有 code="zhong guo" 和 code="zhong wen" 的多字词
        // 单字只存在于前缀匹配中（score 低于多字词）
        let dict = build_dict(&[
            ("zhong guo", "中国", 100),
            ("zhong wen", "中文", 90),
            ("zhong", "中", 30),
            ("zhong", "种", 20),
        ]);

        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 输入 "zhong"
        for ch in ["z", "h", "o", "n", "g"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        let result = scheme.handle_key(&key_event("g"), &ctx);
        if let SchemeResult::Composing { candidates, .. } = result {
            println!("no decode candidates: {:?}", candidates);
            assert!(!candidates.is_empty(), "输入 zhong 后应出现候选词");
            // 即使 decode 结果为空，单音节输入时单字仍应排在前面
            let first_char = &candidates[0];
            assert_eq!(
                first_char.text.chars().count(),
                1,
                "单音节输入时第一个候选应为单字，实际是 '{}'，完整候选: {:?}",
                first_char.text,
                candidates
            );
        } else {
            panic!("输入 zhong 后应处于 Composing 状态");
        }
    }

    #[test]
    fn test_real_dict_single_syllable() {
        // 使用实际 RIME 词库测试
        let dict_path = std::path::Path::new("../../temp/dicts/rime_ice.dict.yaml");
        if !dict_path.exists() {
            println!("跳过测试：实际词库文件不存在");
            return;
        }

        let dict =
            Arc::new(RimeDict::from_rime_dict_cached(dict_path, std::env::temp_dir()).unwrap());
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 输入 "zhong"
        for ch in ["z", "h", "o", "n", "g"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        let result = scheme.handle_key(&key_event("g"), &ctx);
        if let SchemeResult::Composing { candidates, .. } = result {
            println!("real dict candidates for zhong: {:?}", candidates);
            assert!(!candidates.is_empty(), "输入 zhong 后应出现候选词");
            // 单音节输入时，单字应排在多字词前面
            let first_char = &candidates[0];
            assert_eq!(
                first_char.text.chars().count(),
                1,
                "单音节输入时第一个候选应为单字，实际是 '{}'，完整候选: {:?}",
                first_char.text,
                candidates
            );
        } else {
            panic!("输入 zhong 后应处于 Composing 状态");
        }
    }

    #[test]
    fn test_two_syllable_two_char_first() {
        let dict = build_dict(&[
            // 单字 score 低
            ("zhong", "中", 50),
            ("guo", "国", 50),
            // 两字词 score 中等
            ("zhong guo", "中国", 120),
            // 三字词 score 最高（模拟更常用的长词）
            ("zhong guo ren", "中国人", 200),
        ]);

        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 输入 "zhongguo"
        for ch in ["z", "h", "o", "n", "g", "g", "u"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let result = scheme.handle_key(&key_event("o"), &ctx);

        if let SchemeResult::Composing { candidates, .. } = result {
            println!("two syllable candidates: {:?}", candidates);
            assert!(!candidates.is_empty(), "输入 zhongguo 后应出现候选词");
            // 两音节输入时，两字词应排在最前（即使三字词 score 更高）
            let first = &candidates[0];
            assert_eq!(
                first.text.chars().count(),
                2,
                "两音节输入时第一个候选应为两字词，实际是 '{}'，完整候选: {:?}",
                first.text,
                candidates
            );
        } else {
            panic!("输入 zhongguo 后应处于 Composing 状态");
        }
    }

    #[test]
    fn test_three_syllable_three_char_first() {
        let dict = build_dict(&[
            // 单字 score 低
            ("a", "啊", 50),
            // 两字词 score 中等
            ("a ba", "阿爸", 120),
            // 三字词 score 较高
            ("a ba fu", "阿爸父", 150),
            // 四字词 score 最高（模拟更常用的长词）
            ("a ba fu qin", "阿爸父亲", 250),
        ]);

        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext {
            caret_x: 0,
            caret_y: 0,
            caret_h: 20,
        };

        // 输入 "abafu"
        for ch in ["a", "b", "a", "f", "u"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let result = scheme.handle_key(&key_event("u"), &ctx);

        if let SchemeResult::Composing { candidates, .. } = result {
            println!("three syllable candidates: {:?}", candidates);
            assert!(!candidates.is_empty(), "输入 abafu 后应出现候选词");
            // 三音节输入时，三字词应排在最前（即使四字词 score 更高）
            let first = &candidates[0];
            assert_eq!(
                first.text.chars().count(),
                3,
                "三音节输入时第一个候选应为三字词，实际是 '{}'，完整候选: {:?}",
                first.text,
                candidates
            );
        } else {
            panic!("输入 abafu 后应处于 Composing 状态");
        }
    }
}
