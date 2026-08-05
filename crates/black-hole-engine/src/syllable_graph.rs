use rustc_hash::FxHashSet;
use std::collections::HashSet;

/// 音节切分有向无环图（DAG）
///
/// 将连续的拼音字符串（如 "zhongguoren"）的所有合法切分路径，
/// 合并为一个统一的图结构。节点为字节位置，边代表一个合法音节跨度。
#[derive(Debug, Clone)]
pub struct SyllableGraph {
    /// edges[i] 表示从字节位置 i 出发的所有 (end_pos, syllable) 边
    edges: Vec<Vec<(usize, String)>>,
    /// 输入字符串的总字节长度
    total_len: usize,
}

impl SyllableGraph {
    /// 从多种切分结果构建音节图
    ///
    /// 遍历所有切分路径，将每条路径上的每个音节映射为图中的一条边，
    /// 最终合并去重，形成完整的 DAG。
    pub fn from_segmentations(segmentations: &[Vec<String>]) -> Self {
        if segmentations.is_empty() {
            return Self {
                edges: Vec::new(),
                total_len: 0,
            };
        }

        // 计算输入总长度（所有切分路径拼接后长度应相同）
        let total_len = segmentations
            .first()
            .map(|seg| seg.iter().map(|s| s.len()).sum())
            .unwrap_or(0);

        let mut edges: Vec<Vec<(usize, String)>> = vec![Vec::new(); total_len + 1];
        let mut seen: FxHashSet<(usize, usize)> = FxHashSet::default();

        for seg in segmentations {
            let mut pos = 0;
            for syllable in seg {
                let start = pos;
                let end = pos + syllable.len();
                if seen.insert((start, end)) {
                    edges[start].push((end, syllable.clone()));
                }
                pos = end;
            }
        }

        Self { edges, total_len }
    }

    /// 从单条切分路径构建图（用于增量切分场景）
    pub fn from_single_segmentation(syllables: &[String]) -> Self {
        let total_len: usize = syllables.iter().map(|s| s.len()).sum();
        let mut edges: Vec<Vec<(usize, String)>> = vec![Vec::new(); total_len + 1];

        let mut pos = 0;
        for syllable in syllables {
            let start = pos;
            let end = pos + syllable.len();
            edges[start].push((end, syllable.clone()));
            pos = end;
        }

        Self { edges, total_len }
    }

    /// 获取从指定字节位置出发的所有边
    pub fn edges_from(&self, pos: usize) -> &[(usize, String)] {
        self.edges.get(pos).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// 输入字符串的总字节长度
    pub fn total_len(&self) -> usize {
        self.total_len
    }

    /// 枚举所有从起点到终点的完整切分路径
    ///
    /// 使用 DFS 回溯，适用于路径数量可控的场景。
    pub fn all_paths(&self) -> Vec<Vec<String>> {
        if self.total_len == 0 {
            return Vec::new();
        }
        let mut results = Vec::new();
        let mut current = Vec::new();
        self.dfs(0, &mut current, &mut results);
        results
    }

    fn dfs(&self, pos: usize, current: &mut Vec<String>, results: &mut Vec<Vec<String>>) {
        if pos >= self.total_len {
            if pos == self.total_len {
                results.push(current.clone());
            }
            return;
        }

        for (end, syllable) in &self.edges[pos] {
            current.push(syllable.clone());
            self.dfs(*end, current, results);
            current.pop();
        }
    }

    /// 获取所有可达的终点位置（从起点出发）
    pub fn reachable_ends(&self) -> Vec<usize> {
        let mut reachable = HashSet::new();
        reachable.insert(0usize);
        let mut changed = true;
        while changed {
            changed = false;
            for pos in 0..=self.total_len {
                if reachable.contains(&pos) {
                    for (end, _) in &self.edges[pos] {
                        if reachable.insert(*end) {
                            changed = true;
                        }
                    }
                }
            }
        }
        let mut ends: Vec<usize> = reachable.into_iter().collect();
        ends.sort();
        ends
    }

    /// 查找从指定位置到总终点的第一条有效路径上的音节列表
    ///
    /// 使用 DFS 快速返回第一条能到达 `total_len` 的路径。
    pub fn find_path_from(&self, start: usize) -> Option<Vec<String>> {
        if start > self.total_len {
            return None;
        }
        if start == self.total_len {
            return Some(Vec::new());
        }
        let mut current = Vec::new();
        let mut result = None;
        self.dfs_from(start, &mut current, &mut result);
        result
    }

    fn dfs_from(&self, pos: usize, current: &mut Vec<String>, result: &mut Option<Vec<String>>) {
        if result.is_some() {
            return;
        }
        if pos == self.total_len {
            *result = Some(current.clone());
            return;
        }
        if pos > self.total_len {
            return;
        }
        for (end, syllable) in &self.edges[pos] {
            current.push(syllable.clone());
            self.dfs_from(*end, current, result);
            current.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_path() {
        let seg = vec!["zhong".to_string(), "guo".to_string(), "ren".to_string()];
        let graph = SyllableGraph::from_single_segmentation(&seg);

        assert_eq!(graph.total_len(), 11);
        assert_eq!(graph.edges_from(0), &[(5, "zhong".to_string())]);
        assert_eq!(graph.edges_from(5), &[(8, "guo".to_string())]);
        assert_eq!(graph.edges_from(8), &[(11, "ren".to_string())]);

        let paths = graph.all_paths();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], vec!["zhong", "guo", "ren"]);
    }

    #[test]
    fn test_multiple_paths() {
        // 模拟 "zhongguoren" 的两种切分：
        // zhong-guo-ren 和 zhong-gu-o-ren（后者不合法但用于测试合并）
        let segs = vec![
            vec!["zhong".to_string(), "guo".to_string(), "ren".to_string()],
            vec![
                "zhong".to_string(),
                "gu".to_string(),
                "o".to_string(),
                "ren".to_string(),
            ],
        ];
        let graph = SyllableGraph::from_segmentations(&segs);

        assert_eq!(graph.total_len(), 11);

        // 位置 0 应该只有 "zhong"
        let edges_0 = graph.edges_from(0);
        assert_eq!(edges_0.len(), 1);
        assert_eq!(edges_0[0], (5, "zhong".to_string()));

        // 位置 5 应该有 "guo" 和 "gu"
        let edges_5 = graph.edges_from(5);
        assert_eq!(edges_5.len(), 2);

        // 应该能枚举出 2 条路径
        let paths = graph.all_paths();
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn test_reachable_ends() {
        let seg = vec!["zhong".to_string(), "guo".to_string(), "ren".to_string()];
        let graph = SyllableGraph::from_single_segmentation(&seg);
        let ends = graph.reachable_ends();
        assert_eq!(ends, vec![0, 5, 8, 11]);
    }

    #[test]
    fn test_empty_segmentations() {
        let graph = SyllableGraph::from_segmentations(&[]);
        assert_eq!(graph.total_len(), 0);
        assert!(graph.all_paths().is_empty());
    }

    #[test]
    fn test_find_path_from() {
        let seg = vec!["zhong".to_string(), "guo".to_string(), "ren".to_string()];
        let graph = SyllableGraph::from_single_segmentation(&seg);

        assert_eq!(
            graph.find_path_from(0),
            Some(vec![
                "zhong".to_string(),
                "guo".to_string(),
                "ren".to_string()
            ])
        );
        assert_eq!(
            graph.find_path_from(5),
            Some(vec!["guo".to_string(), "ren".to_string()])
        );
        assert_eq!(graph.find_path_from(8), Some(vec!["ren".to_string()]));
        assert_eq!(graph.find_path_from(11), Some(Vec::<String>::new()));
        assert_eq!(graph.find_path_from(100), None);
    }

    #[test]
    fn test_find_path_from_multiple() {
        let segs = vec![
            vec!["zhong".to_string(), "guo".to_string(), "ren".to_string()],
            vec![
                "zhong".to_string(),
                "gu".to_string(),
                "o".to_string(),
                "ren".to_string(),
            ],
        ];
        let graph = SyllableGraph::from_segmentations(&segs);

        let path = graph.find_path_from(5);
        assert!(path.is_some());
        let path = path.unwrap();
        // 应该返回从位置 5 开始的某条有效路径
        assert!(
            path == vec!["guo", "ren"] || path == vec!["gu", "o", "ren"],
            "unexpected path: {:?}",
            path
        );
    }
}
