#[cfg(test)]
use crate::RawEntry;
#[cfg(not(test))]
use crate::default_user_dict_dir;
use crate::punctuation::QuotePair;
use crate::scheme_helpers;
use crate::{
    CandidateRanker, Codec, CodecState, DecodeResult, Dictionary, GraphDecoder, InputScheme,
    LanguageModel, RimeDict, ShuangpinCodec, SimpleRanker, SyllableGraph, UserDictionary,
    global_user_dict, sort_candidates,
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
    /// 语言模型（从词典构建；音节图为全拼，与拼音方案共用同一 LM 评分逻辑）
    lm: LanguageModel,
    user_dict: Option<Arc<Mutex<UserDictionary>>>,
    /// 缓存用户词频（避免每次查询 SQLite）
    user_freq_cache: FxHashMap<String, i64>,
    /// 缓存最近一次查询结果（导航期间复用，保证候选顺序稳定）
    last_query: Option<(String, Vec<Candidate>)>,
    /// 与 last_query 同步缓存的解码产物（text -> words/word_codes/is_partial），
    /// 上屏时按选中文本反查候选来源（整句 vs 词级），供逐词学习使用
    last_decoded: FxHashMap<String, DecodeResult>,
    /// 输入状态版本号：编码变化/重置时递增，用于识别 last_decoded 是否陈旧
    input_version: u64,
    /// last_decoded 写入时的版本号（与 input_version 一致才可信任）
    cached_version: u64,
    /// 上一轮上屏的末词（跨句 Bigram 上下文）：Escape/Reset 不清空，方案切换时清空
    prev_commit: Option<String>,
    /// Bigram 落盘防抖计时
    last_bigram_save: Option<std::time::Instant>,
    /// 个人 Bigram 持久化路径（None 表示不落盘）
    bigram_path: Option<std::path::PathBuf>,
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
        // 与拼音方案保持一致：构建语言模型并接入个人 Bigram（含落盘）
        Self::with_dictionary(Box::new(RimeDict::from_builtin()))
    }

    pub fn with_dictionary(dictionary: Box<dyn Dictionary>) -> Self {
        // 个人 Bigram 持久化路径：与拼音方案共用用户目录下的同一文件
        // （音节图均为全拼，词对语义一致，两方案共享学习成果）；
        // 测试构建指向每次调用唯一且隔离的临时路径，避免读写真实用户数据，
        // 也避免同进程内并行测试共用文件导致计数互相污染
        #[cfg(test)]
        let bigram_path = scheme_helpers::test_bigram_path("shuangpin");
        #[cfg(not(test))]
        let bigram_path = default_user_dict_dir().join("user_bigram.txt");
        Self::with_dictionary_and_bigram(dictionary, bigram_path)
    }

    /// 指定个人 Bigram 路径的构造（测试用隔离落盘文件，避免并行测试共用文件
    /// 导致计数互相污染；生产路径由 `with_dictionary` 传入用户目录路径）
    pub(crate) fn with_dictionary_and_bigram(
        dictionary: Box<dyn Dictionary>,
        bigram_path: std::path::PathBuf,
    ) -> Self {
        let lm = dictionary.build_language_model();
        let mut scheme = Self {
            codec: ShuangpinCodec::new(),
            lm,
            dictionary,
            ranker: Box::new(SimpleRanker::new()),
            user_dict: None,
            user_freq_cache: FxHashMap::default(),
            last_query: None,
            last_decoded: FxHashMap::default(),
            input_version: 0,
            cached_version: 0,
            prev_commit: None,
            last_bigram_save: None,
            bigram_path: Some(bigram_path.clone()),
            expanded: false,
            selected_index: 0,
            completion: None,
            english_buffer: None,
            quote_pair: QuotePair::new(),
        };
        scheme.lm.load_user_bigram(&bigram_path);
        scheme
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

    /// 上屏学习路由（与拼音方案同构）：整句/混合候选按解码产物逐词学习，
    /// 词级候选仍走精确匹配学习。仅当候选缓存与当前输入一致时信任解码产物。
    ///
    /// Bigram 观测：词级候选只观测「上轮末词 → 本词」一次，记 times=2（直接点选，强信号）；
    /// 整句/混合候选观测路径内所有相邻词对，统一记 times=1（整句上屏本身即一次选择）。
    /// 音节图已全拼，word_codes 可直接学习，无须反查。
    fn learn_commit(&mut self, text: &str) {
        let decoded = if self.input_version == self.cached_version {
            self.last_decoded.get(text).cloned()
        } else {
            None
        };
        match decoded {
            Some(decoded) if decoded.is_partial || decoded.words.len() > 1 => {
                scheme_helpers::record_sentence_commit(
                    &decoded.words,
                    &decoded.word_codes,
                    self.user_dict_ref(),
                    SchemeId::Shuangpin,
                    &mut self.user_freq_cache,
                );
                // 句内相邻词对观测（顺带转移 times=1），并跨句连接上轮末词；
                // 混合结果的拼音占位段编码为空，不是真实词，跳过以免污染 Bigram
                let learned = scheme_helpers::learned_words(&decoded.words, &decoded.word_codes);
                self.observe_sentence_bigrams(&learned, 1);
            }
            _ => {
                self.record_user_commit(text);
                self.observe_sentence_bigrams(&[text], 2);
            }
        }
        self.maybe_save_bigram();
    }

    /// 对一段词序列做 Bigram 观测：先句首 (<s> → 首词)，再句内相邻词对；
    /// 上轮上屏的末词（prev_commit）与本轮首词跨句连接一次。
    /// 观测后更新 prev_commit 为本轮末词；空序列不改动上下文。
    fn observe_sentence_bigrams(&mut self, words: &[&str], times: u32) {
        if words.is_empty() {
            return;
        }
        let mut prev: Option<String> = self.prev_commit.take();
        for word in words {
            self.lm
                .observe_user_transition(prev.as_deref(), word, times);
            prev = Some((*word).to_string());
        }
        self.prev_commit = prev;
    }

    /// Bigram 落盘防抖：间隔到期才写盘（与用户词典共用 SAVE_INTERVAL 节奏）
    fn maybe_save_bigram(&mut self) {
        if self.bigram_path.is_none() {
            return;
        }
        let now = std::time::Instant::now();
        if self
            .last_bigram_save
            .is_none_or(|t| now.duration_since(t) >= crate::user_dict::SAVE_INTERVAL)
        {
            self.flush_bigram();
        }
    }

    /// 立即落盘个人 Bigram（绕过防抖），刷新防抖计时
    fn flush_bigram(&mut self) {
        if let Some(path) = &self.bigram_path
            && let Err(e) = self.lm.save_user_bigram(path)
        {
            debug!("save user bigram failed: {}", e);
        }
        self.last_bigram_save = Some(std::time::Instant::now());
    }

    /// 解码期用户词频预取：对音节图内词长 1..=MAX_WORD_SYLLABLES 的连续音节子编码
    /// 查询用户词典，使句中位置的词（非首词）也获得 user_bonus（与拼音方案同构）
    fn prefetch_user_freqs(&mut self, graph: &SyllableGraph) {
        let Some(user_dict) = self.user_dict_ref() else {
            return;
        };
        let sub_codes = scheme_helpers::collect_sub_codes(graph);
        // 全程只加锁一次：子编码数可达数十个，逐编码加锁会反复争用互斥量
        let user_dict = user_dict.lock().unwrap();
        for code in sub_codes {
            for cand in user_dict.lookup(SchemeId::Shuangpin, &code) {
                self.user_freq_cache.entry(cand.text).or_insert(cand.score);
            }
        }
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
        let mut decoded_map: FxHashMap<String, DecodeResult> = FxHashMap::default();
        if graph.total_len() > 0 {
            // 解码前预取用户词频：句中子编码命中的用户词也能获得 user_bonus
            self.prefetch_user_freqs(&graph);
            let decoder = GraphDecoder::new(&*self.dictionary)
                .with_lm(&self.lm)
                .with_user_freqs(&self.user_freq_cache);
            let decode_results = decoder.decode(&graph);
            for result in decode_results {
                // 缓存解码产物：上屏时按选中文本反查候选来源（整句 vs 词级）
                decoded_map.insert(result.text.clone(), result.clone());
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
        self.last_decoded = decoded_map;
        self.cached_version = self.input_version;
        candidates
    }

    /// 提交当前编码：优先返回当前选中的候选词，否则返回编码本身。
    fn commit_current_input(&mut self) -> String {
        let candidates = self.current_candidates();
        let text = if let Some(text) =
            scheme_helpers::pick_candidate_text(&candidates, self.selected_index)
        {
            self.learn_commit(&text);
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
        self.last_decoded.clear();
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
                    self.input_version += 1;
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
                    self.learn_commit(&text);
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
                self.learn_commit(&base);
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
                self.input_version += 1;
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
        self.learn_commit(&text);
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

    /// 方案被切走 / 引擎退出前立即落盘：绕过防抖写个人 Bigram，
    /// 并刷写用户词典，避免防抖窗口内的学习成果丢失
    fn flush_pending(&mut self) {
        self.flush_bigram();
        if let Some(user_dict) = self.user_dict_ref() {
            user_dict.lock().unwrap().flush();
        }
    }

    fn reset(&mut self) {
        self.codec.reset();
        self.input_version += 1;
        self.last_query = None;
        self.last_decoded.clear();
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

    /// 双拼整句解码评分生效：LM 接线后整句路径可用（输入 ul go → yu gou 之类
    /// 全拼音节图可解码），且 last_decoded 缓存路由学习
    #[test]
    fn test_shuangpin_lm_decode_and_sentence_learn() {
        // 全拼词条：shu(书/输) fa(法/发) -> shufa 组合候选
        let dict = build_dict(&[("shu fa", "书法", 120), ("shu", "书", 80), ("fa", "法", 60)]);
        let user_dict = Arc::new(Mutex::new(UserDictionary::open_in_memory()));
        let mut scheme =
            ShuangpinScheme::with_dictionary(Box::new(dict)).with_user_dict(user_dict.clone());
        let ctx = InputContext::caret(0, 0, 20);

        // 小鹤双拼 "uu" = shu，"fa" = 声母 f + 韵母 a 键
        for ch in ["u", "u", "f", "a"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let candidates = scheme.current_candidates();
        let idx = candidates
            .iter()
            .position(|c| c.text == "书法")
            .expect("双拼整句解码应产出 书法 候选");
        let _ = scheme.select_candidate(idx);

        // 整句词（两音节）应写入用户词典
        let learned = user_dict
            .lock()
            .unwrap()
            .lookup(SchemeId::Shuangpin, "shu fa");
        assert!(
            learned.iter().any(|c| c.text == "书法"),
            "双拼整句上屏应学到 书法，实际: {:?}",
            learned
        );
    }

    /// 双拼跨句 Bigram：两轮上屏产生 (前句末词 → 后句首词) 观测
    #[test]
    fn test_shuangpin_cross_sentence_bigram() {
        let dict = build_dict(&[("shu", "书", 80), ("fa", "法", 60)]);
        let mut scheme = ShuangpinScheme::with_dictionary(Box::new(dict));
        let ctx = InputContext::caret(0, 0, 20);

        // 第一轮上屏 "书"（uu = shu）
        for ch in ["u", "u"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let _ = scheme.handle_key(&key_event("Space"), &ctx);

        // 第二轮上屏 "法"（ff = fa）
        for ch in ["f", "f"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let _ = scheme.handle_key(&key_event("Space"), &ctx);

        // (书 → 法) 应被观测：对比未观测的空参照模型
        let reference = LanguageModel::new();
        assert!(
            scheme.lm.score_transition("书", "法", 1) > reference.score_transition("书", "法", 1),
            "双拼跨句转移 (书→法) 应被观测并提升评分"
        );
    }

    /// 双拼已学词命中"用户"层：整句学习后单独输入该编码置顶
    #[test]
    fn test_shuangpin_learned_word_hits_user_layer() {
        let dict = build_dict(&[("shu fa", "书法", 120), ("shu", "书", 80), ("fa", "法", 60)]);
        let user_dict = Arc::new(Mutex::new(UserDictionary::open_in_memory()));
        let mut scheme =
            ShuangpinScheme::with_dictionary(Box::new(dict)).with_user_dict(user_dict.clone());
        let ctx = InputContext::caret(0, 0, 20);

        // 先整句上屏 书法
        for ch in ["u", "u", "f", "a"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let candidates = scheme.current_candidates();
        let idx = candidates
            .iter()
            .position(|c| c.text == "书法")
            .expect("应存在 书法 候选");
        let _ = scheme.select_candidate(idx);

        // 重新输入 uuff：学过的 书法 应命中"用户"层置顶
        for ch in ["u", "u", "f", "a"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let candidates = scheme.current_candidates();
        assert!(
            !candidates.is_empty()
                && candidates[0].text == "书法"
                && candidates[0].comment.as_deref() == Some("用户"),
            "双拼已学词应命中用户层置顶，实际: {:?}",
            candidates.first().map(|c| (&c.text, &c.comment))
        );
    }

    /// 双拼 Escape 取消后跨句上下文保留
    #[test]
    fn test_shuangpin_escape_keeps_cross_sentence_context() {
        let dict = build_dict(&[("shu", "书", 80), ("fa", "法", 60)]);
        let mut scheme = ShuangpinScheme::with_dictionary(Box::new(dict));
        let ctx = InputContext::caret(0, 0, 20);

        // 第一轮上屏 "书"
        for ch in ["u", "u"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let _ = scheme.handle_key(&key_event("Space"), &ctx);

        // 输入一半后 Escape 取消（prev_commit 应保留）
        for ch in ["a", "a"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let _ = scheme.handle_key(&key_event("Escape"), &ctx);

        // 第二轮上屏 "法"
        for ch in ["f", "f"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let _ = scheme.handle_key(&key_event("Space"), &ctx);

        let reference = LanguageModel::new();
        assert!(
            scheme.lm.score_transition("书", "法", 1) > reference.score_transition("书", "法", 1),
            "Escape 取消后双拼跨句上下文应保留"
        );
    }

    /// 双拼预取：已学词在重新输入时进入 user_freq_cache（解码期加成）
    #[test]
    fn test_shuangpin_prefetch_user_freqs() {
        let dict = build_dict(&[("shu fa", "书法", 120), ("shu", "书", 80), ("fa", "法", 60)]);
        let user_dict = Arc::new(Mutex::new(UserDictionary::open_in_memory()));
        let mut scheme =
            ShuangpinScheme::with_dictionary(Box::new(dict)).with_user_dict(user_dict.clone());
        let ctx = InputContext::caret(0, 0, 20);

        // 整句上屏一次，学到 (shu fa, 书法)
        for ch in ["u", "u", "f", "a"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let candidates = scheme.current_candidates();
        let idx = candidates
            .iter()
            .position(|c| c.text == "书法")
            .expect("应存在 书法 候选");
        let _ = scheme.select_candidate(idx);

        // 重新输入：预取应把 书法 填入 user_freq_cache
        for ch in ["u", "u", "f", "a"] {
            let _ = scheme.handle_key(&key_event(ch), &ctx);
        }
        let _ = scheme.current_candidates();
        assert!(
            scheme.user_freq_cache.contains_key("书法"),
            "双拼预取应把已学词写入 user_freq_cache，实际: {:?}",
            scheme.user_freq_cache
        );
    }

    /// 双拼 Bigram 持久化：落盘后新实例恢复（发现 1 回归测试）
    #[test]
    fn test_shuangpin_bigram_persists_across_restart() {
        let dict = build_dict(&[("shu", "书", 80), ("fa", "法", 60)]);
        let bigram_path = std::env::temp_dir().join("bh_test_bigram_sp_restart/user_bigram.txt");
        let _ = std::fs::remove_file(&bigram_path);

        // 第一个实例：上屏 书 + 法，产生跨句观测并显式落盘（防抖窗口内不自动写）
        {
            let mut scheme = ShuangpinScheme::with_dictionary_and_bigram(
                Box::new(build_dict(&[("shu", "书", 80), ("fa", "法", 60)])),
                bigram_path.clone(),
            );
            let ctx = InputContext::caret(0, 0, 20);
            for ch in ["u", "u"] {
                let _ = scheme.handle_key(&key_event(ch), &ctx);
            }
            let _ = scheme.handle_key(&key_event("Space"), &ctx);
            for ch in ["f", "a"] {
                let _ = scheme.handle_key(&key_event(ch), &ctx);
            }
            let _ = scheme.handle_key(&key_event("Space"), &ctx);
            assert!(scheme.lm.has_user_data(), "上屏后应有个人数据");
            let _ = scheme.lm.save_user_bigram(&bigram_path);
        }

        // 新实例（模拟重启）：加载落盘数据后跨句转移评分应仍生效
        let restarted = ShuangpinScheme::with_dictionary_and_bigram(Box::new(dict), bigram_path);
        assert!(restarted.lm.has_user_data(), "重启后应从磁盘恢复个人数据");
        let reference = LanguageModel::new();
        assert!(
            restarted.lm.score_transition("书", "法", 1)
                > reference.score_transition("书", "法", 1),
            "重启后双拼跨句转移评分应仍生效"
        );
    }
}
