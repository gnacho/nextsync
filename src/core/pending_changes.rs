//! Pending local changes of one folder (issue #46).
//!
//! A cheap, bounded preview of what a synchronization would do, computed
//! from the last delete-guard journal (the sorted file list recorded after
//! the last successful run) versus the current local tree. Remote changes
//! are NOT included: the engine owns the remote discovery, and this view is
//! deliberately local-only.

use std::collections::HashSet;
use std::path::Path;
use std::time::SystemTime;

use crate::core::delete_guard::{is_sync_database_name, DeleteGuardManifest};
use crate::core::exclusions::ExclusionMatcher;
use crate::storage::config::{expanduser, folder_fingerprint, AccountConfig, FolderConfig};

/// Rows rendered per kind before the "and N more" line kicks in.
pub const PENDING_LIST_CAP: usize = 50;

/// Local changes of one folder relative to the last journal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingChanges {
    /// Files the journal does not know (they will be uploaded).
    pub created: Vec<String>,
    /// Files present on both sides whose local modification time is newer
    /// than the journal (heuristic: the manifest is written right after a
    /// successful sync, so anything touched later counts as changed).
    pub modified: Vec<String>,
    /// Files the journal knew about that are gone locally (they will be
    /// deleted on the server).
    pub deleted: Vec<String>,
}

impl PendingChanges {
    /// Whether anything changed at all.
    pub fn is_empty(&self) -> bool {
        self.created.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

/// Walk the local tree returning relative paths with modification times.
///
/// Same skipping rules as the delete guard: directories and symlinks are
/// ignored, journal databases and excluded names never count.
pub fn scan_local_files_with_times(
    root: &Path,
    matcher: &ExclusionMatcher,
) -> Vec<(String, SystemTime)> {
    let mut files = Vec::new();
    if !root.is_dir() {
        return files;
    }
    let mut stack: Vec<(std::path::PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((directory, relative_directory)) = stack.pop() {
        let children = match std::fs::read_dir(&directory) {
            Ok(children) => children,
            Err(_) => continue,
        };
        for child in children.flatten() {
            let name = child.file_name().to_string_lossy().into_owned();
            if is_sync_database_name(&name) || matcher.matches_name(&name) {
                continue;
            }
            let relative = if relative_directory.is_empty() {
                name.clone()
            } else {
                format!("{relative_directory}/{name}")
            };
            let kind = match child.file_type() {
                Ok(kind) => kind,
                Err(_) => continue,
            };
            if kind.is_dir() {
                stack.push((child.path(), relative));
            } else if kind.is_file() {
                let modified = child
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((relative, modified));
            }
        }
    }
    files
}

/// Pure diff of the journal against the scanned tree.
pub fn compute_pending_changes(
    previous: &[String],
    current: &[(String, SystemTime)],
    baseline_time: SystemTime,
) -> PendingChanges {
    let previous_set: HashSet<&String> = previous.iter().collect();
    let current_set: HashSet<&String> = current.iter().map(|(path, _)| path).collect();
    let mut created: Vec<String> = current_set
        .difference(&previous_set)
        .map(|path| (*path).clone())
        .collect();
    let mut modified: Vec<String> = current
        .iter()
        .filter(|(path, modified)| previous_set.contains(path) && *modified > baseline_time)
        .map(|(path, _)| path.clone())
        .collect();
    let mut deleted: Vec<String> = previous_set
        .difference(&current_set)
        .map(|path| (*path).clone())
        .collect();
    created.sort();
    modified.sort();
    deleted.sort();
    PendingChanges {
        created,
        modified,
        deleted,
    }
}

/// The bounded rows a dialog should render: at most [`PENDING_LIST_CAP`]
/// paths plus the number of remaining entries.
pub fn bounded_rows(changes: &PendingChanges, cap: usize) -> (Vec<(&'static str, &str)>, usize) {
    let mut rows = Vec::new();
    let mut remaining = 0usize;
    for (kind, paths) in [
        ("new", &changes.created),
        ("changed", &changes.modified),
        ("deleted", &changes.deleted),
    ] {
        for path in paths {
            if rows.len() >= cap {
                remaining += 1;
            } else {
                rows.push((kind, path.as_str()));
            }
        }
    }
    (rows, remaining)
}

/// Compute the pending local changes of one configured folder.
///
/// The journal is the folder's delete-guard manifest. Without one (the
/// folder never completed a synchronization since the guard exists) every
/// local file counts as new; the returned flag tells the caller so the UI
/// can explain it.
pub fn pending_for_folder(
    account: &AccountConfig,
    folder: &FolderConfig,
) -> (PendingChanges, bool) {
    let matcher = ExclusionMatcher::new(
        account.sync.exclude_patterns.clone(),
        account.sync.exclude_patterns_enabled,
    );
    let root = expanduser(&folder.local_root);
    let manifest = DeleteGuardManifest::for_folder(
        &account.server_url,
        &account.login_name,
        &folder.local_root,
        &folder.remote_path,
    );
    let data = manifest.load().filter(|data| {
        data.account_fingerprint
            == folder_fingerprint(
                &account.server_url,
                &account.login_name,
                &folder.local_root,
                &folder.remote_path,
            )
    });
    let Some(data) = data else {
        let current = scan_local_files_with_times(&root, &matcher);
        let changes = compute_pending_changes(&[], &current, SystemTime::UNIX_EPOCH);
        return (changes, false);
    };
    let baseline_time = std::fs::metadata(manifest.path())
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let current = scan_local_files_with_times(&root, &matcher);
    (
        compute_pending_changes(&data.files, &current, baseline_time),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn matcher() -> ExclusionMatcher {
        ExclusionMatcher::new(Vec::<String>::new(), true)
    }

    #[test]
    fn scan_collects_paths_and_mtimes() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "x").expect("write");
        std::fs::create_dir_all(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("sub").join("b.txt"), "x").expect("write");
        std::fs::write(dir.path().join(".sync_1.db"), "x").expect("write");
        let scanned = scan_local_files_with_times(dir.path(), &matcher());
        let mut paths: Vec<&str> = scanned.iter().map(|(path, _)| path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["a.txt", "sub/b.txt"]);
        assert!(scanned
            .iter()
            .all(|(_, time)| *time > SystemTime::UNIX_EPOCH));
        assert!(
            scan_local_files_with_times(Path::new("/nonexistent-pending"), &matcher()).is_empty()
        );
    }

    #[test]
    fn diff_classifies_created_modified_and_deleted() {
        let previous = vec![
            "kept.txt".to_string(),
            "gone.txt".to_string(),
            "touched.txt".to_string(),
        ];
        let now = SystemTime::now();
        let current = vec![
            ("kept.txt".to_string(), now),
            (
                "touched.txt".to_string(),
                now + std::time::Duration::from_secs(10),
            ),
            ("new.txt".to_string(), now),
        ];
        let changes = compute_pending_changes(&previous, &current, now);
        assert_eq!(changes.created, ["new.txt"]);
        assert_eq!(changes.modified, ["touched.txt"]);
        assert_eq!(changes.deleted, ["gone.txt"]);
        assert!(!changes.is_empty());
    }

    #[test]
    fn empty_journal_counts_everything_as_created() {
        let now = SystemTime::now();
        let current = vec![("a.txt".to_string(), now), ("b.txt".to_string(), now)];
        let changes = compute_pending_changes(&[], &current, now);
        assert_eq!(changes.created, ["a.txt", "b.txt"]);
        assert!(changes.modified.is_empty());
        assert!(changes.deleted.is_empty());
    }

    #[test]
    fn bounded_rows_cap_and_count() {
        let mut changes = PendingChanges::default();
        for index in 0..60 {
            changes.created.push(format!("new{index}.txt"));
        }
        for index in 0..10 {
            changes.deleted.push(format!("gone{index}.txt"));
        }
        let (rows, remaining) = bounded_rows(&changes, 50);
        assert_eq!(rows.len(), 50);
        assert_eq!(remaining, 20);
        assert_eq!(rows[0], ("new", "new0.txt"));
        assert_eq!(rows[49], ("new", "new49.txt"));

        let (rows, remaining) = bounded_rows(&changes, 100);
        assert_eq!(rows.len(), 70);
        assert_eq!(remaining, 0);
    }

    #[test]
    fn pending_for_folder_reads_the_folder_manifest() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "x").expect("write");
        std::fs::write(dir.path().join("b.txt"), "x").expect("write");

        let account = AccountConfig {
            server_url: "https://cloud.example.com".to_string(),
            login_name: "alice".to_string(),
            ..AccountConfig::default()
        };
        let folder = FolderConfig {
            id: String::new(),
            local_root: dir.path().to_string_lossy().into_owned(),
            remote_path: "/Docs".to_string(),
            space_id: None,
            size_confirmed: false,
        };

        // Without a manifest everything counts as new and the flag is false.
        let (changes, had_journal) = pending_for_folder(&account, &folder);
        assert!(!had_journal);
        assert_eq!(changes.created, ["a.txt", "b.txt"]);

        // Record a baseline with only a.txt, delete it locally, add c.txt.
        let fingerprint = folder_fingerprint(
            &account.server_url,
            &account.login_name,
            &folder.local_root,
            &folder.remote_path,
        );
        let manifest = DeleteGuardManifest::for_folder(
            &account.server_url,
            &account.login_name,
            &folder.local_root,
            &folder.remote_path,
        );
        manifest
            .save(&fingerprint, dir.path(), &["a.txt".to_string()])
            .expect("save");
        std::fs::remove_file(dir.path().join("a.txt")).expect("remove");
        std::fs::write(dir.path().join("c.txt"), "x").expect("write");
        let (changes, had_journal) = pending_for_folder(&account, &folder);
        assert!(had_journal);
        assert_eq!(changes.created, ["b.txt", "c.txt"]);
        assert_eq!(changes.deleted, ["a.txt"]);
    }
}
