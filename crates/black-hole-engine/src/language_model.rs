use black_hole_shared::Candidate;
use rustc_hash::FxHashMap;
use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// 最大保留的 unigram 条目数，超出时只保留高频词以降低内存
const MAX_UNIGRAM_ENTRIES: usize = 20000;

/// 句首标记：个人 Bigram 中句首词的前词
pub const SENTENCE_START: &str = "<s>";

/// 个人概率里 bigram 部分的权重，其余给个人 unigram
const USER_LAMBDA: f64 = 0.8;

/// 个人插值置信度 μ = c(v)/(c(v)+K) 里的 K：前词见过 K 次时个人数据与静态模型各占一半。
/// 取 8 保证一次误选翻不过静态模型，选两次才翻。
const CONFIDENCE_K: f64 = 8.0;

/// 个人插值置信度封顶：用户数据再多，个人概率最多占一半，保留静态模型的话语权
const MAX_CONFIDENCE: f64 = 0.5;

/// 个人转移计数上限（二元对总数），超过时整体减半衰减
const MAX_USER_TRANSITIONS: usize = 200_000;

/// 个人 Bigram 转移计数层（参考 qingjian UserNgram）
///
/// 只存 (前词, 后词) 计数：每个上屏的词都带着前词（句首用 [`SENTENCE_START`]），
/// 所以一个词的出现次数就是以它为后词的计数之和，无须另存一元表。
/// 与静态模型在概率域插值（见 [`UserBigram::blend`]），个人数据多也不会压死词库概率。
#[derive(Debug, Clone, Default)]
pub struct UserBigram {
    /// 前词 -> (后词 -> 次数)
    pairs: FxHashMap<String, FxHashMap<String, u32>>,
    /// 前词 -> 以它为前词的总次数（二元条件概率分母）
    context_totals: FxHashMap<String, u32>,
    /// 词 -> 出现次数（以它为后词的计数之和）
    word_counts: FxHashMap<String, u32>,
    /// 所有二元计数之和
    total: u64,
    /// 不同的 (前词, 后词) 对数（增量维护，避免每次全扫求和）
    transition_count: usize,
}

impl UserBigram {
    /// 记一次转移（句首前词传 `None`）。超过上限时整体减半，忘掉久远的偏好。
    pub fn observe(&mut self, prev: Option<&str>, curr: &str, times: u32) {
        if curr.is_empty() || times == 0 {
            return;
        }
        let previous = prev.unwrap_or(SENTENCE_START);
        let row = self.pairs.entry(previous.to_owned()).or_default();
        // 词对已存在则累加，新建时增加转移计数（Entry 避免一次额外的哈希查找）
        match row.entry(curr.to_owned()) {
            Entry::Vacant(slot) => {
                slot.insert(times);
                self.transition_count += 1;
            }
            Entry::Occupied(mut slot) => *slot.get_mut() += times,
        }
        *self.context_totals.entry(previous.to_owned()).or_default() += times;
        *self.word_counts.entry(curr.to_owned()).or_default() += times;
        self.total += u64::from(times);
        if self.transition_count > MAX_USER_TRANSITIONS {
            self.decay();
        }
    }

    /// 把静态模型给出的 `log P(word | previous)` 与个人概率插值后返回。
    ///
    /// 个人二元 P₂ = λ·c(v,w)/c(v) + (1-λ)·c(w)/N；
    /// 插值权重 μ = c(v)/(c(v)+K)，封顶 [`MAX_CONFIDENCE`]：
    /// 见过这个前词越多越信个人数据，但永远压不死静态模型。
    /// 前词从没见过时（句首用句首标记）原样返回，行为与无个人数据时一致。
    pub fn blend(&self, prev: Option<&str>, curr: &str, base_log_prob: f64) -> f64 {
        let previous = prev.unwrap_or(SENTENCE_START);
        let Some(&context_total) = self.context_totals.get(previous) else {
            return base_log_prob;
        };
        if context_total == 0 || self.total == 0 {
            return base_log_prob;
        }
        let pair = f64::from(
            self.pairs
                .get(previous)
                .and_then(|m| m.get(curr))
                .copied()
                .unwrap_or(0),
        );
        let unigram = f64::from(*self.word_counts.get(curr).unwrap_or(&0)) / self.total as f64;
        let bigram = USER_LAMBDA * pair / f64::from(context_total) + (1.0 - USER_LAMBDA) * unigram;
        let confidence = (f64::from(context_total) / (f64::from(context_total) + CONFIDENCE_K))
            .min(MAX_CONFIDENCE);
        let blended = (1.0 - confidence) * base_log_prob.exp() + confidence * bigram;
        // base_log_prob 极负时 exp 下溢为 0，夹一个正下界避免 ln(-inf)
        blended.max(1e-12).ln()
    }

    /// 不同的 (前词, 后词) 对数
    pub fn transition_count(&self) -> usize {
        self.transition_count
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// 所有计数整体减半（1/2=0 的条目删除），并重算各项合计。
    /// 而非硬截断：久远偏好按比例淡出，近期高频数据保留。
    fn decay(&mut self) {
        for row in self.pairs.values_mut() {
            row.retain(|_, count| {
                *count /= 2;
                *count > 0
            });
        }
        self.pairs.retain(|_, row| !row.is_empty());
        self.rebuild_totals();
    }

    /// 从 pairs 重算 context_totals / word_counts / total / transition_count
    fn rebuild_totals(&mut self) {
        self.context_totals.clear();
        self.word_counts.clear();
        self.total = 0;
        self.transition_count = 0;
        for (prev, row) in &self.pairs {
            *self.context_totals.entry(prev.clone()).or_default() += row.values().sum::<u32>();
            self.transition_count += row.len();
            for (curr, count) in row {
                *self.word_counts.entry(curr.clone()).or_default() += *count;
                self.total += u64::from(*count);
            }
        }
    }

    /// 从 `前词\t后词\t次数` 文本加载，坏行跳过；只解析，不落盘。
    pub fn parse(source: &str) -> Self {
        let mut user = Self::default();
        for line in source.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split('\t');
            if let (Some(prev), Some(curr), Some(count)) =
                (fields.next(), fields.next(), fields.next())
                && let Ok(count) = count.trim().parse::<u32>()
                && count > 0
            {
                user.observe(Some(prev), curr, count);
            }
        }
        user
    }

    /// 序列化为 TSV（按计数降序、同频按词条稳定排序，保证文件内容可复现）
    pub fn to_tsv(&self) -> String {
        let mut rows: Vec<(&str, &str, u32)> = self
            .pairs
            .iter()
            .flat_map(|(prev, row)| {
                row.iter()
                    .map(move |(curr, count)| (prev.as_str(), curr.as_str(), *count))
            })
            .collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| (a.0, a.1).cmp(&(b.0, b.1))));
        let mut out = String::with_capacity(rows.len() * 24);
        for (prev, curr, count) in rows {
            out.push_str(prev);
            out.push('\t');
            out.push_str(curr);
            out.push('\t');
            out.push_str(&count.to_string());
            out.push('\n');
        }
        out
    }
}

/// `user_bigram.txt` 的原子写入：先写临时文件再 rename，避免崩溃写坏用户数据
fn write_atomic(path: &Path, content: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.flush()?;
    }
    fs::rename(&tmp, path)
}

/// 语言模型（Unigram + Bigram）
///
/// 为维特比解码提供词语概率评分。
/// - Unigram：从词库词频构建，反映词语自身出现概率。
/// - Bigram：从用户历史或外部语料构建，反映词语间转移概率。
/// - 长词偏好：鼓励输出完整词而非单字拼接。
#[derive(Debug, Clone)]
pub struct LanguageModel {
    /// 词语 -> log 概率
    unigram: FxHashMap<String, f64>,
    /// 前词 -> (当前词 -> log 条件概率 P(当前词|前词))
    ///
    /// 两级嵌套结构使 `score_bigram(prev, curr)` 可通过 `&str` 借入查询，
    /// 避免原 `(String, String)` 键每次查询都要做两次堆分配。
    bigram: FxHashMap<String, FxHashMap<String, f64>>,
    /// 未观测 bigram 的回退权重（乘到 unigram 上）
    backoff_weight: f64,
    /// 每个字节的额外 log 奖励（鼓励长词）
    long_word_bonus: f64,
    /// 总词频（用于归一化）
    total_frequency: f64,
    /// 个人 Bigram 转移计数（随上屏在线更新，与静态模型插值）
    user: UserBigram,
}

impl Default for LanguageModel {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageModel {
    pub fn new() -> Self {
        Self {
            unigram: FxHashMap::default(),
            bigram: FxHashMap::default(),
            backoff_weight: -3.0, // log(0.05) ≈ -3.0
            long_word_bonus: 0.3,
            total_frequency: 0.0,
            user: UserBigram::default(),
        }
    }

    /// 从聚合后的 text -> score 构建 Unigram 模型，并限制最大条目数以控制内存
    pub fn from_text_scores(total: i64, mut text_scores: FxHashMap<String, i64>) -> Self {
        let mut lm = Self::new();
        if total > 0 {
            lm.total_frequency = total as f64;

            // 如果词条数过多，只保留高频词，降低内存占用
            if text_scores.len() > MAX_UNIGRAM_ENTRIES {
                let mut entries: Vec<(String, i64)> = text_scores.into_iter().collect();
                entries.sort_by_key(|b| Reverse(b.1));
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
        let mut text_scores: FxHashMap<String, i64> = FxHashMap::default();
        for cand in entries.iter().flat_map(|(_, cands)| cands) {
            *text_scores.entry(cand.text.clone()).or_insert(0) += cand.score.max(1);
        }
        let total: i64 = text_scores.values().sum();
        Self::from_text_scores(total, text_scores)
    }

    /// 加载预计算的 Bigram 概率对
    ///
    /// pairs: (prev_word, curr_word, log_probability)
    pub fn load_bigram_pairs(&mut self, pairs: &[(String, String, f64)]) {
        for (prev, curr, log_prob) in pairs {
            self.bigram
                .entry(prev.clone())
                .or_default()
                .insert(curr.clone(), *log_prob);
        }
    }

    /// 从用户上屏记录中简单学习 Bigram
    ///
    /// 将相邻的上屏文本视为 bigram 共现，统计频率后转为 log 概率。
    /// 适用于用户个性化调频。
    pub fn learn_from_commits(&mut self, commits: &[(String, String)]) {
        // commits: (text, code)
        let mut bigram_counts: FxHashMap<(String, String), u64> = FxHashMap::default();
        let mut unigram_counts: FxHashMap<String, u64> = FxHashMap::default();

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
                self.bigram.entry(prev).or_default().insert(curr, prob.ln());
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
        self.bigram.get(prev).and_then(|m| m.get(curr)).copied()
    }

    /// 综合转移评分：融合 Bigram / Unigram 回退 + 个人 Bigram 插值 + 长词偏好
    ///
    /// prev: 前一个词（句子开头用 "<s>"）
    /// curr: 当前词
    /// word_len: 当前词覆盖的音节数（用于长词奖励）
    pub fn score_transition(&self, prev: &str, curr: &str, word_syllable_len: usize) -> f64 {
        let base = match self.score_bigram(prev, curr) {
            // 有 Bigram 观测值，直接使用
            Some(bi) => bi,
            // 回退到 Unigram + 平滑惩罚
            None => self.score_unigram(curr) + self.backoff_weight,
        };
        // 个人 Bigram 在概率域与静态评分插值；无个人数据时原样返回
        let language_score = self.user.blend(Some(prev), curr, base);
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

    /// 个人 Bigram 观测入口：记录一次上屏转移（句首前词传 `None`）
    pub fn observe_user_transition(&mut self, prev: Option<&str>, curr: &str, times: u32) {
        self.user.observe(prev, curr, times);
    }

    /// 个人 Bigram 是否有数据
    pub fn has_user_data(&self) -> bool {
        !self.user.is_empty()
    }

    /// 从 `前词\t后词\t次数` TSV 加载个人 Bigram（坏行跳过，文件缺失不报错）
    pub fn load_user_bigram(&mut self, path: &Path) {
        match fs::read_to_string(path) {
            Ok(source) => self.user = UserBigram::parse(&source),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "读取个人 Bigram 失败，以空模型继续")
            }
        }
    }

    /// 将个人 Bigram 以原始计数 TSV 原子写入 `path`
    pub fn save_user_bigram(&self, path: &Path) -> io::Result<()> {
        write_atomic(path, &self.user.to_tsv())
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

    /// 无个人数据时，插值不改静态评分（逐位一致）
    #[test]
    fn test_user_bigram_no_data_keeps_base_score() {
        let mut lm = LanguageModel::new();
        lm.load_bigram_pairs(&[("中国".to_string(), "人".to_string(), -0.5)]);
        let with_user = lm.score_transition("中国", "人", 1);

        let mut reference = LanguageModel::new();
        reference.load_bigram_pairs(&[("中国".to_string(), "人".to_string(), -0.5)]);
        let base = reference.score_transition("中国", "人", 1);

        assert_eq!(with_user, base, "无个人数据时评分应与旧实现逐位一致");
    }

    /// 观测后插值评分生效：观测过的转移评分高于未观测的同前词竞争者
    #[test]
    fn test_user_bigram_blend_boosts_observed() {
        let mut lm = LanguageModel::new();
        lm.load_bigram_pairs(&[
            ("天气".to_string(), "晴".to_string(), -1.0),
            ("天气".to_string(), "冷".to_string(), -1.0),
        ]);
        let base_qing = lm.score_transition("天气", "晴", 1);
        let base_leng = lm.score_transition("天气", "冷", 1);
        assert_eq!(base_qing, base_leng, "静态评分相同时应持平");

        // 只观测 (天气 → 晴)
        lm.observe_user_transition(Some("天气"), "晴", 1);
        let blended_qing = lm.score_transition("天气", "晴", 1);
        let blended_leng = lm.score_transition("天气", "冷", 1);
        assert!(
            blended_qing > blended_leng,
            "观测过的转移应获得更高评分: {blended_qing} vs {blended_leng}"
        );
    }

    /// K=8：一次观测（c(prev)=1，μ=1/9）翻不过静态 bigram 观测值
    #[test]
    fn test_user_bigram_single_observe_cannot_flip_static() {
        let mut lm = LanguageModel::new();
        lm.load_bigram_pairs(&[
            ("天气".to_string(), "晴".to_string(), -0.1),
            ("天气".to_string(), "冷".to_string(), -4.0),
        ]);
        let base_leng = lm.score_transition("天气", "冷", 1);
        let base_qing = lm.score_transition("天气", "晴", 1);

        lm.observe_user_transition(Some("天气"), "晴", 1);
        let blended_leng = lm.score_transition("天气", "冷", 1);

        // μ = 1/9 ≈ 0.11，个人概率最多占 1/9，静态项仍占主导：
        // 晴 仍应是首选（学习只缩小差距，不翻转）
        let blended_qing = lm.score_transition("天气", "晴", 1);
        assert!(
            blended_qing > blended_leng,
            "一次观测不应翻转静态首选，只应缩小差距: 晴={blended_qing} 冷={blended_leng}"
        );
        // 晴的增益来自 P_个人=1 的早期退化 + μ=1/9 权重，幅度应有限（概率域 < 1/9 的加权移动）
        assert!(
            blended_qing - base_qing < 1.0,
            "一次观测的评分增益应有限: 晴 {base_qing} -> {blended_qing}"
        );
        assert!(blended_leng > base_leng - 1.0, "未观测项评分不应剧烈恶化");
    }

    /// 多次观测后评分收敛到 μ 封顶（个人概率最多占 0.5）
    #[test]
    fn test_user_bigram_converges_to_confidence_cap() {
        let mut lm = LanguageModel::new();
        lm.load_bigram_pairs(&[("天气".to_string(), "晴".to_string(), -0.1)]);
        for _ in 0..100 {
            lm.observe_user_transition(Some("天气"), "晴", 10);
        }
        // c(prev)=1000, μ = 1000/1008 ≈ 0.992 → 封顶 0.5
        let blended = lm.score_transition("天气", "晴", 1);
        // P = 0.5·exp(-0.1) + 0.5·P_个人，P_个人 → 1（λ 项占满），ln(P) > ln(0.5) ≈ -0.693
        assert!(
            blended > 0.5f64.ln(),
            "收敛后评分应超过 ln(0.5)，实际 {blended}"
        );
    }

    /// decay：转移数超限时整体减半
    #[test]
    fn test_user_bigram_decay_halves_counts() {
        let mut user = UserBigram::default();
        // 每对 4 次 × 4 对 = 16，不足以触发；用小规模直接调 parse 验证比例
        user.observe(Some("a"), "b", 4);
        user.observe(Some("a"), "c", 4);
        user.observe(Some("d"), "b", 4);
        user.observe(Some("d"), "e", 4);
        assert_eq!(user.transition_count(), 4);
        assert_eq!(user.total, 16);

        user.decay();
        assert_eq!(user.transition_count(), 4, "计数减半后条目仍存在");
        assert_eq!(user.total, 8, "总次数应减半");

        // 再减半：1 的条目应被删除
        user.decay();
        assert_eq!(user.total, 4, "计数 1 的条目减半后应删除");
    }

    /// 持久化 roundtrip：TSV 写出再读回，计数保真
    #[test]
    fn test_user_bigram_persistence_roundtrip() {
        let mut lm = LanguageModel::new();
        lm.observe_user_transition(Some("天气"), "晴", 3);
        lm.observe_user_transition(None, "你好", 1);

        let dir = std::env::temp_dir().join("bh_engine_test_bigram");
        let path = dir.join("user_bigram.txt");
        lm.save_user_bigram(&path).expect("保存应成功");

        let mut restored = LanguageModel::new();
        restored.load_user_bigram(&path);
        assert!(restored.has_user_data(), "加载后应有个人数据");

        let score_before = lm.score_transition("天气", "晴", 1);
        let score_after = restored.score_transition("天气", "晴", 1);
        assert!(
            (score_before - score_after).abs() < 1e-9,
            "roundtrip 后评分应一致: {score_before} vs {score_after}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 损坏文件容错：坏行跳过、好行保留且计数精确
    #[test]
    fn test_user_bigram_parse_lenient() {
        let source = "天气\t晴\t3\n\nbroken line\n天气\t\t2\n# comment\n你\t好\tx\n你\t好\t1\n";
        let user = UserBigram::parse(source);
        assert_eq!(user.transition_count(), 2, "只应保留 2 条有效行");

        // 计数精确性：与手工 observe 同样计数的参照模型 blend 结果逐位一致
        // （验证 天气→晴 计数确为 3、你→好 为 1，坏行均未计入）
        let mut reference = UserBigram::default();
        reference.observe(Some("天气"), "晴", 3);
        reference.observe(Some("你"), "好", 1);
        let base = 0.5f64.ln();
        for (prev, curr) in [("天气", "晴"), ("你", "好"), ("天气", "冷")] {
            assert_eq!(
                user.blend(Some(prev), curr, base),
                reference.blend(Some(prev), curr, base),
                "解析计数应与参照模型一致: {prev}->{curr}"
            );
        }

        // 文件缺失容错：以空模型继续
        let mut restored = LanguageModel::new();
        restored.load_user_bigram(std::path::Path::new("/nonexistent/path.txt"));
        assert!(!restored.has_user_data(), "文件缺失应以空模型继续");
    }
}
