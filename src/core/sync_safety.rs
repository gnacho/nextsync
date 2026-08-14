//! First-sync safety checks and stale artifact cleanup (issue #35).
//!
//! Before the first reconciliation of a folder the app must warn about
//! merges (both sides carry files) and about folders that were synchronized
//! before (engine journal files at the top of the tree), and it must not
//! let hidden journal artifacts pile up when the user opts for a fresh
//! start: they go to the system trash instead of lingering.

use std::path::{Path, PathBuf};

use gio::prelude::*;

use crate::core::delete_guard::is_sync_database_name;

/// Whether a file name looks like an engine sync artifact: journal
/// databases (see [`is_sync_database_name`]) plus the other hidden
/// `.sync*` side files the engine leaves at the folder root.
pub fn is_stale_sync_artifact(name: &str) -> bool {
    if is_sync_database_name(name) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    lower.starts_with(".sync") || lower.starts_with("._sync") || lower.starts_with(".-sync")
}

/// The stale engine artifacts at the top of a sync folder, sorted by name.
///
/// Only the folder root is inspected: that is where `nextcloudcmd` and
/// `opencloudcmd` write their journals. Missing/unreadable folders yield an
/// empty list.
pub fn find_stale_sync_artifacts(root: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
        })
        .filter(|entry| is_stale_sync_artifact(&entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect();
    found.sort_by_key(|path| path.file_name().unwrap_or_default().to_owned());
    found
}

/// Base names of the stale artifacts at the top of a local root.
pub fn stale_artifact_names(root: &Path) -> Vec<String> {
    find_stale_sync_artifacts(root)
        .iter()
        .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
        .map(str::to_owned)
        .collect()
}

/// Move the stale artifacts of a folder to the system trash (`gio::File::trash`).
///
/// The gio call is synchronous; it is a handful of small local files, so it
/// runs wherever the user confirms the fresh start. The returned count is
/// the number of files actually moved to the trash. A file that cannot be
/// trashed simply stays in place: the operation is best effort and never
/// destructive.
pub fn trash_stale_artifacts(root: &Path) -> usize {
    let artifacts = find_stale_sync_artifacts(root);
    let mut trashed = 0;
    for artifact in artifacts {
        let file = gio::File::for_path(&artifact);
        if file.trash(None::<&gio::Cancellable>).is_ok() {
            trashed += 1;
        }
    }
    trashed
}

/// Whether a local folder exists and is empty; unreadable folders count as
/// non-empty (mirrors the Python `_local_folder_is_empty`).
pub fn local_folder_is_empty(local_root: &str) -> bool {
    match std::fs::read_dir(crate::storage::config::expanduser(local_root)) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => false,
    }
}

/// What the app knows about a folder right before its first sync.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FirstSyncFacts {
    /// Whether every local root is empty (unreadable counts as non-empty).
    pub local_empty: bool,
    /// Whether the remote tree is empty; `None` when the probe could not
    /// tell (no credentials, unknown provider, probe failure).
    pub remote_empty: Option<bool>,
    /// Base names of the engine journals found in the local roots.
    pub journal_names: Vec<String>,
}

/// A blocking warning the first-sync review must surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstSyncWarning {
    /// Both sides carry files and they will be merged (issue #35).
    Merge,
    /// The folder was synchronized before (journals found, issue #35).
    PreviouslySynced,
}

/// The warnings a first-sync review dialog must show for these facts.
pub fn first_sync_warnings(facts: &FirstSyncFacts) -> Vec<FirstSyncWarning> {
    let mut warnings = Vec::new();
    if facts.remote_empty == Some(false) && !facts.local_empty {
        warnings.push(FirstSyncWarning::Merge);
    }
    if !facts.journal_names.is_empty() {
        warnings.push(FirstSyncWarning::PreviouslySynced);
    }
    warnings
}

/// Whether the facts require a blocking review before starting.
pub fn review_required(facts: &FirstSyncFacts) -> bool {
    !first_sync_warnings(facts).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(base: &Path, name: &str) {
        std::fs::write(base.join(name), "x").expect("write");
    }

    #[test]
    fn stale_artifact_names_cover_journals_and_side_files() {
        assert!(is_stale_sync_artifact(".sync_abc.db"));
        assert!(is_stale_sync_artifact("._sync_abc.db"));
        assert!(is_stale_sync_artifact(".-sync_abc.db"));
        assert!(is_stale_sync_artifact(".sync_journal"));
        assert!(!is_stale_sync_artifact("notes.txt"));
        assert!(!is_stale_sync_artifact(".gitignore"));
        // User files that merely contain "sync" stay untouched.
        assert!(!is_stale_sync_artifact("synced-report.pdf"));
    }

    #[test]
    fn find_stale_sync_artifacts_reads_only_the_folder_root() {
        let dir = tempdir().expect("tempdir");
        write(dir.path(), "keep.txt");
        write(dir.path(), ".sync_1.db");
        write(dir.path(), "._sync_2.db");
        std::fs::create_dir_all(dir.path().join("sub")).expect("mkdir");
        write(&dir.path().join("sub"), ".sync_3.db");
        let names = stale_artifact_names(dir.path());
        assert_eq!(names, ["._sync_2.db", ".sync_1.db"]);
        assert!(find_stale_sync_artifacts(Path::new("/nonexistent-artifacts")).is_empty());
    }

    #[test]
    fn warnings_cover_merge_and_previous_sync() {
        let quiet = FirstSyncFacts {
            local_empty: true,
            remote_empty: Some(true),
            journal_names: Vec::new(),
        };
        assert_eq!(first_sync_warnings(&quiet), Vec::new());
        assert!(!review_required(&quiet));

        let download = FirstSyncFacts {
            local_empty: true,
            remote_empty: Some(false),
            journal_names: Vec::new(),
        };
        assert_eq!(first_sync_warnings(&download), Vec::new());

        let merge = FirstSyncFacts {
            local_empty: false,
            remote_empty: Some(false),
            journal_names: Vec::new(),
        };
        assert_eq!(first_sync_warnings(&merge), [FirstSyncWarning::Merge]);
        assert!(review_required(&merge));

        // Unknown remote state never triggers the merge warning alone…
        let unknown = FirstSyncFacts {
            local_empty: false,
            remote_empty: None,
            journal_names: Vec::new(),
        };
        assert_eq!(first_sync_warnings(&unknown), Vec::new());

        // …but journals always demand a review.
        let reused = FirstSyncFacts {
            local_empty: true,
            remote_empty: Some(true),
            journal_names: vec![".sync_1.db".to_string()],
        };
        assert_eq!(
            first_sync_warnings(&reused),
            [FirstSyncWarning::PreviouslySynced]
        );
        assert!(review_required(&reused));
    }
}
