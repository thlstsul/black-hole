#[cfg(test)]
use crate::RawEntry;
use crate::punctuation::QuotePair;
use crate::scheme_helpers;
use crate::{
    Codec, CodecState, Dictionary, GraphDecoder, InputScheme, LanguageModel, PinyinCodec, RimeDict,
    UserDictionary, global_user_dict, sort_candidates,
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
use tracing::debug;

/// 拼音输入方案
pub struct PinyinScheme {
    codec: PinyinCodec,
    dictionary: Box<dyn Dictionary>,
    lm: LanguageModel,
    user_dict: Option<Arc<Mutex<UserDictionary>>>,
    /// 缓存用户词频（避免每次查询 SQLite）
    user_freq_cache: FxHashMap<String, i64>,
    /// 优化：缓存最近一次查询结果，避免重复查询
    last_query: Option<(String, Vec<Candidate>)>,
    /// 输入版本号，每次编码变化时递增，不变时跳过重复计算
    input_version: u64,
    /// 已缓存候选对应的版本号
    cached_version: u64,
    /// 候选窗是否展开为完整列表
    expanded: bool,
    /// 当前选中的候选索引
    selected_index: usize,
    /// LLM 整句补全结果（异步到达，Tab 提交时校验后拼入上屏文本）
    completion: Option<CompletionHint>,
    /// 临时英文输入缓冲（大写字母开头时进入）
    english_buffer: Option<String>,
    /// 中文引号配对状态（' 与 " 交替输出左右引号）
    quote_pair: QuotePair,
}

impl Default for PinyinScheme {
    fn default() -> Self {
        Self::new()
    }
}

impl PinyinScheme {
    pub fn new() -> Self {
        Self::with_dictionary(Arc::new(RimeDict::from_builtin()))
    }

    pub fn with_dictionary(dict: Arc<RimeDict>) -> Self {
        let lm = dict.build_language_model();
        Self {
            codec: PinyinCodec::new(),
            dictionary: Box::new(dict),
            lm,
            user_dict: None,
            user_freq_cache: FxHashMap::default(),
            last_query: None,
            input_version: 0,
            cached_version: 0,
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
            SchemeId::Pinyin,
            &mut self.user_freq_cache,
        );
    }

    fn current_candidates(&mut self) -> Vec<Candidate> {
        debug!(
            "current_candidates start: code='{}'",
            self.codec.full_code()
        );

        if self.user_freq_cache.len() > 500 {
            self.user_freq_cache.clear();
        }

        // 版本号未变（编码未变），直接返回上一轮缓存结果
        // 方向键导航、数字选词等操作不改变编码，可跳过整条流水线
        if self.input_version == self.cached_version
            && let Some((_, cached_candidates)) = &self.last_query
        {
            return cached_candidates.clone();
        }

        let full_code = self.codec.full_code();
        let spaced_code = self.codec.spaced_code();
        let abbreviated = self.codec.abbreviated_code();

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen_texts = FxHashSet::default();
        let mut acc = scheme_helpers::CandidateSet::new(&mut candidates, &mut seen_texts);

        let graph = self.codec.syllable_graph();
        if graph.total_len() > 0 {
            let decoder = GraphDecoder::new(&*self.dictionary)
                .with_lm(&self.lm)
                .with_user_freqs(&self.user_freq_cache);
            let decode_results = decoder.decode(&graph);
            debug!(
                "pinyin decode_results: count={}, top5={:?}",
                decode_results.len(),
                decode_results
                    .iter()
                    .take(5)
                    .map(|r| (&r.text, r.score, r.is_partial))
                    .collect::<Vec<_>>()
            );

            for result in decode_results {
                let comment = if result.is_partial {
                    Some("组合".to_string())
                } else {
                    Some("整句".to_string())
                };
                acc.push(result.text, (result.score * 100.0) as i64, comment);
            }
        }

        let prefix_query = if !spaced_code.is_empty() {
            &spaced_code
        } else {
            &full_code
        };
        if !prefix_query.is_empty() {
            for cand in self.dictionary.prefix_lookup(prefix_query) {
                acc.push_or_replace(cand.text, cand.score, Some(prefix_query.clone()));
            }
        }

        if !abbreviated.is_empty() && abbreviated.len() >= 2 {
            for cand in self.dictionary.prefix_lookup(&abbreviated) {
                acc.push_or_replace(
                    cand.text,
                    cand.score / 2,
                    Some(format!("简拼 {}", abbreviated)),
                );
            }
        }

        if let Some(ref ud) = self.user_dict_ref() {
            let user_cands = ud.lock().unwrap().lookup(SchemeId::Pinyin, &spaced_code);
            for cand in user_cands {
                self.user_freq_cache.insert(cand.text.clone(), cand.score);
                let boost = scheme_helpers::user_boost(cand.score);
                acc.push_or_boost(cand.text, boost, Some("用户".to_string()));
            }
        }

        // 当输入恰好被完整切分为音节时，优先将字数等于音节数的候选排在前面
        let syllable_count = spaced_code.split_whitespace().count();
        let is_fully_segmented =
            !spaced_code.is_empty() && full_code == spaced_code.replace(" ", "");

        // 排序：按来源分层，用户词 > 整句精确匹配 > 组合 > 前缀匹配 > 简拼匹配
        // 输入完整切分时，字数等于音节数的候选额外优先
        sort_candidates(&mut candidates, syllable_count, is_fully_segmented);

        self.last_query = Some((full_code.clone(), candidates.clone()));
        self.cached_version = self.input_version;

        debug!(
            "current_candidates end: code='{}', candidates={}",
            full_code,
            candidates.len()
        );

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
            self.codec.full_code()
        };
        self.reset_codec_state();
        self.last_query = None;
        text
    }

    /// 重置输入编码与 UI 状态，但不触碰补全/英文缓冲等跨合成状态。
    fn reset_codec_state(&mut self) {
        self.codec.reset();
        self.input_version += 1;
        self.expanded = false;
        self.selected_index = 0;
    }
}

impl InputScheme for PinyinScheme {
    fn name(&self) -> &str {
        "拼音"
    }

    fn scheme_id(&self) -> SchemeId {
        SchemeId::Pinyin
    }

    fn handle_key(&mut self, key: &KeyEvent, ctx: &InputContext) -> SchemeResult {
        debug!("handle_key start: key='{}'", key.key);
        if key.state != KeyState::Press {
            return SchemeResult::Ignored;
        }

        // 临时英文模式优先处理
        if let Some(result) =
            scheme_helpers::handle_temporary_english_key(&mut self.english_buffer, key)
        {
            return result;
        }

        let result = match key.key.as_str() {
            "Backspace" => {
                if !self.codec.pop() {
                    self.reset_codec_state();
                    return SchemeResult::Committed {
                        text: String::new(),
                        temporary_english: false,
                    };
                }
                self.input_version += 1;
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
                // 无活动编码时透传给应用（Linux 非合成态会把 Esc 直接送引擎，
                // 吞掉会丢失应用自身的 Esc）；有活动编码时取消输入（通知平台层
                // 结束合成并隐藏候选窗）。Windows 侧 TSF 仅在合成中拦截 Esc，
                // 故此分支在 Windows 必为有编码态。
                if self.codec.full_code().is_empty() {
                    SchemeResult::Ignored
                } else {
                    self.reset_codec_state();
                    self.last_query = None;
                    SchemeResult::Cancelled
                }
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
                    let code = self.codec.full_code();
                    (format!("{} ", code), !code.is_empty())
                };
                self.reset_codec_state();
                self.last_query = None;
                SchemeResult::Committed {
                    text,
                    temporary_english,
                }
            }
            "Enter" => {
                // Enter 原样上屏编码：同无候选 Space，视为临时英文结束上屏，
                // 让平台层以英文为基线锁定自动切换。空编码不锁定。
                let code = self.codec.full_code();
                let temporary_english = !code.is_empty();
                self.reset_codec_state();
                SchemeResult::Committed {
                    text: code,
                    temporary_english,
                }
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
                    && hint.matches(&self.codec.full_code(), self.selected_index)
                {
                    format!("{}{}", base, hint.text)
                } else {
                    base
                };
                self.reset_codec_state();
                self.last_query = None;
                self.completion = None;
                SchemeResult::Committed {
                    text,
                    temporary_english: false,
                }
            }
            "ArrowLeft" | "ArrowRight" | "ArrowDown" | "ArrowUp" => {
                let candidates = self.current_candidates();
                if let Some(result) = scheme_helpers::handle_arrow_key(
                    &key.key,
                    self.codec.full_code(),
                    candidates,
                    &mut self.selected_index,
                    &mut self.expanded,
                ) {
                    return result;
                }
                SchemeResult::Ignored
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
                    scheme_helpers::DigitKeyAction::Ignore => SchemeResult::Ignored,
                }
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
                        self.input_version += 1;
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
                        let Some(cn) =
                            scheme_helpers::convert_to_cn_punct(ch, &mut self.quote_pair, ctx)
                        else {
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
                        SchemeResult::Committed {
                            text,
                            temporary_english: false,
                        }
                    }
                }
            }
        };

        debug!(
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
        self.input_version += 1;
        self.last_query = None; // 清除缓存
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
    fn build_dict(entries: &[(&str, &str, i64)]) -> Arc<RimeDict> {
        Arc::new(
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
        let mut scheme = PinyinScheme::new();
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
    fn test_raw_code_commit_flags_as_temporary_english() {
        // 空词典：任何编码都无候选
        let dict = build_dict(&[]);
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext::caret(0, 0, 20);

        // 无候选 Space：原样上屏编码，视为临时英文性质上屏
        for ch in ["n", "h"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let r = scheme.handle_key(&key_event("Space"), &ctx);
        assert!(
            matches!(r, SchemeResult::Committed { ref text, temporary_english: true } if text == "nh "),
            "无候选 Space 原样上屏编码应标记 temporary_english=true，实际: {:?}",
            r
        );

        // Enter：原样上屏编码，同样视为临时英文性质上屏
        for ch in ["o", "k"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let r = scheme.handle_key(&key_event("Enter"), &ctx);
        assert!(
            matches!(r, SchemeResult::Committed { ref text, temporary_english: true } if text == "ok"),
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
    fn test_pinyin_escape_cancels_composition() {
        let mut scheme = PinyinScheme::new();
        let ctx = InputContext::caret(0, 0, 20);

        // 输入编码后按 Esc：取消输入（Cancelled），平台层据此结束合成并隐藏候选窗
        for ch in ["z", "h", "o", "n", "g"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let r = scheme.handle_key(&key_event("Escape"), &ctx);
        assert_eq!(
            r,
            SchemeResult::Cancelled,
            "有活动编码时 Esc 应取消输入，实际: {:?}",
            r
        );
        // 取消后编码已重置：再次输入 Esc 应透传（Ignored），不吞应用自身的 Esc
        assert_eq!(
            scheme.handle_key(&key_event("Escape"), &ctx),
            SchemeResult::Ignored
        );
    }

    #[test]
    fn test_pinyin_escape_passes_through_when_no_code() {
        let mut scheme = PinyinScheme::new();
        let ctx = InputContext::caret(0, 0, 20);

        // 无活动编码时 Esc 应透传给应用（Ignored），避免吞掉应用自身的 Esc
        assert_eq!(
            scheme.handle_key(&key_event("Escape"), &ctx),
            SchemeResult::Ignored,
            "无活动编码时 Esc 应透传，避免吞掉应用自身的 Esc"
        );
    }

    #[test]
    fn test_pinyin_temporary_english_escape_cancels() {
        let mut scheme = PinyinScheme::new();
        let ctx = InputContext::caret(0, 0, 20);

        // 临时英文模式：Shift+字母 进入后按 Esc 取消
        let _ = scheme.handle_key(&shift_key_event("a"), &ctx);
        let _ = scheme.handle_key(&shift_key_event("b"), &ctx);
        let r = scheme.handle_key(&key_event("Escape"), &ctx);
        assert_eq!(r, SchemeResult::Cancelled, "临时英文 Esc 应取消输入");
        // 已退出临时英文模式：后续普通字母按拼音编码处理（非英文缓冲）
        let r = scheme.handle_key(&shift_key_event("c"), &ctx);
        assert!(
            matches!(r, SchemeResult::Composing { ref code, .. } if code == "c"),
            "取消临时英文后 Shift+字母 应重新进入临时英文，实际: {:?}",
            r
        );
    }

    #[test]
    fn test_pinyin_scheme_quote_pairing() {
        let mut scheme = PinyinScheme::new();

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
    fn test_pinyin_scheme_single_quote_pairing() {
        let mut scheme = PinyinScheme::new();
        let r1 = scheme.handle_key(&key_event("'"), &InputContext::caret(0, 0, 20));
        assert!(matches!(r1, SchemeResult::Committed { ref text, .. } if text == "‘"));
        let r2 = scheme.handle_key(&key_event("'"), &ctx_with_preceding("他说‘你好"));
        assert!(matches!(r2, SchemeResult::Committed { ref text, .. } if text == "’"));
    }

    #[test]
    fn test_pinyin_scheme_spaced_code_lookup() {
        let dict = build_dict_with_spaced_code();
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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
        let dict_path = Path::new("../../temp/dicts/rime_ice.dict.yaml");
        if !dict_path.exists() {
            println!("跳过测试：实际词库文件不存在");
            return;
        }

        let dict = Arc::new(RimeDict::from_rime_dict_cached(dict_path, env::temp_dir()).unwrap());
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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
        let ctx = InputContext::caret(0, 0, 20);

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

    #[test]
    fn test_tab_commits_with_completion() {
        let dict = build_dict(&[("zhong guo", "中国", 100), ("ren min", "人民", 100)]);
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext::caret(0, 0, 20);

        // 输入 "zhongguo"，首选应为 "中国"
        let composing = {
            for ch in ["z", "h", "o", "n", "g", "g", "u"] {
                let _ = scheme.handle_key(&key_event(ch), &ctx);
            }
            scheme.handle_key(&key_event("o"), &ctx)
        };
        let first = match &composing {
            SchemeResult::Composing { candidates, .. } if !candidates.is_empty() => {
                candidates[0].text.clone()
            }
            other => panic!("输入 zhongguo 后应处于 Composing 状态，实际: {:?}", other),
        };
        assert_eq!(first, "中国");

        // LLM 补全异步到达：编码 zhongguo、首选索引 0，补全 "人民"
        scheme.update_completion(Some(CompletionHint {
            code: "zhongguo".to_string(),
            selected_index: 0,
            text: "人民".to_string(),
        }));

        // 按 Tab：选中词 + 补全整句上屏
        let result = scheme.handle_key(&key_event("Tab"), &ctx);
        assert_eq!(
            result,
            SchemeResult::Committed {
                text: "中国人民".to_string(),
                temporary_english: false,
            },
            "Tab 应将选中词与补全拼为整句上屏"
        );
    }

    #[test]
    fn test_tab_falls_back_without_completion() {
        let dict = build_dict(&[("zhong guo", "中国", 100)]);
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext::caret(0, 0, 20);

        for ch in ["z", "h", "o", "n", "g", "g", "u", "o"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        // 无补全时 Tab 与 Space 行为一致：仅提交选中词
        let result = scheme.handle_key(&key_event("Tab"), &ctx);
        assert_eq!(
            result,
            SchemeResult::Committed {
                text: "中国".to_string(),
                temporary_english: false,
            }
        );
    }

    #[test]
    fn test_tab_ignores_stale_completion() {
        let dict = build_dict(&[("zhong guo", "中国", 100)]);
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext::caret(0, 0, 20);

        for ch in ["z", "h", "o", "n", "g", "g", "u", "o"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }

        // 补全对应的编码与当前编码不一致（输入已变化），应视为过期丢弃
        scheme.update_completion(Some(CompletionHint {
            code: "zhong".to_string(),
            selected_index: 0,
            text: "国人".to_string(),
        }));

        let result = scheme.handle_key(&key_event("Tab"), &ctx);
        assert_eq!(
            result,
            SchemeResult::Committed {
                text: "中国".to_string(),
                temporary_english: false,
            },
            "过期补全不应拼入上屏文本"
        );
    }

    #[test]
    fn test_tab_no_candidates_ignored() {
        let dict = build_dict(&[("zhong guo", "中国", 100)]);
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext::caret(0, 0, 20);

        // 未输入任何编码时按 Tab 应 Ignored（不吞掉应用的 Tab 焦点切换）
        assert_eq!(
            scheme.handle_key(&key_event("Tab"), &ctx),
            SchemeResult::Ignored
        );
    }

    #[test]
    fn test_reset_clears_completion() {
        let dict = build_dict(&[("zhong guo", "中国", 100)]);
        let mut scheme = PinyinScheme::with_dictionary(dict);
        let ctx = InputContext::caret(0, 0, 20);

        for ch in ["z", "h", "o", "n", "g", "g", "u", "o"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        scheme.update_completion(Some(CompletionHint {
            code: "zhongguo".to_string(),
            selected_index: 0,
            text: "人民".to_string(),
        }));
        scheme.reset();

        // reset 清空编码与补全；重新输入同样的编码后按 Tab，
        // 补全已被清空，应只提交选中词"中国"
        for ch in ["z", "h", "o", "n", "g", "g", "u", "o"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let result = scheme.handle_key(&key_event("Tab"), &ctx);
        assert_eq!(
            result,
            SchemeResult::Committed {
                text: "中国".to_string(),
                temporary_english: false,
            },
            "reset 后补全应被清空"
        );
    }
}
