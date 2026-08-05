//! 基于 `rime-dict` crate 的词典实现（替代原 SQLite 词典）。
//!
//! 编译管线：`.dict.yaml` 解析 → 递归 import → 缺码词条自动注音（pinyin crate）
//! → `PrismBin` / `TableBin` 构建 → 缓存到本地 `.prism.bin` / `.table.bin`。
//!
//! 查询映射：
//! - `lookup`（精确）→ `Table::query_phrases`（四级以上长词条按完整编码过滤）
//! - `prefix_lookup`（前缀）→ 音节切分 + 末音节前缀扩展 + `Table::query_prefix`
//!   （切分路径去重 + 前缀结果按音节序列缓存，避免重复输入时的全量重查）

use crate::{Dictionary, LanguageModel};
use black_hole_shared::Candidate;
use pinyin::ToPinyin;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use ::rime_dict::{
    DictYaml, Prism, PrismBuilder, SpellingAlgebra, SyllableId, Table, TableBuilder, crc32,
};

/// 词条构建输入（供测试与内置词典使用）
pub use ::rime_dict::RawEntry;

/// Prism 构建所用的字母集
const ALPHABET: &str = "abcdefghijklmnopqrstuvwxyz";
/// 前缀查询最终结果上限
const PREFIX_RESULT_LIMIT: usize = 500;
/// 前缀查询单次最多枚举的切分路径数（防长句输入组合爆炸）
const MAX_SEGMENTATION_PATHS: usize = 32;
/// 前缀查询缓存条目：`(text, score)` 列表
type PrefixEntries = Vec<(String, i64)>;

/// 首音节前缀缓存中每个音节的词条上限
const PREFIX1_CACHE_LIMIT: usize = 100;

/// 精确查询缓存上限，超出后整体清空（防御性，防编码前缀组合无限增长）
const LOOKUP_CACHE_LIMIT: usize = 1024;

/// `prefix_lookup` 性能统计（用于定位卡顿：枚举路径数 vs 实际表查询次数）
#[derive(Default)]
struct PrefixLookupStats {
    /// 实际发起的 `Table::query_prefix` 次数（未命中缓存的多音节前缀 + 缓存未命中的单音节）
    queries: usize,
    /// 单音节前缀缓存命中次数
    cache_hits: usize,
    /// 多音节前缀查询返回的原始词条总数
    entries: usize,
    /// `Table::query_prefix` 累计耗时
    query_elapsed: std::time::Duration,
}

/// RIME 词库加载错误
#[derive(Debug, thiserror::Error)]
pub enum RimeDictError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("rime-dict error: {0}")]
    RimeDict(#[from] ::rime_dict::Error),
}

/// 基于 rime-dict 二进制格式（.prism.bin + .table.bin）的词典
pub struct RimeDict {
    prism_bin: Vec<u8>,
    table_bin: Vec<u8>,
    /// 音节 id → 音节文本（已排序，与 prism/table 内部 id 一致）
    syllabary: Vec<String>,
    /// 音节文本 → 音节 id
    syllable_to_id: FxHashMap<String, SyllableId>,
    /// 前缀查询缓存（音节 id 序列 → `(text, score)` 列表），惰性填充。
    /// 单音节前缀缓存 top N；多音节前缀结果天然较小，全量缓存。
    prefix_cache: RwLock<FxHashMap<Vec<SyllableId>, PrefixEntries>>,
    /// 精确查询缓存（编码字符串 → 候选）。词典加载后只读，结果恒定；
    /// 输入逐键增长时相邻按键共享前缀编码，命中率高。
    lookup_cache: RwLock<FxHashMap<String, Vec<Candidate>>>,
    /// 语言模型（惰性构建，共享实例只建一次）
    lm: OnceLock<LanguageModel>,
}

impl RimeDict {
    /// 从词条列表直接构建（内存编译，用于内置词典与测试）
    pub fn from_entries(entries: Vec<RawEntry>) -> Result<Self, RimeDictError> {
        let syllabary = collect_syllabary(&entries);
        let algebra = SpellingAlgebra::new(vec![]);
        let prism_bin = PrismBuilder::build(&syllabary, &algebra, ALPHABET, 0, 0).into_bytes();
        let table_bin = TableBuilder::build(&entries, 0)?.into_bytes();
        Self::from_bins(prism_bin, table_bin)
    }

    /// 从已编译的二进制映像加载
    fn from_bins(prism_bin: Vec<u8>, table_bin: Vec<u8>) -> Result<Self, RimeDictError> {
        // 加载校验并读取音节表（table 与 prism 的音节 id 一致：均为排序去重列表）
        let table = Table::load(&table_bin)?;
        let syllabary = table.syllabary_entries()?;
        Prism::load(&prism_bin)?;

        let syllable_to_id: FxHashMap<String, SyllableId> = syllabary
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), i as SyllableId))
            .collect();

        Ok(Self {
            prism_bin,
            table_bin,
            syllabary,
            syllable_to_id,
            prefix_cache: RwLock::new(FxHashMap::default()),
            lookup_cache: RwLock::new(FxHashMap::default()),
            lm: OnceLock::new(),
        })
    }

    /// 从 RIME 词库文件加载并编译（不使用磁盘缓存）
    ///
    /// 支持 `.dict.yaml`（YAML 头 + TSV 正文）与无头纯码表 `.txt`，
    /// 支持 `import_tables` 递归导入；缺码词条自动生成拼音编码。
    pub fn from_rime_dict(path: impl AsRef<Path>) -> Result<Self, RimeDictError> {
        let (entries, checksum) = load_dict_entries(path.as_ref())?;
        Self::compile(&entries, checksum)
    }

    /// 从 RIME 词库文件加载，并缓存编译结果到指定目录。
    ///
    /// 首次加载时编译并写出 `.prism.bin` / `.table.bin`；
    /// 后续若源词库文件均未修改，则直接加载缓存，跳过编译。
    pub fn from_rime_dict_cached(
        src_path: impl AsRef<Path>,
        cache_dir: impl AsRef<Path>,
    ) -> Result<Self, RimeDictError> {
        let src_path = src_path.as_ref();
        let cache_dir = cache_dir.as_ref();
        let _ = std::fs::create_dir_all(cache_dir);

        let (prism_path, table_path) = cache_paths(src_path, cache_dir);

        if cache_is_valid(&prism_path, &table_path, src_path) {
            tracing::info!("Using cached rime dictionary: {}", table_path.display());
            let prism_bin = std::fs::read(&prism_path)?;
            let table_bin = std::fs::read(&table_path)?;
            return Self::from_bins(prism_bin, table_bin);
        }

        tracing::info!("Compiling rime dictionary from: {}", src_path.display());
        let (entries, checksum) = load_dict_entries(src_path)?;
        let dict = Self::compile(&entries, checksum)?;

        // 缓存写出失败不阻塞加载
        let _ = std::fs::write(&prism_path, &dict.prism_bin);
        let _ = std::fs::write(&table_path, &dict.table_bin);

        Ok(dict)
    }

    /// 全局共享加载：同一词库路径在进程内只编译/加载一次
    pub fn shared(path: impl AsRef<Path>, cache_dir: impl AsRef<Path>) -> Option<Arc<RimeDict>> {
        static SHARED_DICTS: OnceLock<Mutex<HashMap<PathBuf, Arc<RimeDict>>>> = OnceLock::new();

        let path = path.as_ref();
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let cache = SHARED_DICTS.get_or_init(|| Mutex::new(HashMap::new()));

        if let Some(dict) = cache.lock().unwrap().get(&key) {
            return Some(dict.clone());
        }

        match Self::from_rime_dict_cached(path, cache_dir) {
            Ok(dict) => {
                let dict = Arc::new(dict);
                cache.lock().unwrap().insert(key, dict.clone());
                Some(dict)
            }
            Err(e) => {
                tracing::error!("Failed to load RIME dictionary: {}", e);
                None
            }
        }
    }

    /// 编译词条为二进制映像并加载
    fn compile(entries: &[RawEntry], checksum: u32) -> Result<Self, RimeDictError> {
        let syllabary = collect_syllabary(entries);
        let algebra = SpellingAlgebra::new(vec![]);
        let prism_bin =
            PrismBuilder::build(&syllabary, &algebra, ALPHABET, checksum, 0).into_bytes();
        let table_bin = TableBuilder::build(entries, checksum)?.into_bytes();
        Self::from_bins(prism_bin, table_bin)
    }

    /// 构建内置小词典（无外部词库时的回退）
    pub fn from_builtin() -> Self {
        // 常见词汇（使用空格分隔的拼音编码，与 RIME 词库格式一致）
        const WORDS: &[(&str, &str, i64)] = &[
            ("zhong wen", "中文", 100),
            ("zhong wen", "中温", 80),
            ("zhong wen", "钟文", 60),
            ("shu ru", "输入", 100),
            ("shu ru", "熟入", 50),
            ("shu ru fa", "输入法", 100),
            ("shu ru fa", "输入阀", 30),
            ("pin yin", "拼音", 100),
            ("pin yin", "品引", 40),
            ("cang jie", "仓颉", 100),
            ("wu bi", "五笔", 100),
            ("han yu", "汉语", 100),
            ("han yu", "韩语", 80),
            ("han zi", "汉字", 100),
            ("ji suan ji", "计算机", 100),
            ("ji suan ji", "计算器", 80),
            ("cheng xu", "程序", 100),
            ("cheng xu", "承续", 50),
            ("bian cheng", "编程", 100),
            ("bian cheng", "边城", 80),
            ("wen zi", "文字", 100),
            ("wen zi", "蚊子", 80),
            ("wen zi", "纹子", 40),
            ("yu yan", "语言", 100),
            ("yu yan", "寓言", 80),
            ("xie xie", "谢谢", 100),
            ("xie xie", "写写", 50),
            ("ni hao", "你好", 100),
            ("zai jian", "再见", 100),
            ("wo men", "我们", 100),
            ("wo men", "我门", 40),
            ("ta men de", "他们的", 100),
            ("zhong guo", "中国", 100),
            ("bei jing", "北京", 100),
            ("bei jing", "背景", 80),
            ("shang hai", "上海", 100),
            ("shang hai", "伤害", 60),
            ("guang zhou", "广州", 100),
            ("shen zhen", "深圳", 100),
            ("xiang gang", "香港", 100),
            ("tai wan", "台湾", 100),
            ("xin wen", "新闻", 100),
            ("dian shi", "电视", 100),
            ("dian nao", "电脑", 100),
            ("shou ji", "手机", 100),
            ("wang luo", "网络", 100),
            ("wang luo", "网罗", 50),
            ("you xi", "游戏", 100),
            ("yin le", "音乐", 100),
            ("dian ying", "电影", 100),
            ("mei shi", "美食", 100),
            ("lv xing", "旅行", 100),
            ("xue xi", "学习", 100),
            ("gong zuo", "工作", 100),
            ("sheng huo", "生活", 100),
            ("shi jie", "世界", 100),
            ("shi jie", "时节", 60),
            ("shi jian", "时间", 100),
            ("shi jian", "事件", 80),
            ("kong jian", "空间", 100),
            ("kong jian", "控件", 70),
            ("ren min", "人民", 100),
            ("gong min", "公民", 100),
            ("guo jia", "国家", 100),
            ("cheng shi", "城市", 100),
            ("xiang cun", "乡村", 100),
            ("dao lu", "道路", 100),
            ("qi che", "汽车", 100),
            ("huo che", "火车", 100),
            ("fei ji", "飞机", 100),
            ("chuan", "船", 100),
            ("zi xing che", "自行车", 100),
            ("di tie xian", "地铁线", 100),
            ("gong jiao che", "公交车", 100),
        ];

        let mut entries: Vec<RawEntry> = WORDS
            .iter()
            .map(|(code, text, weight)| RawEntry {
                text: text.to_string(),
                code: code.to_string(),
                weight: Some(*weight as f32),
            })
            .collect();

        // 常用单字（高频 500 字），通过 pinyin crate 自动生成拼音映射
        let common_chars = "的一是了我不人在他有这个上们来到时大地为子中你说生国年着就那和要她出也得里后自以会家可下而过天去能对小多然于心学之都好看起发当没成只如事把还用第样道想作种开美总从无情己面最女但现前些所同日手又行意动方期它头经长儿回位分爱老因很给名法间斯知世什两次使身者被高已亲其进此话常与活正感";
        for ch in common_chars.chars() {
            if let Some(py) = ch.to_pinyin() {
                let py_str = py.plain();
                if !py_str.is_empty() {
                    entries.push(RawEntry {
                        text: ch.to_string(),
                        code: py_str.to_string(),
                        weight: Some(95.0),
                    });
                }
            }
        }

        Self::from_entries(entries).expect("builtin dictionary should compile")
    }

    /// 流式构建语言模型：聚合 text -> total_score
    pub fn build_language_model(&self) -> LanguageModel {
        self.lm
            .get_or_init(|| {
                self.build_language_model_impl()
                    .unwrap_or_else(|_| LanguageModel::new())
            })
            .clone()
    }

    fn build_language_model_impl(&self) -> Result<LanguageModel, RimeDictError> {
        let table = self.table()?;
        let exported = table.export_entries()?;

        let mut text_scores: FxHashMap<String, i64> = FxHashMap::default();
        for (_, text_id, weight) in exported {
            if let Ok(text) = table.string_table().string(text_id) {
                *text_scores.entry(text).or_insert(0) += (weight as i64).max(1);
            }
        }

        let total: i64 = text_scores.values().sum();
        Ok(LanguageModel::from_text_scores(total, text_scores))
    }

    /// Prism 视图（零拷贝借用，解析仅读头部，开销可忽略）
    fn prism(&self) -> Result<Prism<'_>, ::rime_dict::Error> {
        Prism::load(&self.prism_bin)
    }

    /// Table 视图（零拷贝借用）
    fn table(&self) -> Result<Table<'_>, ::rime_dict::Error> {
        Table::load(&self.table_bin)
    }

    /// 空格分隔编码 → 音节 id 序列；任一音节不在音节表中则返回 None
    fn code_to_ids(&self, code: &str) -> Option<Vec<SyllableId>> {
        code.split_whitespace()
            .map(|s| self.syllable_to_id.get(s).copied())
            .collect()
    }

    /// 音节字符串前缀扩展：`"zhon" → [zhong]`，`"w" → [wa, wai, ..., wu]`
    ///
    /// syllabary 有序（编译期 BTreeSet / 加载期音节表均排序），
    /// 用二分定位前缀区间起点，再向后扫描到第一个不匹配项，
    /// 从 O(音节总数) 降到 O(log N + 命中数)。
    fn expand_syllable_prefix(&self, prefix: &str) -> Vec<SyllableId> {
        if prefix.is_empty() {
            return Vec::new();
        }
        let lo = self.syllabary.partition_point(|s| s.as_str() < prefix);
        let mut out = Vec::new();
        for (i, s) in self.syllabary[lo..].iter().enumerate() {
            if !s.starts_with(prefix) {
                break;
            }
            out.push((lo + i) as SyllableId);
        }
        out
    }

    /// 对输入做全音节切分，返回所有 `(音节 id 路径, 已消费字节数)`。
    ///
    /// 无法继续切分的位置记为死端（剩余输入由调用方按前缀扩展处理）。
    fn segment_paths(&self, input: &str) -> Vec<(Vec<SyllableId>, usize)> {
        let Ok(prism) = self.prism() else {
            return Vec::new();
        };
        let bytes = input.as_bytes();
        let mut out: Vec<(Vec<SyllableId>, usize)> = Vec::new();
        // 中间节点也去重：不同切分路径可能到达相同的 (pos, ids)，
        // 若不去重会重复展开同一子树，枚举量随音节数组合爆炸。
        let mut seen: FxHashSet<(usize, Vec<SyllableId>)> = FxHashSet::default();
        let mut stack: Vec<(usize, Vec<SyllableId>)> = vec![(0, Vec::new())];
        seen.insert((0, Vec::new()));

        while let Some((pos, ids)) = stack.pop() {
            if out.len() >= MAX_SEGMENTATION_PATHS {
                // 路径上限：前缀查询只需 top-N 候选，截断枚举防止长句组合爆炸
                break;
            }
            if pos >= bytes.len() {
                if !ids.is_empty() {
                    out.push((ids, pos));
                }
                continue;
            }
            let matches = prism.common_prefix_search_bytes(&bytes[pos..]);
            if matches.is_empty() {
                // 死端：记录部分路径（含空路径，表示从头即不可切分）
                out.push((ids, pos));
                continue;
            }
            for m in matches {
                if let Ok(spellings) = prism.query_spelling(m.spelling_id) {
                    for s in spellings.flatten() {
                        let mut next_ids = ids.clone();
                        next_ids.push(s.syllable_id);
                        let key = (pos + m.length, next_ids.clone());
                        if seen.insert(key) {
                            stack.push((pos + m.length, next_ids));
                        }
                    }
                }
            }
        }
        out
    }

    /// 前缀查询：收集以 `ids` 为编码前缀的全部词条（含精确命中）。
    ///
    /// 结果按 `ids` 缓存（单音节缓存 top N，多音节全量），重复输入零查表。
    fn collect_prefix(
        &self,
        ids: &[SyllableId],
        out: &mut Vec<(String, i64)>,
        stats: &mut PrefixLookupStats,
    ) {
        if ids.is_empty() {
            return;
        }
        {
            let cache = self.prefix_cache.read().unwrap();
            if let Some(cached) = cache.get(ids) {
                stats.cache_hits += 1;
                out.extend(cached.iter().cloned());
                return;
            }
        }
        let t = std::time::Instant::now();
        let mut entries = self.query_prefix_resolved(ids);
        stats.query_elapsed += t.elapsed();
        stats.queries += 1;
        stats.entries += entries.len();
        if ids.len() == 1 {
            entries.truncate(PREFIX1_CACHE_LIMIT);
        }
        let mut cache = self.prefix_cache.write().unwrap();
        // 防御性清理：防止缓存无限增长（不同音节前缀组合随输入累积）
        if cache.len() > 1024 {
            cache.clear();
        }
        cache.insert(ids.to_vec(), entries.clone());
        out.extend(entries);
    }

    /// `Table::query_prefix` + 文本解析，按权重降序。
    fn query_prefix_resolved(&self, ids: &[SyllableId]) -> Vec<(String, i64)> {
        let Ok(table) = self.table() else {
            return Vec::new();
        };
        let Ok(entries) = table.query_prefix(ids) else {
            return Vec::new();
        };
        let mut out: Vec<(String, i64)> = entries
            .into_iter()
            .filter_map(|(_, text_id, weight)| {
                table
                    .string_table()
                    .string(text_id)
                    .ok()
                    .map(|t| (t, weight as i64))
            })
            .collect();
        out.sort_by_key(|e| std::cmp::Reverse(e.1));
        // 每路只保留 top N：最终 top-100 中的词条在其所在路径内的排名
        // 必然不高于全局排名，按路径截断不会丢失最终 top-100
        // （后续跨路径去重会取每个文本的最高分）。
        out.truncate(PREFIX_RESULT_LIMIT);
        out
    }

    /// 精确查询（不缓存）：编码已拆分为音节 id
    fn lookup_uncached(&self, ids: &[SyllableId]) -> Vec<Candidate> {
        let Ok(table) = self.table() else {
            return Vec::new();
        };
        let Ok(accessor) = table.query_phrases(ids) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        let mut i = 0;
        while let Ok(Some(entry)) = accessor.entry_at(i) {
            // 四级以上长词条需按完整编码过滤（query_phrases 只索引前三级）
            let matches = if ids.len() > ::rime_dict::INDEX_CODE_MAX_LENGTH {
                accessor.code_for(i).map(|c| c == ids).unwrap_or(false)
            } else {
                true
            };
            if matches && let Ok(text) = table.entry_text(&entry) {
                out.push(Candidate {
                    text,
                    comment: None,
                    score: entry.weight as i64,
                });
            }
            i += 1;
        }
        out.sort_by_key(|c| std::cmp::Reverse(c.score));
        out
    }
}

impl Dictionary for RimeDict {
    fn lookup(&self, code: &str) -> Vec<Candidate> {
        // 精确查询结果只取决于编码与只读词典，按编码字符串缓存；
        // 输入逐键增长时共享前缀命中率高，避免重复查表。
        {
            let cache = self.lookup_cache.read().unwrap();
            if let Some(cached) = cache.get(code) {
                return cached.clone();
            }
        }
        let candidates = match self.code_to_ids(code) {
            Some(ids) if !ids.is_empty() => self.lookup_uncached(&ids),
            _ => Vec::new(),
        };
        let mut cache = self.lookup_cache.write().unwrap();
        // 防御性清理：防止不同编码前缀组合随输入累积无限增长
        if cache.len() > LOOKUP_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(code.to_string(), candidates.clone());
        candidates
    }

    fn prefix_lookup(&self, code: &str) -> Vec<Candidate> {
        if code.is_empty() {
            return Vec::new();
        }

        let started = std::time::Instant::now();
        let mut stats = PrefixLookupStats::default();
        let mut paths: usize = 0;
        let mut results: Vec<(String, i64)> = Vec::new();

        if code.contains(' ') {
            // 空格分隔编码：前段必须是完整音节，末段可按字符串前缀扩展
            let parts: Vec<&str> = code.split_whitespace().collect();
            let (fixed, last) = parts.split_at(parts.len() - 1);
            let Some(mut base) = fixed
                .iter()
                .map(|s| self.syllable_to_id.get(*s).copied())
                .collect::<Option<Vec<_>>>()
            else {
                return Vec::new();
            };
            for id in self.expand_syllable_prefix(last[0]) {
                paths += 1;
                base.push(id);
                self.collect_prefix(&base, &mut results, &mut stats);
                base.pop();
            }
        } else {
            // 连续编码：枚举全部音节切分路径，剩余部分按前缀扩展
            // 不同切分可能产生相同音节序列（如 [ni,ang] 既可完整切分、
            // 也可由 [ni]+"ang" 扩展得到），查询前去重避免重复查表
            let mut seen_ids: FxHashSet<Vec<SyllableId>> = FxHashSet::default();
            let mut collect = |ids: Vec<SyllableId>,
                               paths: &mut usize,
                               results: &mut Vec<(String, i64)>,
                               stats: &mut PrefixLookupStats| {
                *paths += 1;
                if seen_ids.insert(ids.clone()) {
                    self.collect_prefix(&ids, results, stats);
                }
            };
            for (ids, consumed) in self.segment_paths(code) {
                let remainder = &code[consumed..];
                if remainder.is_empty() {
                    // 完整切分：前缀查询 + 末音节再按字符串前缀扩展
                    if let Some((&last_id, prefix)) = ids.split_last() {
                        collect(ids.clone(), &mut paths, &mut results, &mut stats);
                        for id in self.expand_syllable_prefix(&self.syllabary[last_id as usize]) {
                            if id == last_id {
                                continue;
                            }
                            let mut p = prefix.to_vec();
                            p.push(id);
                            collect(p, &mut paths, &mut results, &mut stats);
                        }
                    }
                } else {
                    for id in self.expand_syllable_prefix(remainder) {
                        let mut p = ids.clone();
                        p.push(id);
                        collect(p, &mut paths, &mut results, &mut stats);
                    }
                }
            }
        }

        // 按文本去重（保留最高分），降序，截断
        let mut best: FxHashMap<String, i64> = FxHashMap::default();
        for (text, score) in results {
            best.entry(text)
                .and_modify(|s| {
                    if score > *s {
                        *s = score;
                    }
                })
                .or_insert(score);
        }
        // 补充精确匹配结果：确保单字候选（如 碳、炭 等低频字）
        // 不会被多音节词条挤掉（#issue: prefix_lookup 按分数截断时）
        if !code.is_empty() {
            let exact = self.lookup(code);
            for c in &exact {
                best.entry(c.text.clone())
                    .and_modify(|s| {
                        if c.score > *s {
                            *s = c.score;
                        }
                    })
                    .or_insert(c.score);
            }
        }
        let mut out: Vec<Candidate> = best
            .into_iter()
            .map(|(text, score)| Candidate {
                text,
                comment: None,
                score,
            })
            .collect();
        // HashMap 迭代顺序随机，同分候选需以文本作为决胜键，
        // 保证同一编码多次查询返回确定顺序（导航期间候选列表不抖动）
        out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.text.cmp(&b.text)));
        out.truncate(PREFIX_RESULT_LIMIT);

        tracing::debug!(
            "prefix_lookup: code='{}', paths={}, queries={}, cache_hits={}, entries={}, query_us={}, total_us={}",
            code,
            paths,
            stats.queries,
            stats.cache_hits,
            stats.entries,
            stats.query_elapsed.as_micros(),
            started.elapsed().as_micros()
        );
        out
    }
}

/// 共享实例可直接作为词典后端使用（方案切换时零拷贝共享）
impl Dictionary for Arc<RimeDict> {
    fn lookup(&self, code: &str) -> Vec<Candidate> {
        (**self).lookup(code)
    }

    fn prefix_lookup(&self, code: &str) -> Vec<Candidate> {
        (**self).prefix_lookup(code)
    }
}

// 线程安全断言：共享缓存要求 RimeDict 可跨线程使用
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RimeDict>();
};

// ============================================================================
// 编译管线
// ============================================================================

/// 收集词条中的不重复音节（排序，下标即音节 id；与 TableBin 内部排序一致）
fn collect_syllabary(entries: &[RawEntry]) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for entry in entries {
        for syl in entry.code.split_whitespace() {
            if !syl.is_empty() {
                set.insert(syl.to_owned());
            }
        }
    }
    set.into_iter().collect()
}

/// 将纯汉字文本转换为拼音编码（空格分隔）；任一字符无拼音则返回 None
fn text_to_pinyin_code(text: &str) -> Option<String> {
    let syllables: Vec<String> = text
        .chars()
        .filter_map(|ch| ch.to_pinyin().map(|py| py.plain().to_string()))
        .collect();

    if syllables.is_empty() || syllables.len() != text.chars().count() {
        None
    } else {
        Some(syllables.join(" "))
    }
}

/// 解析词库文件内容：`.dict.yaml` 直接解析；无 YAML 头的纯码表按 `[text]` 列合成头部
fn parse_dict_content(content: &str) -> Result<DictYaml, RimeDictError> {
    if content.starts_with("---") || content.contains("\n---") {
        DictYaml::parse(content).map_err(|e| RimeDictError::Parse(e.to_string()))
    } else {
        // 无头纯码表：每行 `文本[\t编码[\t权重]]`，缺码词条后续自动注音
        let synthesized =
            format!("---\nname: custom\ncolumns:\n  - text\n  - code\n  - weight\n...\n{content}");
        DictYaml::parse(&synthesized).map_err(|e| RimeDictError::Parse(e.to_string()))
    }
}

/// 加载词库及全部 import 的词条，返回 `(去重词条, 源文件 CRC-32)`
fn load_dict_entries(src_path: &Path) -> Result<(Vec<RawEntry>, u32), RimeDictError> {
    let content = std::fs::read_to_string(src_path)?;
    let checksum = crc32(content.as_bytes());

    let mut visited = HashSet::new();
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    load_entries_recursive(src_path, &content, &mut visited, &mut entries, &mut seen)?;
    Ok((entries, checksum))
}

/// 递归加载词条：本词典词条优先，import 按声明顺序追加；
/// 按 `(text, code)` 去重并保留先出现者（与 rime-ice「上方词库权重生效」语义一致）。
fn load_entries_recursive(
    path: &Path,
    content: &str,
    visited: &mut HashSet<PathBuf>,
    entries: &mut Vec<RawEntry>,
    seen: &mut HashSet<(String, String)>,
) -> Result<(), RimeDictError> {
    let canonical = path.canonicalize()?;
    if !visited.insert(canonical) {
        return Ok(());
    }

    let dict = parse_dict_content(content)?;

    for mut entry in dict.entries {
        if entry.code.trim().is_empty() {
            // 缺码词条自动注音；无法注音的跳过
            let Some(code) = text_to_pinyin_code(&entry.text) else {
                continue;
            };
            entry.code = code;
        }
        if seen.insert((entry.text.clone(), entry.code.clone())) {
            entries.push(entry);
        }
    }

    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    for name in &dict.import_tables {
        let yaml_path = base_dir.join(format!("{name}.dict.yaml"));
        let txt_path = base_dir.join(format!("{name}.txt"));
        let import_path = if yaml_path.exists() {
            Some(yaml_path)
        } else if txt_path.exists() {
            Some(txt_path)
        } else {
            None
        };
        // 与 RIME 行为一致：导入文件不存在时静默跳过
        if let Some(p) = import_path {
            let import_content = std::fs::read_to_string(&p)?;
            load_entries_recursive(&p, &import_content, visited, entries, seen)?;
        }
    }
    Ok(())
}

/// 缓存文件路径：以源文件路径哈希为名，保证稳定且唯一
fn cache_paths(src_path: &Path, cache_dir: &Path) -> (PathBuf, PathBuf) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let canonical = src_path
        .canonicalize()
        .unwrap_or_else(|_| src_path.to_path_buf());
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    let hash = hasher.finish();

    (
        cache_dir.join(format!("dict_{hash:x}.prism.bin")),
        cache_dir.join(format!("dict_{hash:x}.table.bin")),
    )
}

/// 缓存是否有效：两个缓存文件都存在，且不旧于源目录树下任何词库文件
fn cache_is_valid(prism_path: &Path, table_path: &Path, src_path: &Path) -> bool {
    let (Ok(pm), Ok(tm)) = (prism_path.metadata(), table_path.metadata()) else {
        return false;
    };
    let (Ok(p_mtime), Ok(t_mtime)) = (pm.modified(), tm.modified()) else {
        return false;
    };
    let cache_mtime = p_mtime.min(t_mtime);

    let newer_than_cache = |path: &Path| -> bool {
        path.metadata()
            .and_then(|m| m.modified())
            .map(|t| t > cache_mtime)
            .unwrap_or(false)
    };

    if newer_than_cache(src_path) {
        return false;
    }

    // 递归扫描源目录树（import_tables 可能位于子目录，如 cn_dicts/）
    if let Some(dir) = src_path.parent() {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let ext = p.extension().and_then(|e| e.to_str());
                if matches!(ext, Some("yaml") | Some("txt")) && newer_than_cache(&p) {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn entry(code: &str, text: &str, weight: i64) -> RawEntry {
        RawEntry {
            text: text.to_string(),
            code: code.to_string(),
            weight: Some(weight as f32),
        }
    }

    #[test]
    fn test_from_entries_lookup() {
        let dict = RimeDict::from_entries(vec![
            entry("a", "啊", 1),
            entry("a", "阿", 2),
            entry("ai", "爱", 100),
            entry("zhong wen", "中文", 90),
        ])
        .unwrap();

        let candidates = dict.lookup("a");
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|c| c.text == "啊" && c.score == 1));
        assert!(candidates.iter().any(|c| c.text == "阿" && c.score == 2));

        let candidates = dict.lookup("ai");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].text, "爱");
        assert_eq!(candidates[0].score, 100);

        let candidates = dict.lookup("zhong wen");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].text, "中文");
    }

    #[test]
    fn test_from_entries_lookup_long_phrase() {
        // 四级以上长词条（tail）需按完整编码过滤
        let dict = RimeDict::from_entries(vec![
            entry("a ba fu qin", "阿爸父亲", 50),
            entry("a ba fu", "阿爸父", 100),
            entry("a ba fu ren", "阿爸富人", 30),
        ])
        .unwrap();

        let candidates = dict.lookup("a ba fu qin");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].text, "阿爸父亲");

        let candidates = dict.lookup("a ba fu");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].text, "阿爸父");
    }

    #[test]
    fn test_prefix_lookup_deterministic_order() {
        // 同权重候选经 HashMap 去重，多次查询必须返回确定顺序
        let dict = RimeDict::from_entries(vec![
            entry("ni", "你", 100),
            entry("ni", "尼", 100),
            entry("ni", "泥", 100),
            entry("ni", "逆", 100),
            entry("ni", "匿", 100),
            entry("ni", "腻", 100),
            entry("ni", "妮", 100),
            entry("ni", "霓", 100),
            entry("ni", "倪", 100),
            entry("ni", "坭", 100),
            entry("ni", "猊", 100),
            entry("ni", "怩", 100),
        ])
        .unwrap();

        let first: Vec<String> = dict
            .prefix_lookup("ni")
            .iter()
            .map(|c| c.text.clone())
            .collect();
        assert!(first.len() >= 12, "应返回全部同权重候选");
        for _ in 0..10 {
            let again: Vec<String> = dict
                .prefix_lookup("ni")
                .iter()
                .map(|c| c.text.clone())
                .collect();
            assert_eq!(first, again, "同一编码多次前缀查询的顺序必须一致");
        }
    }

    #[test]
    fn test_from_entries_prefix_lookup() {
        let dict = RimeDict::from_entries(vec![
            entry("zhong", "中", 10),
            entry("zhong wen", "中文", 20),
            entry("zhong guo", "中国", 30),
        ])
        .unwrap();

        // 单音节前缀：单字 + 多字词
        let candidates = dict.prefix_lookup("zhong");
        assert_eq!(candidates.len(), 3);

        // 完整双音节前缀
        let candidates = dict.prefix_lookup("zhong wen");
        assert!(candidates.iter().any(|c| c.text == "中文"));
        assert!(!candidates.iter().any(|c| c.text == "中国"));

        // 部分末音节
        let candidates = dict.prefix_lookup("zhong w");
        assert!(candidates.iter().any(|c| c.text == "中文"));

        // 连续编码前缀
        let candidates = dict.prefix_lookup("zhon");
        assert!(candidates.iter().any(|c| c.text == "中"));
        assert!(candidates.iter().any(|c| c.text == "中文"));
    }

    #[test]
    fn test_rime_dict_basic() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            "# Rime dict\n\
             ---\n\
             name: test\n\
             version: \"1.0\"\n\
             ...\n\
             \n\
             啊\ta\t1\n\
             阿\ta\t2\n\
             爱\tai\t100\n"
        )
        .unwrap();

        let dict = RimeDict::from_rime_dict(tmp.path()).unwrap();
        assert_eq!(dict.lookup("a").len(), 2);
        let candidates = dict.lookup("ai");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].text, "爱");
        assert_eq!(candidates[0].score, 100);
    }

    #[test]
    fn test_rime_dict_without_yaml_header() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "你好\tni hao\t10").unwrap();

        let dict = RimeDict::from_rime_dict(tmp.path()).unwrap();
        let candidates = dict.lookup("ni hao");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].text, "你好");
        assert_eq!(candidates[0].score, 10);
    }

    #[test]
    fn test_rime_dict_default_weight() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "世界\thello").unwrap();

        let dict = RimeDict::from_rime_dict(tmp.path()).unwrap();
        let candidates = dict.lookup("hello");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].score, 0);
    }

    #[test]
    fn test_rime_dict_import_tables() {
        let dir = tempfile::tempdir().unwrap();

        let sub_path = dir.path().join("sub.dict.yaml");
        std::fs::write(&sub_path, "吧\tb\t5\n把\tb\t3\n").unwrap();

        let main_path = dir.path().join("main.dict.yaml");
        std::fs::write(
            &main_path,
            "---\n\
             name: main\n\
             version: \"1.0\"\n\
             import_tables:\n\
             - sub\n\
             ...\n\
             啊\ta\t1\n\
             阿\ta\t2\n",
        )
        .unwrap();

        let dict = RimeDict::from_rime_dict(&main_path).unwrap();
        assert_eq!(dict.lookup("a").len(), 2);

        let candidates = dict.lookup("b");
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|c| c.text == "吧" && c.score == 5));
        assert!(candidates.iter().any(|c| c.text == "把" && c.score == 3));
    }

    #[test]
    fn test_rime_dict_single_column_yaml_skipped() {
        // .dict.yaml 缺省布局下的单列条目：按 RIME 惯例跳过
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            "---\n\
             name: test\n\
             ...\n\
             \n\
             打印\n\
             输入法\n"
        )
        .unwrap();

        let dict = RimeDict::from_rime_dict(tmp.path()).unwrap();
        assert!(dict.lookup("da yin").is_empty());
        assert!(dict.lookup("shu ru fa").is_empty());
    }

    #[test]
    fn test_rime_dict_headerless_txt_auto_pinyin() {
        // 无头纯码表：单列词条自动生成拼音编码
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "打印\n输入法\n").unwrap();

        let dict = RimeDict::from_rime_dict(tmp.path()).unwrap();
        let dayin = dict.lookup("da yin");
        assert_eq!(dayin.len(), 1);
        assert_eq!(dayin[0].text, "打印");

        let shurufa = dict.lookup("shu ru fa");
        assert_eq!(shurufa.len(), 1);
        assert_eq!(shurufa[0].text, "输入法");
    }

    #[test]
    fn test_rime_dict_single_column_non_hanzi() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "M1\n3G\n3D打印\n").unwrap();

        // 非汉字单列条目应被静默跳过，不 panic、不报错
        let dict = RimeDict::from_rime_dict(tmp.path()).unwrap();
        assert!(dict.lookup("M1").is_empty());
        assert!(dict.lookup("3G").is_empty());
    }

    #[test]
    fn test_rime_dict_columns_text_weight() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            "---\n\
             name: test\n\
             columns:\n\
             - text\n\
             - weight\n\
             ...\n\
             \n\
             打印\t100\n\
             输入法\t200\n"
        )
        .unwrap();

        let dict = RimeDict::from_rime_dict(tmp.path()).unwrap();
        let dayin = dict.lookup("da yin");
        assert_eq!(dayin.len(), 1);
        assert_eq!(dayin[0].text, "打印");
        assert_eq!(dayin[0].score, 100);

        let shurufa = dict.lookup("shu ru fa");
        assert_eq!(shurufa.len(), 1);
        assert_eq!(shurufa[0].text, "输入法");
        assert_eq!(shurufa[0].score, 200);
    }

    #[test]
    fn test_rime_dict_circular_import() {
        let dir = tempfile::tempdir().unwrap();

        let a_path = dir.path().join("a.dict.yaml");
        let b_path = dir.path().join("b.dict.yaml");

        std::fs::write(
            &a_path,
            "---\nname: a\nimport_tables:\n- b\n...\n啊\ta\t1\n",
        )
        .unwrap();
        std::fs::write(
            &b_path,
            "---\nname: b\nimport_tables:\n- a\n...\n吧\tb\t2\n",
        )
        .unwrap();

        // 循环 import 不应死循环，两词库词条均可见
        let dict = RimeDict::from_rime_dict(&a_path).unwrap();
        assert_eq!(dict.lookup("a").len(), 1);
        assert_eq!(dict.lookup("b").len(), 1);
    }

    #[test]
    fn test_rime_dict_cached_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("main.dict.yaml");
        std::fs::write(
            &src,
            "啊\ta\t1\n爱\tai\t100\n中文\tzhong wen\t90\n中国\tzhong guo\t80\n",
        )
        .unwrap();
        let cache_dir = dir.path().join("cache");

        let dict = RimeDict::from_rime_dict_cached(&src, &cache_dir).unwrap();
        assert_eq!(dict.lookup("ai")[0].text, "爱");
        assert_eq!(dict.prefix_lookup("zhong").len(), 2);

        // 再次加载应命中缓存（源文件未修改）
        let dict2 = RimeDict::from_rime_dict_cached(&src, &cache_dir).unwrap();
        assert_eq!(dict2.lookup("a").len(), 1);
        assert_eq!(dict2.lookup("zhong wen")[0].text, "中文");
    }

    #[test]
    fn test_real_dict_contains_tan_chars() {
        // 使用实际 RIME 词库验证 tan 读音包含碳、炭等常见字
        let cwd = std::env::current_dir().unwrap();
        let dict_path = cwd.join("../../assets/dicts/rime_ice.dict.yaml");
        if !dict_path.exists() {
            println!("跳过测试：实际词库文件不存在于 {:?}", dict_path);
            return;
        }

        let dict = RimeDict::from_rime_dict_cached(&dict_path, std::env::temp_dir()).unwrap();
        let candidates = dict.lookup("tan");
        let texts: Vec<&str> = candidates.iter().map(|c| c.text.as_str()).collect();

        println!("lookup('tan') returned {} candidates", candidates.len());
        for (i, c) in candidates.iter().enumerate() {
            println!("  {}: {} (score={})", i, c.text, c.score);
        }

        assert!(
            texts.contains(&"碳"),
            "lookup('tan') 应包含'碳'，实际: {:?}",
            texts
        );
        assert!(
            texts.contains(&"炭"),
            "lookup('tan') 应包含'炭'，实际: {:?}",
            texts
        );

        // 同时测试 prefix_lookup
        let prefix_candidates = dict.prefix_lookup("tan");
        let prefix_texts: Vec<&str> = prefix_candidates.iter().map(|c| c.text.as_str()).collect();
        println!("\nprefix_lookup('tan') returned {} candidates", prefix_candidates.len());
        for (i, c) in prefix_candidates.iter().enumerate() {
            println!("  {}: {} (score={})", i, c.text, c.score);
        }
        assert!(
            prefix_texts.contains(&"碳"),
            "prefix_lookup('tan') 应包含'碳'，实际: {:?}",
            prefix_texts
        );
        assert!(
            prefix_texts.contains(&"炭"),
            "prefix_lookup('tan') 应包含'炭'，实际: {:?}",
            prefix_texts
        );
    }

    #[test]
    fn test_rime_dict_shared() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("main.dict.yaml");
        std::fs::write(&src, "啊\ta\t1\n").unwrap();
        let cache_dir = dir.path().join("cache");

        let d1 = RimeDict::shared(&src, &cache_dir).unwrap();
        let d2 = RimeDict::shared(&src, &cache_dir).unwrap();
        assert!(Arc::ptr_eq(&d1, &d2));
    }
}
