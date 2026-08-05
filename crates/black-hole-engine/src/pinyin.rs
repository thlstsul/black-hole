use crate::{Codec, CodecState, SyllableGraph};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::OnceLock;

/// 拼音音节切分器
pub struct PinyinCodec {
    syllables: Vec<String>,
    raw_input: String,
    full_input: String,
    valid_syllables: &'static HashSet<String>,
    /// 存储所有可能的切分结果（用于多策略查询）
    all_segmentations: Vec<Vec<String>>,
    /// 优化：缓存历史计算结果，避免删除时重复计算
    segmentation_cache: std::collections::HashMap<String, Vec<Vec<String>>>,
}

impl Default for PinyinCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl PinyinCodec {
    pub fn new() -> Self {
        Self {
            syllables: Vec::new(),
            raw_input: String::new(),
            full_input: String::new(),
            valid_syllables: build_syllable_set(),
            all_segmentations: Vec::new(),
            segmentation_cache: std::collections::HashMap::new(),
        }
    }

    pub fn syllables(&self) -> &[String] {
        &self.syllables
    }

    /// 返回基于完整输入重新做一次性贪心切分的音节列表
    ///
    /// 与 `syllables()` 的区别：该方法忽略增量切分历史，
    /// 直接对 `full_input` 做最长匹配，避免增量切分导致的
    /// 局部最优问题（如 "gu" + "o" 被错误切分为 "gu" 和 "o"）。
    pub fn syllables_resegmented(&self) -> Vec<String> {
        greedy_segment(&self.full_input, self.valid_syllables).0
    }

    /// 返回完整的原始输入（包括已切分和未切分部分）
    pub fn full_code(&self) -> String {
        self.syllables.join("") + &self.raw_input
    }

    /// 返回空格分隔的拼音编码（基于完整输入的最长匹配切分），用于匹配 RIME 词库
    pub fn spaced_code(&self) -> String {
        self.syllables_resegmented().join(" ")
    }

    /// 返回所有可能的切分结果（空格分隔的字符串列表）
    pub fn all_spaced_codes(&self) -> Vec<String> {
        self.all_segmentations
            .iter()
            .map(|seg| seg.join(" "))
            .collect()
    }

    /// 返回音节切分图（DAG）
    pub fn syllable_graph(&self) -> SyllableGraph {
        SyllableGraph::from_segmentations(&self.all_segmentations)
    }

    /// 返回简拼编码（基于完整输入的最长匹配切分，每个音节的首字母）
    pub fn abbreviated_code(&self) -> String {
        self.syllables_resegmented()
            .iter()
            .filter_map(|s| s.chars().next())
            .collect()
    }

    /// 删除最后一个字符，返回是否成功删除
    pub fn pop(&mut self) -> bool {
        if self.full_input.is_empty() {
            return false;
        }
        self.full_input.pop();
        self.rebuild();
        !self.full_input.is_empty()
    }

    fn rebuild(&mut self) {
        tracing::debug!("rebuild start: input='{}'", self.full_input);
        self.syllables.clear();
        self.raw_input.clear();

        // 优化：先检查缓存，避免重复计算
        if let Some(cached) = self.segmentation_cache.get(&self.full_input) {
            self.all_segmentations = cached.clone();
        } else {
            self.all_segmentations.clear();
        }

        let chars: Vec<char> = self.full_input.chars().collect();
        for ch in chars {
            self.raw_input.push(ch);
            self.try_segment();
        }

        // 如果缓存中没有，才重新计算
        if self.all_segmentations.is_empty() && !self.full_input.is_empty() {
            self.compute_all_segmentations();
            // 限制缓存大小，防止无限增长
            if self.segmentation_cache.len() >= 100 {
                self.segmentation_cache.clear();
            }
            self.segmentation_cache
                .insert(self.full_input.clone(), self.all_segmentations.clone());
        }

        tracing::debug!(
            "rebuild end: input='{}', syllables={}, segmentations={}",
            self.full_input,
            self.syllables.len(),
            self.all_segmentations.len()
        );
    }

    /// 尝试将当前累积的 raw_input 切分为有效拼音音节
    /// 使用贪心最长匹配策略（保留向后兼容）
    fn try_segment(&mut self) {
        let (result, remaining) = greedy_segment(&self.raw_input, self.valid_syllables);
        if !result.is_empty() {
            self.syllables.extend(result);
            self.raw_input = remaining;
        }
    }

    /// 使用动态规划计算所有可能的切分结果
    fn compute_all_segmentations(&mut self) {
        if self.full_input.is_empty() {
            return;
        }
        self.all_segmentations =
            dp_segment(&self.full_input, self.valid_syllables, MAX_SEGMENTATIONS);
        if self.all_segmentations.is_empty() {
            self.compute_partial_segmentations();
        }
    }

    /// 计算部分切分（允许末尾有未识别的字符）
    fn compute_partial_segmentations(&mut self) {
        let len = self.full_input.len();
        for end_pos in (1..=len).rev() {
            let prefix = &self.full_input[..end_pos];
            let result = dp_segment(prefix, self.valid_syllables, MAX_SEGMENTATIONS);
            if !result.is_empty() {
                self.all_segmentations = result;
                break;
            }
        }
    }
}

impl Codec for PinyinCodec {
    fn push(&mut self, ch: char) -> CodecState {
        if !ch.is_ascii_alphabetic() {
            return CodecState::Rejected;
        }
        self.full_input.push(ch.to_ascii_lowercase());
        self.rebuild();
        CodecState::Accepted
    }

    fn code(&self) -> &str {
        &self.full_input
    }

    fn reset(&mut self) {
        self.syllables.clear();
        self.raw_input.clear();
        self.full_input.clear();
        self.all_segmentations.clear();
        // 优化：保留缓存，因为后续可能还会用到
        // self.segmentation_cache.clear();
    }
}

const MAX_SEGMENTATIONS: usize = 50;

/// 贪心最长匹配切分
///
/// 对给定输入字符串，从前往后做最长匹配，返回切分结果和剩余未识别部分。
fn greedy_segment(input: &str, valid_syllables: &HashSet<String>) -> (Vec<String>, String) {
    let mut remaining = input;
    let mut result = Vec::new();

    while !remaining.is_empty() {
        let mut found = false;
        for len in (1..=remaining.len().min(6)).rev() {
            let candidate = &remaining[..len];
            if valid_syllables.contains(candidate) {
                result.push(candidate.to_string());
                remaining = &remaining[len..];
                found = true;
                break;
            }
        }
        if !found {
            break;
        }
    }

    (result, remaining.to_string())
}

/// 共享尾部的切分路径节点：路径用单向链表表示，扩展一个音节只分配一个新节点，
/// 尾部复用已有路径，避免对整条路径做深拷贝（消除 dp 表的 O(路径长度) 克隆）。
struct SegPathNode {
    syllable: String,
    rest: Option<Rc<SegPathNode>>,
}

/// 切分路径：`None` 表示空路径（基础情况）
type SegPath = Option<Rc<SegPathNode>>;

/// 将链表路径物化为字符串列表（头节点即路径首音节）
fn materialize_path(path: &SegPath) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = path;
    while let Some(node) = cur {
        out.push(node.syllable.clone());
        cur = &node.rest;
    }
    out
}

/// 使用动态规划计算所有可能的切分结果
///
/// 对给定输入字符串，从后往前做 DP，返回从位置 0 开始的所有完整切分。
/// 路径使用共享尾部链表存储：每次扩展只做 O(1) 的新节点分配，
/// 整体开销从 O(n²×max)（每次克隆整条路径）降为 O(n×6×max)，
/// 长输入逐键重建不再随长度二次增长。
fn dp_segment(
    input: &str,
    valid_syllables: &HashSet<String>,
    max_segmentations: usize,
) -> Vec<Vec<String>> {
    let len = input.len();
    let mut dp: Vec<Vec<SegPath>> = vec![Vec::new(); len + 1];
    dp[len] = vec![None]; // 基础情况：空字符串有一种切分方式

    for i in (0..len).rev() {
        for syllable_len in 1..=6 {
            if i + syllable_len > len {
                break;
            }

            let syllable = &input[i..i + syllable_len];
            if valid_syllables.contains(syllable) {
                if dp[i].len() >= max_segmentations {
                    continue;
                }

                // 浅拷贝：SegPath 是 Rc 指针，克隆仅递增引用计数，
                // 路径体共享，不复制音节字符串。
                let rest_segments = dp[i + syllable_len].clone();
                for rest in rest_segments {
                    if dp[i].len() >= max_segmentations {
                        break;
                    }
                    dp[i].push(Some(Rc::new(SegPathNode {
                        syllable: syllable.to_string(),
                        rest,
                    })));
                }
            }
        }
    }

    dp[0].iter().map(materialize_path).collect()
}

/// 构建有效拼音音节集合（使用 OnceLock 全局缓存）
fn build_syllable_set() -> &'static HashSet<String> {
    static SET: OnceLock<HashSet<String>> = OnceLock::new();
    SET.get_or_init(|| {
        let syllables = [
            "a", "ai", "an", "ang", "ao", "ba", "bai", "ban", "bang", "bao", "bei", "ben", "beng",
            "bi", "bian", "biao", "bie", "bin", "bing", "bo", "bu", "ca", "cai", "can", "cang",
            "cao", "ce", "cen", "ceng", "cha", "chai", "chan", "chang", "chao", "che", "chen",
            "cheng", "chi", "chong", "chou", "chu", "chua", "chuai", "chuan", "chuang", "chui",
            "chun", "chuo", "ci", "cong", "cou", "cu", "cuan", "cui", "cun", "cuo", "da", "dai",
            "dan", "dang", "dao", "de", "dei", "den", "deng", "di", "dia", "dian", "diao", "die",
            "ding", "diu", "dong", "dou", "du", "duan", "dui", "dun", "duo", "e", "ei", "en",
            "eng", "er", "fa", "fan", "fang", "fei", "fen", "feng", "fiao", "fo", "fou", "fu",
            "ga", "gai", "gan", "gang", "gao", "ge", "gei", "gen", "geng", "gong", "gou", "gu",
            "gua", "guai", "guan", "guang", "gui", "gun", "guo", "ha", "hai", "han", "hang", "hao",
            "he", "hei", "hen", "heng", "hong", "hou", "hu", "hua", "huai", "huan", "huang", "hui",
            "hun", "huo", "ji", "jia", "jian", "jiang", "jiao", "jie", "jin", "jing", "jiong",
            "jiu", "ju", "juan", "jue", "jun", "ka", "kai", "kan", "kang", "kao", "ke", "kei",
            "ken", "keng", "kong", "kou", "ku", "kua", "kuai", "kuan", "kuang", "kui", "kun",
            "kuo", "la", "lai", "lan", "lang", "lao", "le", "lei", "leng", "li", "lia", "lian",
            "liang", "liao", "lie", "lin", "ling", "liu", "lo", "long", "lou", "lu", "luan", "lun",
            "luo", "lv", "lve", "ma", "mai", "man", "mang", "mao", "me", "mei", "men", "meng",
            "mi", "mian", "miao", "mie", "min", "ming", "miu", "mo", "mou", "mu", "na", "nai",
            "nan", "nang", "nao", "ne", "nei", "nen", "neng", "ni", "nian", "niang", "niao", "nie",
            "nin", "ning", "niu", "nong", "nou", "nu", "nuan", "nun", "nuo", "nv", "nve", "o",
            "ou", "pa", "pai", "pan", "pang", "pao", "pei", "pen", "peng", "pi", "pian", "piao",
            "pie", "pin", "ping", "po", "pou", "pu", "qi", "qia", "qian", "qiang", "qiao", "qie",
            "qin", "qing", "qiong", "qiu", "qu", "quan", "que", "qun", "ran", "rang", "rao", "re",
            "ren", "reng", "ri", "rong", "rou", "ru", "rua", "ruan", "rui", "run", "ruo", "sa",
            "sai", "san", "sang", "sao", "se", "sen", "seng", "sha", "shai", "shan", "shang",
            "shao", "she", "shei", "shen", "sheng", "shi", "shou", "shu", "shua", "shuai", "shuan",
            "shuang", "shui", "shun", "shuo", "si", "song", "sou", "su", "suan", "sui", "sun",
            "suo", "ta", "tai", "tan", "tang", "tao", "te", "tei", "teng", "ti", "tian", "tiao",
            "tie", "ting", "tong", "tou", "tu", "tuan", "tui", "tun", "tuo", "wa", "wai", "wan",
            "wang", "wei", "wen", "weng", "wo", "wu", "xi", "xia", "xian", "xiang", "xiao", "xie",
            "xin", "xing", "xiong", "xiu", "xu", "xuan", "xue", "xun", "ya", "yan", "yang", "yao",
            "ye", "yi", "yin", "ying", "yo", "yong", "you", "yu", "yuan", "yue", "yun", "za",
            "zai", "zan", "zang", "zao", "ze", "zei", "zen", "zeng", "zha", "zhai", "zhan",
            "zhang", "zhao", "zhe", "zhei", "zhen", "zheng", "zhi", "zhong", "zhou", "zhu", "zhua",
            "zhuai", "zhuan", "zhuang", "zhui", "zhun", "zhuo", "zi", "zong", "zou", "zu", "zuan",
            "zui", "zun", "zuo",
        ];

        syllables.iter().map(|s| s.to_string()).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pinyin_segmentation() {
        let mut codec = PinyinCodec::new();
        for ch in "zhongwen".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["zhong", "wen"]);
        assert_eq!(codec.code(), "zhongwen");
        assert_eq!(codec.full_code(), "zhongwen");
    }

    #[test]
    fn test_pinyin_partial() {
        let mut codec = PinyinCodec::new();
        for ch in "zhongw".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["zhong"]);
        assert_eq!(codec.code(), "zhongw");
        assert_eq!(codec.full_code(), "zhongw");
    }

    #[test]
    fn test_pinyin_multiple_syllables() {
        let mut codec = PinyinCodec::new();
        for ch in "shurufa".chars() {
            codec.push(ch);
        }
        assert_eq!(codec.syllables(), &["shu", "ru", "fa"]);
        assert_eq!(codec.code(), "shurufa");
        assert_eq!(codec.full_code(), "shurufa");
    }

    #[test]
    fn test_all_segmentations() {
        let mut codec = PinyinCodec::new();
        for ch in "zhongwen".chars() {
            codec.push(ch);
        }

        let all_codes = codec.all_spaced_codes();
        // 应该至少包含一种切分结果
        assert!(!all_codes.is_empty(), "应该有至少一种切分结果");
        // 检查是否包含 "zhong wen" 或其他合理切分
        let has_valid_segmentation = all_codes
            .iter()
            .any(|s| s.contains("zhong") && s.contains("wen"));
        assert!(
            has_valid_segmentation,
            "应该包含有效的切分结果，实际: {:?}",
            all_codes
        );
    }

    #[test]
    fn test_abbreviated_code() {
        let mut codec = PinyinCodec::new();
        for ch in "zhongwen".chars() {
            codec.push(ch);
        }

        let abbreviated = codec.abbreviated_code();
        assert_eq!(abbreviated, "zw");
    }

    #[test]
    fn test_complex_segmentation() {
        let mut codec = PinyinCodec::new();
        for ch in "xiangan".chars() {
            codec.push(ch);
        }

        let syllables = codec.syllables();
        // 应该是 ["xiang", "an"] 或类似的合理切分
        assert!(!syllables.is_empty(), "应该有切分结果");

        let all_codes = codec.all_spaced_codes();
        // 应该至少有一种切分
        assert!(!all_codes.is_empty(), "应该有至少一种切分结果");
    }

    #[test]
    fn test_partial_input_segmentation() {
        let mut codec = PinyinCodec::new();
        for ch in "zhong".chars() {
            codec.push(ch);
        }

        // 完整输入时，应该能切分出音节
        let syllables = codec.syllables();
        assert!(syllables.contains(&"zhong".to_string()));

        // 所有切分结果应该有
        let all_codes = codec.all_spaced_codes();
        assert!(!all_codes.is_empty(), "应该有切分结果");
    }
}
