use crate::CandidateRanker;
use blackhole_shared::Candidate;

/// 基于词频的简单候选排序器
pub struct SimpleRanker;

impl Default for SimpleRanker {
    fn default() -> Self {
        Self::new()
    }
}

impl SimpleRanker {
    pub fn new() -> Self {
        Self
    }
}

impl CandidateRanker for SimpleRanker {
    fn rank(&self, _code: &str, candidates: &mut [Candidate]) {
        rank_by_score_desc(candidates);
    }
}

/// 按 score 降序排列候选词
fn rank_by_score_desc(candidates: &mut [Candidate]) {
    candidates.sort_by_key(|b| std::cmp::Reverse(b.score));
}

/// 融合用户历史的候选排序器
/// 用户历史词频作为 bonus 加到基础 score 上
pub struct UserAwareRanker {
    /// 用户词频权重系数
    user_weight: i64,
}

impl Default for UserAwareRanker {
    fn default() -> Self {
        Self::new()
    }
}

impl UserAwareRanker {
    pub fn new() -> Self {
        Self { user_weight: 1000 }
    }

    pub fn with_weight(user_weight: i64) -> Self {
        Self { user_weight }
    }

    /// 为每个候选应用用户词频 bonus，然后排序
    pub fn rank_with_frequencies(
        &self,
        candidates: &mut [Candidate],
        user_freqs: &std::collections::HashMap<String, i64>,
    ) {
        for c in candidates.iter_mut() {
            if let Some(freq) = user_freqs.get(&c.text) {
                c.score += freq * self.user_weight;
            }
        }
        rank_by_score_desc(candidates);
    }
}

impl CandidateRanker for UserAwareRanker {
    fn rank(&self, _code: &str, candidates: &mut [Candidate]) {
        // 在没有用户数据时，行为与 SimpleRanker 相同
        rank_by_score_desc(candidates);
    }
}
