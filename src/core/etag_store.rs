//! Persistence of the per-folder root ETag (issue #195).
//!
//! The ETag gate (#189) skips the periodic remote reconciliation when the
//! root ETag is unchanged. That ETag used to live only in the engine's
//! in-memory `etag_slot`, so after a restart the first interval of every
//! folder re-scanned even with zero remote changes. This module persists the
//! ETag per folder in the app's state directory (`state_dir()/etags/`),
//! outside the synced folder (writing inside the folder would be seen by the
//! filesystem watcher and could requeue the folder, issue #181).
//!
//! Reads and writes are best-effort: a missing/stale value degrades to "the
//! ETag changed", which is the safe direction (it reconciles).

use std::path::{Path, PathBuf};

use crate::util::paths::state_dir;

/// Where the ETag of one folder lives, keyed by folder id (unique per folder,
/// stable across restarts). The base directory is the state dir.
pub fn etag_path(folder_id: &str) -> PathBuf {
    etag_path_in(&state_dir(), folder_id)
}

/// The ETag file path under a given base directory (test-injectable).
fn etag_path_in(base: &Path, folder_id: &str) -> PathBuf {
    base.join("etags").join(folder_id)
}

/// Read the last recorded ETag for `folder_id`, if any.
pub fn read_etag(folder_id: &str) -> Option<String> {
    read_etag_in(&state_dir(), folder_id)
}

fn read_etag_in(base: &Path, folder_id: &str) -> Option<String> {
    std::fs::read_to_string(etag_path_in(base, folder_id))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Record the ETag for `folder_id`. Best-effort: failures are ignored (the
/// gate still works, it just re-scans once more next run).
pub fn write_etag(folder_id: &str, etag: &str) {
    write_etag_in(&state_dir(), folder_id, etag);
}

fn write_etag_in(base: &Path, folder_id: &str, etag: &str) {
    if etag.is_empty() {
        return;
    }
    let path = etag_path_in(base, folder_id);
    let _ = path.parent().map(std::fs::create_dir_all);
    let _ = std::fs::write(&path, etag);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_at(base: &Path, folder_id: &str, value: &str) {
        let path = etag_path_in(base, folder_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, value).unwrap();
    }

    #[test]
    fn read_returns_the_recorded_etag() {
        let dir = tempfile::tempdir().unwrap();
        write_at(dir.path(), "folder-1", "\"abc\"\n");
        assert_eq!(
            read_etag_in(dir.path(), "folder-1").as_deref(),
            Some("\"abc\"")
        );
    }

    #[test]
    fn missing_or_empty_etag_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_etag_in(dir.path(), "folder-1"), None);
        write_at(dir.path(), "folder-1", "   \n");
        assert_eq!(read_etag_in(dir.path(), "folder-1"), None);
    }

    #[test]
    fn write_then_read_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        write_etag_in(dir.path(), "folder-2", "\"xyz\"");
        assert_eq!(
            read_etag_in(dir.path(), "folder-2").as_deref(),
            Some("\"xyz\"")
        );
    }

    #[test]
    fn empty_etag_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        write_etag_in(dir.path(), "folder-3", "");
        assert!(!etag_path_in(dir.path(), "folder-3").exists());
    }
}
