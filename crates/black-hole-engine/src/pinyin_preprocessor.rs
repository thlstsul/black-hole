use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;

/// 拼音预处理器
///
/// 提供以下功能：
/// 1. 模糊音支持（zh/z, ch/c, sh/s, n/l, 等）
/// 2. 简拼扩展（zhw -> zhong wen）
/// 3. 常见纠错（形近音、常见拼写错误）
pub struct PinyinPreprocessor {
    /// 模糊音映射
    fuzzy_map: HashMap<char, Vec<char>>,
    /// 常见纠错映射
    correction_map: HashMap<String, Vec<String>>,
}

impl Default for PinyinPreprocessor {
    fn default() -> Self {
        Self::new()
    }
}

impl PinyinPreprocessor {
    pub fn new() -> Self {
        let mut preprocessor = Self {
            fuzzy_map: HashMap::new(),
            correction_map: HashMap::new(),
        };

        preprocessor.init_fuzzy_map();
        preprocessor.init_correction_map();

        preprocessor
    }

    /// 初始化模糊音映射
    fn init_fuzzy_map(&mut self) {
        // 鼻音边音混淆
        self.fuzzy_map.insert('n', vec!['n', 'l']);
        self.fuzzy_map.insert('l', vec!['l', 'n']);

        // 前后鼻音混淆（在音节级别处理）
        // an/ang, en/eng, in/ing
    }

    /// 初始化常见纠错映射
    fn init_correction_map(&mut self) {
        // 常见拼写错误
        self.correction_map
            .insert("zhon".to_string(), vec!["zhong".to_string()]);
        self.correction_map
            .insert("zheng".to_string(), vec!["zheng".to_string()]);
        self.correction_map.insert(
            "chian".to_string(),
            vec!["qian".to_string(), "chang".to_string()],
        );
    }

    /// 对拼音输入进行预处理，返回所有可能的变体
    pub fn preprocess(&self, input: &str) -> Vec<String> {
        let mut results = vec![input.to_string()];
        results.extend(self.generate_fuzzy_variants(input));
        results.extend(self.generate_correction_variants(input));
        results.sort();
        results.dedup();
        results
    }

    /// 生成模糊音变体
    fn generate_fuzzy_variants(&self, input: &str) -> Vec<String> {
        let mut variants = Vec::new();

        // 对整个音节应用模糊规则（例如 an <-> ang）
        // 这里简单处理：只处理首字符；非 ASCII 输入直接跳过（模糊映射只覆盖 ASCII 字母）
        // 空输入由下方 let-else 提前返回（chars().next() 返回 None）
        let Some(first_char) = input.chars().next() else {
            return variants;
        };
        if let Some(replacements) = self.fuzzy_map.get(&first_char) {
            let tail = &input[first_char.len_utf8()..];
            for &replacement in replacements {
                if replacement != first_char {
                    let mut variant = String::with_capacity(input.len());
                    variant.push(replacement);
                    variant.push_str(tail);
                    variants.push(variant);
                }
            }
        }

        // 处理音节级别的模糊（如 an/ang）；仅对 "ng" 结尾生成前鼻音变体
        // （"ang"→"an"、"eng"→"en"），其它以裸 'g' 结尾的输入不受影响；
        // strip_suffix 按字符匹配，多字节结尾不会命中
        if input.ends_with('n') && !input.ends_with("ng") {
            // 添加后鼻音版本
            let mut variant = input.to_string();
            variant.push('g');
            variants.push(variant);
        } else if let Some(prefix) = input.strip_suffix("ng") {
            // 添加前鼻音版本（"ng" → "n"）
            variants.push(format!("{}n", prefix));
        }

        variants
    }

    /// 生成纠错变体
    fn generate_correction_variants(&self, input: &str) -> Vec<String> {
        let mut variants = Vec::new();

        // 检查是否有匹配的纠错规则
        for (prefix, corrections) in &self.correction_map {
            if input.starts_with(prefix) {
                for correction in corrections {
                    let variant = format!("{}{}", correction, &input[prefix.len()..]);
                    variants.push(variant);
                }
            }
        }

        variants
    }

    /// 扩展简拼为可能的全拼组合
    /// 例如：zw -> ["zhongwen", "ziwen", "zhuangwang", ...]
    pub fn expand_abbreviated(&self, _abbreviated: &str) -> Vec<String> {
        // 这里返回空，因为完整扩展需要大量计算
        // 实际使用时应根据上下文智能扩展
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuzzy_variants() {
        let preprocessor = PinyinPreprocessor::new();

        // 测试鼻音边音模糊
        let variants = preprocessor.preprocess("nan");
        assert!(variants.contains(&"nan".to_string()));
        assert!(variants.contains(&"lan".to_string()));

        // 测试前后鼻音模糊
        let variants = preprocessor.preprocess("an");
        assert!(variants.contains(&"an".to_string()));
        assert!(variants.contains(&"ang".to_string()));
    }

    #[test]
    fn test_correction_variants() {
        let preprocessor = PinyinPreprocessor::new();

        // 测试纠错
        let variants = preprocessor.preprocess("zhonwen");
        assert!(variants.contains(&"zhongwen".to_string()));
    }

    #[test]
    fn test_no_duplicates() {
        let preprocessor = PinyinPreprocessor::new();

        let variants = preprocessor.preprocess("zhong");
        // 确保没有重复
        let unique_count = variants.iter().collect::<HashSet<_>>().len();
        assert_eq!(variants.len(), unique_count);
    }

    #[test]
    fn test_non_ascii_input_does_not_panic() {
        // 模糊音/纠错路径假定 ASCII 拼音输入；多字节输入不应 panic
        // （此前按字节切片 input[1..] / input[..len-1] 会越界）
        let preprocessor = PinyinPreprocessor::new();

        for input in ["中文", "测试", "中", "a中", "中n", "n中"] {
            let variants = preprocessor.preprocess(input);
            // 原始输入始终在结果中，且不产生崩溃
            assert!(variants.contains(&input.to_string()), "input={input}");
        }
    }

    #[test]
    fn test_ng_suffix_only_generates_front_nasal_variant() {
        // 前鼻音变体只对 "ng" 结尾生成（"ang"→"an"、"eng"→"en"），
        // 其它以裸 'g' 结尾的输入（g/ag/hg）不得产生丢字母变体
        let preprocessor = PinyinPreprocessor::new();

        // "ng" 结尾：生成前鼻音变体
        let variants = preprocessor.preprocess("ang");
        assert!(variants.contains(&"an".to_string()));
        let variants = preprocessor.preprocess("eng");
        assert!(variants.contains(&"en".to_string()));

        // 裸 'g' 结尾：不生成去掉 'g' 的变体（如 "g"→"" 或 "ag"→"a"）
        for input in ["g", "ag", "hg", "中g"] {
            let variants = preprocessor.preprocess(input);
            assert!(
                !variants.contains(&String::new()),
                "input={input}: unexpected empty variant"
            );
            assert!(
                !variants.contains(&input[..input.len().saturating_sub(1)].to_string()),
                "input={input}: trailing 'g' should not be dropped"
            );
            // 原始输入始终保留
            assert!(variants.contains(&input.to_string()), "input={input}");
        }
    }
}
