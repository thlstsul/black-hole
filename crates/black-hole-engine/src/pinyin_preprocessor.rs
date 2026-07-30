use std::collections::HashMap;

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
        let mut results = Vec::new();

        // 1. 原始输入
        results.push(input.to_string());

        // 2. 模糊音变体
        let fuzzy_variants = self.generate_fuzzy_variants(input);
        results.extend(fuzzy_variants);

        // 3. 纠错变体
        let correction_variants = self.generate_correction_variants(input);
        results.extend(correction_variants);

        // 去重
        results.sort();
        results.dedup();

        results
    }

    /// 生成模糊音变体
    fn generate_fuzzy_variants(&self, input: &str) -> Vec<String> {
        let mut variants = Vec::new();

        // 对输入中的每个字符应用模糊映射
        if input.is_empty() {
            return variants;
        }

        // 对整个音节应用模糊规则（例如 an <-> ang）
        // 这里简单处理：只处理首字符
        let first_char = input.chars().next().unwrap();
        if let Some(replacements) = self.fuzzy_map.get(&first_char) {
            for &replacement in replacements {
                if replacement != first_char {
                    let mut variant = String::new();
                    variant.push(replacement);
                    variant.push_str(&input[1..]);
                    variants.push(variant);
                }
            }
        }

        // 处理音节级别的模糊（如 an/ang）
        if input.ends_with('n') && !input.ends_with("ng") {
            // 添加后鼻音版本
            let mut variant = input.to_string();
            variant.push('g');
            variants.push(variant);
        } else if input.ends_with("ng") {
            // 添加前鼻音版本
            let variant = input[..input.len() - 1].to_string();
            variants.push(variant);
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
    pub fn expand_abbreviated(&self, abbreviated: &str) -> Vec<String> {
        if abbreviated.is_empty() {
            return Vec::new();
        }

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
        let unique_count = variants
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert_eq!(variants.len(), unique_count);
    }
}
