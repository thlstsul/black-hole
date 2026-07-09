use crate::{Codec, CodecState, SyllableGraph};

/// 小鹤双拼编解码器
///
/// 将双拼按键序列转换为全拼编码，用于查询拼音词典。
/// 小鹤双拼键位映射（标准）：
/// - 声母：b/c/d/f/g/h/j/k/l/m/n/p/q/r/s/t/w/x/y/z 不变，ch→i, sh→u, zh→v
/// - 韵母：a→a, b→in, c→ao, d→ai, e→e, f→en, g→eng, h→ang, i→i,
///   j→an, k→uai, l→uang/iang, m→ian, n→iao, o→o/uo, p→ie, q→iu,
///   r→uan, s→ong/iong, t→ue, u→u, v→ui/ü, w→ei, x→ia/ua, y→un,
///   z→ou, ;→ing
/// - 零声母：以韵母首字母为零声母（a/e/o）。单字母韵母重复（aa/oo/ee），
///   双字母韵母保持全拼方式（ai/ei/ao/ou/an/en/er），
///   三字母韵母为首字母加韵母所在键（ah→ang, eg→eng, os→ong）。
///   i/u/ü 开头用 y/w/y 作声母加韵母映射。
pub struct ShuangpinCodec {
    syllables: Vec<String>,
    pending: String,
    full_input: String,
}

impl Default for ShuangpinCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl ShuangpinCodec {
    pub fn new() -> Self {
        Self {
            syllables: Vec::new(),
            pending: String::new(),
            full_input: String::new(),
        }
    }

    pub fn syllables(&self) -> &[String] {
        &self.syllables
    }

    /// 返回转换后的全拼编码（已解析音节 + pending 声母前缀）
    pub fn full_code(&self) -> String {
        let mut result =
            String::with_capacity(self.syllables.iter().map(|s| s.len()).sum::<usize>() + 3);
        for s in &self.syllables {
            result.push_str(s);
        }
        if let Some(ch) = self.pending.chars().next() {
            if let Some(prefix) = initial_to_pinyin(ch) {
                result.push_str(prefix);
            } else {
                result.push(ch);
            }
        }
        result
    }

    /// 返回空格分隔的全拼音节编码，用于匹配 RIME 词库中的多音节词
    pub fn spaced_code(&self) -> String {
        self.syllables.join(" ")
    }

    /// 返回音节切分图（DAG），用于整句解码
    pub fn syllable_graph(&self) -> SyllableGraph {
        SyllableGraph::from_single_segmentation(&self.syllables)
    }

    /// 删除最后一个字符，返回是否成功删除
    pub fn pop(&mut self) -> bool {
        if self.full_input.is_empty() {
            return false;
        }
        self.full_input.pop();

        if !self.pending.is_empty() {
            self.pending.pop();
        } else if !self.syllables.is_empty() {
            self.syllables.pop();
            // 双拼每个音节固定占2个输入字符
            self.pending = self.full_input[self.syllables.len() * 2..].to_string();
        }

        !self.full_input.is_empty()
    }

    fn is_valid_key(&self, ch: char) -> bool {
        ch.is_ascii_alphabetic() || ch == ';'
    }
}

impl Codec for ShuangpinCodec {
    fn push(&mut self, ch: char) -> CodecState {
        if !self.is_valid_key(ch) {
            return CodecState::Rejected;
        }
        let ch = ch.to_ascii_lowercase();
        self.full_input.push(ch);

        // 增量解析：若 pending 中恰有1个字符，尝试与新字符组成双拼对
        if self.pending.len() == 1 {
            let first = self.pending.chars().next().unwrap();
            if let Some(syllable) = decode_pair(first, ch) {
                self.syllables.push(syllable);
                self.pending.clear();
                return CodecState::Accepted;
            }
        }

        self.pending.push(ch);
        CodecState::Accepted
    }

    fn code(&self) -> &str {
        &self.full_input
    }

    fn reset(&mut self) {
        self.syllables.clear();
        self.pending.clear();
        self.full_input.clear();
    }
}

/// 将声母键转换为全拼声母前缀
const fn initial_to_pinyin(ch: char) -> Option<&'static str> {
    get_initial(ch)
}

/// 获取声母键对应的全拼声母前缀
const fn get_initial(ch: char) -> Option<&'static str> {
    match ch {
        'b' => Some("b"),
        'c' => Some("c"),
        'd' => Some("d"),
        'f' => Some("f"),
        'g' => Some("g"),
        'h' => Some("h"),
        'i' => Some("ch"),
        'j' => Some("j"),
        'k' => Some("k"),
        'l' => Some("l"),
        'm' => Some("m"),
        'n' => Some("n"),
        'p' => Some("p"),
        'q' => Some("q"),
        'r' => Some("r"),
        's' => Some("s"),
        't' => Some("t"),
        'u' => Some("sh"),
        'v' => Some("zh"),
        'w' => Some("w"),
        'x' => Some("x"),
        'y' => Some("y"),
        'z' => Some("z"),
        _ => None,
    }
}

/// 将两字符解码为全拼音节
fn decode_pair(first: char, second: char) -> Option<String> {
    // 零声母单韵母（首字母重复）
    match (first, second) {
        ('a', 'a') => return Some("a".to_string()),
        ('o', 'o') => return Some("o".to_string()),
        ('e', 'e') => return Some("e".to_string()),
        _ => {}
    }

    // 尝试作为辅音声母 + 韵母解码
    if let Some(initial) = get_initial(first) {
        return decode_consonant_pair(initial, second);
    }

    // 零声母解码：首字母非辅音声母（a/e/o），作为韵母首字母
    // 规则：双字母韵母保持全拼方式，一/三字母韵母为首字母加韵母所在键
    match (first, second) {
        // a-group: ai(2字母全拼), ao(2字母全拼), an(2字母全拼), ang(3字母→a+h)
        ('a', 'i') => Some("ai".to_string()),
        ('a', 'o') => Some("ao".to_string()),
        ('a', 'n') => Some("an".to_string()),
        ('a', 'h') => Some("ang".to_string()),
        // e-group: ei(2字母全拼), en(2字母全拼), er(2字母全拼), eng(3字母→e+g)
        ('e', 'i') => Some("ei".to_string()),
        ('e', 'n') => Some("en".to_string()),
        ('e', 'r') => Some("er".to_string()),
        ('e', 'g') => Some("eng".to_string()),
        // o-group: ou(2字母全拼), ong(3字母→o+s)
        ('o', 'u') => Some("ou".to_string()),
        ('o', 's') => Some("ong".to_string()),
        _ => None,
    }
}

/// 辅音声母 + 韵母解码
fn decode_consonant_pair(initial: &str, second: char) -> Option<String> {
    let final_str = match second {
        'a' => "a",
        'b' => "in",
        'c' => "ao",
        'd' => "ai",
        'e' => "e",
        'f' => "en",
        'g' => "eng",
        'h' => "ang",
        'i' => "i",
        'j' => "an",
        'k' => match initial {
            "zh" | "ch" | "sh" | "g" | "k" | "h" | "w" => "uai",
            _ => "ing",
        },
        'l' => match initial {
            "j" | "q" | "x" | "n" | "l" => "iang",
            _ => "uang",
        },
        'm' => "ian",
        'n' => "iao",
        'o' => match initial {
            "b" | "p" | "m" | "f" | "w" => "o",
            _ => "uo",
        },
        'p' => "ie",
        'q' => "iu",
        'r' => "uan",
        's' => match initial {
            "j" | "q" | "x" => "iong",
            _ => "ong",
        },
        't' => "ue",
        'u' => "u",
        'v' => match initial {
            "l" | "n" => "v",
            _ => "ui",
        },
        'w' => "ei",
        'x' => match initial {
            "j" | "q" | "x" => "ia",
            _ => "ua",
        },
        'y' => "un",
        'z' => "ou",
        ';' => "ing",
        _ => return None,
    };

    let mut result = String::with_capacity(initial.len() + final_str.len());
    result.push_str(initial);
    result.push_str(final_str);
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shuangpin_basic() {
        let mut codec = ShuangpinCodec::new();
        for ch in "vs".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["zhong"]);
        assert_eq!(codec.full_code(), "zhong");
    }

    #[test]
    fn test_shuangpin_multiple() {
        let mut codec = ShuangpinCodec::new();
        for ch in "vsgo".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["zhong", "guo"]);
        assert_eq!(codec.full_code(), "zhongguo");
    }

    #[test]
    fn test_shuangpin_zero_syllable_ang() {
        // ah → ang（3字母韵母：a + h键）
        let mut codec = ShuangpinCodec::new();
        for ch in "ah".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["ang"]);
        assert_eq!(codec.full_code(), "ang");
    }

    #[test]
    fn test_shuangpin_zero_syllable_ai() {
        // ai → ai（2字母韵母：保持全拼）
        let mut codec = ShuangpinCodec::new();
        for ch in "ai".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["ai"]);
        assert_eq!(codec.full_code(), "ai");
    }

    #[test]
    fn test_shuangpin_zero_syllable_en() {
        // en → en（2字母韵母：保持全拼）
        let mut codec = ShuangpinCodec::new();
        for ch in "en".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["en"]);
        assert_eq!(codec.full_code(), "en");
    }

    #[test]
    fn test_shuangpin_zero_syllable_ou() {
        // ou → ou（2字母韵母：保持全拼）
        let mut codec = ShuangpinCodec::new();
        for ch in "ou".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["ou"]);
        assert_eq!(codec.full_code(), "ou");
    }

    #[test]
    fn test_shuangpin_zero_syllable_eng() {
        // eg → eng（3字母韵母：e + g键）
        let mut codec = ShuangpinCodec::new();
        for ch in "eg".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["eng"]);
        assert_eq!(codec.full_code(), "eng");
    }

    #[test]
    fn test_shuangpin_zero_syllable_ong() {
        // os → ong（3字母韵母：o + s键）
        let mut codec = ShuangpinCodec::new();
        for ch in "os".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["ong"]);
        assert_eq!(codec.full_code(), "ong");
    }

    #[test]
    fn test_shuangpin_partial() {
        let mut codec = ShuangpinCodec::new();
        codec.push('v');
        assert_eq!(codec.full_code(), "zh");
    }

    #[test]
    fn test_shuangpin_ing() {
        let mut codec = ShuangpinCodec::new();
        for ch in "d;".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["ding"]);
    }

    #[test]
    fn test_shuangpin_spaced_code() {
        let mut codec = ShuangpinCodec::new();
        for ch in "vsgo".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.spaced_code(), "zhong guo");
        assert_eq!(codec.syllable_graph().total_len(), 8); // zhong(5) + guo(3) = 8
    }

    #[test]
    fn test_shuangpin_lv_nv() {
        // l+v 应解析为 lv（ü 用 v 表示，与词典编码一致）
        let mut codec = ShuangpinCodec::new();
        for ch in "lv".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["lv"]);
        assert_eq!(codec.full_code(), "lv");

        // n+v 应解析为 nv
        let mut codec = ShuangpinCodec::new();
        for ch in "nv".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["nv"]);
        assert_eq!(codec.full_code(), "nv");
    }

    #[test]
    fn test_shuangpin_yong() {
        // y+s 应解析为 yong，不是 yiong
        let mut codec = ShuangpinCodec::new();
        for ch in "ys".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["yong"]);
        assert_eq!(codec.full_code(), "yong");
    }

    #[test]
    fn test_shuangpin_bo_po_mo_fo_wo() {
        // b/p/m/f/w + o 应解析为 o，不是 uo
        let mut codec = ShuangpinCodec::new();
        for ch in "bo".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["bo"]);

        let mut codec = ShuangpinCodec::new();
        for ch in "po".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["po"]);

        let mut codec = ShuangpinCodec::new();
        for ch in "wo".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["wo"]);
    }

    #[test]
    fn test_shuangpin_duo_tuo_guo() {
        // d/t/g + o 应解析为 uo，不是 o
        let mut codec = ShuangpinCodec::new();
        for ch in "do".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["duo"]);

        let mut codec = ShuangpinCodec::new();
        for ch in "to".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["tuo"]);

        let mut codec = ShuangpinCodec::new();
        for ch in "go".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["guo"]);
    }

    #[test]
    fn test_shuangpin_er() {
        // e+r 应解析为 er（儿/而/二等），e 不是辅音声母键
        let mut codec = ShuangpinCodec::new();
        for ch in "er".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["er"]);
        assert_eq!(codec.full_code(), "er");
    }

    #[test]
    fn test_shuangpin_k_ing_uai() {
        // k 键双功能：ing / uai
        // j/q/x/y/b/p/m/d/t/n/l + k → ing
        let mut codec = ShuangpinCodec::new();
        for ch in "jk".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["jing"]);

        let mut codec = ShuangpinCodec::new();
        for ch in "qk".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["qing"]);

        let mut codec = ShuangpinCodec::new();
        for ch in "dk".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["ding"]);

        let mut codec = ShuangpinCodec::new();
        for ch in "yk".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["ying"]);

        // zh/ch/sh/g/k/h/w + k → uai
        let mut codec = ShuangpinCodec::new();
        for ch in "kk".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["kuai"]);

        let mut codec = ShuangpinCodec::new();
        for ch in "hk".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["huai"]);

        let mut codec = ShuangpinCodec::new();
        for ch in "ik".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["chuai"]);
    }
}
