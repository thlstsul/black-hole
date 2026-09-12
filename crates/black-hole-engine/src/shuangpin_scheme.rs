#[cfg(test)]
use crate::RawEntry;
use crate::punctuation::QuotePair;
use crate::scheme_helpers;
use crate::{
    CandidateRanker, Codec, CodecState, Dictionary, GraphDecoder, InputScheme, RimeDict,
    ShuangpinCodec, SimpleRanker, UserDictionary, global_user_dict, sort_candidates,
};
use black_hole_shared::{
    Candidate, CompletionHint, InputContext, KeyEvent, KeyState, SchemeId, SchemeResult,
};
use rustc_hash::{FxHashMap, FxHashSet};
#[cfg(test)]
use std::env;
#[cfg(test)]
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::debug;

/// 小鹤双拼输入方案
pub struct ShuangpinScheme {
    codec: ShuangpinCodec,
    dictionary: Box<dyn Dictionary>,
    ranker: Box<dyn CandidateRanker>,
    user_dict: Option<Arc<Mutex<UserDictionary>>>,
    /// 缓存用户词频（避免每次查询 SQLite）
    user_freq_cache: FxHashMap<String, i64>,
    /// 缓存最近一次查询结果（导航期间复用，保证候选顺序稳定）
    last_query: Option<(String, Vec<Candidate>)>,
    expanded: bool,
    selected_index: usize,
    /// LLM 整句补全结果（异步到达，Tab 提交时校验后拼入上屏文本）
    completion: Option<CompletionHint>,
    /// 临时英文输入缓冲（大写字母开头时进入）
    english_buffer: Option<String>,
    /// 中文引号配对状态（' 与 " 交替输出左右引号）
    quote_pair: QuotePair,
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
            user_freq_cache: FxHashMap::default(),
            last_query: None,
            expanded: false,
            selected_index: 0,
            completion: None,
            english_buffer: None,
            quote_pair: QuotePair::new(),
        }
    }

    pub fn with_dictionary(dictionary: Box<dyn Dictionary>) -> Self {
        Self {
            codec: ShuangpinCodec::new(),
            dictionary,
            ranker: Box::new(SimpleRanker::new()),
            user_dict: None,
            user_freq_cache: FxHashMap::default(),
            last_query: None,
            expanded: false,
            selected_index: 0,
            completion: None,
            english_buffer: None,
            quote_pair: QuotePair::new(),
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

    fn record_user_commit(&mut self, text: &str) {
        scheme_helpers::record_user_commit(
            &self.codec.spaced_code(),
            text,
            &*self.dictionary,
            self.user_dict_ref(),
            SchemeId::Shuangpin,
            &mut self.user_freq_cache,
        );
    }

    fn current_candidates(&mut self) -> Vec<Candidate> {
        let started = Instant::now();
        let full_code = self.codec.full_code();
        let spaced_code = self.codec.spaced_code();
        let has_pending = self.codec.has_pending();

        let input_code = self.codec.code().to_string();
        if let Some((cached_code, cached_candidates)) = &self.last_query
            && cached_code == &input_code
        {
            return cached_candidates.clone();
        }

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen_texts = FxHashSet::default();
        let mut acc = scheme_helpers::CandidateSet::new(&mut candidates, &mut seen_texts);

        let graph = self.codec.syllable_graph();
        if graph.total_len() > 0 {
            let decoder =
                GraphDecoder::new(&*self.dictionary).with_user_freqs(&self.user_freq_cache);
            let decode_results = decoder.decode(&graph);
            for result in decode_results {
                let is_partial = result.is_partial || has_pending;
                let comment = if is_partial {
                    Some("组合".to_string())
                } else {
                    Some("整句".to_string())
                };
                acc.push(result.text, (result.score * 100.0) as i64, comment);
            }
        }
        let decode_elapsed = started.elapsed();

        let t = Instant::now();
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
                acc.push_or_replace(cand.text, cand.score, cand.comment);
            }
        }
        let prefix_elapsed = t.elapsed();

        let t = Instant::now();
        if let Some(pending_query) = self.codec.spaced_code_with_pending_initial() {
            for cand in self.dictionary.prefix_lookup(&pending_query) {
                acc.push_or_boost(
                    cand.text.clone(),
                    cand.score + 5000,
                    Some("整句".to_string()),
                );
            }
        }
        let pending_prefix_elapsed = t.elapsed();

        let t = Instant::now();
        if let Some(ref ud) = self.user_dict_ref() {
            let user_cands = ud.lock().unwrap().lookup(SchemeId::Shuangpin, &spaced_code);
            for cand in user_cands {
                self.user_freq_cache.insert(cand.text.clone(), cand.score);
                let boost = scheme_helpers::user_boost(cand.score);
                acc.push_or_boost(cand.text, boost, Some("用户".to_string()));
            }
        }
        let userdb_elapsed = t.elapsed();

        // === 排序 ===
        // 当刚好完整切分时，字数等于音节数的候选优先。
        // 有 pending 时（如 "uuy" 的 'y'→下一字声母），有效音节数含 pending 音节，
        // 使双字词获得字数匹配优先。且此时跳过 ranker（ranker 按分数排序会打乱层序）。
        let t = Instant::now();
        let syllable_count = spaced_code.split_whitespace().count();
        let is_fully_segmented =
            !spaced_code.is_empty() && full_code == spaced_code.replace(" ", "");
        let eff_syl_count = if has_pending {
            syllable_count + 1
        } else {
            syllable_count
        };

        sort_candidates(
            &mut candidates,
            eff_syl_count,
            is_fully_segmented || has_pending,
        );

        if !candidates.is_empty() && !is_fully_segmented && !has_pending {
            self.ranker.rank(&full_code, &mut candidates);
        }
        let sort_elapsed = t.elapsed();

        debug!(
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
        let text = if let Some(text) =
            scheme_helpers::pick_candidate_text(&candidates, self.selected_index)
        {
            self.record_user_commit(&text);
            text
        } else {
            self.codec.code().to_string()
        };
        self.reset_codec_state();
        self.last_query = None;
        text
    }

    /// 重置输入编码与 UI 状态，但不触碰补全/英文缓冲等跨合成状态。
    fn reset_codec_state(&mut self) {
        self.codec.reset();
        self.expanded = false;
        self.selected_index = 0;
    }
}

impl InputScheme for ShuangpinScheme {
    fn name(&self) -> &str {
        "小鹤双拼"
    }

    fn scheme_id(&self) -> SchemeId {
        SchemeId::Shuangpin
    }

    fn handle_key(&mut self, key: &KeyEvent, ctx: &InputContext) -> SchemeResult {
        if key.state != KeyState::Press {
            return SchemeResult::Ignored;
        }

        // 临时英文模式优先处理
        if let Some(result) =
            scheme_helpers::handle_temporary_english_key(&mut self.english_buffer, key)
        {
            return result;
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
                self.reset_codec_state();
                return SchemeResult::Committed {
                    text: String::new(),
                    temporary_english: false,
                };
            }
            "Escape" => {
                // 无活动编码时透传给应用（Linux 非合成态会把 Esc 直接送引擎，
                // 吞掉会丢失应用自身的 Esc）；有活动编码时取消输入（通知平台层
                // 结束合成并隐藏候选窗）。
                if self.codec.full_code().is_empty() {
                    return SchemeResult::Ignored;
                }
                self.reset_codec_state();
                return SchemeResult::Cancelled;
            }
            "Space" => {
                let candidates = self.current_candidates();
                let (text, temporary_english) = if let Some(text) =
                    scheme_helpers::pick_candidate_text(&candidates, self.selected_index)
                {
                    self.record_user_commit(&text);
                    (text, false)
                } else {
                    // 无候选时原样上屏编码：这是键盘输入的英文串（方案编码仅
                    // 由 ASCII 字母构成），上屏后语境必为英文。与 Enter 一样
                    // 视为临时英文结束上屏，让平台层以英文为基线锁定自动切换，
                    // 避免紧随其后的中→英自动切换把用户拉入全英文模式。
                    // 空编码上屏（仅尾部空格）不锁定。
                    let code = self.codec.code().to_string();
                    (format!("{} ", code), !code.is_empty())
                };
                self.reset_codec_state();
                self.last_query = None;
                return SchemeResult::Committed {
                    text,
                    temporary_english,
                };
            }
            "Enter" => {
                // Enter 原样上屏编码：同无候选 Space，视为临时英文结束上屏，
                // 让平台层以英文为基线锁定自动切换。空编码不锁定。
                let code = self.codec.code().to_string();
                let temporary_english = !code.is_empty();
                self.reset_codec_state();
                return SchemeResult::Committed {
                    text: code,
                    temporary_english,
                };
            }
            "Tab" => {
                // 整句上屏：校验 LLM 补全仍匹配当前编码与选中项，匹配则拼入
                let candidates = self.current_candidates();
                let Some(base) =
                    scheme_helpers::pick_candidate_text(&candidates, self.selected_index)
                else {
                    return SchemeResult::Ignored;
                };
                // 只记录选中词部分上屏词频，LLM 预测的补全部分不写入用户词典，
                // 避免模型输出污染候选排序（须在 base 被 move 进 text 前记录）
                self.record_user_commit(&base);
                let text = if let Some(hint) = &self.completion
                    && hint.matches(self.codec.code(), self.selected_index)
                {
                    format!("{}{}", base, hint.text)
                } else {
                    base
                };
                self.reset_codec_state();
                self.last_query = None;
                self.completion = None;
                return SchemeResult::Committed {
                    text,
                    temporary_english: false,
                };
            }
            "ArrowLeft" | "ArrowRight" | "ArrowDown" | "ArrowUp" => {
                let candidates = self.current_candidates();
                if let Some(result) = scheme_helpers::handle_arrow_key(
                    &key.key,
                    self.codec.code().to_string(),
                    candidates,
                    &mut self.selected_index,
                    &mut self.expanded,
                ) {
                    return result;
                }
                return SchemeResult::Ignored;
            }
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                let code = self.codec.code().to_string();
                let candidates = self.current_candidates();
                match scheme_helpers::handle_digit_key(
                    &code,
                    key,
                    &candidates,
                    self.selected_index,
                    self.expanded,
                ) {
                    scheme_helpers::DigitKeyAction::Direct(result) => return result,
                    scheme_helpers::DigitKeyAction::Select(index) => {
                        self.expanded = false;
                        return self
                            .select_candidate(index)
                            .unwrap_or(SchemeResult::Ignored);
                    }
                    scheme_helpers::DigitKeyAction::Ignore => return SchemeResult::Ignored,
                }
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
                temporary_english: false,
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
                let Some(cn) = scheme_helpers::convert_to_cn_punct(ch, &mut self.quote_pair, ctx)
                else {
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
                SchemeResult::Committed {
                    text,
                    temporary_english: false,
                }
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
        self.reset_codec_state();
        self.last_query = None;
        Some(SchemeResult::Committed {
            text,
            temporary_english: false,
        })
    }

    fn update_completion(&mut self, completion: Option<CompletionHint>) {
        self.completion = completion;
    }

    fn reset(&mut self) {
        self.codec.reset();
        self.last_query = None;
        self.expanded = false;
        self.selected_index = 0;
        self.completion = None;
        self.english_buffer = None;
        self.quote_pair.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, PinyinScheme, ShuangpinScheme};
    use black_hole_shared::{EngineCommand, InputContext, KeyEvent, KeyState, Modifiers};

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
                .map(|(code, text, weight)| RawEntry {
                    code: code.to_string(),
                    text: text.to_string(),
                    weight: Some(*weight as f32),
                })
                .collect(),
        )
        .unwrap()
    }

    /// 构造带光标前文的上下文
    fn ctx_with_preceding(text: &str) -> InputContext {
        InputContext {
            preceding_text: Some(text.to_string()),
            ..InputContext::caret(0, 0, 20)
        }
    }

    /// 构造按住 Shift 的按键事件（触发临时英文模式）
    fn shift_key_event(key: &str) -> KeyEvent {
        let mut e = key_event(key);
        e.modifiers.shift = true;
        e
    }

    #[test]
    fn test_temporary_english_commit_flags() {
        let mut scheme = ShuangpinScheme::new();
        let ctx = InputContext::caret(0, 0, 20);

        // Shift+字母 进入临时英文模式
        let r = scheme.handle_key(&shift_key_event("a"), &ctx);
        assert!(matches!(r, SchemeResult::Composing { ref code, .. } if code == "a"));

        // 继续追加字母
        let _ = scheme.handle_key(&shift_key_event("b"), &ctx);

        // Space 结束上屏：临时英文上屏（temporary_english=true，带尾随空格）
        let r = scheme.handle_key(&key_event("Space"), &ctx);
        assert!(
            matches!(r, SchemeResult::Committed { ref text, temporary_english: true } if text == "ab "),
            "临时英文 Space 上屏应标记 temporary_english=true，实际: {:?}",
            r
        );

        // 重新进入临时英文，Enter 结束上屏（不带空格）
        let _ = scheme.handle_key(&shift_key_event("c"), &ctx);
        let _ = scheme.handle_key(&shift_key_event("d"), &ctx);
        let r = scheme.handle_key(&key_event("Enter"), &ctx);
        assert!(
            matches!(r, SchemeResult::Committed { ref text, temporary_english: true } if text == "cd"),
            "临时英文 Enter 上屏应标记 temporary_english=true，实际: {:?}",
            r
        );

        // 进入临时英文后全部 Backspace 清空：上屏空串，非临时英文结束
        let _ = scheme.handle_key(&shift_key_event("e"), &ctx);
        let r = scheme.handle_key(&key_event("Backspace"), &ctx);
        assert!(
            matches!(r, SchemeResult::Committed { ref text, temporary_english: false } if text.is_empty()),
            "临时英文清空上屏应为非临时英文，实际: {:?}",
            r
        );
    }

    #[test]
    fn test_shuangpin_escape_cancels_composition() {
        let mut scheme = ShuangpinScheme::new();
        let ctx = InputContext::caret(0, 0, 20);

        // 输入编码（ni）后按 Esc：取消输入（Cancelled）
        for ch in ["n", "i"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let r = scheme.handle_key(&key_event("Escape"), &ctx);
        assert_eq!(
            r,
            SchemeResult::Cancelled,
            "有活动编码时 Esc 应取消输入，实际: {:?}",
            r
        );
        // 取消后编码已重置：再次输入 Esc 应透传（Ignored）
        assert_eq!(
            scheme.handle_key(&key_event("Escape"), &ctx),
            SchemeResult::Ignored
        );
    }

    #[test]
    fn test_shuangpin_escape_passes_through_when_no_code() {
        let mut scheme = ShuangpinScheme::new();
        let ctx = InputContext::caret(0, 0, 20);

        // 无活动编码时 Esc 应透传给应用（Ignored），避免吞掉应用自身的 Esc
        assert_eq!(
            scheme.handle_key(&key_event("Escape"), &ctx),
            SchemeResult::Ignored,
            "无活动编码时 Esc 应透传"
        );
    }

    #[test]
    fn test_shuangpin_temporary_english_escape_cancels() {
        let mut scheme = ShuangpinScheme::new();
        let ctx = InputContext::caret(0, 0, 20);

        // 临时英文模式：Shift+字母 进入后按 Esc 取消
        let _ = scheme.handle_key(&shift_key_event("a"), &ctx);
        let r = scheme.handle_key(&key_event("Escape"), &ctx);
        assert_eq!(r, SchemeResult::Cancelled, "临时英文 Esc 应取消输入");
    }

    #[test]
    fn test_shuangpin_scheme_quote_pairing() {
        let mut scheme = ShuangpinScheme::new();

        // 前文为空 → 输出左引号
        let r1 = scheme.handle_key(&key_event("\""), &InputContext::caret(0, 0, 20));
        assert!(
            matches!(r1, SchemeResult::Committed { ref text, .. } if text == "“"),
            "空前文应按引号输出左引号，实际: {:?}",
            r1
        );

        // 前文包含未闭合的左引号 → 输出右引号（配对闭合）
        let r2 = scheme.handle_key(&key_event("\""), &ctx_with_preceding("他说“你好"));
        assert!(
            matches!(r2, SchemeResult::Committed { ref text, .. } if text == "”"),
            "前文有未闭合左引号应输出右引号，实际: {:?}",
            r2
        );

        // 前文已闭合 → 再次输出左引号
        let r3 = scheme.handle_key(&key_event("\""), &ctx_with_preceding("他说“你好”"));
        assert!(
            matches!(r3, SchemeResult::Committed { ref text, .. } if text == "“"),
            "前文已闭合应输出左引号，实际: {:?}",
            r3
        );
    }

    #[test]
    fn test_shuangpin_scheme_single_quote_pairing() {
        let mut scheme = ShuangpinScheme::new();
        let r1 = scheme.handle_key(&key_event("'"), &InputContext::caret(0, 0, 20));
        assert!(matches!(r1, SchemeResult::Committed { ref text, .. } if text == "‘"));
        let r2 = scheme.handle_key(&key_event("'"), &ctx_with_preceding("他说‘你好"));
        assert!(matches!(r2, SchemeResult::Committed { ref text, .. } if text == "’"));
    }

    #[test]
    fn test_shuangpin_le_not_leng() {
        // 模拟 RIME 词库：le -> 了，leng -> 冷
        let dict = build_dict(&[("le", "了", 100), ("leng", "冷", 200)]);

        let mut scheme = ShuangpinScheme::with_dictionary(Box::new(dict));
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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
        let dict_path = Path::new("../../temp/dicts/rime_ice.dict.yaml");
        if !dict_path.exists() {
            println!("跳过测试：实际词库文件不存在");
            return;
        }

        let dict = RimeDict::from_rime_dict_cached(dict_path, env::temp_dir()).unwrap();
        let mut scheme = ShuangpinScheme::with_dictionary(Box::new(dict));
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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
        let dict_path = Path::new("../../temp/dicts/rime_ice.dict.yaml");
        if !dict_path.exists() {
            println!("跳过测试：实际词库文件不存在");
            return;
        }

        let ctx = InputContext::caret(0, 0, 20);

        // 手动加载外部词典，分别构建拼音和双拼引擎（避免用户词典干扰）
        let cache_dir = env::temp_dir();
        let pinyin_dict = Arc::new(
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
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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

    #[test]
    fn test_raw_code_commit_flags_as_temporary_english() {
        // 空词典：任何编码都无候选
        let dict = build_dict(&[]);
        let mut scheme = ShuangpinScheme::with_dictionary(Box::new(dict));
        let ctx = InputContext::caret(0, 0, 20);

        // 无候选 Space：原样上屏编码，视为临时英文性质上屏
        for ch in ["u", "u"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let r = scheme.handle_key(&key_event("Space"), &ctx);
        assert!(
            matches!(r, SchemeResult::Committed { ref text, temporary_english: true } if text == "uu "),
            "无候选 Space 原样上屏编码应标记 temporary_english=true，实际: {:?}",
            r
        );

        // Enter：原样上屏编码，同样视为临时英文性质上屏
        for ch in ["a", "a"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let r = scheme.handle_key(&key_event("Enter"), &ctx);
        assert!(
            matches!(r, SchemeResult::Committed { ref text, temporary_english: true } if text == "aa"),
            "Enter 原样上屏编码应标记 temporary_english=true，实际: {:?}",
            r
        );

        // 空编码按 Space：仅上屏空格，不视为临时英文
        let r = scheme.handle_key(&key_event("Space"), &ctx);
        assert!(
            matches!(r, SchemeResult::Committed { ref text, temporary_english: false } if text == " "),
            "空编码 Space 上屏不应标记 temporary_english，实际: {:?}",
            r
        );
    }

    #[test]
    fn test_shuangpin_tab_commits_with_completion() {
        let dict = build_dict(&[("shu", "书", 200)]);
        let mut scheme = ShuangpinScheme::with_dictionary(Box::new(dict));
        let ctx = InputContext::caret(0, 0, 20);

        // 输入 uu -> shu，首选应为 "书"
        for ch in ["u", "u"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        // 先给一个不匹配的编码（错误编码），Tab 应回退为仅选中词
        scheme.update_completion(Some(CompletionHint {
            code: "yy".to_string(),
            selected_index: 0,
            text: "本".to_string(),
        }));
        let result = scheme.handle_key(&key_event("Tab"), &ctx);
        assert_eq!(
            result,
            SchemeResult::Committed {
                text: "书".to_string(),
                temporary_english: false,
            },
            "编码不匹配时 Tab 应回退为仅提交选中词"
        );

        // 重新输入 uu，给正确编码的补全：uu + 首选索引 0，补全 "本"
        for ch in ["u", "u"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        scheme.update_completion(Some(CompletionHint {
            code: "uu".to_string(),
            selected_index: 0,
            text: "本".to_string(),
        }));
        let result = scheme.handle_key(&key_event("Tab"), &ctx);
        assert_eq!(
            result,
            SchemeResult::Committed {
                text: "书本".to_string(),
                temporary_english: false,
            },
            "编码匹配时 Tab 应将选中词与补全拼为整句上屏"
        );
    }
}
