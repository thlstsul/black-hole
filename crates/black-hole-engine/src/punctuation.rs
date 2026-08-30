/// 将英文标点符号映射为中文标点符号。
///
/// 当输入法激活且当前无编码输入时，按下英文标点键直接输出对应的中文标点。
/// 若当前有编码输入，则先提交当前候选再追加中文标点。
pub fn convert_punctuation(ch: char) -> Option<char> {
    match ch {
        ',' => Some('，'),
        '.' => Some('。'),
        '/' => Some('、'),
        ';' => Some('；'),
        '\'' => Some('‘'),
        '[' => Some('【'),
        ']' => Some('】'),
        '`' => Some('·'),
        '-' => Some('－'),
        '=' => Some('＝'),
        '!' => Some('！'),
        '#' => Some('＃'),
        '$' => Some('￥'),
        '%' => Some('％'),
        '^' => Some('…'),
        '&' => Some('＆'),
        '*' => Some('×'),
        '(' => Some('（'),
        ')' => Some('）'),
        '_' => Some('—'),
        '+' => Some('＋'),
        '{' => Some('｛'),
        '}' => Some('｝'),
        '|' => Some('｜'),
        ':' => Some('：'),
        '"' => Some('“'),
        '<' => Some('《'),
        '>' => Some('》'),
        '?' => Some('？'),
        '\\' => Some('、'),
        '@' => Some('·'),
        '~' => Some('～'),
        _ => None,
    }
}

/// 中文引号配对器。
///
/// 解决"中文引号只能输入一边"的问题：按下 `'` / `"` 键时应交替输出左/右引号，
/// 而不是固定输出左引号。
///
/// 判定规则（优先级从高到低）：
/// 1. 优先依据光标前文（`ctx.preceding_text`）：前文最后一个同类引号为左引号（未闭合）时，
///    输出右引号将其闭合；否则输出左引号。这能正确处理光标移动、手动删除等真实文档状态。
/// 2. 前文不可用（如应用不支持读取周围文本）时，回退到内部状态机：相邻两次按键交替输出左右引号。
#[derive(Debug, Default)]
pub struct QuotePair {
    /// 上一次输出的左引号类型（`'‘'` 或 `'“'`），用于前文不可用时的交替回退。
    last_left: Option<char>,
}

impl QuotePair {
    pub fn new() -> Self {
        Self { last_left: None }
    }

    /// 重置配对状态（方案 reset 时调用）。
    pub fn reset(&mut self) {
        self.last_left = None;
    }

    /// 根据按键与光标前文决定应输出的中文引号，并同步内部状态。
    /// `ch` 仅接受 `'` 或 `"`，其余字符返回原值。
    pub fn next(&mut self, ch: char, preceding: Option<&str>) -> char {
        let (left, right) = match ch {
            '\'' => ('‘', '’'),
            '"' => ('“', '”'),
            _ => return ch,
        };

        if let Some(text) = preceding {
            // 从末尾向前找最近的一个同类引号
            let last_quote = text.chars().rev().find(|c| *c == left || *c == right);
            let out = match last_quote {
                Some(c) if c == left => right,
                _ => left,
            };
            // 同步内部状态，使后续前文不可用时也能延续交替
            self.last_left = if out == left { Some(left) } else { None };
            return out;
        }

        // 前文不可用时的交替回退
        if self.last_left == Some(left) {
            self.last_left = None;
            right
        } else {
            self.last_left = Some(left);
            left
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quote_pair_context() {
        let mut q = QuotePair::new();
        // 前文为空 → 左引号
        assert_eq!(q.next('"', None), '“');
        // 前文最后一个同类引号是右引号（已闭合）→ 左引号
        assert_eq!(q.next('"', Some("他说“你好”")), '“');
        // 前文最后一个同类引号是左引号（未闭合）→ 右引号
        assert_eq!(q.next('"', Some("他说“你好")), '”');
        // 前文无引号 → 左引号
        assert_eq!(q.next('"', Some("你好世界")), '“');
    }

    #[test]
    fn test_quote_pair_single_quote() {
        let mut q = QuotePair::new();
        assert_eq!(q.next('\'', None), '‘');
        assert_eq!(q.next('\'', Some("他说‘你好")), '’');
        assert_eq!(q.next('\'', Some("他说‘你好’")), '‘');
    }

    #[test]
    fn test_quote_pair_fallback_alternates() {
        // 前文不可用时回退到内部状态机：连续按键交替输出左右引号
        let mut q = QuotePair::new();
        assert_eq!(q.next('"', None), '“');
        assert_eq!(q.next('"', None), '”');
        assert_eq!(q.next('"', None), '“');
        assert_eq!(q.next('"', None), '”');
    }

    #[test]
    fn test_quote_pair_ignores_other_chars() {
        let mut q = QuotePair::new();
        assert_eq!(q.next('a', None), 'a');
        assert_eq!(q.next(',', None), ',');
    }
}
