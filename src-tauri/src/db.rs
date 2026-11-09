use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DbEntry {
    /// Relative, '/'-separated, NFC, no leading slash.
    pub path: String,
    pub is_dir: bool,
    pub local_mtime: Option<i64>,
    pub local_size: Option<i64>,
    pub remote_etag: Option<String>,
    pub remote_fileid: Option<String>,
}

pub struct Db {
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS entries (
  path TEXT PRIMARY KEY,
  is_dir INTEGER NOT NULL,
  local_mtime INTEGER,
  local_size INTEGER,
  remote_etag TEXT,
  remote_fileid TEXT
);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
";

fn row(r: &rusqlite::Row) -> rusqlite::Result<DbEntry> {
    Ok(DbEntry {
        path: r.get(0)?,
        is_dir: r.get(1)?,
        local_mtime: r.get(2)?,
        local_size: r.get(3)?,
        remote_etag: r.get(4)?,
        remote_fileid: r.get(5)?,
    })
}

const COLS: &str = "path, is_dir, local_mtime, local_size, remote_etag, remote_fileid";

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // Safe in WAL mode: a crash can only lose the last commits, never corrupt the DB.
        // Lost rows just look like new files and get re-recorded.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db { conn })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Db> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db { conn })
    }

    #[cfg(test)]
    pub fn get(&self, path: &str) -> rusqlite::Result<Option<DbEntry>> {
        self.conn
            .query_row(
                &format!("SELECT {COLS} FROM entries WHERE path = ?1"),
                [path],
                row,
            )
            .optional()
    }

    pub fn upsert(&self, e: &DbEntry) -> rusqlite::Result<()> {
        self.conn.execute(
            &format!("INSERT OR REPLACE INTO entries ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"),
            params![
                e.path,
                e.is_dir,
                e.local_mtime,
                e.local_size,
                e.remote_etag,
                e.remote_fileid
            ],
        )?;
        Ok(())
    }

    pub fn delete(&self, path: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM entries WHERE path = ?1", [path])?;
        Ok(())
    }

    /// Deletes `dir` and everything below it. ('/' + 1 == '0', so the range covers "dir/…".)
    pub fn delete_prefix(&self, dir: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM entries WHERE path = ?1 OR (path >= ?1 || '/' AND path < ?1 || '0')",
            [dir],
        )?;
        Ok(())
    }

    pub fn all(&self) -> rusqlite::Result<Vec<DbEntry>> {
        let mut st = self.conn.prepare(&format!("SELECT {COLS} FROM entries"))?;
        st.query_map([], row)?.collect()
    }

    pub fn clear(&self) -> rusqlite::Result<()> {
        self.conn
            .execute_batch("DELETE FROM entries; DELETE FROM meta;")
    }

    pub fn get_meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            [key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(path: &str) -> DbEntry {
        DbEntry {
            path: path.into(),
            ..Default::default()
        }
    }

    #[test]
    fn crud_and_prefix() {
        let db = Db::open_in_memory().unwrap();
        for p in ["a", "a/b", "a/b/c", "a.txt", "a0", "ab"] {
            db.upsert(&e(p)).unwrap();
        }
        assert_eq!(db.get("a/b").unwrap(), Some(e("a/b")));
        db.delete_prefix("a").unwrap();
        let mut left: Vec<_> = db.all().unwrap().into_iter().map(|e| e.path).collect();
        left.sort();
        assert_eq!(left, ["a.txt", "a0", "ab"]);
        db.delete("ab").unwrap();
        assert!(db.get("ab").unwrap().is_none());
        db.set_meta("root_etag", "x").unwrap();
        assert_eq!(db.get_meta("root_etag").unwrap().as_deref(), Some("x"));
        db.clear().unwrap();
        assert!(db.all().unwrap().is_empty());
        assert!(db.get_meta("root_etag").unwrap().is_none());
    }
}
