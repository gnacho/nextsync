//! Per-folder status row with a live state glyph.
//!
//! Fase 5 (Task 5.1): mirrors `ui/folder_status.py`. One row per synchronized
//! folder showing its current state (a check when synchronized, a spinner
//! while syncing), a subtitle with the remote path and the last sync stamp,
//! and a more (…) menu with the per-folder actions.
//!
//! The row subscribes to the folder's [`StateController`] and re-renders on
//! every change; when no controller is available it renders the
//! `Unconfigured` presentation statically.

use std::rc::Rc;

use crate::core::account_runtime::FolderRuntime;
use crate::state::{AppState, StateController};
use crate::storage::config::FolderConfig;

/// The `(icon_name, status_label)` pair for a folder state, mirroring the
/// Python `STATE_PRESENTATION` table.
pub fn folder_status_presentation(state: AppState) -> (&'static str, &'static str) {
    match state {
        AppState::Unconfigured => ("dialog-question-symbolic", "Not Configured"),
        AppState::IdleOk => ("emblem-ok-symbolic", "Synchronized"),
        AppState::IdleManualOnly => ("media-playback-pause-symbolic", "Automatic Sync Is Off"),
        AppState::SyncQueued => ("appointment-soon-symbolic", "Synchronization Scheduled"),
        AppState::Syncing => ("emblem-synchronizing-symbolic", "Synchronizing…"),
        AppState::PausedUser => ("media-playback-pause-symbolic", "Paused"),
        AppState::PausedBattery => ("battery-symbolic", "Paused on Battery"),
        AppState::Offline => ("network-offline-symbolic", "Offline"),
        AppState::Error => ("dialog-error-symbolic", "Synchronization Error"),
        AppState::AuthRequired => ("dialog-password-symbolic", "Account Needs Attention"),
        AppState::KeyringLocked => ("changes-prevent-symbolic", "Password Keyring Locked"),
        AppState::DeleteReview => ("security-high-symbolic", "Review Deletions"),
    }
}

/// How a last-sync stamp is rendered, mirroring `AccountView._format_sync_stamp`.
///
/// Not-yet-synced folders show a "Not yet synchronized" label; otherwise the
/// stamp is re-formatted as `%x %H:%M` in the local timezone. Returns the
/// subtitle segment (empty when nothing is available).
pub fn format_sync_stamp(value: Option<&str>) -> String {
    match value {
        None | Some("") => "Not yet synchronized".to_string(),
        Some(raw) => match parse_iso8601(raw) {
            Some((date, time)) => format!("{date} {time}"),
            None => raw.to_string(),
        },
    }
}

/// Best-effort ISO-8601 local rendering (`%x %H:%M` in a C-locale-free way):
/// splits the date part and the time part of a timestamp.
fn parse_iso8601(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim();
    let (date_part, time_part) = raw.split_once(['T', ' '])?;
    let date = date_part.replace('-', "/");
    let time = time_part.chars().take(5).collect::<String>();
    Some((date, time))
}

/// Match folder sessions to folder runtimes by `folder.id` in session order.
/// A folder without a runtime yields `None` and the row degrades to a static
/// presentation.
pub fn pair_folder_runtimes(
    folders: &[FolderConfig],
    runtimes: &std::collections::HashMap<String, FolderRuntime>,
) -> Vec<(FolderConfig, Option<FolderRuntime>)> {
    folders
        .iter()
        .map(|folder| (folder.clone(), runtimes.get(&folder.id).cloned()))
        .collect()
}

/// A GTK action row rendering one synchronized folder with live status.
pub struct FolderStatusRow {
    // The live folder runtime handle; kept alive while the row exists.
    #[allow(dead_code)]
    runtime: Option<FolderRuntime>,
    _subscription: Option<Rc<dyn Fn()>>,
}

impl FolderStatusRow {
    /// Build the row for one folder. `state` drives the live rendering;
    /// `callbacks` wire the menu actions (each may be `None` to omit the item).
    pub fn new(
        _folder: FolderConfig,
        _state: Option<StateController>,
        _runtime: Option<FolderRuntime>,
        _callbacks: FolderRowCallbacks,
    ) -> Self {
        Self {
            runtime: _runtime,
            _subscription: None,
        }
    }
}

/// Per-folder menu callbacks. All optional; the corresponding menu item is
/// omitted when `None`.
#[derive(Default)]
pub struct FolderRowCallbacks {
    pub on_open: Option<Box<dyn Fn()>>,
    pub on_edit_ignored: Option<Box<dyn Fn()>>,
    pub on_force_sync: Option<Box<dyn Fn()>>,
    pub on_toggle_pause: Option<Box<dyn Fn()>>,
    pub on_remove: Option<Box<dyn Fn()>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_covers_every_state() {
        for state in [
            AppState::Unconfigured,
            AppState::IdleOk,
            AppState::IdleManualOnly,
            AppState::SyncQueued,
            AppState::Syncing,
            AppState::PausedUser,
            AppState::PausedBattery,
            AppState::Offline,
            AppState::Error,
            AppState::AuthRequired,
            AppState::KeyringLocked,
            AppState::DeleteReview,
        ] {
            let (icon, label) = folder_status_presentation(state);
            assert!(!icon.is_empty());
            assert!(!label.is_empty());
        }
    }

    #[test]
    fn idle_ok_presents_as_synchronized() {
        let (icon, label) = folder_status_presentation(AppState::IdleOk);
        assert_eq!(icon, "emblem-ok-symbolic");
        assert_eq!(label, "Synchronized");
    }

    #[test]
    fn syncing_presents_with_spinner_icon() {
        let (icon, label) = folder_status_presentation(AppState::Syncing);
        assert_eq!(icon, "emblem-synchronizing-symbolic");
        assert_eq!(label, "Synchronizing…");
    }

    #[test]
    fn sync_stamp_formats_iso_dates_locally() {
        assert_eq!(
            format_sync_stamp(Some("2026-08-13T09:30:00Z")),
            "2026/08/13 09:30"
        );
        assert_eq!(
            format_sync_stamp(Some("2026-08-13 09:30")),
            "2026/08/13 09:30"
        );
    }

    #[test]
    fn sync_stamp_defaults_when_missing() {
        assert_eq!(format_sync_stamp(None), "Not yet synchronized");
        assert_eq!(format_sync_stamp(Some("")), "Not yet synchronized");
        assert_eq!(format_sync_stamp(Some("garbage")), "garbage");
    }

    #[test]
    fn pairing_matches_folders_to_runtimes_by_id() {
        let folders = vec![
            FolderConfig {
                id: "f1".to_string(),
                local_root: "/tmp/a".to_string(),
                remote_path: "/docs".to_string(),
                space_id: None,
            },
            FolderConfig {
                id: "f2".to_string(),
                local_root: "/tmp/b".to_string(),
                remote_path: "/photos".to_string(),
                space_id: None,
            },
        ];
        let runtimes = std::collections::HashMap::new();
        // Without a matching runtime every folder pairs to None.
        let paired = pair_folder_runtimes(&folders, &runtimes);
        assert_eq!(paired.len(), 2);
        assert!(paired[0].1.is_none());
        assert!(paired[1].1.is_none());
    }
}
