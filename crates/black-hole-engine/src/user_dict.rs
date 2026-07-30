//! 用户词典：基于 rime-dict `UserDb` 的词频存储（`.userdb.txt` 格式，与 librime 互通）。
//!
//! 每个输入方案一个 UserDb 实例，持久化到用户数据目录：
//! - Windows: `%APPDATA%/black-hole/{scheme}.userdb.txt`
//! - Linux/macOS: `~/.local/share/black-hole/{scheme}.userdb.txt`

use black_hole_shared::{Candidate, SchemeId};
use rime_dict::UserDb;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// 全局用户词典实例
static GLOBAL_USER_DICT: OnceLock<Arc<Mutex<UserDictionary>>> = OnceLock::new();

/// 初始化全局用户词典。应在应用启动时调用一次。
pub fn init_global_user_dict(dir: impl AsRef<Path>) -> std::io::Result<()> {
    let dict = UserDictionary::open(dir)?;
    let _ = GLOBAL_USER_DICT.set(Arc::new(Mutex::new(dict)));
    Ok(())
}

/// 获取全局用户词典实例
pub fn global_user_dict() -> Option<Arc<Mutex<UserDictionary>>> {
    GLOBAL_USER_DICT.get().cloned()
}

/// 获取默认用户数据目录
/// - Windows: `%APPDATA%/black-hole`
/// - Linux/macOS: `~/.local/share/black-hole`
pub fn default_user_dict_dir() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("black-hole")
}

/// 方案对应的 `.userdb.txt` 文件名
fn userdb_file_name(scheme: SchemeId) -> &'static str {
    match scheme {
        SchemeId::Pinyin => "pinyin.userdb.txt",
        SchemeId::Shuangpin => "shuangpin.userdb.txt",
    }
}

/// 用户词典：基于 rime-dict `UserDb` 的词频存储
pub struct UserDictionary {
    pinyin: UserDb,
    shuangpin: UserDb,
    /// 持久化目录；`None` 表示内存模式（测试用，不落盘）
    dir: Option<PathBuf>,
}

impl UserDictionary {
    /// 打开用户数据目录，加载已有的 `.userdb.txt` 快照
    pub fn open<P: AsRef<Path>>(dir: P) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let mut dict = Self {
            pinyin: UserDb::new(),
            shuangpin: UserDb::new(),
            dir: Some(dir),
        };
        dict.load(SchemeId::Pinyin);
        dict.load(SchemeId::Shuangpin);
        Ok(dict)
    }

    /// 创建内存用户词典（用于测试，不读写磁盘）
    pub fn open_in_memory() -> Self {
        Self {
            pinyin: UserDb::new(),
            shuangpin: UserDb::new(),
            dir: None,
        }
    }

    fn db(&self, scheme: SchemeId) -> &UserDb {
        match scheme {
            SchemeId::Pinyin => &self.pinyin,
            SchemeId::Shuangpin => &self.shuangpin,
        }
    }

    fn db_mut(&mut self, scheme: SchemeId) -> &mut UserDb {
        match scheme {
            SchemeId::Pinyin => &mut self.pinyin,
            SchemeId::Shuangpin => &mut self.shuangpin,
        }
    }

    /// 从磁盘加载指定方案的用户词频（文件缺失或解析失败时忽略，从空开始）
    fn load(&mut self, scheme: SchemeId) {
        let Some(ref dir) = self.dir else { return };
        let path = dir.join(userdb_file_name(scheme));
        let Ok(txt) = std::fs::read_to_string(&path) else {
            return;
        };
        if let Err(e) = self.db_mut(scheme).import_txt(&txt) {
            tracing::warn!("Failed to parse user dict {:?}: {}", path, e);
        }
    }

    /// 将指定方案的用户词频写回磁盘（写穿透）
    fn save(&self, scheme: SchemeId) {
        let Some(ref dir) = self.dir else { return };
        let path = dir.join(userdb_file_name(scheme));
        if let Err(e) = std::fs::write(&path, self.db(scheme).export_txt()) {
            tracing::warn!("Failed to save user dict {:?}: {}", path, e);
        }
    }

    /// 记录一次上屏事件，词频 +1 并落盘
    pub fn record_commit(&mut self, scheme: SchemeId, code: &str, text: &str) {
        self.db_mut(scheme).update(code, text);
        self.save(scheme);
    }

    /// 获取某个编码下的用户候选词（按词频降序）
    pub fn lookup(&self, scheme: SchemeId, code: &str) -> Vec<Candidate> {
        let mut candidates: Vec<Candidate> = self
            .db(scheme)
            .iter()
            .filter(|e| e.code == code)
            .map(|e| Candidate {
                text: e.text,
                comment: Some("user".to_string()),
                score: e.count as i64,
            })
            .collect();
        candidates.sort_by(|a, b| b.score.cmp(&a.score).then(a.text.cmp(&b.text)));
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_dict() {
        let mut dict = UserDictionary::open_in_memory();

        dict.record_commit(SchemeId::Pinyin, "zhong wen", "中文");
        dict.record_commit(SchemeId::Pinyin, "zhong wen", "中文");
        dict.record_commit(SchemeId::Pinyin, "zhong wen", "中温");
        // 不同方案互不影响
        dict.record_commit(SchemeId::Shuangpin, "zhong wen", "种文");

        let candidates = dict.lookup(SchemeId::Pinyin, "zhong wen");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].text, "中文");
        assert_eq!(candidates[0].score, 2);
        assert_eq!(candidates[1].text, "中温");
        assert_eq!(candidates[1].score, 1);

        let shuangpin = dict.lookup(SchemeId::Shuangpin, "zhong wen");
        assert_eq!(shuangpin.len(), 1);
        assert_eq!(shuangpin[0].text, "种文");

        // 未记录的编码无候选
        assert!(dict.lookup(SchemeId::Pinyin, "ni hao").is_empty());
    }

    #[test]
    fn test_persistence_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("black-hole_userdb_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        {
            let mut dict = UserDictionary::open(&dir).unwrap();
            dict.record_commit(SchemeId::Pinyin, "ni hao", "你好");
            dict.record_commit(SchemeId::Pinyin, "ni hao", "你好");
            dict.record_commit(SchemeId::Shuangpin, "wo", "我");
        }

        // 重新打开后词频保留，且与 librime `.userdb.txt` 格式互通
        let dict = UserDictionary::open(&dir).unwrap();
        let candidates = dict.lookup(SchemeId::Pinyin, "ni hao");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].score, 2);
        assert_eq!(dict.lookup(SchemeId::Shuangpin, "wo")[0].text, "我");

        let txt = std::fs::read_to_string(dir.join("pinyin.userdb.txt")).unwrap();
        assert_eq!(txt, "ni hao\t你好\t2\n");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
