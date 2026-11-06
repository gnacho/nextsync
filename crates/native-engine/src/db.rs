//! Persistent sync journal.

use crate::Result;
use rusqlite::{params, Connection};
use std::path::Path;

const SCHEMA_VERSION: i64 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub etag: String,
    pub fileid: Option<String>,
    pub inode: u64,
}

pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < SCHEMA_VERSION {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS entries(
                    path TEXT PRIMARY KEY,
                    is_dir INTEGER NOT NULL,
                    size INTEGER NOT NULL,
                    mtime INTEGER NOT NULL,
                    etag TEXT NOT NULL,
                    fileid TEXT,
                    inode INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        // Retry blacklist: a transfer that keeps failing must not be retried
        // on every single run, or it burns the whole pass. The official engine
        // blacklists an item after three consecutive errors
        // (nc-desktop src/libsync/syncengine.cpp, checkErrorBlacklisting).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS failures(
                path TEXT PRIMARY KEY,
                signature TEXT NOT NULL,
                attempts INTEGER NOT NULL,
                last_error TEXT NOT NULL,
                at INTEGER NOT NULL
            );",
        )?;
        Ok(Self { conn })
    }

    pub fn load_all(&self) -> Result<Vec<DbEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, is_dir, size, mtime, etag, fileid, inode FROM entries ORDER BY path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(DbEntry {
                path: r.get(0)?,
                is_dir: r.get::<_, i64>(1)? != 0,
                size: r.get::<_, i64>(2)? as u64,
                mtime: r.get(3)?,
                etag: r.get(4)?,
                fileid: r.get(5)?,
                inode: r.get::<_, i64>(6)? as u64,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn upsert(&self, e: &DbEntry) -> Result<()> {
        self.conn.execute(
            "INSERT INTO entries(path,is_dir,size,mtime,etag,fileid,inode)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(path) DO UPDATE SET is_dir=?2,size=?3,mtime=?4,etag=?5,fileid=?6,inode=?7",
            params![
                e.path,
                e.is_dir as i64,
                e.size as i64,
                e.mtime,
                e.etag,
                e.fileid,
                e.inode as i64
            ],
        )?;
        Ok(())
    }

    pub fn remove(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM entries WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE OR REPLACE entries SET path = ?2 WHERE path = ?1",
            params![from, to],
        )?;
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        self.conn.execute("DELETE FROM entries", [])?;
        Ok(())
    }

    pub fn count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
            params![key, value],
        )?;
        Ok(())
    }

    /// A transfer that keeps failing is retried up to `attempts` runs and then
    /// left alone until its signature changes (`#266`-style blacklisting).
    pub fn failure_attempts(&self, path: &str, signature: &str) -> Result<u32> {
        let mut stmt = self
            .conn
            .prepare("SELECT attempts, signature FROM failures WHERE path = ?1")?;
        let mut rows = stmt.query(params![path])?;
        match rows.next()? {
            Some(row) => {
                let attempts: i64 = row.get(0)?;
                let stored: String = row.get(1)?;
                // A different version of the file gets a fresh chance.
                if stored != signature {
                    return Ok(0);
                }
                Ok(attempts.max(0) as u32)
            }
            None => Ok(0),
        }
    }

    pub fn record_failure(&self, path: &str, signature: &str, error: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO failures(path, signature, attempts, last_error, at)
             VALUES(?1, ?2, 1, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET
                attempts = CASE WHEN failures.signature = ?2 THEN failures.attempts + 1 ELSE 1 END,
                signature = ?2,
                last_error = ?3,
                at = ?4",
            params![path, signature, error, crate::http::now_epoch()],
        )?;
        Ok(())
    }

    pub fn clear_failure(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM failures WHERE path = ?1", params![path])?;
        Ok(())
    }

    /// Drop blacklist rows for a path and everything below it: a directory that
    /// disappeared must not keep its children blacklisted forever.
    pub fn clear_failures_under(&self, path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM failures WHERE path = ?1 OR path LIKE ?2",
            params![path, format!("{path}/%")],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> DbEntry {
        DbEntry {
            path: path.into(),
            is_dir: false,
            size: 3,
            mtime: 10,
            etag: "\"e\"".into(),
            fileid: Some("1".into()),
            inode: 42,
        }
    }

    #[test]
    fn roundtrip_upsert_remove_rename() {
        let dir = tempfile::tempdir().unwrap();
        let db = StateDb::open(&dir.path().join("s.db")).unwrap();
        assert_eq!(db.count().unwrap(), 0);
        db.upsert(&entry("a")).unwrap();
        db.upsert(&entry("b")).unwrap();
        assert_eq!(db.count().unwrap(), 2);
        let mut e = entry("a");
        e.size = 99;
        db.upsert(&e).unwrap();
        let all = db.load_all().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].size, 99);
        db.rename("a", "c").unwrap();
        assert!(db.load_all().unwrap().iter().any(|x| x.path == "c"));
        db.remove("c").unwrap();
        assert_eq!(db.count().unwrap(), 1);
        db.set_meta("k", "v").unwrap();
        assert_eq!(db.get_meta("k").unwrap().as_deref(), Some("v"));
    }
}
