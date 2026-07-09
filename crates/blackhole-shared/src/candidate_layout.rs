use crate::Candidate;

pub const CANDIDATE_WINDOW_WIDTH: f32 = 520.0;
/// 内层可用宽度（扣除 Frame inner_margin 10 * 2）
pub const AVAILABLE_WIDTH: f32 = CANDIDATE_WINDOW_WIDTH - 20.0;
/// 展开状态可用宽度（与折叠状态保持一致，实现同一个界面）
pub const EXPANDED_AVAILABLE_WIDTH: f32 = AVAILABLE_WIDTH;
pub const ITEM_SPACING: f32 = 12.0;
pub const MAX_PER_ROW: usize = 9;
/// 候选词项外层 Frame 的左右内边距之和（inner_margin 4 * 2）
pub const FRAME_PADDING_X: f32 = 8.0;

/// 估算单个候选词的显示宽度（含 Frame padding）
pub fn estimate_candidate_item_width(candidate: &Candidate) -> f32 {
    let label_w = 20.0f32; // "N." 在 14px 下约 16-18px
    let char_width = 16.0f32; // 16px 中文字体实际 advance 约 16px
    label_w + candidate.text.chars().count() as f32 * char_width + FRAME_PADDING_X
}

/// 将候选列表按可用宽度分行
pub fn layout_candidates_into_rows(
    candidates: &[Candidate],
    available_width: f32,
    item_spacing: f32,
) -> Vec<Vec<usize>> {
    layout_candidates_into_rows_excluding(candidates, available_width, item_spacing, None)
}

/// 将候选列表按可用宽度分行，可排除指定索引
pub fn layout_candidates_into_rows_excluding(
    candidates: &[Candidate],
    available_width: f32,
    item_spacing: f32,
    excluded_index: Option<usize>,
) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut current_row: Vec<usize> = Vec::new();
    let mut current_width = 0.0f32;

    for (i, candidate) in candidates.iter().enumerate() {
        if excluded_index == Some(i) {
            continue;
        }
        let w = estimate_candidate_item_width(candidate);
        let would_exceed_width =
            !current_row.is_empty() && current_width + item_spacing + w > available_width;
        let would_exceed_count = !current_row.is_empty() && current_row.len() >= MAX_PER_ROW;
        if would_exceed_width || would_exceed_count {
            rows.push(current_row);
            current_row = vec![i];
            current_width = w;
        } else {
            if !current_row.is_empty() {
                current_width += item_spacing;
            }
            current_row.push(i);
            current_width += w;
        }
    }
    if !current_row.is_empty() {
        rows.push(current_row);
    }
    rows
}

/// 网格导航方向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridDirection {
    Left,
    Right,
    Up,
    Down,
}

/// 计算折叠状态下第二行第一个候选词的全局索引
///
/// 折叠时第二行显示的是除 selected_index 外的候选词，按原始索引顺序排列。
/// 此函数返回该序列中的第一个索引。
pub fn second_row_first_candidate_index(candidates: &[Candidate], selected_index: usize) -> usize {
    if candidates.len() <= 1 {
        return selected_index;
    }
    for i in 0..candidates.len() {
        if i != selected_index {
            return i;
        }
    }
    selected_index
}

/// 将数字键（1-9）映射为全局候选索引
///
/// - 展开状态：数字对应当前选中行内的列偏移
/// - 折叠状态：数字对应第二行（非选中候选词）中的第N个
pub fn digit_to_candidate_index(
    candidates: &[Candidate],
    selected_index: usize,
    expanded: bool,
    digit: usize,
) -> Option<usize> {
    digit_to_candidate_index_excluding(candidates, selected_index, expanded, digit, None)
}

/// 将数字键（1-9）映射为全局候选索引，可排除指定索引
///
/// - 展开状态：数字对应当前选中行内的列偏移
///   若 selected_index 被排除，则数字对应网格第一行（即 UI 第二行）的列偏移
/// - 折叠状态：数字对应第二行（非选中候选词）中的第N个
pub fn digit_to_candidate_index_excluding(
    candidates: &[Candidate],
    selected_index: usize,
    expanded: bool,
    digit: usize,
    excluded_index: Option<usize>,
) -> Option<usize> {
    if digit == 0 || digit > 9 {
        return None;
    }

    if expanded {
        let rows = layout_candidates_into_rows_excluding(
            candidates,
            EXPANDED_AVAILABLE_WIDTH,
            ITEM_SPACING,
            excluded_index,
        );
        // 若 selected_index 被排除，默认使用网格第一行（UI 第二行）
        let current_row_idx = if excluded_index == Some(selected_index) {
            0
        } else {
            rows.iter().position(|row| row.contains(&selected_index))?
        };
        let row = rows.get(current_row_idx)?;
        let col = digit - 1;
        if col < row.len() {
            Some(row[col])
        } else {
            None
        }
    } else {
        // 折叠状态：序号对应第二行中第N个非选中候选词
        let mut visible_count = 0;
        for i in 0..candidates.len() {
            if i == selected_index {
                continue;
            }
            if visible_count >= 9 {
                return None;
            }
            if visible_count == digit - 1 {
                return Some(i);
            }
            visible_count += 1;
        }
        None
    }
}

/// 在候选网格中导航，返回新的选中索引
pub fn navigate_grid(
    candidates: &[Candidate],
    selected_index: usize,
    available_width: f32,
    direction: GridDirection,
) -> Option<usize> {
    navigate_grid_excluding(candidates, selected_index, available_width, direction, None)
}

/// 在候选网格中导航，可排除指定索引
///
/// 若 selected_index 被排除，则视其在网格上方，仅 Down 方向可进入网格第一行
pub fn navigate_grid_excluding(
    candidates: &[Candidate],
    selected_index: usize,
    available_width: f32,
    direction: GridDirection,
    excluded_index: Option<usize>,
) -> Option<usize> {
    let rows = layout_candidates_into_rows_excluding(
        candidates,
        available_width,
        ITEM_SPACING,
        excluded_index,
    );

    // 若 selected_index 被排除，特殊处理：仅 Down 可进入网格第一行
    if excluded_index == Some(selected_index) {
        return match direction {
            GridDirection::Down => rows.first().and_then(|row| row.first()).copied(),
            _ => None,
        };
    }

    let current_row_idx = rows.iter().position(|row| row.contains(&selected_index))?;
    let current_col_idx = rows[current_row_idx]
        .iter()
        .position(|&idx| idx == selected_index)?;

    match direction {
        GridDirection::Left => current_col_idx
            .checked_sub(1)
            .map(|c| rows[current_row_idx][c]),
        GridDirection::Right => {
            let row = &rows[current_row_idx];
            (current_col_idx + 1 < row.len()).then(|| row[current_col_idx + 1])
        }
        GridDirection::Up => current_row_idx.checked_sub(1).map(|r| {
            let prev_row = &rows[r];
            prev_row[current_col_idx.min(prev_row.len().saturating_sub(1))]
        }),
        GridDirection::Down => rows
            .get(current_row_idx + 1)
            .map(|next_row| next_row[current_col_idx.min(next_row.len().saturating_sub(1))]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Candidate;

    fn make_candidates(count: usize) -> Vec<Candidate> {
        (0..count)
            .map(|i| Candidate {
                text: format!("{}", (b'a' + i as u8) as char),
                comment: None,
                score: 0,
            })
            .collect()
    }

    #[test]
    fn test_navigate_grid_down_from_last_column() {
        // 单字词宽度 = 20 + 16 + 8 = 44，间距 12
        // 9个 = 9*44 + 8*12 = 492 <= 500
        // 10个 = 548 > 500，所以第一行9个，第二行8个
        let candidates = make_candidates(17);
        // 从第一行第9位（索引8）按 Down → 第二行第8位（原始索引16）
        assert_eq!(
            navigate_grid(&candidates, 8, AVAILABLE_WIDTH, GridDirection::Down),
            Some(16)
        );
    }

    #[test]
    fn test_navigate_grid_up_from_last_column() {
        let candidates = make_candidates(17);
        // 从第二行第8位（原始索引16）按 Up → 第一行第8位（原始索引7）
        assert_eq!(
            navigate_grid(&candidates, 16, AVAILABLE_WIDTH, GridDirection::Up),
            Some(7)
        );
    }

    #[test]
    fn test_navigate_grid_up_from_first_row() {
        let candidates = make_candidates(17);
        // 从第一行第1位（索引0）按 Up → None
        assert_eq!(
            navigate_grid(&candidates, 0, AVAILABLE_WIDTH, GridDirection::Up),
            None
        );
    }

    #[test]
    fn test_navigate_grid_down_from_last_row() {
        let candidates = make_candidates(17);
        // 从最后一行最后一位（索引16）按 Down → None
        assert_eq!(
            navigate_grid(&candidates, 16, AVAILABLE_WIDTH, GridDirection::Down),
            None
        );
    }

    #[test]
    fn test_navigate_grid_right_boundary() {
        let candidates = make_candidates(17);
        // 从第一行最后一位（索引8）按 Right → None
        assert_eq!(
            navigate_grid(&candidates, 8, AVAILABLE_WIDTH, GridDirection::Right),
            None
        );
        // 从第二行最后一位（索引16）按 Right → None
        assert_eq!(
            navigate_grid(&candidates, 16, AVAILABLE_WIDTH, GridDirection::Right),
            None
        );
    }

    #[test]
    fn test_navigate_grid_left_boundary() {
        let candidates = make_candidates(17);
        // 从第一行第1位（索引0）按 Left → None
        assert_eq!(
            navigate_grid(&candidates, 0, AVAILABLE_WIDTH, GridDirection::Left),
            None
        );
        // 从第二行第1位（索引9）按 Left → None
        assert_eq!(
            navigate_grid(&candidates, 9, AVAILABLE_WIDTH, GridDirection::Left),
            None
        );
    }

    #[test]
    fn test_navigate_grid_cross_row_mismatched_columns() {
        let candidates = make_candidates(17);
        // 第一行有9个(0-8)，第二行有8个(9-16)
        // 从第一行第5位(索引4)按 Down → 第二行第5位(原始索引12)
        assert_eq!(
            navigate_grid(&candidates, 4, AVAILABLE_WIDTH, GridDirection::Down),
            Some(13) // 第二行: 9,10,11,12,13,14,15,16 → 索引4 = 13
        );
        // 从第二行第5位(原始索引13)按 Up → 第一行第5位(原始索引4)
        assert_eq!(
            navigate_grid(&candidates, 13, AVAILABLE_WIDTH, GridDirection::Up),
            Some(4)
        );
    }

    #[test]
    fn test_digit_to_index_expanded() {
        let candidates = make_candidates(17);
        // EXPANDED_AVAILABLE_WIDTH = 500（与 AVAILABLE_WIDTH 保持一致）
        // 单字词宽度 = 20 + 16 + 8 = 44，间距 12
        // 9个 = 9*44 + 8*12 = 492 <= 500
        // 10个 = 548 > 500，所以第一行9个(0-8)，第二行8个(9-16)
        // 选中第二行第2位(原始索引10)
        // 按1 → 第二行第1位(原始索引9)
        assert_eq!(digit_to_candidate_index(&candidates, 10, true, 1), Some(9));
        // 按2 → 第二行第2位(原始索引10，即当前选中)
        assert_eq!(digit_to_candidate_index(&candidates, 10, true, 2), Some(10));
        // 按3 → 第二行第3位(原始索引11)
        assert_eq!(digit_to_candidate_index(&candidates, 10, true, 3), Some(11));
        // 按8 → 第二行第8位(原始索引16)
        assert_eq!(digit_to_candidate_index(&candidates, 10, true, 8), Some(16));
        // 按9 → 超出范围（第二行只有8个）
        assert_eq!(digit_to_candidate_index(&candidates, 10, true, 9), None);
    }

    #[test]
    fn test_digit_to_index_expanded_first_row() {
        let candidates = make_candidates(17);
        // 选中第一行第1位(原始索引0)
        // 按1 → 第一行第1位(原始索引0)
        assert_eq!(digit_to_candidate_index(&candidates, 0, true, 1), Some(0));
        // 按8 → 第一行第8位(原始索引7)
        assert_eq!(digit_to_candidate_index(&candidates, 0, true, 8), Some(7));
        // 按9 → 第一行第9位(原始索引8)
        assert_eq!(digit_to_candidate_index(&candidates, 0, true, 9), Some(8));
        // 按10 → 超出范围（第一行只有9个）
        assert_eq!(digit_to_candidate_index(&candidates, 0, true, 10), None);
    }

    #[test]
    fn test_digit_to_index_collapsed() {
        let candidates = make_candidates(12);
        // 折叠状态，selected_index = 0
        // 第二行显示候选词1..11，序号1对应全局索引1
        assert_eq!(digit_to_candidate_index(&candidates, 0, false, 1), Some(1));
        assert_eq!(digit_to_candidate_index(&candidates, 0, false, 9), Some(9));

        // 折叠状态，selected_index = 3
        // 第二行跳过索引3，显示0,1,2,4,5,6,7,8,9
        assert_eq!(digit_to_candidate_index(&candidates, 3, false, 1), Some(0));
        assert_eq!(digit_to_candidate_index(&candidates, 3, false, 4), Some(4));
        assert_eq!(digit_to_candidate_index(&candidates, 3, false, 9), Some(9));
    }

    #[test]
    fn test_digit_to_index_invalid_digit() {
        let candidates = make_candidates(5);
        assert_eq!(digit_to_candidate_index(&candidates, 0, true, 0), None);
        assert_eq!(digit_to_candidate_index(&candidates, 0, true, 10), None);
    }
}
