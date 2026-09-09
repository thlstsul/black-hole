use crate::punctuation::{QuotePair, convert_punctuation};
use crate::{Dictionary, UserDictionary};
use black_hole_shared::candidate_layout::{
    EXPANDED_AVAILABLE_WIDTH, GridDirection, digit_to_candidate_index_excluding,
    navigate_grid_excluding,
};
use black_hole_shared::{Candidate, InputContext, KeyEvent, SchemeId, SchemeResult};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::{Arc, Mutex};

/// 临时英文模式下的按键处理（两套方案逻辑完全一致）。
/// 返回 `Some(result)` 表示已处理，`None` 表示未进入临时英文模式。
pub(super) fn handle_temporary_english_key(
    english_buffer: &mut Option<String>,
    key: &KeyEvent,
) -> Option<SchemeResult> {
    let buffer = english_buffer.as_mut()?;
    Some(match key.key.as_str() {
        "Backspace" => {
            buffer.pop();
            if buffer.is_empty() {
                *english_buffer = None;
                SchemeResult::Committed {
                    text: String::new(),
                    temporary_english: false,
                }
            } else {
                SchemeResult::Composing {
                    code: buffer.clone(),
                    candidates: vec![],
                    selected_index: 0,
                    expanded: false,
                }
            }
        }
        "Escape" => {
            *english_buffer = None;
            SchemeResult::Cancelled
        }
        "Space" => {
            let text = format!("{} ", buffer);
            *english_buffer = None;
            SchemeResult::Committed {
                text,
                temporary_english: true,
            }
        }
        "Enter" => {
            let text = buffer.clone();
            *english_buffer = None;
            SchemeResult::Committed {
                text,
                temporary_english: true,
            }
        }
        _ => {
            let Some(ch) = key
                .key
                .chars()
                .next()
                .filter(|&c| key.key.len() == 1 && c.is_ascii_alphabetic())
            else {
                return Some(SchemeResult::Ignored);
            };
            buffer.push(ch);
            SchemeResult::Composing {
                code: buffer.clone(),
                candidates: vec![],
                selected_index: 0,
                expanded: false,
            }
        }
    })
}

/// 将按键转换为中文标点（引号配对 + 标点映射）。
/// 返回 `Some(char)` 表示转换成功，`None` 表示非标点键应忽略。
pub(super) fn convert_to_cn_punct(
    ch: char,
    quote_pair: &mut QuotePair,
    ctx: &InputContext,
) -> Option<char> {
    if ch == '\'' || ch == '"' {
        Some(quote_pair.next(ch, ctx.preceding_text.as_deref()))
    } else {
        convert_punctuation(ch)
    }
}

/// 方向键导航（Left/Right/Up/Down）的共享逻辑。
/// 返回 `Some(result)` 表示已处理，`None` 表示按键不匹配。
pub(super) fn handle_arrow_key(
    key: &str,
    code: String,
    candidates: Vec<Candidate>,
    selected_index: &mut usize,
    expanded: &mut bool,
) -> Option<SchemeResult> {
    let direction = match key {
        "ArrowLeft" => GridDirection::Left,
        "ArrowRight" => GridDirection::Right,
        "ArrowDown" => GridDirection::Down,
        "ArrowUp" => GridDirection::Up,
        _ => return None,
    };

    if candidates.is_empty() {
        return Some(SchemeResult::Ignored);
    }

    // Left/Right/Up 要求已展开，Down 首次展开
    match direction {
        GridDirection::Down => {
            if !*expanded {
                *expanded = true;
                if *selected_index == 0 && candidates.len() > 1 {
                    *selected_index = 1;
                }
                return Some(SchemeResult::Composing {
                    code,
                    candidates,
                    selected_index: *selected_index,
                    expanded: *expanded,
                });
            }
        }
        _ => {
            if !*expanded {
                return Some(SchemeResult::Ignored);
            }
        }
    }

    match navigate_grid_excluding(
        &candidates,
        *selected_index,
        EXPANDED_AVAILABLE_WIDTH,
        direction,
        Some(0),
    ) {
        Some(new_index) => {
            *selected_index = new_index;
            Some(SchemeResult::Composing {
                code,
                candidates,
                selected_index: *selected_index,
                expanded: *expanded,
            })
        }
        None => {
            // Up 到顶时折叠
            if direction == GridDirection::Up {
                *expanded = false;
                if *selected_index != 0 {
                    *selected_index = 0;
                }
                Some(SchemeResult::Composing {
                    code,
                    candidates,
                    selected_index: *selected_index,
                    expanded: *expanded,
                })
            } else {
                Some(SchemeResult::Ignored)
            }
        }
    }
}

/// 数字键（0-9）的解析结果。
pub(super) enum DigitKeyAction {
    /// 无编码时直接上屏该数字。
    Direct(SchemeResult),
    /// 有编码时选中对应候选。
    Select(usize),
    /// 按键不匹配或候选不存在，应忽略。
    Ignore,
}

/// 从当前候选列表中按选中索引取出上屏文本。
/// 候选列表为空时返回 `None`。
pub(super) fn pick_candidate_text(
    candidates: &[Candidate],
    selected_index: usize,
) -> Option<String> {
    if candidates.is_empty() {
        return None;
    }
    let idx = selected_index.min(candidates.len().saturating_sub(1));
    Some(candidates[idx].text.clone())
}

/// 数字键（0-9）选候选的共享逻辑。
pub(super) fn handle_digit_key(
    code: &str,
    key: &KeyEvent,
    candidates: &[Candidate],
    selected_index: usize,
    expanded: bool,
) -> DigitKeyAction {
    if code.is_empty() {
        return DigitKeyAction::Direct(SchemeResult::Committed {
            text: key.key.clone(),
            temporary_english: false,
        });
    }
    let Ok(digit) = key.key.parse::<usize>() else {
        return DigitKeyAction::Ignore;
    };
    if digit == 0 {
        return DigitKeyAction::Ignore;
    }
    match digit_to_candidate_index_excluding(candidates, selected_index, expanded, digit, Some(0)) {
        Some(index) => DigitKeyAction::Select(index),
        None => DigitKeyAction::Ignore,
    }
}

/// 候选去重集合：保证同一文本只插入一次，并提供分数合并策略。
pub(super) struct CandidateSet<'a> {
    candidates: &'a mut Vec<Candidate>,
    seen: &'a mut FxHashSet<String>,
}

impl<'a> CandidateSet<'a> {
    pub(super) fn new(candidates: &'a mut Vec<Candidate>, seen: &'a mut FxHashSet<String>) -> Self {
        Self { candidates, seen }
    }

    /// 插入新候选；已存在时忽略。
    pub(super) fn push(&mut self, text: String, score: i64, comment: Option<String>) {
        if self.seen.insert(text.clone()) {
            self.candidates.push(Candidate {
                text,
                comment,
                score,
            });
        }
    }

    /// 插入新候选；已存在时取较高分数。
    pub(super) fn push_or_replace(&mut self, text: String, score: i64, comment: Option<String>) {
        if self.seen.insert(text.clone()) {
            self.candidates.push(Candidate {
                text,
                comment,
                score,
            });
        } else if let Some(existing) = self.candidates.iter_mut().find(|c| c.text == text)
            && score > existing.score
        {
            existing.score = score;
        }
    }

    /// 插入新候选；已存在时累加分数并覆盖注释。
    pub(super) fn push_or_boost(&mut self, text: String, score: i64, comment: Option<String>) {
        if self.seen.insert(text.clone()) {
            self.candidates.push(Candidate {
                text,
                comment,
                score,
            });
        } else if let Some(existing) = self.candidates.iter_mut().find(|c| c.text == text) {
            existing.score += score;
            if let Some(c) = comment {
                existing.comment = Some(c);
            }
        }
    }
}

/// 用户词频上屏 boost。
pub(super) fn user_boost(score: i64) -> i64 {
    (score * 50).min(3000) + 500
}

/// 记录用户上屏：校验精确匹配后写入用户词典并刷新词频缓存。
pub(super) fn record_user_commit(
    code: &str,
    text: &str,
    dictionary: &dyn Dictionary,
    user_dict: Option<Arc<Mutex<UserDictionary>>>,
    scheme_id: SchemeId,
    user_freq_cache: &mut FxHashMap<String, i64>,
) {
    if code.is_empty() || text == code {
        return;
    }
    let exact_match = dictionary.lookup(code).iter().any(|c| c.text == text);
    if !exact_match {
        return;
    }
    if let Some(ud) = user_dict {
        ud.lock().unwrap().record_commit(scheme_id, code, text);
        user_freq_cache.remove(text);
    }
}
