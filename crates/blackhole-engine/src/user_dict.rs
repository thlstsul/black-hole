use blackhole_shared::{Candidate, SchemeId};
use rusqlite::{Connection, Result as SqliteResult};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// 全局用户词典实例
static GLOBAL_USER_DICT: OnceLock<Arc<Mutex<UserDictionary>>> = OnceLock::new();

/// 初始化全局用户词典。应在应用启动时调用一次。
pub fn init_global_user_dict(path: impl AsRef<Path>) -> SqliteResult<()> {
    let dict = UserDictionary::open(path)?;
    let _ = GLOBAL_USER_DICT.set(Arc::new(Mutex::new(dict)));
    Ok(())
}

/// 获取全局用户词典实例
pub fn global_user_dict() -> Option<Arc<Mutex<UserDictionary>>> {
    GLOBAL_USER_DICT.get().cloned()
}

/// 获取默认用户词典文件路径
/// - Windows: `%APPDATA%/blackhole/user_dict.db`
/// - Linux/macOS: `~/.local/share/blackhole/user_dict.db`
pub fn default_user_dict_path() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("blackhole").join("user_dict.db")
}

/// 用户词典：基于 SQLite 的词频和历史记录存储
pub struct UserDictionary {
    conn: Connection,
}

impl UserDictionary {
    /// 打开或创建用户词典数据库
    pub fn open<P: AsRef<Path>>(path: P) -> SqliteResult<Self> {
        let conn = Connection::open(path)?;
        let dict = Self { conn };
        dict.init_tables()?;
        Ok(dict)
    }

    /// 在内存中创建临时用户词典（用于测试）
    pub fn open_in_memory() -> SqliteResult<Self> {
        let conn = Connection::open_in_memory()?;
        let dict = Self { conn };
        dict.init_tables()?;
        Ok(dict)
    }

    fn init_tables(&self) -> SqliteResult<()> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS user_words (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scheme TEXT NOT NULL,
                code TEXT NOT NULL,
                text TEXT NOT NULL,
                frequency INTEGER NOT NULL DEFAULT 0,
                last_used INTEGER NOT NULL DEFAULT 0,
                UNIQUE(scheme, code, text)
            )",
            [],
        )?;

        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_user_words_lookup
             ON user_words(scheme, code)",
            [],
        )?;

        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS input_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scheme TEXT NOT NULL,
                text TEXT NOT NULL,
                timestamp INTEGER NOT NULL DEFAULT 0
            )",
            [],
        )?;

        Ok(())
    }

    /// 记录一次上屏事件，更新词频
    pub fn record_commit(&self, scheme: SchemeId, code: &str, text: &str) -> SqliteResult<()> {
        let scheme_str = scheme_to_str(scheme);
        let now = current_timestamp();

        self.conn.execute(
            "INSERT INTO user_words (scheme, code, text, frequency, last_used)
             VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT(scheme, code, text)
             DO UPDATE SET
                frequency = frequency + 1,
                last_used = ?4",
            [scheme_str, code, text, &now.to_string()],
        )?;

        self.conn.execute(
            "INSERT INTO input_history (scheme, text, timestamp)
             VALUES (?1, ?2, ?3)",
            [scheme_str, text, &now.to_string()],
        )?;

        Ok(())
    }

    /// 查询用户词频（用于候选排序提升）
    pub fn get_frequency(&self, scheme: SchemeId, code: &str, text: &str) -> SqliteResult<i64> {
        let scheme_str = scheme_to_str(scheme);
        let mut stmt = self.conn.prepare(
            "SELECT frequency FROM user_words
             WHERE scheme = ?1 AND code = ?2 AND text = ?3",
        )?;
        let mut rows = stmt.query([scheme_str, code, text])?;
        if let Some(row) = rows.next()? {
            Ok(row.get(0)?)
        } else {
            Ok(0)
        }
    }

    /// 获取某个编码下的用户候选词（按词频排序）
    pub fn lookup(&self, scheme: SchemeId, code: &str) -> SqliteResult<Vec<Candidate>> {
        let scheme_str = scheme_to_str(scheme);
        let mut stmt = self.conn.prepare(
            "SELECT text, frequency FROM user_words
             WHERE scheme = ?1 AND code = ?2
             ORDER BY frequency DESC, last_used DESC",
        )?;
        let rows = stmt.query_map([scheme_str, code], |row| {
            Ok(Candidate {
                text: row.get(0)?,
                comment: Some("user".to_string()),
                score: row.get::<_, i64>(1)?,
            })
        })?;

        let mut candidates = Vec::new();
        for candidate in rows {
            candidates.push(candidate?);
        }
        Ok(candidates)
    }

    /// 获取某个 scheme 下最常用的 N 个词
    pub fn top_words(&self, scheme: SchemeId, limit: usize) -> SqliteResult<Vec<(String, i64)>> {
        let scheme_str = scheme_to_str(scheme);
        let mut stmt = self.conn.prepare(
            "SELECT text, frequency FROM user_words
             WHERE scheme = ?1
             ORDER BY frequency DESC, last_used DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map([scheme_str, &limit.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// 清空用户词典
    pub fn clear(&self) -> SqliteResult<()> {
        self.conn.execute("DELETE FROM user_words", [])?;
        self.conn.execute("DELETE FROM input_history", [])?;
        Ok(())
    }
}

fn scheme_to_str(scheme: SchemeId) -> &'static str {
    match scheme {
        SchemeId::Pinyin => "pinyin",
        SchemeId::Shuangpin => "shuangpin",
    }
}

fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_dict() {
        let dict = UserDictionary::open_in_memory().unwrap();

        dict.record_commit(SchemeId::Pinyin, "zhongwen", "中文")
            .unwrap();
        dict.record_commit(SchemeId::Pinyin, "zhongwen", "中文")
            .unwrap();
        dict.record_commit(SchemeId::Pinyin, "zhongwen", "中温")
            .unwrap();

        let freq = dict
            .get_frequency(SchemeId::Pinyin, "zhongwen", "中文")
            .unwrap();
        assert_eq!(freq, 2);

        let candidates = dict.lookup(SchemeId::Pinyin, "zhongwen").unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].text, "中文");
        assert_eq!(candidates[0].score, 2);
    }
}
