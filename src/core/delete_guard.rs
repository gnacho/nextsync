//! Lightweight mass-deletion guard.
//!
//! Fase 3 (Task 3.1): `nextcloudcmd --non-interactive` never blocks a mass
//! local deletion (verified in `nextcloud/desktop`: `handleMassDeletion`
//! disables its confirm dialog when `isCmd()`), so the app keeps its own
//! safety net. [`DeleteGuard`] records the last locally verified file set in
//! a [`DeleteGuardManifest`] (a sorted path list, no content hashes) and
//! compares it before every sync: when a previously populated folder lost too
//! many files the guard produces a [`DeleteAlert`] that blocks the scheduler.
//!
//! Reference implementation: `core/delete_guard.py`. The guard only cares
//! about regular files disappearing: directories, symlinks, special files,
//! `nextcloudcmd` journal databases and excluded names are all skipped, so the
//! manifest matches what the engine itself sees.
//!
//! [`DeleteAlert`]: crate::core::scheduler::DeleteAlert

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::exclusions::ExclusionMatcher;
use crate::core::scheduler::DeleteAlert;
use crate::storage::config::{
    ensure_private_directory, expanduser, folder_fingerprint, AccountConfig, DeleteGuardConfig,
    FolderConfig,
};
use crate::util::paths::state_dir;

/// Version of the manifest layout. Manifests with another format are ignored.
pub const GUARD_FORMAT: u32 = 1;

const MESSAGE_FOLDER_MISSING: &str = "The local synchronization folder is missing or unavailable.";
const MESSAGE_FOLDER_EMPTIED: &str = "A previously populated synchronization folder is now empty.";
const MESSAGE_MASS_DELETION: &str =
    "An unusual number of local files disappeared and could be deleted from Nextcloud.";

/// Serialized content of one deletion-guard manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteGuardManifestData {
    /// Layout version (see [`GUARD_FORMAT`]).
    pub format: u32,
    /// Folder fingerprint the manifest belongs to (server+login+local+remote).
    pub account_fingerprint: String,
    /// Absolute local root the file list refers to.
    pub local_root: String,
    /// Sorted relative paths of the last locally verified file set.
    pub files: Vec<String>,
}

/// Per-folder record of the last locally verified file set.
///
/// The lightweight counterpart to `nextcloudcmd`'s own journal: only the
/// sorted list of local file paths, so the app can notice a mass local
/// deletion before the engine propagates it to the server.
#[derive(Debug, Clone)]
pub struct DeleteGuardManifest {
    path: PathBuf,
}

impl DeleteGuardManifest {
    /// A manifest stored at an explicit path (used by tests).
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The default per-folder manifest path in the XDG state directory:
    /// `$XDG_STATE_HOME/nextsync/delete-guard-<fingerprint>.json`.
    pub fn for_folder(
        server_url: &str,
        login_name: &str,
        local_root: &str,
        remote_path: &str,
    ) -> Self {
        let fingerprint = folder_fingerprint(server_url, login_name, local_root, remote_path);
        Self::new(state_dir().join(format!("delete-guard-{fingerprint}.json")))
    }

    /// The file this manifest is stored in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the manifest, returning `None` when missing, corrupt or written
    /// with a different format.
    pub fn load(&self) -> Option<DeleteGuardManifestData> {
        let text = fs::read_to_string(&self.path).ok()?;
        let payload: DeleteGuardManifestData = serde_json::from_str(&text).ok()?;
        if payload.format != GUARD_FORMAT {
            return None;
        }
        Some(payload)
    }

    /// Atomically write the manifest (temp file + rename, mode 0600, fsync).
    pub fn save(&self, fingerprint: &str, local_root: &Path, files: &[String]) -> io::Result<()> {
        let payload = DeleteGuardManifestData {
            format: GUARD_FORMAT,
            account_fingerprint: fingerprint.to_string(),
            local_root: local_root.to_string_lossy().into_owned(),
            files: {
                let mut sorted = files.to_vec();
                sorted.sort();
                sorted
            },
        };
        let parent = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "manifest path has no parent")
        })?;
        ensure_private_directory(parent)?;
        let temporary = self.path.with_extension("tmp");
        let write_result = (|| -> io::Result<()> {
            let mut handle = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            serde_json::to_writer(&mut handle, &payload).map_err(io::Error::other)?;
            handle.write_all(b"\n")?;
            handle.sync_all()?;
            drop(handle);
            fs::rename(&temporary, &self.path)
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }
}

/// Whether a file name looks like a `nextcloudcmd` journal database, matching
/// the Python `^\.?_?sync.*\.db(?:[-.].*)?$` (case-insensitive).
pub fn is_sync_database_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let lower = lower.strip_prefix('.').unwrap_or(&lower);
    let lower = lower.strip_prefix('_').unwrap_or(lower);
    let Some(tail) = lower.strip_prefix("sync") else {
        return false;
    };
    let Some(dot) = tail.find(".db") else {
        return false;
    };
    let rest = &tail[dot + 3..];
    rest.is_empty() || rest.starts_with('.') || rest.starts_with('-')
}

/// The `nextcloudcmd` journal files at the top of a sync folder, sorted by
/// name. Missing/unreadable folders yield an empty list.
pub fn find_sync_databases(root: &Path) -> Vec<PathBuf> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
        })
        .filter(|entry| is_sync_database_name(&entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect();
    found.sort_by_key(|path| path.file_name().unwrap_or_default().to_owned());
    found
}

/// Walk a sync folder and return relative file paths.
///
/// Directories, symlinks and special files are skipped (the guard only cares
/// about regular files disappearing); journal databases and excluded names are
/// ignored, matching what `nextcloudcmd` sees.
pub fn scan_local_files(root: &Path, matcher: &ExclusionMatcher) -> Vec<String> {
    let root = PathBuf::from(root);
    let mut files = Vec::new();
    if !root.is_dir() {
        return files;
    }
    let mut stack: Vec<(PathBuf, String)> = vec![(root, String::new())];
    while let Some((directory, relative_directory)) = stack.pop() {
        let children = match fs::read_dir(&directory) {
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
                files.push(relative);
            }
        }
    }
    files
}

/// Runs the deletion-guard policy before a reconciliation. Implemented by
/// [`DeleteGuard`]; the scheduler holds it as an injectable box so the guard
/// can be faked in tests.
pub trait GuardCheck {
    /// Compare the current tree against the last manifest and return an alert
    /// when a mass deletion is detected. Expensive (filesystem walk).
    fn check(&mut self) -> Option<DeleteAlert>;

    /// Record the current tree as the new baseline.
    fn record_current(&mut self) -> bool;

    /// Remove the local sync journals and record a fresh baseline so the
    /// guard stops blocking (the engine then re-downloads from the server).
    /// Returns the number of journal files removed.
    fn restore_from_server(&mut self) -> usize;
}

/// Block a sync when the local tree lost too many previously known files.
#[derive(Debug, Clone)]
pub struct DeleteGuard {
    fingerprint: String,
    local_root: PathBuf,
    matcher: ExclusionMatcher,
    config: DeleteGuardConfig,
    manifest: DeleteGuardManifest,
}

impl DeleteGuard {
    /// Build the guard for one configured folder pair.
    pub fn for_folder(account: &AccountConfig, folder: &FolderConfig) -> Self {
        Self::new(
            folder_fingerprint(
                &account.server_url,
                &account.login_name,
                &folder.local_root,
                &folder.remote_path,
            ),
            &folder.local_root,
            account.sync.exclude_patterns.clone(),
            account.sync.exclude_patterns_enabled,
            account.delete_guard.clone(),
            DeleteGuardManifest::for_folder(
                &account.server_url,
                &account.login_name,
                &folder.local_root,
                &folder.remote_path,
            ),
        )
    }

    /// Build the guard from its parts (used by tests).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        fingerprint: String,
        local_root: impl AsRef<Path>,
        exclude_patterns: Vec<String>,
        exclusions_enabled: bool,
        config: DeleteGuardConfig,
        manifest: DeleteGuardManifest,
    ) -> Self {
        Self {
            fingerprint,
            local_root: local_root.as_ref().to_path_buf(),
            matcher: ExclusionMatcher::new(exclude_patterns, exclusions_enabled),
            config,
            manifest,
        }
    }

    /// The absolute local root this guard protects.
    pub fn local_root(&self) -> PathBuf {
        absolute_root(&self.local_root)
    }

    /// Compare the current local tree against the last manifest.
    pub fn check(&self) -> Option<DeleteAlert> {
        if !self.config.enabled {
            return None;
        }
        let manifest = self.manifest.load()?;
        if manifest.account_fingerprint != self.fingerprint {
            return None;
        }
        let root = self.local_root();
        if !root.exists() || !root.is_dir() {
            return Some(DeleteAlert {
                reason: "folder_missing".to_string(),
                message: MESSAGE_FOLDER_MISSING.to_string(),
                missing_paths: Vec::new(),
                previous_count: manifest.files.len(),
                current_count: 0,
                can_approve_once: false,
            });
        }

        let current: HashSet<String> = scan_local_files(&root, &self.matcher).into_iter().collect();
        let previous: HashSet<String> = manifest.files.iter().cloned().collect();
        let mut missing: Vec<String> = previous.difference(&current).cloned().collect();
        missing.sort();
        let previous_count = previous.len();
        let current_count = current.len();

        if previous_count > 0 && current_count == 0 {
            return Some(DeleteAlert {
                reason: "folder_emptied".to_string(),
                message: MESSAGE_FOLDER_EMPTIED.to_string(),
                missing_paths: missing,
                previous_count,
                current_count,
                can_approve_once: true,
            });
        }

        let count_limit = self.config.count_threshold.max(1) as usize;
        let percent_limit = self.config.percent_threshold.clamp(1, 100) as f64;
        let missing_percent = if previous_count > 0 {
            missing.len() as f64 * 100.0 / previous_count as f64
        } else {
            0.0
        };
        if !missing.is_empty() && (missing.len() >= count_limit || missing_percent >= percent_limit)
        {
            return Some(DeleteAlert {
                reason: "mass_local_deletion".to_string(),
                message: MESSAGE_MASS_DELETION.to_string(),
                missing_paths: missing,
                previous_count,
                current_count,
                can_approve_once: true,
            });
        }
        None
    }

    /// Scan the current tree and store it as the new baseline. Returns whether
    /// the manifest could be written.
    pub fn record_current(&self) -> bool {
        let files = scan_local_files(&self.local_root(), &self.matcher);
        self.manifest
            .save(&self.fingerprint, &self.local_root, &files)
            .is_ok()
    }

    /// Remove every local sync journal so the next reconciliation re-downloads
    /// the remote tree (with the journal gone `nextcloudcmd` treats the server
    /// as authoritative), then reset the baseline. Returns the number of
    /// journal files removed.
    pub fn restore_from_server(&self) -> usize {
        let root = self.local_root();
        let removed = find_sync_databases(&root)
            .into_iter()
            .filter(|database| fs::remove_file(database).is_ok())
            .count();
        let _ = self.record_current();
        removed
    }
}

impl GuardCheck for DeleteGuard {
    fn check(&mut self) -> Option<DeleteAlert> {
        <DeleteGuard>::check(self)
    }

    fn record_current(&mut self) -> bool {
        <DeleteGuard>::record_current(self)
    }

    fn restore_from_server(&mut self) -> usize {
        <DeleteGuard>::restore_from_server(self)
    }
}

/// Absolute, `~`-expanded form of a local root.
fn absolute_root(path: &Path) -> PathBuf {
    let expanded = expanduser(&path.to_string_lossy());
    std::path::absolute(&expanded).unwrap_or(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{tempdir, TempDir};

    fn root() -> TempDir {
        tempdir().expect("tempdir works")
    }

    fn write_files(base: &Path, files: &[&str]) {
        for name in files {
            let path = base.join(name);
            fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
            fs::write(path, "x").expect("write");
        }
    }

    fn guard_for(base: &Path, guard: DeleteGuardConfig) -> (DeleteGuard, TempDir) {
        let manifest_dir = root();
        let manifest = DeleteGuardManifest::new(manifest_dir.path().join("guard.json"));
        let guard = DeleteGuard::new(
            "test-fingerprint".to_string(),
            base,
            vec!["*.swp".to_string()],
            true,
            guard,
            manifest,
        );
        (guard, manifest_dir)
    }

    fn guard_config(count: i64, percent: i64) -> DeleteGuardConfig {
        DeleteGuardConfig {
            enabled: true,
            count_threshold: count,
            percent_threshold: percent,
        }
    }

    // ---- scan_local_files ---------------------------------------------------

    #[test]
    fn scan_lists_regular_files_recursively() {
        let dir = root();
        write_files(dir.path(), &["a.txt", "sub/b.bin"]);
        let matcher = ExclusionMatcher::new(Vec::<String>::new(), true);
        let mut files = scan_local_files(dir.path(), &matcher);
        files.sort();
        assert_eq!(files, ["a.txt", "sub/b.bin"]);
    }

    #[test]
    fn scan_ignores_journal_and_excluded_names() {
        let dir = root();
        write_files(dir.path(), &[".sync_1.db", "junk.swp", "keep.txt"]);
        let matcher = ExclusionMatcher::new(["*.swp"], true);
        assert_eq!(scan_local_files(dir.path(), &matcher), ["keep.txt"]);
    }

    #[test]
    fn scan_missing_root_is_empty() {
        let matcher = ExclusionMatcher::new(Vec::<String>::new(), true);
        assert!(scan_local_files(Path::new("/nonexistent/guard-root"), &matcher).is_empty());
    }

    #[test]
    fn scan_skips_symlinks() {
        let dir = root();
        write_files(dir.path(), &["real.txt"]);
        let link = dir.path().join("linked.txt");
        let _ = std::os::unix::fs::symlink(dir.path().join("real.txt"), &link);
        let matcher = ExclusionMatcher::new(Vec::<String>::new(), true);
        assert_eq!(scan_local_files(dir.path(), &matcher), ["real.txt"]);
    }

    // ---- sync database names ------------------------------------------------

    #[test]
    fn sync_database_names() {
        assert!(is_sync_database_name(".sync_foo.db"));
        assert!(is_sync_database_name(".sync.db"));
        assert!(is_sync_database_name(".Sync.db"));
        assert!(is_sync_database_name("_sync.db"));
        assert!(is_sync_database_name("sync_1.db.bak"));
        assert!(!is_sync_database_name("report.pdf"));
        assert!(!is_sync_database_name(".gitignore"));
        assert!(!is_sync_database_name("syncable.txt"));
    }

    #[test]
    fn find_sync_databases_returns_only_top_level_journals() {
        let dir = root();
        write_files(dir.path(), &[".sync_1.db", ".sync_2.db", "sub/.sync_3.db"]);
        let found = find_sync_databases(dir.path());
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|path| path.parent() == Some(dir.path())));
    }

    // ---- manifest -----------------------------------------------------------

    #[test]
    fn manifest_paths_differ_per_folder() {
        let a =
            DeleteGuardManifest::for_folder("https://cloud.example.com", "alice", "/tmp/NC", "");
        let b =
            DeleteGuardManifest::for_folder("https://cloud.example.com", "alice", "/tmp/Other", "");
        assert_ne!(a.path(), b.path());
        assert!(a
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("delete-guard-"));
        assert!(a.path().to_string_lossy().ends_with(".json"));
    }

    #[test]
    fn manifest_save_and_load_round_trip() {
        let dir = root();
        let manifest = DeleteGuardManifest::new(dir.path().join("guard.json"));
        manifest
            .save(
                "fp",
                dir.path(),
                &["b.txt".to_string(), "a.txt".to_string()],
            )
            .expect("save");
        let loaded = manifest.load().expect("load");
        assert_eq!(loaded.files, ["a.txt", "b.txt"]);
        assert_eq!(loaded.account_fingerprint, "fp");
        let meta = fs::metadata(manifest.path()).expect("metadata");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn missing_or_corrupt_manifest_loads_none() {
        let dir = root();
        let manifest = DeleteGuardManifest::new(dir.path().join("guard.json"));
        assert!(manifest.load().is_none());
        fs::write(manifest.path(), "not json").expect("write");
        assert!(manifest.load().is_none());
    }

    #[test]
    fn wrong_format_manifest_loads_none() {
        let dir = root();
        let manifest = DeleteGuardManifest::new(dir.path().join("guard.json"));
        let payload = DeleteGuardManifestData {
            format: 99,
            account_fingerprint: "fp".into(),
            local_root: "/tmp".into(),
            files: vec![],
        };
        let text = serde_json::to_string(&payload).unwrap();
        fs::write(manifest.path(), text).expect("write");
        assert!(manifest.load().is_none());
    }

    // ---- DeleteGuard behaviour ----------------------------------------------

    #[test]
    fn no_manifest_means_no_alert() {
        let dir = root();
        write_files(dir.path(), &["a.txt"]);
        let (guard, _manifest_dir) = guard_for(dir.path(), guard_config(3, 60));
        assert!(guard.check().is_none());
    }

    #[test]
    fn mass_deletion_is_blocked() {
        let dir = root();
        write_files(dir.path(), &["a.txt", "b.txt", "c.txt", "d.txt"]);
        let (guard, _manifest_dir) = guard_for(dir.path(), guard_config(3, 60));
        assert!(guard.record_current());
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::remove_file(dir.path().join(name)).expect("remove");
        }
        let alert = guard.check().expect("alert");
        assert_eq!(alert.reason, "mass_local_deletion");
        assert_eq!(alert.missing_paths.len(), 3);
        assert!(alert.can_approve_once);
        assert_eq!(alert.previous_count, 4);
        assert_eq!(alert.current_count, 1);
    }

    #[test]
    fn percent_threshold_can_trigger_with_few_files() {
        let dir = root();
        write_files(dir.path(), &["a.txt", "b.txt"]);
        let (guard, _manifest_dir) = guard_for(dir.path(), guard_config(100, 50));
        assert!(guard.record_current());
        fs::remove_file(dir.path().join("a.txt")).expect("remove");
        let alert = guard.check().expect("alert");
        assert_eq!(alert.reason, "mass_local_deletion");
    }

    #[test]
    fn emptied_folder_is_blocked() {
        let dir = root();
        write_files(dir.path(), &["a.txt"]);
        let (guard, _manifest_dir) = guard_for(dir.path(), guard_config(3, 60));
        assert!(guard.record_current());
        fs::remove_file(dir.path().join("a.txt")).expect("remove");
        let alert = guard.check().expect("alert");
        assert_eq!(alert.reason, "folder_emptied");
        assert!(alert.can_approve_once);
    }

    #[test]
    fn missing_folder_is_blocked_and_not_approvable() {
        let dir = root();
        write_files(dir.path(), &["a.txt"]);
        let (guard, _manifest_dir) = guard_for(dir.path(), guard_config(3, 60));
        assert!(guard.record_current());
        fs::remove_dir_all(dir.path()).expect("rmtree");
        let alert = guard.check().expect("alert");
        assert_eq!(alert.reason, "folder_missing");
        assert!(!alert.can_approve_once);
    }

    #[test]
    fn small_deletion_is_allowed() {
        let dir = root();
        write_files(dir.path(), &["a.txt", "b.txt", "c.txt", "d.txt"]);
        let (guard, _manifest_dir) = guard_for(dir.path(), guard_config(3, 60));
        assert!(guard.record_current());
        fs::remove_file(dir.path().join("a.txt")).expect("remove");
        assert!(guard.check().is_none());
    }

    #[test]
    fn disabled_guard_never_alerts() {
        let dir = root();
        write_files(dir.path(), &["a.txt"]);
        let manifest_dir = root();
        let guard = DeleteGuard::new(
            "test-fingerprint".to_string(),
            dir.path(),
            Vec::<String>::new(),
            true,
            DeleteGuardConfig {
                enabled: false,
                count_threshold: 3,
                percent_threshold: 60,
            },
            DeleteGuardManifest::new(manifest_dir.path().join("guard.json")),
        );
        let _manifest_dir = manifest_dir;
        assert!(guard.record_current());
        fs::remove_file(dir.path().join("a.txt")).expect("remove");
        assert!(guard.check().is_none());
    }

    #[test]
    fn record_current_updates_the_baseline() {
        let dir = root();
        write_files(dir.path(), &["a.txt"]);
        let (guard, _manifest_dir) = guard_for(dir.path(), guard_config(3, 60));
        assert!(guard.record_current());
        write_files(dir.path(), &["b.txt"]);
        assert!(guard.record_current());
        fs::remove_file(dir.path().join("a.txt")).expect("remove");
        assert!(guard.check().is_none());
    }

    #[test]
    fn fingerprint_mismatch_ignores_the_manifest() {
        let dir = root();
        write_files(dir.path(), &["a.txt"]);
        let manifest_dir = root();
        let manifest = || DeleteGuardManifest::new(manifest_dir.path().join("guard.json"));
        let other = DeleteGuard::new(
            "different-fingerprint".to_string(),
            dir.path(),
            Vec::<String>::new(),
            true,
            guard_config(3, 60),
            manifest(),
        );
        assert!(other.record_current());
        fs::remove_file(dir.path().join("a.txt")).expect("remove");
        let guard = DeleteGuard::new(
            "test-fingerprint".to_string(),
            dir.path(),
            Vec::<String>::new(),
            true,
            guard_config(3, 60),
            manifest(),
        );
        let _manifest_dir = manifest_dir;
        assert!(guard.check().is_none());
    }

    #[test]
    fn restore_from_server_removes_journals_and_resets_baseline() {
        let dir = root();
        write_files(
            dir.path(),
            &[
                "a.txt",
                "b.txt",
                ".sync_1.db",
                ".sync_2.db",
                "sub/.sync_3.db",
            ],
        );
        let (guard, _manifest_dir) = guard_for(dir.path(), guard_config(3, 60));
        assert!(guard.record_current());
        fs::remove_file(dir.path().join("a.txt")).expect("remove");
        fs::remove_file(dir.path().join("b.txt")).expect("remove");
        fs::remove_file(dir.path().join(".sync_1.db")).expect("remove");
        assert!(guard.check().is_some());

        let removed = guard.restore_from_server();
        // Two top-level journals; the nested one is out of reach.
        assert_eq!(removed, 1);
        assert!(find_sync_databases(dir.path()).is_empty());
        // The baseline now matches the emptied tree, so the guard is quiet.
        assert!(guard.check().is_none());
        assert!(guard.record_current());
    }

    #[test]
    fn restore_without_journals_still_resets_baseline() {
        let dir = root();
        write_files(dir.path(), &["a.txt"]);
        let (guard, _manifest_dir) = guard_for(dir.path(), guard_config(3, 60));
        assert!(guard.record_current());
        fs::remove_file(dir.path().join("a.txt")).expect("remove");
        assert!(guard.check().is_some());
        assert_eq!(guard.restore_from_server(), 0);
        assert!(guard.check().is_none());
    }
}
