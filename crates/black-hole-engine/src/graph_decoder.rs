use crate::{Dictionary, LanguageModel, SyllableGraph};
use std::collections::HashMap;

/// 解码结果
#[derive(Debug, Clone, PartialEq)]
pub struct DecodeResult {
    /// 完整汉字文本
    pub text: String,
    /// 综合评分（越高越好）
    pub score: f64,
    /// 分词结果（每个词的汉字）
    pub words: Vec<String>,
    /// 是否为部分覆盖的混合结果（前缀词 + 单字/拼音后缀）
    pub is_partial: bool,
}

/// 词图边：从音节图位置 start 到 end 的一个候选词
#[derive(Debug, Clone)]
struct WordEdge {
    end_pos: usize,
    text: String,
    base_score: f64,
    syllable_count: usize,
}

/// 维特比解码状态
#[derive(Debug, Clone)]
struct ViterbiState {
    score: f64,
    text: String,
    words: Vec<String>,
}

impl ViterbiState {
    /// 将解码状态转换为 DecodeResult，去掉开头的 <s> 标记。
    fn into_decode_result(self, penalty: f64, is_partial: bool) -> DecodeResult {
        DecodeResult {
            text: self.text,
            score: self.score - penalty,
            words: self.words.into_iter().skip(1).collect(),
            is_partial,
        }
    }
}

/// 评分配置参数
#[derive(Debug, Clone)]
pub struct ScoringConfig {
    /// 多字词偏好系数（每个音节）
    pub word_preference_factor: f64,
    /// 完整覆盖整句的多字词奖励
    pub coverage_bonus: f64,
    /// 混合拼接回退惩罚
    pub hybrid_penalty: f64,
    /// 未覆盖音节惩罚系数（每个音节）
    pub uncovered_penalty_factor: f64,
    /// 最终保底回退惩罚
    pub final_fallback_penalty: f64,
    /// 长词奖励系数（每个音节，仅在无语言模型时使用）
    pub long_word_bonus: f64,
    /// 用户词频奖励系数
    pub user_freq_factor: f64,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            word_preference_factor: 15.0,
            coverage_bonus: 80.0,
            hybrid_penalty: 3.0,
            uncovered_penalty_factor: 0.5,
            final_fallback_penalty: 5.0,
            long_word_bonus: 1.0,
            user_freq_factor: 3.0,
        }
    }
}

/// 评分配置构建器
#[derive(Debug, Default)]
pub struct ScoringConfigBuilder {
    config: ScoringConfig,
}

impl ScoringConfigBuilder {
    pub fn word_preference_factor(mut self, v: f64) -> Self {
        self.config.word_preference_factor = v;
        self
    }

    pub fn coverage_bonus(mut self, v: f64) -> Self {
        self.config.coverage_bonus = v;
        self
    }

    pub fn hybrid_penalty(mut self, v: f64) -> Self {
        self.config.hybrid_penalty = v;
        self
    }

    pub fn uncovered_penalty_factor(mut self, v: f64) -> Self {
        self.config.uncovered_penalty_factor = v;
        self
    }

    pub fn final_fallback_penalty(mut self, v: f64) -> Self {
        self.config.final_fallback_penalty = v;
        self
    }

    pub fn long_word_bonus(mut self, v: f64) -> Self {
        self.config.long_word_bonus = v;
        self
    }

    pub fn user_freq_factor(mut self, v: f64) -> Self {
        self.config.user_freq_factor = v;
        self
    }

    pub fn build(self) -> ScoringConfig {
        self.config
    }
}

impl ScoringConfig {
    pub fn builder() -> ScoringConfigBuilder {
        ScoringConfigBuilder::default()
    }
}

/// 词图构建器与维特比解码器
pub struct GraphDecoder<'a> {
    dict: &'a dyn Dictionary,
    lm: Option<&'a LanguageModel>,
    user_freqs: Option<&'a HashMap<String, i64>>,
    beam_width: usize,
    max_word_syllables: usize,
    config: ScoringConfig,
}

impl<'a> GraphDecoder<'a> {
    pub fn new(dict: &'a dyn Dictionary) -> Self {
        Self {
            dict,
            lm: None,
            user_freqs: None,
            beam_width: 10,
            max_word_syllables: 4,
            config: ScoringConfig::default(),
        }
    }

    pub fn with_lm(mut self, lm: &'a LanguageModel) -> Self {
        self.lm = Some(lm);
        self
    }

    pub fn with_user_freqs(mut self, freqs: &'a HashMap<String, i64>) -> Self {
        self.user_freqs = Some(freqs);
        self
    }

    pub fn with_beam_width(mut self, width: usize) -> Self {
        self.beam_width = width;
        self
    }

    pub fn with_scoring_config(mut self, config: ScoringConfig) -> Self {
        self.config = config;
        self
    }

    /// 对音节图执行维特比解码，返回候选整句列表
    pub fn decode(&self, graph: &SyllableGraph) -> Vec<DecodeResult> {
        let word_edges = self.build_word_edges(graph);
        let n = graph.total_len();

        tracing::debug!(
            "decode start: n={}, word_edges={}",
            n,
            word_edges.iter().map(|v| v.len()).sum::<usize>()
        );

        let dp = self.run_viterbi_decode(&word_edges, n);

        let mut results = self.collect_full_coverage_results(&dp, n);
        if results.is_empty() {
            results = self.fallback_hybrid_assembly(&dp, graph, n);
        }

        self.sort_and_dedup_results(&mut results);
        results
    }

    /// 执行维特比 DP 解码，返回每个音节位置的最优路径集合。
    fn run_viterbi_decode(&self, word_edges: &[Vec<WordEdge>], n: usize) -> Vec<Vec<ViterbiState>> {
        let mut dp: Vec<Vec<ViterbiState>> = vec![Vec::new(); n + 1];
        dp[0].push(ViterbiState {
            score: 0.0,
            text: String::new(),
            words: vec!["<s>".to_string()],
        });

        for i in 0..=n {
            if dp[i].len() > self.beam_width {
                dp[i].sort_by(|a, b| b.score.total_cmp(&a.score));
                dp[i].truncate(self.beam_width);
            }

            if dp[i].is_empty() {
                continue;
            }

            let states = dp[i].clone();
            for edge in &word_edges[i] {
                for state in &states {
                    let new_score = self.calculate_edge_score(state, edge, i, n);
                    self.extend_state(&mut dp[edge.end_pos], state, edge, new_score);
                }
            }
        }

        dp
    }

    /// 计算从当前状态经 word_edge 转移后的新分数。
    fn calculate_edge_score(
        &self,
        state: &ViterbiState,
        edge: &WordEdge,
        start_pos: usize,
        total_len: usize,
    ) -> f64 {
        let last_word = state.words.last().unwrap();
        let lm_score = if let Some(lm) = self.lm {
            lm.score_transition(last_word, &edge.text, edge.syllable_count)
        } else {
            edge.syllable_count as f64 * self.config.long_word_bonus
        };
        let user_bonus = self.user_bonus(&edge.text);
        let base_score = (edge.base_score + 1.0).ln();
        let word_preference = if edge.syllable_count >= 2 {
            edge.syllable_count as f64 * self.config.word_preference_factor
        } else {
            0.0
        };
        let coverage_bonus =
            if start_pos == 0 && edge.end_pos == total_len && edge.syllable_count >= 2 {
                self.config.coverage_bonus
            } else {
                0.0
            };

        state.score + lm_score + base_score + word_preference + coverage_bonus + user_bonus
    }

    /// 将新状态加入 DP 表对应位置。
    fn extend_state(
        &self,
        dp_entry: &mut Vec<ViterbiState>,
        state: &ViterbiState,
        edge: &WordEdge,
        new_score: f64,
    ) {
        let mut new_text = String::with_capacity(state.text.len() + edge.text.len());
        new_text.push_str(&state.text);
        new_text.push_str(&edge.text);

        let mut new_words = Vec::with_capacity(state.words.len() + 1);
        new_words.clone_from(&state.words);
        new_words.push(edge.text.clone());

        dp_entry.push(ViterbiState {
            score: new_score,
            text: new_text,
            words: new_words,
        });
    }

    /// 收集完整覆盖到音节末尾的结果。
    fn collect_full_coverage_results(
        &self,
        dp: &[Vec<ViterbiState>],
        n: usize,
    ) -> Vec<DecodeResult> {
        let mut results = Vec::new();
        if let Some(final_states) = dp.get(n) {
            for state in final_states {
                results.push(state.clone().into_decode_result(0.0, false));
            }
        }
        tracing::debug!(
            "decode full-coverage results: count={}, texts={:?}",
            results.len(),
            results.iter().map(|r| &r.text).collect::<Vec<_>>()
        );
        results
    }

    /// 无完整覆盖时，回退到混合拼接：最长前缀 + 单字后缀。
    fn fallback_hybrid_assembly(
        &self,
        dp: &[Vec<ViterbiState>],
        graph: &SyllableGraph,
        n: usize,
    ) -> Vec<DecodeResult> {
        tracing::debug!("decode: no full coverage, falling back to hybrid");

        for end in (1..=n).rev() {
            let Some(states) = dp.get(end) else { continue };
            if states.is_empty() {
                continue;
            }

            let fallback_suffixes = self.fallback_for_range(graph, end, n);
            if fallback_suffixes.is_empty() {
                continue;
            }

            let uncovered_penalty = (n - end) as f64 * self.config.uncovered_penalty_factor;
            let mut hybrid_results = Vec::new();

            for state in states.iter().take(self.beam_width) {
                for (suffix_text, suffix_score) in &fallback_suffixes {
                    let text = state.text.clone() + suffix_text;
                    let score =
                        state.score + suffix_score - self.config.hybrid_penalty - uncovered_penalty;
                    hybrid_results.push(DecodeResult {
                        text,
                        score,
                        words: Vec::new(),
                        is_partial: true,
                    });
                }
            }

            if !hybrid_results.is_empty() {
                tracing::debug!(
                    "decode hybrid results: count={}, texts={:?}",
                    hybrid_results.len(),
                    hybrid_results.iter().map(|r| &r.text).collect::<Vec<_>>()
                );
                return hybrid_results;
            }
        }

        self.fallback_to_longest_prefix(dp, n)
    }

    /// 最终保底：返回可达的最长前缀作为部分结果。
    fn fallback_to_longest_prefix(&self, dp: &[Vec<ViterbiState>], n: usize) -> Vec<DecodeResult> {
        for end in (1..=n).rev() {
            if let Some(states) = dp.get(end)
                && let Some(best) = states.first()
            {
                return vec![
                    best.clone()
                        .into_decode_result(self.config.final_fallback_penalty, true),
                ];
            }
        }
        Vec::new()
    }

    /// 对结果按分数降序排列并去重。
    fn sort_and_dedup_results(&self, results: &mut Vec<DecodeResult>) {
        results.sort_by(|a, b| b.score.total_cmp(&a.score));
        results.dedup_by(|a, b| a.text == b.text);

        tracing::debug!(
            "decode end: final_results count={}, top5={:?}",
            results.len(),
            results
                .iter()
                .take(5)
                .map(|r| (&r.text, r.score, r.is_partial))
                .collect::<Vec<_>>()
        );
    }

    // ------------------------------------------------------------------
    // 内部方法
    // ------------------------------------------------------------------

    /// 从音节图构建词图边
    ///
    /// 遍历音节图的每个起始位置，沿着合法音节路径前进，
    /// 实时查询词典获取匹配的词，生成词边。
    fn build_word_edges(&self, graph: &SyllableGraph) -> Vec<Vec<WordEdge>> {
        let n = graph.total_len();
        let mut word_edges: Vec<Vec<WordEdge>> = vec![Vec::new(); n + 1];

        for (start, edges) in word_edges.iter_mut().enumerate().take(n + 1) {
            // 从 start 出发，沿着音节图做深度优先搜索，
            // 收集所有长度不超过 max_word_syllables 的音节串前缀
            let mut stack: Vec<(usize, String, usize)> = Vec::new();

            for (end, syllable) in graph.edges_from(start) {
                stack.push((*end, syllable.clone(), 1));
            }

            while let Some((pos, spaced_prefix, syllable_count)) = stack.pop() {
                // 实时查询词典
                let candidates = self.dict.lookup(&spaced_prefix);
                for cand in &candidates {
                    edges.push(WordEdge {
                        end_pos: pos,
                        text: cand.text.clone(),
                        base_score: cand.score as f64,
                        syllable_count,
                    });
                }

                // 继续延伸，如果音节数未超限且还有后续边
                if syllable_count < self.max_word_syllables && pos < n {
                    for (next_end, next_syllable) in graph.edges_from(pos) {
                        let mut new_prefix =
                            String::with_capacity(spaced_prefix.len() + 1 + next_syllable.len());
                        new_prefix.push_str(&spaced_prefix);
                        new_prefix.push(' ');
                        new_prefix.push_str(next_syllable);
                        stack.push((*next_end, new_prefix, syllable_count + 1));
                    }
                }
            }
        }

        word_edges
    }

    /// 用户词频 bonus
    fn user_bonus(&self, text: &str) -> f64 {
        self.user_freqs
            .and_then(|m| m.get(text))
            .map(|&freq| (freq as f64 + 1.0).ln() * self.config.user_freq_factor)
            .unwrap_or(0.0)
    }

    /// 对给定字节范围 [start, end) 内的每个音节查询单字候选，
    /// 返回拼接后的文本和分数列表（top-k 组合）。
    ///
    /// 如果某个音节无单字候选，保留原拼音字母作为占位并施加惩罚。
    fn fallback_for_range(
        &self,
        graph: &SyllableGraph,
        start: usize,
        end: usize,
    ) -> Vec<(String, f64)> {
        tracing::debug!("fallback_for_range: start={}, end={}", start, end);
        let Some(syllables) = graph.find_path_from(start) else {
            tracing::debug!("fallback_for_range: no path from start={}", start);
            return Vec::new();
        };

        let mut results = vec![(String::new(), 0.0)];
        let mut pos = start;

        for syllable in &syllables {
            let next_pos = pos + syllable.len();
            if next_pos > end {
                break;
            }

            let candidates = self.dict.lookup(syllable);
            let mut new_results = Vec::new();

            if candidates.is_empty() {
                // 无单字匹配，用拼音占位并惩罚
                for (text, score) in results {
                    let mut t = text;
                    t.push_str(syllable);
                    new_results.push((t, score - 5.0));
                }
            } else {
                for (text, score) in results {
                    for cand in &candidates {
                        let mut t = text.clone();
                        t.push_str(&cand.text);
                        let s = score + (cand.score as f64 + 1.0).ln();
                        new_results.push((t, s));
                    }
                }
            }

            results = new_results;
            results.sort_by(|a, b| b.1.total_cmp(&a.1));
            results.truncate(5);
            pos = next_pos;

            if pos >= end {
                break;
            }
        }

        if pos < end {
            // 未能覆盖到 end，返回空表示失败
            tracing::debug!(
                "fallback_for_range: failed to cover to end={}, pos={}",
                end,
                pos
            );
            Vec::new()
        } else {
            tracing::debug!(
                "fallback_for_range: results count={}, top3={:?}",
                results.len(),
                results.iter().take(3).collect::<Vec<_>>()
            );
            results
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LanguageModel, RawEntry, RimeDict};
    use black_hole_shared::Candidate;

    fn cand(text: &str, score: i64) -> Candidate {
        Candidate {
            text: text.to_string(),
            comment: None,
            score,
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

    fn build_test_dict() -> RimeDict {
        build_dict(&[
            ("zhong", "中", 50),
            ("guo", "国", 50),
            ("ren", "人", 50),
            ("zhong guo", "中国", 120),
            ("zhong guo ren", "中国人", 100),
            ("ren min", "人民", 120),
        ])
    }

    fn build_test_lm() -> LanguageModel {
        let entries = vec![
            ("zhong".to_string(), vec![cand("中", 50)]),
            ("guo".to_string(), vec![cand("国", 50)]),
            ("ren".to_string(), vec![cand("人", 50)]),
            ("zhong guo".to_string(), vec![cand("中国", 120)]),
            ("zhong guo ren".to_string(), vec![cand("中国人", 100)]),
            ("ren min".to_string(), vec![cand("人民", 120)]),
        ];
        LanguageModel::from_entries(&entries)
    }

    #[test]
    fn test_decode_zhongguo() {
        let dict = build_test_dict();
        let lm = build_test_lm();
        let decoder = GraphDecoder::new(&dict).with_lm(&lm);

        // 音节图: zhong(0-5) -> guo(5-8)
        let graph =
            SyllableGraph::from_single_segmentation(&["zhong".to_string(), "guo".to_string()]);

        let results = decoder.decode(&graph);
        assert!(!results.is_empty());

        // 最高分应该是 "中国"（长词偏好 + 高频）
        assert_eq!(results[0].text, "中国");
    }

    #[test]
    fn test_decode_zhongguoren() {
        let dict = build_test_dict();
        let lm = build_test_lm();
        let decoder = GraphDecoder::new(&dict).with_lm(&lm);

        let graph = SyllableGraph::from_single_segmentation(&[
            "zhong".to_string(),
            "guo".to_string(),
            "ren".to_string(),
        ]);

        let results = decoder.decode(&graph);
        assert!(!results.is_empty());

        // 应该优先 "中国人" 而非 "中国"+"人"
        assert_eq!(results[0].text, "中国人");
    }

    #[test]
    fn test_decode_multiple_paths() {
        let dict = build_test_dict();
        let lm = build_test_lm();
        let decoder = GraphDecoder::new(&dict).with_lm(&lm);

        // 两种切分路径：zhong-guo-ren 和 zho-ng-guo-ren（后者不合法，仅测试）
        let graph = SyllableGraph::from_segmentations(&[vec![
            "zhong".to_string(),
            "guo".to_string(),
            "ren".to_string(),
        ]]);

        let results = decoder.decode(&graph);
        assert!(!results.is_empty());
    }

    #[test]
    fn test_partial_decode_with_single_char_fallback() {
        // 词库只有 "zhong guo" -> 中国和单字 "ren" -> 人，
        // 没有 "zhong guo ren" 整句，验证混合拼接回退
        // 注意：不插入 "zhong"、"guo" 单字，确保只有词边能走到位置 8
        let dict = build_dict(&[("zhong guo", "中国", 120), ("ren", "人", 50)]);

        let decoder = GraphDecoder::new(&dict);

        let graph = SyllableGraph::from_single_segmentation(&[
            "zhong".to_string(),
            "guo".to_string(),
            "ren".to_string(),
        ]);

        let results = decoder.decode(&graph);
        assert!(!results.is_empty(), "部分覆盖时应至少返回混合候选");

        // 应该包含 "中国人"
        assert!(
            results.iter().any(|r| r.text == "中国人"),
            "应能拼接出 '中国人'，实际结果: {:?}",
            results
        );
    }

    #[test]
    fn test_partial_decode_unknown_suffix() {
        // 词库只有 "zhong guo" -> 中国，没有 "ren" 的任何词条
        let dict = build_dict(&[("zhong guo", "中国", 120)]);

        let decoder = GraphDecoder::new(&dict);

        let graph = SyllableGraph::from_single_segmentation(&[
            "zhong".to_string(),
            "guo".to_string(),
            "ren".to_string(),
        ]);

        let results = decoder.decode(&graph);
        assert!(
            !results.is_empty(),
            "部分覆盖且单字也无匹配时应返回拼音占位候选"
        );

        // 应该包含 "中国ren"（拼音占位）
        assert!(
            results.iter().any(|r| r.text == "中国ren"),
            "应能拼接出 '中国ren'，实际结果: {:?}",
            results
        );
    }
}
