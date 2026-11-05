//! Diff local state, remote state and journal into a plan.

use crate::db::DbEntry;
use crate::http::RemoteEntry;
use crate::local::LocalEntry;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Download {
        rel: String,
        etag: String,
        checksum: Option<String>,
        size: u64,
        mtime: Option<i64>,
    },
    Upload {
        rel: String,
    },
    DeleteLocal {
        rel: String,
    },
    DeleteRemote {
        rel: String,
    },
    MkdirLocal {
        rel: String,
    },
    MkdirRemote {
        rel: String,
    },
    MoveLocal {
        from: String,
        to: String,
    },
    MoveRemote {
        from: String,
        to: String,
    },
    Conflict {
        rel: String,
    },
    RecordOnly {
        rel: String,
    },
}

impl Action {
    pub fn rel(&self) -> &str {
        match self {
            Action::Download { rel, .. }
            | Action::Upload { rel }
            | Action::DeleteLocal { rel }
            | Action::DeleteRemote { rel }
            | Action::MkdirLocal { rel }
            | Action::MkdirRemote { rel }
            | Action::Conflict { rel }
            | Action::RecordOnly { rel } => rel,
            Action::MoveLocal { to, .. } | Action::MoveRemote { to, .. } => to,
        }
    }

    pub fn is_delete(&self) -> bool {
        matches!(
            self,
            Action::DeleteLocal { .. } | Action::DeleteRemote { .. }
        )
    }

    pub fn is_write(&self) -> bool {
        !matches!(self, Action::RecordOnly { .. })
    }
}

const MTIME_TOLERANCE: i64 = 2;

/// True when some ancestor of `key` exists locally as a file. The remote side
/// then describes a subtree that the local side holds as one file, which is a
/// type mismatch, never a deletion (#271).
fn shadowed_by_local_file(key: &str, local: &BTreeMap<&str, &LocalEntry>) -> bool {
    let mut prefix = String::new();
    for segment in key.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(segment);
        if prefix == key {
            break;
        }
        if let Some(entry) = local.get(prefix.as_str()) {
            if !entry.is_dir {
                return true;
            }
        }
    }
    false
}

pub fn reconcile(
    local: &[LocalEntry],
    remote: &[RemoteEntry],
    db: &[DbEntry],
    remote_scan_complete: bool,
) -> Vec<Action> {
    let loc: BTreeMap<&str, &LocalEntry> = local.iter().map(|e| (e.rel.as_str(), e)).collect();
    let rem: BTreeMap<&str, &RemoteEntry> = remote.iter().map(|e| (e.path.as_str(), e)).collect();
    let mut jr: BTreeMap<&str, &DbEntry> = BTreeMap::new();
    for e in db {
        jr.insert(e.path.as_str(), e);
    }

    let mut actions: Vec<Action> = Vec::new();
    let mut moved_remote_old: Vec<String> = Vec::new();

    if remote_scan_complete {
        let mut by_fileid: HashMap<&str, &DbEntry> = HashMap::new();
        for e in db {
            if let Some(fid) = e.fileid.as_deref() {
                by_fileid.insert(fid, e);
            }
        }
        for r in remote {
            if r.is_dir {
                continue;
            }
            let Some(fid) = r.fileid.as_deref() else {
                continue;
            };
            let Some(old) = by_fileid.get(fid) else {
                continue;
            };
            if old.path == r.path {
                continue;
            }
            if rem.contains_key(old.path.as_str()) || !loc.contains_key(old.path.as_str()) {
                continue;
            }
            actions.push(Action::MoveLocal {
                from: old.path.clone(),
                to: r.path.clone(),
            });
            moved_remote_old.push(old.path.clone());
        }
    }

    let mut keys: Vec<&str> = Vec::new();
    keys.extend(loc.keys().copied());
    keys.extend(rem.keys().copied());
    keys.extend(jr.keys().copied());
    keys.sort_unstable();
    keys.dedup();

    let budget = |e: &DbEntry| -> (bool, bool) {
        let l = loc.get(e.path.as_str());
        let r = rem.get(e.path.as_str());
        (l.is_some(), r.is_some())
    };

    for key in keys {
        // The journal lives inside the synced folder. It must never take part
        // in reconciliation: an earlier run could have uploaded it, and with a
        // complete remote listing the engine would then delete the local copy.
        if key == ".nextsync" || key.starts_with(".nextsync/") {
            continue;
        }
        let l = loc.get(key);
        let r = rem.get(key);
        let j = jr.get(key);

        if let Some((from, to)) = actions.iter().find_map(|a| match a {
            Action::MoveLocal { from, to } if to == key => Some((from.clone(), to.clone())),
            _ => None,
        }) {
            let _ = (from, to);
            continue;
        }

        match (l, r, j) {
            (Some(l), Some(r), Some(j)) => {
                let remote_changed = r.etag != j.etag;
                // A different inode does not mean different content: restoring
                // a backup, copying with `cp -a` or moving a tree across
                // filesystems changes every inode while leaving bytes and
                // mtimes untouched. Treating that as a local edit queued the
                // whole tree for upload, so only size and mtime decide here.
                let local_changed = l.size != j.size || l.mtime != j.mtime;
                if l.is_dir != r.is_dir || (remote_changed && local_changed) {
                    actions.push(Action::Conflict {
                        rel: key.to_string(),
                    });
                } else if remote_changed && !l.is_dir {
                    actions.push(Action::Download {
                        rel: key.to_string(),
                        etag: r.etag.clone(),
                        size: r.size,
                        mtime: r.mtime,
                        checksum: r.checksum.clone(),
                    });
                } else if local_changed && !l.is_dir {
                    actions.push(Action::Upload {
                        rel: key.to_string(),
                    });
                } else {
                    actions.push(Action::RecordOnly {
                        rel: key.to_string(),
                    });
                }
            }
            (Some(l), None, Some(_)) => {
                if !remote_scan_complete || moved_remote_old.iter().any(|o| o == key) {
                    continue;
                }
                let _ = l;
                actions.push(Action::DeleteLocal {
                    rel: key.to_string(),
                });
            }
            (None, Some(_r), Some(_)) => {
                // Never delete a remote entry whose path is shadowed locally by
                // a file: a type mismatch (file here, directory there) says the
                // two sides disagree, not that the user deleted anything (#271).
                if remote_scan_complete && !shadowed_by_local_file(key, &loc) {
                    actions.push(Action::DeleteRemote {
                        rel: key.to_string(),
                    });
                }
            }
            (None, None, Some(_)) => {
                // Gone on both sides: the journal entry is stale, there is
                // nothing to delete anywhere. Deleting here used to fire a
                // no-op DELETE per pruned entry (#271).
                continue;
            }
            (Some(l), None, None) => {
                if l.is_dir {
                    actions.push(Action::MkdirRemote {
                        rel: key.to_string(),
                    });
                } else {
                    actions.push(Action::Upload {
                        rel: key.to_string(),
                    });
                }
            }
            (None, Some(r), None) => {
                if r.is_dir {
                    actions.push(Action::MkdirLocal {
                        rel: key.to_string(),
                    });
                } else {
                    actions.push(Action::Download {
                        rel: key.to_string(),
                        etag: r.etag.clone(),
                        size: r.size,
                        mtime: r.mtime,
                        checksum: r.checksum.clone(),
                    });
                }
            }
            (Some(l), Some(r), None) => {
                if (l.is_dir && r.is_dir)
                    || (!l.is_dir && !r.is_dir && l.size == r.size && same_mtime(l.mtime, r.mtime))
                {
                    actions.push(Action::RecordOnly {
                        rel: key.to_string(),
                    });
                } else {
                    actions.push(Action::Conflict {
                        rel: key.to_string(),
                    });
                }
            }
            (None, None, None) => {}
        }
    }

    let _ = budget;
    sort_plan(&mut actions);
    actions
}

fn same_mtime(local: i64, remote: Option<i64>) -> bool {
    match remote {
        Some(r) => (local - r).abs() <= MTIME_TOLERANCE,
        None => false,
    }
}

/// Directories first, then the rest, deletes last.
fn sort_plan(actions: &mut [Action]) {
    actions.sort_by_key(|a| match a {
        Action::MkdirLocal { .. } | Action::MkdirRemote { .. } => (0, a.rel().to_string()),
        Action::MoveLocal { .. } | Action::MoveRemote { .. } => (1, a.rel().to_string()),
        Action::Download { .. } | Action::Upload { .. } | Action::Conflict { .. } => {
            (2, a.rel().to_string())
        }
        Action::RecordOnly { .. } => (3, a.rel().to_string()),
        Action::DeleteRemote { .. } | Action::DeleteLocal { .. } => (4, a.rel().to_string()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(rel: &str, size: u64, mtime: i64, inode: u64) -> LocalEntry {
        LocalEntry {
            rel: rel.into(),
            is_dir: false,
            size,
            mtime,
            inode,
        }
    }
    fn ldir(rel: &str) -> LocalEntry {
        LocalEntry {
            rel: rel.into(),
            is_dir: true,
            size: 0,
            mtime: 0,
            inode: 0,
        }
    }
    fn rem(rel: &str, etag: &str, fileid: Option<&str>) -> RemoteEntry {
        RemoteEntry {
            path: rel.into(),
            is_dir: false,
            size: 10,
            etag: etag.into(),
            checksum: None,
            fileid: fileid.map(str::to_string),
            mtime: Some(100),
        }
    }
    fn rdir(rel: &str) -> RemoteEntry {
        RemoteEntry {
            path: rel.into(),
            is_dir: true,
            size: 0,
            etag: String::new(),
            checksum: None,
            fileid: None,
            mtime: None,
        }
    }
    fn db(rel: &str, size: u64, mtime: i64, etag: &str, fileid: Option<&str>) -> DbEntry {
        DbEntry {
            path: rel.into(),
            is_dir: false,
            size,
            mtime,
            etag: etag.into(),
            fileid: fileid.map(str::to_string),
            inode: 1,
        }
    }

    #[test]
    fn unchanged_records_only() {
        let acts = reconcile(
            &[loc("a", 10, 100, 1)],
            &[rem("a", "e1", Some("1"))],
            &[db("a", 10, 100, "e1", Some("1"))],
            true,
        );
        assert_eq!(acts, vec![Action::RecordOnly { rel: "a".into() }]);
    }

    #[test]
    fn same_size_local_edit_is_detected_by_mtime() {
        let acts = reconcile(
            &[loc("a", 10, 200, 1)],
            &[rem("a", "e1", None)],
            &[db("a", 10, 100, "e1", None)],
            true,
        );
        assert_eq!(acts, vec![Action::Upload { rel: "a".into() }]);
    }

    #[test]
    fn remote_change_downloads() {
        let acts = reconcile(
            &[loc("a", 10, 100, 1)],
            &[rem("a", "e2", None)],
            &[db("a", 10, 100, "e1", None)],
            true,
        );
        assert!(matches!(acts[0], Action::Download { .. }));
    }

    #[test]
    fn both_changed_conflicts() {
        let acts = reconcile(
            &[loc("a", 10, 300, 1)],
            &[rem("a", "e2", None)],
            &[db("a", 10, 100, "e1", None)],
            true,
        );
        assert_eq!(acts, vec![Action::Conflict { rel: "a".into() }]);
    }

    #[test]
    fn new_local_uploads_new_remote_downloads() {
        let acts = reconcile(&[loc("l", 1, 1, 1)], &[rem("r", "e", None)], &[], true);
        assert!(acts.contains(&Action::Upload { rel: "l".into() }));
        assert!(acts
            .iter()
            .any(|a| matches!(a, Action::Download { rel, .. } if rel == "r")));
    }

    #[test]
    fn first_run_equal_files_are_recorded_not_conflicted() {
        let mut r = rem("a", "e9", None);
        r.size = 10;
        r.mtime = Some(100);
        let acts = reconcile(&[loc("a", 10, 100, 1)], &[r], &[], true);
        assert_eq!(acts, vec![Action::RecordOnly { rel: "a".into() }]);
    }

    #[test]
    fn first_run_different_files_conflict() {
        let mut r = rem("a", "e9", None);
        r.size = 99;
        let acts = reconcile(&[loc("a", 10, 100, 1)], &[r], &[], true);
        assert_eq!(acts, vec![Action::Conflict { rel: "a".into() }]);
    }

    #[test]
    fn missing_local_deletes_remote() {
        let acts = reconcile(
            &[],
            &[rem("a", "e9", None)],
            &[db("a", 1, 1, "e", None)],
            true,
        );
        assert_eq!(acts, vec![Action::DeleteRemote { rel: "a".into() }]);
    }

    #[test]
    fn entry_gone_on_both_sides_produces_no_action() {
        let acts = reconcile(&[], &[], &[db("a", 1, 1, "e", None)], true);
        assert!(acts.is_empty(), "expected no actions, got {acts:?}");
    }

    #[test]
    fn missing_remote_deletes_local() {
        let acts = reconcile(&[loc("a", 1, 1, 1)], &[], &[db("a", 1, 1, "e", None)], true);
        assert_eq!(acts, vec![Action::DeleteLocal { rel: "a".into() }]);
    }

    #[test]
    fn remote_rename_moves_local() {
        let acts = reconcile(
            &[loc("old", 10, 100, 1)],
            &[rem("new", "e1", Some("77"))],
            &[db("old", 10, 100, "e1", Some("77"))],
            true,
        );
        assert_eq!(
            acts,
            vec![Action::MoveLocal {
                from: "old".into(),
                to: "new".into()
            }]
        );
        assert!(!acts.iter().any(|a| matches!(a, Action::DeleteLocal { .. })));
    }

    #[test]
    fn new_directory_both_sides_records() {
        let acts = reconcile(&[ldir("d")], &[rdir("d")], &[], true);
        assert_eq!(acts, vec![Action::RecordOnly { rel: "d".into() }]);
    }

    #[test]
    fn incomplete_remote_scan_never_deletes() {
        let acts = reconcile(
            &[loc("a", 1, 1, 1)],
            &[],
            &[db("a", 1, 1, "e", None)],
            false,
        );
        assert!(!acts.iter().any(|a| a.is_delete()));
    }
}
