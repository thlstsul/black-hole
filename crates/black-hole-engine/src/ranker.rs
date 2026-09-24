use crate::CandidateRanker;
use black_hole_shared::Candidate;
use std::cmp::Reverse;

/// 基于词频的简单候选排序器
#[derive(Default)]
pub struct SimpleRanker;

impl SimpleRanker {
    pub fn new() -> Self {
        Self
    }
}

impl CandidateRanker for SimpleRanker {
    fn rank(&self, _code: &str, candidates: &mut [Candidate]) {
        // 按 score 降序排列候选词
        candidates.sort_by_key(|b| Reverse(b.score));
    }
}
