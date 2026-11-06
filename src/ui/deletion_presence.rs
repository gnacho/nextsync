//! Server presence of the paths shown by the deletion review (issue #294).
//!
//! The review lists local files that the remote listing no longer has, and
//! the answers differ in what they can recover: a rescan brings back only
//! what the server still has, the trash only what was deleted server-side.
//! This module probes each reviewed path and marks its row, off the UI
//! thread and capped (see [`crate::core::server_state`]), so the dialog
//! never claims more than it checked.

use std::collections::{HashMap, HashSet};

use libadwaita::prelude::*;

use crate::core::server_state::{
    group_subtitle, row_subtitle, PathPresence, PresenceSummary, GROUP_PRESENCE_CAP,
    MAX_PRESENCE_CHECKS,
};
use crate::nextcloud::api::{ApiError, NextcloudApi};
use crate::nextcloud::credentials::CredentialsStore;
use crate::nextcloud::provider::Provider;
use crate::storage::config::{AccountConfig, FolderConfig};
use crate::util::i18n::t;

/// Workers probing the server at once: one shallow `PROPFIND` each, enough
/// to keep a 200-path check well under a second without hammering a small
/// server.
const PRESENCE_WORKERS: usize = 4;

/// One reviewed row that stands for exactly one path.
struct PresenceRow {
    path: String,
    row: libadwaita::ActionRow,
}

/// One group row plus every path it counts, so its subtitle can summarize
/// what the probes found for the folder.
struct PresenceGroup {
    paths: Vec<String>,
    row: libadwaita::ExpanderRow,
}

/// Rows of a deletion review, ready to be marked with the server state.
#[derive(Default)]
pub(crate) struct PresenceJob {
    rows: Vec<PresenceRow>,
    groups: Vec<PresenceGroup>,
}

impl PresenceJob {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register one row that shows a single path.
    pub(crate) fn add_row(&mut self, path: impl Into<String>, row: &libadwaita::ActionRow) {
        self.rows.push(PresenceRow {
            path: path.into(),
            row: row.clone(),
        });
    }

    /// Register one group row with all the paths it counts.
    pub(crate) fn add_group(&mut self, paths: Vec<String>, row: &libadwaita::ExpanderRow) {
        self.groups.push(PresenceGroup {
            paths,
            row: row.clone(),
        });
    }

    /// Probe the collected paths and rewrite the subtitles with the answer.
    /// Nothing happens when the dialog shows no path at all.
    pub(crate) fn start(self, account: &AccountConfig, folder: &FolderConfig) {
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        for reviewed in &self.rows {
            reviewed.row.set_subtitle(t("checking the server…"));
        }
        let account = account.clone();
        let folder = folder.clone();
        let handle = gio::spawn_blocking(move || probe(&account, &folder, &targets));
        glib::spawn_future_local(async move {
            let Ok(results) = handle.await else {
                return;
            };
            if results
                .iter()
                .all(|(_, presence)| *presence == PathPresence::Unchecked)
            {
                // Nothing could be checked (offline, rejected credentials):
                // go back to the plain list instead of labelling every row.
                for reviewed in &self.rows {
                    reviewed.row.set_subtitle("");
                }
                return;
            }
            let by_path: HashMap<&str, PathPresence> = results
                .iter()
                .map(|(path, presence)| (path.as_str(), *presence))
                .collect();
            for reviewed in &self.rows {
                let subtitle = by_path
                    .get(reviewed.path.as_str())
                    .map(|presence| row_subtitle(*presence))
                    .unwrap_or_default();
                reviewed.row.set_subtitle(subtitle);
            }
            for group in &self.groups {
                let summary =
                    PresenceSummary::of(group.paths.iter().enumerate().map(|(index, path)| {
                        if index >= GROUP_PRESENCE_CAP {
                            PathPresence::Unchecked
                        } else {
                            by_path
                                .get(path.as_str())
                                .copied()
                                .unwrap_or(PathPresence::Unchecked)
                        }
                    }));
                group
                    .row
                    .set_subtitle(&group_subtitle(group.paths.len(), &summary));
            }
        });
    }

    /// Paths to probe: group prefixes first (their subtitle summarizes a
    /// whole folder, which is what a mass deletion needs), then single rows.
    fn targets(&self) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut targets: Vec<String> = Vec::new();
        let mut push = |path: &str| {
            if targets.len() >= MAX_PRESENCE_CHECKS {
                return;
            }
            if seen.insert(path.to_string()) {
                targets.push(path.to_string());
            }
        };
        for group in &self.groups {
            for path in group.paths.iter().take(GROUP_PRESENCE_CAP) {
                push(path);
            }
        }
        for reviewed in &self.rows {
            push(&reviewed.path);
        }
        targets
    }
}

/// Ask the server about every path, four at a time. A failure stops its
/// worker and leaves the untried paths as [`PathPresence::Unchecked`], so a
/// dropped connection never reads as "the file is gone".
fn probe(
    account: &AccountConfig,
    folder: &FolderConfig,
    targets: &[String],
) -> Vec<(String, PathPresence)> {
    let Some(secret) =
        CredentialsStore::get_for_account(&account.id, &account.server_url, &account.login_name)
            .ok()
            .flatten()
    else {
        return unchecked(targets);
    };
    let chunks: Vec<&[String]> = targets
        .chunks(targets.len().div_ceil(PRESENCE_WORKERS).max(1))
        .collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                let secret = secret.clone();
                scope.spawn(move || probe_chunk(account, folder, &secret, chunk))
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    })
}

/// Probe one slice of the paths with its own API handle. Stops at the first
/// error: the rest stay unchecked.
fn probe_chunk(
    account: &AccountConfig,
    folder: &FolderConfig,
    secret: &str,
    paths: &[String],
) -> Vec<(String, PathPresence)> {
    let api = NextcloudApi::new();
    let mut results = Vec::with_capacity(paths.len());
    for path in paths {
        match exists(&api, account, folder, secret, path) {
            Ok(true) => results.push((path.clone(), PathPresence::Present)),
            Ok(false) => results.push((path.clone(), PathPresence::Gone)),
            Err(error) => {
                eprintln!("server presence check for {path} stopped: {error}");
                break;
            }
        }
    }
    results
}

/// One path, provider-aware.
fn exists(
    api: &NextcloudApi,
    account: &AccountConfig,
    folder: &FolderConfig,
    secret: &str,
    path: &str,
) -> Result<bool, ApiError> {
    match account.provider {
        Provider::Nextcloud => api.path_exists(
            &account.server_url,
            &account.login_name,
            secret,
            &folder.remote_path,
            path,
        ),
        Provider::OpenCloud => {
            let space = folder.space_id.as_deref().unwrap_or_default();
            api.opencloud_path_exists(
                &account.server_url,
                &account.login_name,
                secret,
                space,
                &folder.remote_path,
                path,
            )
        }
    }
}

fn unchecked(targets: &[String]) -> Vec<(String, PathPresence)> {
    targets
        .iter()
        .map(|path| (path.clone(), PathPresence::Unchecked))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The targets of a review: group prefixes first, duplicates collapsed.
    #[test]
    fn targets_put_groups_first_and_drop_duplicates() {
        crate::ui::test_helpers::gtk_smoke(|| {
            let mut job = PresenceJob::new();
            let group = libadwaita::ExpanderRow::new();
            job.add_group(
                vec!["Documents/a.txt".to_string(), "Documents/b.txt".to_string()],
                &group,
            );
            job.add_row("Documents/b.txt", &libadwaita::ActionRow::new());
            job.add_row("top.txt", &libadwaita::ActionRow::new());
            assert_eq!(
                job.targets(),
                vec![
                    "Documents/a.txt".to_string(),
                    "Documents/b.txt".to_string(),
                    "top.txt".to_string()
                ]
            );
        });
    }

    #[test]
    fn targets_stop_at_the_cap() {
        crate::ui::test_helpers::gtk_smoke(|| {
            let mut job = PresenceJob::new();
            for index in 0..(MAX_PRESENCE_CHECKS + 10) {
                job.add_row(format!("file-{index}.txt"), &libadwaita::ActionRow::new());
            }
            assert_eq!(job.targets().len(), MAX_PRESENCE_CHECKS);
        });
    }

    #[test]
    fn a_group_only_targets_its_first_paths() {
        crate::ui::test_helpers::gtk_smoke(|| {
            let mut job = PresenceJob::new();
            let paths: Vec<String> = (0..40).map(|index| format!("SDK/f{index}")).collect();
            job.add_group(paths, &libadwaita::ExpanderRow::new());
            assert_eq!(job.targets().len(), GROUP_PRESENCE_CAP);
        });
    }
}
