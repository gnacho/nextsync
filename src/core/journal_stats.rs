//! Read-only statistics from the native engine journals.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

/// Number of file entries tracked by one folder's native engine journal.
/// Missing, incomplete or unreadable journals contribute zero files.
pub fn synchronized_file_count(local_root: &Path) -> u64 {
    let path = local_root.join(".nextsync").join("state.db");
    if !path.is_file() {
        return 0;
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|connection| {
            connection.query_row("SELECT COUNT(*) FROM entries WHERE is_dir = 0", [], |row| {
                row.get::<_, i64>(0)
            })
        })
        .map(|count| count as u64)
        .unwrap_or(0)
}

/// Sum synchronized files across every configured folder of an account.
pub fn account_synchronized_file_count(local_roots: &[PathBuf]) -> u64 {
    local_roots.iter().fold(0u64, |total, root| {
        total.saturating_add(synchronized_file_count(root))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create_journal(root: &Path, entries: &[(&str, bool)]) {
        let journal_dir = root.join(".nextsync");
        std::fs::create_dir_all(&journal_dir).unwrap();
        let connection = Connection::open(journal_dir.join("state.db")).unwrap();
        connection
            .execute_batch("CREATE TABLE entries(path TEXT PRIMARY KEY, is_dir INTEGER NOT NULL);")
            .unwrap();
        for (path, is_dir) in entries {
            connection
                .execute(
                    "INSERT INTO entries(path, is_dir) VALUES (?1, ?2)",
                    rusqlite::params![path, is_dir],
                )
                .unwrap();
        }
    }

    #[test]
    fn account_count_sums_files_and_ignores_directories() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let missing = tempdir().unwrap();
        create_journal(
            first.path(),
            &[("a.txt", false), ("Documents", true), ("b.txt", false)],
        );
        create_journal(second.path(), &[("photo.jpg", false)]);
        let roots = vec![
            first.path().to_path_buf(),
            second.path().to_path_buf(),
            missing.path().to_path_buf(),
        ];
        assert_eq!(account_synchronized_file_count(&roots), 3);
    }

    #[test]
    fn missing_journal_counts_zero_without_creating_a_database() {
        let root = tempdir().unwrap();
        let journal = root.path().join(".nextsync").join("state.db");
        assert_eq!(synchronized_file_count(root.path()), 0);
        assert!(!journal.exists());
        assert_eq!(
            account_synchronized_file_count(&[root.path().to_path_buf()]),
            0
        );
    }
}
