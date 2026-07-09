use crate::Dictionary;
use blackhole_shared::Candidate;
use pinyin::ToPinyin;
use rusqlite::{Connection, ToSql, params};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// RIME 词库加载错误
#[derive(Debug, thiserror::Error)]
pub enum RimeDictError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Parse error at line {line}: {msg}")]
    Parse { line: usize, msg: String },
    #[error("YAML header error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

/// RIME 词库列定义
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Column {
    Text,
    Code,
    Weight,
    Stem,
    #[serde(other)]
    Other,
}

/// RIME 词库 YAML 头部
#[derive(Debug, Default, serde::Deserialize)]
struct RimeDictHeader {
    #[serde(default)]
    import_tables: Vec<String>,
    #[serde(default)]
    columns: Vec<Column>,
}

impl RimeDictHeader {
    fn columns(&self) -> Vec<Column> {
        if self.columns.is_empty() {
            vec![Column::Text, Column::Code, Column::Weight]
        } else {
            self.columns.clone()
        }
    }
}

/// 基于 SQLite 的词典实现，显著降低内存占用
pub struct SqliteDictionary {
    conn: Connection,
    db_path: PathBuf,
    /// 为 true 时不删除底层数据库文件（用于缓存数据库）
    is_persistent: bool,
}

impl Default for SqliteDictionary {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl SqliteDictionary {
    /// 创建内存数据库
    pub fn in_memory() -> Self {
        let conn =
            Connection::open_in_memory().expect("Failed to create in-memory SQLite database");
        Self::init_db(conn, PathBuf::from(":memory:"))
    }

    /// 初始化数据库表结构
    fn init_db(conn: Connection, db_path: PathBuf) -> Self {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA cache_size=2000;
             PRAGMA temp_store=MEMORY;

             CREATE TABLE IF NOT EXISTS entries (
                 code TEXT NOT NULL,
                 text TEXT NOT NULL,
                 score INTEGER NOT NULL DEFAULT 0
             );

             CREATE INDEX IF NOT EXISTS idx_code ON entries(code);",
        )
        .expect("Failed to initialize database");

        Self {
            conn,
            db_path,
            is_persistent: false,
        }
    }

    /// 插入词条
    pub fn insert(&mut self, code: impl Into<String>, text: impl Into<String>, score: i64) {
        let code = code.into();
        let text = text.into();

        self.conn
            .execute(
                "INSERT INTO entries (code, text, score) VALUES (?1, ?2, ?3)",
                params![code, text, score],
            )
            .expect("Failed to insert entry");
    }

    /// 合并另一个词典的数据到当前词典
    pub fn merge(&mut self, other: Self) {
        let other_path = other.db_path.clone();
        let is_persistent = other.is_persistent;

        // 如果 other 是临时数据库，阻止其 Drop 时删除文件
        if !is_persistent {
            std::mem::forget(other);
        }

        // 附加数据库进行合并
        if other_path.as_os_str() != ":memory:" && other_path.exists() {
            let other_path_str = other_path.to_string_lossy().to_string();

            self.conn
                .execute(
                    &format!("ATTACH DATABASE '{}' AS other_db", other_path_str),
                    [],
                )
                .expect("Failed to attach database");

            self.conn.execute(
                "INSERT INTO entries (code, text, score) SELECT code, text, score FROM other_db.entries",
                [],
            ).expect("Failed to merge entries");

            let _ = self.conn.execute("DETACH DATABASE other_db", []);
        }

        // 如果 other 是持久化数据库且没被 forget，这里它会被正常 Drop
    }

    /// 从 RIME 词库文件加载词典
    ///
    /// 支持格式：
    /// - `.dict.yaml`：可选 YAML 头部 + 制表符分隔的码表正文
    /// - `custom_phrase.txt`：纯文本码表，支持注释
    /// - 支持 `import_tables` 递归导入其它词库文件
    ///
    /// 每行默认格式：`text\tcode\t[weight]`，weight 为可选整数，默认 0。
    /// 可通过 YAML 头部的 `columns` 字段自定义列顺序。
    pub fn from_rime_dict(path: impl AsRef<Path>) -> Result<Self, RimeDictError> {
        Self::from_rime_dict_impl(path.as_ref(), None)
    }

    /// 从 RIME 词库文件加载词典，并缓存到指定目录。
    ///
    /// 首次加载时会解析源文件并生成缓存 SQLite 数据库；
    /// 后续若源文件未修改，则直接打开缓存，跳过解析。
    pub fn from_rime_dict_cached(
        src_path: impl AsRef<Path>,
        cache_dir: impl AsRef<Path>,
    ) -> Result<Self, RimeDictError> {
        let src_path = src_path.as_ref();
        let cache_dir = cache_dir.as_ref();
        let _ = std::fs::create_dir_all(cache_dir);

        // 用源文件路径的哈希作为缓存文件名，保证稳定且唯一
        let src_hash = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let canonical = src_path
                .canonicalize()
                .unwrap_or_else(|_| src_path.to_path_buf());
            let mut hasher = DefaultHasher::new();
            canonical.hash(&mut hasher);
            hasher.finish()
        };
        let cache_path = cache_dir.join(format!("dict_{:x}.db", src_hash));

        if Self::cache_is_valid(&cache_path, src_path) {
            tracing::info!("Using cached dictionary: {}", cache_path.display());
            let conn = Connection::open(&cache_path)
                .map_err(|e| RimeDictError::Io(std::io::Error::other(e.to_string())))?;
            return Ok(Self {
                conn,
                db_path: cache_path,
                is_persistent: true,
            });
        }

        tracing::info!("Building dictionary cache from: {}", src_path.display());
        let dict = Self::from_rime_dict_impl(src_path, Some(&cache_path))?;

        // 若上面的实现未能生成缓存（比如中途失败），从临时文件复制一份
        if !cache_path.exists() && dict.db_path.exists() {
            let _ = std::fs::copy(&dict.db_path, &cache_path);
        }

        // 重新打开缓存文件，使 db_path 指向缓存（避免 Drop 时删除缓存）
        if cache_path.exists() {
            let conn = Connection::open(&cache_path)
                .map_err(|e| RimeDictError::Io(std::io::Error::other(e.to_string())))?;
            return Ok(Self {
                conn,
                db_path: cache_path,
                is_persistent: true,
            });
        }

        // 回退：无法创建缓存，直接返回临时数据库
        Ok(dict)
    }

    /// 检查缓存是否仍然有效（缓存文件存在且比所有源词库文件都新）
    fn cache_is_valid(cache_path: &Path, src_path: &Path) -> bool {
        if !cache_path.exists() {
            return false;
        }
        let cache_meta = match cache_path.metadata() {
            Ok(m) => m,
            Err(_) => return false,
        };
        let cache_mtime = match cache_meta.modified() {
            Ok(t) => t,
            Err(_) => return false,
        };

        // 检查主源文件
        let src_meta = match src_path.metadata() {
            Ok(m) => m,
            Err(_) => return false,
        };
        if let Ok(src_mtime) = src_meta.modified()
            && src_mtime > cache_mtime
        {
            return false;
        }

        // 检查同目录下其他词库文件（import_tables 可能引用的子词库）
        if let Some(src_dir) = src_path.parent() {
            for entry in std::fs::read_dir(src_dir).ok().into_iter().flatten() {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str());
                if matches!(ext, Some("yaml") | Some("txt"))
                    && let Ok(meta) = entry.metadata()
                    && let Ok(mtime) = meta.modified()
                    && mtime > cache_mtime
                {
                    return false;
                }
            }
        }

        true
    }

    fn from_rime_dict_impl(path: &Path, target_path: Option<&Path>) -> Result<Self, RimeDictError> {
        let db_path = if let Some(target) = target_path {
            target.to_path_buf()
        } else {
            let temp_dir = std::env::temp_dir();
            let unique_id = format!(
                "{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            temp_dir.join(format!("blackhole_dict_{}.db", unique_id))
        };

        let conn = Connection::open(&db_path)
            .map_err(|e| RimeDictError::Io(std::io::Error::other(e.to_string())))?;

        let mut dict = Self::init_db(conn, db_path.clone());

        // 加载词库数据
        Self::from_rime_dict_internal(path, &mut HashSet::new(), &mut dict)?;

        // 创建前缀匹配所需的索引
        dict.conn
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_code_like ON entries(code)",
                [],
            )
            .map_err(|e| RimeDictError::Io(std::io::Error::other(e.to_string())))?;

        Ok(dict)
    }

    fn from_rime_dict_internal(
        path: &Path,
        visited: &mut HashSet<PathBuf>,
        dict: &mut Self,
    ) -> Result<(), RimeDictError> {
        let canonical = path.canonicalize()?;
        if !visited.insert(canonical.clone()) {
            // 检测到循环导入，直接返回避免无限递归
            return Ok(());
        }

        let content = std::fs::read_to_string(path)?;
        let (header_str, body_lines) = Self::split_header_body(&content);

        let header: RimeDictHeader = if header_str.trim().is_empty() {
            RimeDictHeader::default()
        } else {
            serde_yaml::from_str(&header_str)?
        };

        // 递归加载 import_tables 中指定的词库
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        for table_name in &header.import_tables {
            let import_path_yaml = base_dir.join(format!("{}.dict.yaml", table_name));
            let import_path_txt = base_dir.join(format!("{}.txt", table_name));

            if import_path_yaml.exists() {
                Self::from_rime_dict_internal(&import_path_yaml, visited, dict)?;
            } else if import_path_txt.exists() {
                Self::from_rime_dict_internal(&import_path_txt, visited, dict)?;
            }
            // 若文件不存在则静默跳过，与 RIME 行为一致
        }

        // 解析当前文件正文
        let columns = header.columns();
        Self::parse_body_lines(&body_lines, &columns, dict)?;

        Ok(())
    }

    /// 分离 YAML 头部和正文
    ///
    /// 策略：
    /// 1. 若存在 `...` 行，正文从 `...` 之后开始
    /// 2. 否则，第一个包含制表符且非注释/非空行开始正文
    fn split_header_body(content: &str) -> (String, Vec<String>) {
        let lines: Vec<&str> = content.lines().collect();
        let mut split_at = lines.len();

        // 先查找显式的 YAML 结束标记 `...`
        for (i, line) in lines.iter().enumerate() {
            if line.trim() == "..." {
                split_at = i + 1;
                break;
            }
        }

        // 若没找到 `...`，用制表符行作为分界
        if split_at == lines.len() {
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim();
                if !trimmed.starts_with('#') && !trimmed.is_empty() && trimmed.contains('\t') {
                    split_at = i;
                    break;
                }
            }
        }

        let header = lines[..split_at].join("\n");
        let body = lines[split_at..].iter().map(|s| s.to_string()).collect();
        (header, body)
    }

    /// 将纯汉字文本转换为拼音编码（带空格分隔）
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

    /// 解析正文行
    ///
    /// 容错策略：
    /// - 跳过空行和注释行
    /// - 跳过 YAML 键值对（含 `:` 但不含制表符/空格分隔字段）
    /// - 同时支持制表符和空格作为分隔符
    /// - 字段数不足时，对纯汉字单列表条目自动生成拼音编码
    /// - 解析失败的行静默跳过，不中断加载
    fn parse_body_lines(
        lines: &[String],
        columns: &[Column],
        dict: &mut Self,
    ) -> Result<(), RimeDictError> {
        // 使用事务批量插入，大幅提升性能
        let tx = dict
            .conn
            .transaction()
            .map_err(|e| RimeDictError::Io(std::io::Error::other(e.to_string())))?;

        {
            let mut stmt = tx
                .prepare("INSERT INTO entries (code, text, score) VALUES (?1, ?2, ?3)")
                .map_err(|e| RimeDictError::Io(std::io::Error::other(e.to_string())))?;

            for (line_no, line) in lines.iter().enumerate() {
                let trimmed = line.trim();

                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }

                // 跳过 YAML 键值对（如 `use_preset_vocabulary: true`）
                // 特征：含 `:` 且不含制表符/多空格
                if trimmed.contains(':')
                    && !trimmed.contains('\t')
                    && trimmed.split_whitespace().count() < 2
                {
                    continue;
                }

                // 优先尝试制表符分隔，否则尝试空格分隔
                let parts: Vec<&str> = if trimmed.contains('\t') {
                    trimmed.split('\t').collect()
                } else {
                    trimmed.split_whitespace().collect()
                };

                if parts.len() == 1 {
                    // RIME 词库允许单列表条目（仅 text 列），尝试自动生成拼音编码
                    if let Some(code) = Self::text_to_pinyin_code(trimmed) {
                        stmt.execute(params![code, trimmed, 0])
                            .map_err(|e| RimeDictError::Io(std::io::Error::other(e.to_string())))?;
                    }
                    // 无法生成拼音的非汉字单列表条目静默跳过，不制造 warning 噪音
                    continue;
                }

                let mut text = None;
                let mut code = None;
                let mut weight = 0i64;
                let mut skip = false;

                for (i, col) in columns.iter().enumerate() {
                    if i >= parts.len() {
                        break;
                    }
                    match col {
                        Column::Text => text = Some(parts[i].trim()),
                        Column::Code => code = Some(parts[i].trim()),
                        Column::Weight => match parts[i].trim().parse::<i64>() {
                            Ok(v) => weight = v,
                            Err(e) => {
                                tracing::warn!(
                                    "Skipping line {}: invalid weight '{}': {}",
                                    line_no + 1,
                                    parts[i].trim(),
                                    e
                                );
                                skip = true;
                                break;
                            }
                        },
                        Column::Stem | Column::Other => {}
                    }
                }

                if skip {
                    continue;
                }

                let code = match code {
                    Some(c) if !c.is_empty() => c.to_string(),
                    _ => match text.and_then(Self::text_to_pinyin_code) {
                        Some(c) => c,
                        None => continue,
                    },
                };

                let text = match text.filter(|t| !t.is_empty()) {
                    Some(t) => t.to_string(),
                    None => continue,
                };

                stmt.execute(params![code, text, weight])
                    .map_err(|e| RimeDictError::Io(std::io::Error::other(e.to_string())))?;
            }
        }

        tx.commit()
            .map_err(|e| RimeDictError::Io(std::io::Error::other(e.to_string())))?;

        Ok(())
    }

    pub fn from_builtin() -> Self {
        let mut dict = Self::in_memory();

        // 常见词汇（使用空格分隔的拼音编码，与 RIME 词库格式一致）
        dict.insert("zhong wen", "中文", 100);
        dict.insert("zhong wen", "中温", 80);
        dict.insert("zhong wen", "钟文", 60);
        dict.insert("shu ru", "输入", 100);
        dict.insert("shu ru", "熟入", 50);
        dict.insert("shu ru fa", "输入法", 100);
        dict.insert("shu ru fa", "输入阀", 30);
        dict.insert("pin yin", "拼音", 100);
        dict.insert("pin yin", "品引", 40);
        dict.insert("cang jie", "仓颉", 100);
        dict.insert("wu bi", "五笔", 100);
        dict.insert("han yu", "汉语", 100);
        dict.insert("han yu", "韩语", 80);
        dict.insert("han zi", "汉字", 100);
        dict.insert("ji suan ji", "计算机", 100);
        dict.insert("ji suan ji", "计算器", 80);
        dict.insert("cheng xu", "程序", 100);
        dict.insert("cheng xu", "承续", 50);
        dict.insert("bian cheng", "编程", 100);
        dict.insert("bian cheng", "边城", 80);
        dict.insert("wen zi", "文字", 100);
        dict.insert("wen zi", "蚊子", 80);
        dict.insert("wen zi", "纹子", 40);
        dict.insert("yu yan", "语言", 100);
        dict.insert("yu yan", "寓言", 80);
        dict.insert("xie xie", "谢谢", 100);
        dict.insert("xie xie", "写写", 50);
        dict.insert("ni hao", "你好", 100);
        dict.insert("zai jian", "再见", 100);
        dict.insert("wo men", "我们", 100);
        dict.insert("wo men", "我门", 40);
        dict.insert("ta men de", "他们的", 100);
        dict.insert("zhong guo", "中国", 100);
        dict.insert("zhong guo", "中国", 90);
        dict.insert("bei jing", "北京", 100);
        dict.insert("bei jing", "背景", 80);
        dict.insert("shang hai", "上海", 100);
        dict.insert("shang hai", "伤害", 60);
        dict.insert("guang zhou", "广州", 100);
        dict.insert("shen zhen", "深圳", 100);
        dict.insert("xiang gang", "香港", 100);
        dict.insert("tai wan", "台湾", 100);
        dict.insert("xin wen", "新闻", 100);
        dict.insert("dian shi", "电视", 100);
        dict.insert("dian nao", "电脑", 100);
        dict.insert("shou ji", "手机", 100);
        dict.insert("wang luo", "网络", 100);
        dict.insert("wang luo", "网罗", 50);
        dict.insert("you xi", "游戏", 100);
        dict.insert("yin le", "音乐", 100);
        dict.insert("dian ying", "电影", 100);
        dict.insert("mei shi", "美食", 100);
        dict.insert("lv xing", "旅行", 100);
        dict.insert("xue xi", "学习", 100);
        dict.insert("gong zuo", "工作", 100);
        dict.insert("sheng huo", "生活", 100);
        dict.insert("shi jie", "世界", 100);
        dict.insert("shi jie", "时节", 60);
        dict.insert("shi jian", "时间", 100);
        dict.insert("shi jian", "事件", 80);
        dict.insert("kong jian", "空间", 100);
        dict.insert("kong jian", "控件", 70);
        dict.insert("ren min", "人民", 100);
        dict.insert("gong min", "公民", 100);
        dict.insert("guo jia", "国家", 100);
        dict.insert("cheng shi", "城市", 100);
        dict.insert("xiang cun", "乡村", 100);
        dict.insert("dao lu", "道路", 100);
        dict.insert("qi che", "汽车", 100);
        dict.insert("huo che", "火车", 100);
        dict.insert("fei ji", "飞机", 100);
        dict.insert("chuan", "船", 100);
        dict.insert("zi xing che", "自行车", 100);
        dict.insert("di tie xian", "地铁线", 100);
        dict.insert("gong jiao che", "公交车", 100);

        // 常用单字（高频 500 字），通过 pinyin crate 自动生成拼音映射
        let common_chars = "的一是了我不人在他有这个上们来到时大地为子中你说生国年着就那和要她出也得里后自以会家可下而过天去能对小多然于心学之都好看起发当没成只如事把还用第样道想作种开美总从无情己面最女但现前些所同日手又行意动方期它头经长儿回位分爱老因很给名法间斯知世什两次使身者被高已亲其进此话常与活正感";
        for ch in common_chars.chars() {
            if let Some(py) = ch.to_pinyin() {
                let py_str = py.plain();
                if !py_str.is_empty() {
                    dict.insert(py_str, ch.to_string(), 95);
                }
            }
        }

        dict
    }

    /// 流式构建语言模型，避免全量导出到内存的 Vec
    ///
    /// 直接从数据库读取并聚合 text -> total_score，然后构建 LanguageModel。
    pub fn build_language_model(&self) -> crate::LanguageModel {
        let mut stmt = self
            .conn
            .prepare("SELECT text, score FROM entries")
            .expect("Failed to prepare LM build statement");

        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .expect("Failed to query entries for LM");

        let mut text_scores: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        for row in rows.filter_map(|r| r.ok()) {
            let (text, score) = row;
            *text_scores.entry(text).or_insert(0) += score.max(1);
        }

        let total: i64 = text_scores.values().sum();
        crate::LanguageModel::from_text_scores(total, text_scores)
    }

    /// 将所有词条导出为 (code, candidates) 列表
    ///
    /// 按 code 分组，每个 code 对应其所有候选列表。
    pub fn to_entries(&self) -> Vec<(String, Vec<Candidate>)> {
        let mut stmt = self
            .conn
            .prepare("SELECT code, text, score FROM entries ORDER BY code")
            .expect("Failed to prepare export statement");

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .expect("Failed to export entries");

        let mut groups: std::collections::BTreeMap<String, Vec<Candidate>> =
            std::collections::BTreeMap::new();

        for row in rows.filter_map(|r| r.ok()) {
            let (code, text, score) = row;
            groups.entry(code).or_default().push(Candidate {
                text,
                comment: None,
                score,
            });
        }

        groups.into_iter().collect()
    }

    /// 执行 SQL 查询并将结果映射为 Candidate 列表的通用辅助方法。
    fn query_candidates(&self, sql: &'static str, params: &[&dyn ToSql]) -> Vec<Candidate> {
        let mut stmt = self
            .conn
            .prepare_cached(sql)
            .expect("Failed to prepare statement");

        stmt.query_map(params, |row| {
            Ok(Candidate {
                text: row.get(0)?,
                comment: None,
                score: row.get(1)?,
            })
        })
        .expect("Failed to execute query")
        .filter_map(|r| r.ok())
        .collect()
    }
}

/// 计算前缀范围的上界（字典序下一个字符串），用于将 LIKE 前缀匹配转换为索引友好的范围查询。
/// 例如："sh" -> "si"，"zhong" -> "zhonh"
fn prefix_upper_bound(prefix: &str) -> String {
    let mut chars: Vec<char> = prefix.chars().collect();
    if let Some(last) = chars.last_mut()
        && let Some(next) = std::char::from_u32((*last as u32) + 1)
    {
        *last = next;
    }
    chars.into_iter().collect()
}

impl Dictionary for SqliteDictionary {
    fn lookup(&self, code: &str) -> Vec<Candidate> {
        tracing::debug!("lookup start: code='{}'", code);
        let candidates = self.query_candidates(
            "SELECT text, score FROM entries WHERE code = ?1 ORDER BY score DESC",
            &[&code],
        );

        tracing::debug!(
            "lookup end: code='{}', candidates={}",
            code,
            candidates.len()
        );

        candidates
    }

    fn prefix_lookup(&self, code: &str) -> Vec<Candidate> {
        if code.is_empty() {
            return Vec::new();
        }

        tracing::debug!("prefix_lookup start: code='{}'", code);
        // 使用范围查询替代 LIKE，确保 SQLite 能利用 code 列的索引
        let upper = prefix_upper_bound(code);
        let candidates = self.query_candidates(
            "SELECT text, score FROM entries WHERE code >= ?1 AND code < ?2 ORDER BY score DESC LIMIT 100",
            &[&code, &upper],
        );

        tracing::debug!(
            "prefix_lookup end: code='{}', candidates={}",
            code,
            candidates.len()
        );

        candidates
    }

    fn syllable_match(&self, pattern: &str) -> Vec<Candidate> {
        if pattern.is_empty() {
            return Vec::new();
        }

        // 单音节情况（不含空格）与 prefix_lookup 语义等价，直接复用已优化的范围查询
        if !pattern.contains(' ') {
            return self.prefix_lookup(pattern);
        }

        tracing::debug!("syllable_match start: pattern='{}'", pattern);
        // 将空格分隔的音节模式转换为 SQL LIKE 模式
        // 例如："zhong wen" -> "zhong%wen%"
        let like_pattern: String = pattern
            .split_whitespace()
            .map(|syllable| format!("{}%", syllable))
            .collect();

        let candidates = self.query_candidates(
            "SELECT text, score FROM entries WHERE code LIKE ?1 ORDER BY score DESC LIMIT 100",
            &[&like_pattern],
        );

        tracing::debug!(
            "syllable_match end: pattern='{}', candidates={}",
            pattern,
            candidates.len()
        );

        candidates
    }

    fn fuzzy_match(&self, pattern: &str) -> Vec<Candidate> {
        if pattern.is_empty() {
            return Vec::new();
        }

        tracing::debug!("fuzzy_match start: pattern='{}'", pattern);
        // 将通配符 * 转换为 SQL 的 %
        let sql_pattern: String = pattern
            .chars()
            .map(|c| if c == '*' { '%' } else { c })
            .collect();

        let candidates = self.query_candidates(
            "SELECT text, score FROM entries WHERE code LIKE ?1 ORDER BY score DESC LIMIT 100",
            &[&sql_pattern],
        );

        tracing::debug!(
            "fuzzy_match end: pattern='{}', candidates={}",
            pattern,
            candidates.len()
        );

        candidates
    }
}

impl Drop for SqliteDictionary {
    fn drop(&mut self) {
        // 仅删除临时（非持久化）数据库文件
        if !self.is_persistent && self.db_path.as_os_str() != ":memory:" && self.db_path.exists() {
            let _ = std::fs::remove_file(&self.db_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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

        let dict = SqliteDictionary::from_rime_dict(tmp.path()).unwrap();
        let candidates = dict.lookup("a");
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|c| c.text == "啊" && c.score == 1));
        assert!(candidates.iter().any(|c| c.text == "阿" && c.score == 2));

        let candidates = dict.lookup("ai");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].text, "爱");
        assert_eq!(candidates[0].score, 100);
    }

    #[test]
    fn test_rime_dict_without_yaml_header() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "你好\tnihao\t10").unwrap();

        let dict = SqliteDictionary::from_rime_dict(tmp.path()).unwrap();
        let candidates = dict.lookup("nihao");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].text, "你好");
        assert_eq!(candidates[0].score, 10);
    }

    #[test]
    fn test_rime_dict_default_weight() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "世界\thello").unwrap();

        let dict = SqliteDictionary::from_rime_dict(tmp.path()).unwrap();
        let candidates = dict.lookup("hello");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].score, 0);
    }

    #[test]
    fn test_rime_dict_prefix_lookup() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            "中\tzhong\t10\n\
             中文\tzhongwen\t20\n"
        )
        .unwrap();

        let dict = SqliteDictionary::from_rime_dict(tmp.path()).unwrap();
        let candidates = dict.prefix_lookup("zhong");
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn test_rime_dict_import_tables() {
        let dir = tempfile::tempdir().unwrap();

        // 创建子词库
        let sub_path = dir.path().join("sub.dict.yaml");
        std::fs::write(
            &sub_path,
            "吧\tb\t5\n\
             把\tb\t3\n",
        )
        .unwrap();

        // 创建主词库，导入子词库
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

        let dict = SqliteDictionary::from_rime_dict(&main_path).unwrap();

        // 主词库的词条
        let candidates = dict.lookup("a");
        assert_eq!(candidates.len(), 2);

        // 导入子词库的词条
        let candidates = dict.lookup("b");
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|c| c.text == "吧" && c.score == 5));
        assert!(candidates.iter().any(|c| c.text == "把" && c.score == 3));
    }

    #[test]
    fn test_rime_dict_single_column_pinyin() {
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

        let dict = SqliteDictionary::from_rime_dict(tmp.path()).unwrap();
        // text_to_pinyin_code 现在生成带空格的编码："da yin"
        let dayin = dict.lookup("da yin");
        assert_eq!(dayin.len(), 1);
        assert_eq!(dayin[0].text, "打印");

        // "shu ru fa"
        let shurufa = dict.lookup("shu ru fa");
        assert_eq!(shurufa.len(), 1);
        assert_eq!(shurufa[0].text, "输入法");
    }

    #[test]
    fn test_rime_dict_single_column_non_hanzi() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            "---\n\
             name: test\n\
             ...\n\
             \n\
             M1\n\
             3G\n\
             3D打印\n"
        )
        .unwrap();

        // 非汉字单列表条目应被静默跳过，不 panic、不报错
        let dict = SqliteDictionary::from_rime_dict(tmp.path()).unwrap();
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

        let dict = SqliteDictionary::from_rime_dict(tmp.path()).unwrap();
        // text_to_pinyin_code 现在生成带空格的编码："da yin"
        let dayin = dict.lookup("da yin");
        assert_eq!(dayin.len(), 1);
        assert_eq!(dayin[0].text, "打印");
        assert_eq!(dayin[0].score, 100);

        // "shu ru fa"
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
            "import_tables:\n\
             - b\n\
             啊\ta\t1\n",
        )
        .unwrap();

        std::fs::write(
            &b_path,
            "import_tables:\n\
             - a\n\
             吧\tb\t2\n",
        )
        .unwrap();

        let dict = SqliteDictionary::from_rime_dict(&a_path).unwrap();
        let a_candidates = dict.lookup("a");
        let b_candidates = dict.lookup("b");

        // 循环导入不应导致死循环或 panic
        assert_eq!(a_candidates.len(), 1);
        assert_eq!(b_candidates.len(), 1);
    }
}
