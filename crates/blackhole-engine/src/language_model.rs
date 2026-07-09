use blackhole_shared::Candidate;
use std::collections::HashMap;

/// 最大保留的 unigram 条目数，超出时只保留高频词以降低内存
const MAX_UNIGRAM_ENTRIES: usize = 20000;

/// 语言模型（Unigram + Bigram）
///
/// 为维特比解码提供词语概率评分。
/// - Unigram：从词库词频构建，反映词语自身出现概率。
/// - Bigram：从用户历史或外部语料构建，反映词语间转移概率。
/// - 长词偏好：鼓励输出完整词而非单字拼接。
#[derive(Debug, Clone)]
pub struct LanguageModel {
    /// 词语 -> log 概率
    unigram: HashMap<String, f64>,
    /// (前词, 当前词) -> log 条件概率 P(当前词|前词)
    bigram: HashMap<(String, String), f64>,
    /// 未观测 bigram 的回退权重（乘到 unigram 上）
    backoff_weight: f64,
    /// 每个字节的额外 log 奖励（鼓励长词）
    long_word_bonus: f64,
    /// 总词频（用于归一化）
    total_frequency: f64,
}

impl Default for LanguageModel {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageModel {
    pub fn new() -> Self {
        Self {
            unigram: HashMap::new(),
            bigram: HashMap::new(),
            backoff_weight: -3.0, // log(0.05) ≈ -3.0
            long_word_bonus: 0.3,
            total_frequency: 0.0,
        }
    }

    /// 从聚合后的 text -> score 构建 Unigram 模型，并限制最大条目数以控制内存
    pub fn from_text_scores(total: i64, mut text_scores: HashMap<String, i64>) -> Self {
        let mut lm = Self::new();
        if total > 0 {
            lm.total_frequency = total as f64;

            // 如果词条数过多，只保留高频词，降低内存占用
            if text_scores.len() > MAX_UNIGRAM_ENTRIES {
                let mut entries: Vec<(String, i64)> = text_scores.into_iter().collect();
                entries.sort_by_key(|b| std::cmp::Reverse(b.1));
                entries.truncate(MAX_UNIGRAM_ENTRIES);
                text_scores = entries.into_iter().collect();
            }

            for (text, score) in text_scores {
                let prob = score as f64 / lm.total_frequency;
                lm.unigram.insert(text, prob.ln());
            }
        }
        lm
    }

    /// 从词库条目构建 Unigram 模型
    ///
    /// entries: (code, candidates) 列表，candidates 包含 text 和 score。
    /// 相同 text 在不同 code 下出现时会合并 score。
    pub fn from_entries(entries: &[(String, Vec<Candidate>)]) -> Self {
        let mut text_scores: HashMap<String, i64> = HashMap::new();
        for (_code, cands) in entries {
            for cand in cands {
                *text_scores.entry(cand.text.clone()).or_insert(0) += cand.score.max(1);
            }
        }
        let total: i64 = text_scores.values().sum();
        Self::from_text_scores(total, text_scores)
    }

    /// 加载预计算的 Bigram 概率对
    ///
    /// pairs: (prev_word, curr_word, log_probability)
    pub fn load_bigram_pairs(&mut self, pairs: &[(String, String, f64)]) {
        for (prev, curr, log_prob) in pairs {
            self.bigram.insert((prev.clone(), curr.clone()), *log_prob);
        }
    }

    /// 从用户上屏记录中简单学习 Bigram
    ///
    /// 将相邻的上屏文本视为 bigram 共现，统计频率后转为 log 概率。
    /// 适用于用户个性化调频。
    pub fn learn_from_commits(&mut self, commits: &[(String, String)]) {
        // commits: (text, code)
        let mut bigram_counts: HashMap<(String, String), u64> = HashMap::new();
        let mut unigram_counts: HashMap<String, u64> = HashMap::new();

        for i in 1..commits.len() {
            let prev = &commits[i - 1].0;
            let curr = &commits[i].0;
            *bigram_counts
                .entry((prev.clone(), curr.clone()))
                .or_insert(0) += 1;
            *unigram_counts.entry(prev.clone()).or_insert(0) += 1;
        }

        // 最后一个词也要计入 unigram
        if let Some(last) = commits.last() {
            *unigram_counts.entry(last.0.clone()).or_insert(0) += 1;
        }

        for ((prev, curr), count) in bigram_counts {
            if let Some(&prev_count) = unigram_counts.get(&prev)
                && prev_count > 0
            {
                let prob = count as f64 / prev_count as f64;
                self.bigram.insert((prev, curr), prob.ln());
            }
        }
    }

    /// 获取词语的 Unigram log 概率
    pub fn score_unigram(&self, word: &str) -> f64 {
        self.unigram
            .get(word)
            .copied()
            .unwrap_or(self.unknown_word_score(word))
    }

    /// 获取 Bigram 转移 log 概率 P(curr | prev)
    pub fn score_bigram(&self, prev: &str, curr: &str) -> Option<f64> {
        self.bigram
            .get(&(prev.to_string(), curr.to_string()))
            .copied()
    }

    /// 综合转移评分：融合 Bigram / Unigram 回退 + 长词偏好
    ///
    /// prev: 前一个词（句子开头用 "<s>"）
    /// curr: 当前词
    /// word_len: 当前词覆盖的音节数（用于长词奖励）
    pub fn score_transition(&self, prev: &str, curr: &str, word_syllable_len: usize) -> f64 {
        let language_score = match self.score_bigram(prev, curr) {
            // 有 Bigram 观测值，直接使用
            Some(bi) => bi,
            // 回退到 Unigram + 平滑惩罚
            None => self.score_unigram(curr) + self.backoff_weight,
        };

        let length_bonus = word_syllable_len as f64 * self.long_word_bonus;

        language_score + length_bonus
    }

    /// 未知词的概率估计（基于字数的简单启发）
    fn unknown_word_score(&self, word: &str) -> f64 {
        // 字越多越不可能，给予惩罚
        let char_count = word.chars().count().max(1);
        let base = 1.0 / (self.total_frequency + 1000.0);
        (base / char_count as f64).ln()
    }

    /// 设置长词偏好奖励系数
    pub fn set_long_word_bonus(&mut self, bonus: f64) {
        self.long_word_bonus = bonus;
    }

    /// 设置 Bigram 回退权重
    pub fn set_backoff_weight(&mut self, weight: f64) {
        self.backoff_weight = weight;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(text: &str, score: i64) -> Candidate {
        Candidate {
            text: text.to_string(),
            comment: None,
            score,
        }
    }

    #[test]
    fn test_unigram_from_entries() {
        let entries = vec![
            (
                "zhong guo".to_string(),
                vec![cand("中国", 100), cand("种过", 20)],
            ),
            ("ren".to_string(), vec![cand("人", 80)]),
            ("zhong guo ren".to_string(), vec![cand("中国人", 50)]),
        ];

        let lm = LanguageModel::from_entries(&entries);

        // "中国" 的 score 最高，概率应最大
        let score_zhongguo = lm.score_unigram("中国");
        let score_ren = lm.score_unigram("人");
        let score_zhongguoren = lm.score_unigram("中国人");

        // 中国: 100, 人: 80, 中国人: 50, 种过: 20
        // total = 250
        assert!(score_zhongguo > score_ren); // 100 > 80
        assert!(score_ren > score_zhongguoren); // 80 > 50
    }

    #[test]
    fn test_bigram_transition() {
        let mut lm = LanguageModel::new();
        lm.load_bigram_pairs(&[("中国".to_string(), "人".to_string(), -0.5)]);

        // 有 bigram 时
        let score_with_bi = lm.score_transition("中国", "人", 1);
        // 无 bigram 时（回退到 unigram，未知词得分应更低）
        let score_without_bi = lm.score_transition("未知词A", "未知词B", 1);

        assert!(score_with_bi > score_without_bi);
    }

    #[test]
    fn test_long_word_bonus() {
        let entries = vec![
            ("zhong guo".to_string(), vec![cand("中国", 100)]),
            ("zhong guo ren".to_string(), vec![cand("中国人", 100)]),
        ];
        let lm = LanguageModel::from_entries(&entries);

        // 相同 unigram 概率下，长词应有更高评分
        let score_short = lm.score_transition("<s>", "中国", 2);
        let score_long = lm.score_transition("<s>", "中国人", 3);

        assert!(score_long > score_short);
    }

    #[test]
    fn test_learn_from_commits() {
        let mut lm = LanguageModel::new();
        // 模拟用户连续上屏
        let commits = vec![
            ("中国".to_string(), "zhong guo".to_string()),
            ("人民".to_string(), "ren min".to_string()),
            ("中国".to_string(), "zhong guo".to_string()),
            ("人民".to_string(), "ren min".to_string()),
        ];
        lm.learn_from_commits(&commits);

        // "中国" -> "人民" 的 bigram 应该被学习到
        let score = lm.score_bigram("中国", "人民");
        assert!(score.is_some());
        assert!(score.unwrap() <= 0.0); // log 概率为非正数
    }
}
