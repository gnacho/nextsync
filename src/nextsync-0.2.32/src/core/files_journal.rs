//! Read-only access to the external sync engine journal.
//!
//! `nextcloudcmd`/`opencloudcmd` keep their own reconciliation journal per
//! local folder: a `nextcloud`-style SQLite database named `.sync_<hash>.db`
//! inside the folder's local root. The journal stores one row per known file,
//! including its remote file id (`metadata.fileid`).
//!
//! The official Nextcloud desktop client resolves `notify_file_id` push hints
//! against exactly this table via `SyncJournalDb::hasFileIds`, comparing the
//! numeric file ids from the push hint against the `metadata.fileid` column
//! cast to an integer (the column holds a string like `00152532ocwv4xsuk6ni`,
//! whose numeric prefix is the file id). This module mirrors that lookup so a
//! remote change can be routed to the folder that actually contains the
//! notified file, instead of re-syncing every folder of the account (issue
//! #183).
//!
//! It is intentionally read-only and best-effort: a missing journal (first
//! sync, or the folder was never touched) simply answers `false`, which is the
//! conservative fallback for the push routing.

use std::path::{Path, PathBuf};

/// Find the sync engine journal for a local folder root.
///
/// The journal is a `.sync_<hash>.db` file whose name is derived from the
/// folder path and account, so we match any `.sync*.db` regular file directly
/// under `local_root` (the engines never nest them). Returns the first match,
/// or `None` when the folder has never been reconciled.
pub fn find_journal(local_root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(local_root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if name.starts_with(".sync_") && name.ends_with(".db") && path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Whether any of `file_ids` is present in the journal for `local_root`.
///
/// Opens the journal read-only (without connecting to a database managed
/// concurrently by the engine) and answers whether at least one notified id
/// maps to a file this folder knows about. Mirrors
/// `SyncJournalDb::hasFileIds`: the `metadata.fileid` string column is
/// compared to each given integer id after a `CAST(... AS INTEGER)`.
///
/// Returns `false` when the journal cannot be read (missing, busy, or the
/// query fails), which routes the hint conservatively.
pub fn contains_file_ids(local_root: &Path, file_ids: &[i64]) -> bool {
    if file_ids.is_empty() {
        return false;
    }
    let Some(journal) = find_journal(local_root) else {
        return false;
    };
    journal_contains(&journal, file_ids)
}

/// Open `journal` and test membership of any `file_ids` in `metadata`.
fn journal_contains(journal: &Path, file_ids: &[i64]) -> bool {
    let conn = match rusqlite::Connection::open_with_flags(
        journal,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) {
        Ok(conn) => conn,
        Err(_) => return false,
    };
    // The journal may be mid-write by the engine; a busy query is not a
    // mismatch, so fall back to `false` (the caller routes conservatively).
    let sql = match build_file_id_membership_sql(file_ids.len()) {
        Ok(sql) => sql,
        Err(_) => return false,
    };
    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(_) => return false,
    };
    let params: Vec<i64> = file_ids.to_vec();
    stmt.query_row(rusqlite::params_from_iter(&params), |_| Ok(()))
        .is_ok()
}

/// Build the membership query for `n` ids: a single placeholder per id
/// (`CAST(fileid AS INTEGER) IN (?1, ?2, ...)`). The `metadata.fileid`
/// column holds a string like `00152532ocwv4xsuk6ni`, so we cast it to an
/// integer to match the numeric ids sent by the push hint (the official
/// client compares the same way).
fn build_file_id_membership_sql(count: usize) -> Result<String, String> {
    if count == 0 {
        return Err("no file ids to test".to_string());
    }
    let mut sql = String::from("SELECT 1 FROM metadata WHERE CAST(fileid AS INTEGER) IN (");
    for i in 0..count {
        if i > 0 {
            sql.push(',');
        }
        sql.push('?');
    }
    sql.push_str(") LIMIT 1");
    Ok(sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_journal(db_path: &Path, rows: &[(&str, &str)]) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata(phash INTEGER(8),pathlen INTEGER,path VARCHAR(4096),
             inode INTEGER,uid INTEGER,gid INTEGER,mode INTEGER,modtime INTEGER(8),
             type INTEGER,md5 VARCHAR(32), fileid VARCHAR(128), remotePerm VARCHAR(128),
             filesize BIGINT, ignoredChildrenRemote INT);",
        )
        .unwrap();
        let mut stmt = conn
            .prepare("INSERT INTO metadata(path, fileid) VALUES (?1, ?2)")
            .unwrap();
        for (path, fileid) in rows {
            stmt.execute([path, fileid]).unwrap();
        }
    }

    #[test]
    fn find_journal_returns_sync_db_in_folder_root() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".sync_abc123.db");
        write_journal(&db, &[("/file.txt", "152532ocwv4xsuk6ni")]);
        assert_eq!(find_journal(dir.path()), Some(db));
    }

    #[test]
    fn contains_file_ids_matches_numeric_prefix_of_string_id() {
        let dir = tempfile::tempdir().unwrap();
        write_journal(
            &dir.path().join(".sync_x.db"),
            &[("/a/b.txt", "00152532ocwv4")],
        );
        assert!(contains_file_ids(dir.path(), &[152532]));
        assert!(!contains_file_ids(dir.path(), &[999]));
    }

    #[test]
    fn contains_file_ids_matches_any_of_multiple_ids() {
        let dir = tempfile::tempdir().unwrap();
        write_journal(&dir.path().join(".sync_x.db"), &[("/a/b.txt", "17ocwv4")]);
        assert!(contains_file_ids(dir.path(), &[5, 17]));
    }

    #[test]
    fn missing_journal_answers_false() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!contains_file_ids(dir.path(), &[42]));
    }

    #[test]
    fn empty_id_list_answers_false() {
        let dir = tempfile::tempdir().unwrap();
        write_journal(&dir.path().join(".sync_x.db"), &[("/a", "42")]);
        assert!(!contains_file_ids(dir.path(), &[]));
    }
}
