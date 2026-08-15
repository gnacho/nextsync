//! Sync-status emblems on synchronized folders in the file manager.
//!
//! Issue #44: Nautilus renders the `metadata::emblems` attribute (a
//! `stringv`) that the gvfs metadata daemon stores for local files, so the
//! app mirrors each folder's live [`AppState`] onto its local directory as
//! a themed emblem: a check when synchronized, a synchronizing swirl while
//! syncing, an error badge on problems and a pause badge while paused.
//!
//! The mechanism was verified by hand with
//! `gio set -t stringv <dir> metadata::emblems emblem-default` (the
//! attribute shows up in `gio info` and Nautilus renders it). Emblem names
//! are the freedesktop icon-name spec ones; on current Adwaita only
//! `emblem-default` and `emblem-synchronizing` ship as icons, so
//! `emblem-error` and `emblem-paused` may not render until the theme
//! provides them (verified on Adwaita 47-era packages).

use std::path::Path;

use gio::prelude::*;

use crate::state::AppState;

/// The themed emblem for a folder state, or `None` when the state should
/// not show one (`Unconfigured`, e.g. a folder without a runtime).
pub fn folder_emblem_for(state: AppState) -> Option<&'static str> {
    match state {
        AppState::Unconfigured => None,
        AppState::IdleOk => Some("emblem-default"),
        AppState::SyncQueued | AppState::Syncing => Some("emblem-synchronizing"),
        AppState::IdleManualOnly
        | AppState::PausedUser
        | AppState::PausedBattery
        | AppState::Offline => Some("emblem-paused"),
        AppState::Error
        | AppState::AuthRequired
        | AppState::KeyringLocked
        | AppState::DeleteReview => Some("emblem-error"),
    }
}

/// Write (or clear) the `metadata::emblems` attribute of a local folder.
///
/// Setting goes through [`gio::FileInfo`] + `set_attributes_from_info`,
/// the same path the `gio set -t stringv` tool uses; clearing writes an
/// empty emblem list, which file managers render as no emblem.
pub fn set_folder_emblem(path: &Path, emblem: Option<&str>) -> Result<(), glib::Error> {
    let file = gio::File::for_path(path);
    let info = gio::FileInfo::new();
    match emblem {
        Some(emblem) => info.set_attribute_stringv("metadata::emblems", vec![emblem.to_string()]),
        None => info.set_attribute_stringv("metadata::emblems", Vec::<String>::new()),
    }
    file.set_attributes_from_info(
        &info,
        gio::FileQueryInfoFlags::NONE,
        None::<&gio::Cancellable>,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_states() -> Vec<AppState> {
        vec![
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
        ]
    }

    #[test]
    fn every_state_maps_to_an_emblem_decision() {
        for state in all_states() {
            match folder_emblem_for(state) {
                Some(name) => assert!(name.starts_with("emblem-"), "{state:?} -> {name}"),
                None => assert_eq!(state, AppState::Unconfigured),
            }
        }
    }

    #[test]
    fn emblems_distinguish_the_four_outcomes() {
        assert_eq!(folder_emblem_for(AppState::IdleOk), Some("emblem-default"));
        assert_eq!(
            folder_emblem_for(AppState::Syncing),
            Some("emblem-synchronizing")
        );
        assert_eq!(
            folder_emblem_for(AppState::SyncQueued),
            Some("emblem-synchronizing")
        );
        assert_eq!(folder_emblem_for(AppState::Error), Some("emblem-error"));
        assert_eq!(
            folder_emblem_for(AppState::PausedUser),
            Some("emblem-paused")
        );
        assert_eq!(folder_emblem_for(AppState::Offline), Some("emblem-paused"));
    }

    /// Round-trip against the real gvfs metadata daemon (same pattern as
    /// the keyring test: skip when the environment cannot store metadata).
    #[test]
    fn emblem_round_trips_through_gio_metadata() {
        let _env = crate::util::test_env::lock();
        let Ok(home) = std::env::var("HOME") else {
            eprintln!("skipped: no HOME to write metadata within");
            return;
        };
        let dir = std::path::PathBuf::from(home)
            .join(format!("nextsync-emblem-test-{}", std::process::id()));
        if std::fs::create_dir_all(&dir).is_err() {
            eprintln!("skipped: could not create the emblem test directory");
            return;
        }
        let read_emblems = || {
            gio::File::for_path(&dir)
                .query_info(
                    "metadata::*",
                    gio::FileQueryInfoFlags::NONE,
                    None::<&gio::Cancellable>,
                )
                .map(|info| info.attribute_stringv("metadata::emblems").to_vec())
        };
        if set_folder_emblem(&dir, Some("emblem-default")).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            eprintln!("skipped: gvfs metadata is unavailable here");
            return;
        }
        match read_emblems() {
            Ok(emblems) => assert_eq!(emblems, vec!["emblem-default".to_string()]),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&dir);
                eprintln!("skipped: could not read metadata back: {error}");
                return;
            }
        }
        set_folder_emblem(&dir, None).expect("clearing the emblem works");
        if let Ok(emblems) = read_emblems() {
            assert!(emblems.is_empty(), "cleared emblems: {emblems:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
