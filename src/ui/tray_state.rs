//! Pure tray presentation logic.
//!
//! Fase 5 (Task 5.5): maps an [`AppState`] to the visual presentation of the
//! tray icon and menu, replicating `ui/tray_state.py` of the Python client
//! (v0.4.0). GTK-free on purpose so the full table is unit-testable without a
//! display or a DBus session.

use crate::state::AppState;

/// Complete tray presentation for an application state.
///
/// Mirrors the `TrayPresentation` dataclass of `tray_state.py`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrayPresentation {
    /// Key of the status icon (`ok`, `syncing`, `paused`, `battery`,
    /// `offline`, `error`), matching the `nextsync-status-<key>-symbolic`
    /// SVGs installed by the packaging.
    pub icon_key: &'static str,
    /// StatusNotifierItem `Status` property (`"Active"` |
    /// `"NeedsAttention"`).
    pub status: &'static str,
    /// Human-readable state label (title suffix and tooltip).
    pub label: &'static str,
    /// Whether the state is a user pause.
    pub user_paused: bool,
}

/// The exact presentation table of the Python `STATE_PRESENTATIONS`.
///
/// Kept as the single source of truth so both `presentation_for` and the tests
/// cannot drift apart.
const STATE_PRESENTATIONS: [(AppState, TrayPresentation); 12] = [
    (
        AppState::Unconfigured,
        TrayPresentation {
            icon_key: "offline",
            status: "NeedsAttention",
            label: "Not Configured",
            user_paused: false,
        },
    ),
    (
        AppState::IdleOk,
        TrayPresentation {
            icon_key: "ok",
            status: "Active",
            label: "Synchronized",
            user_paused: false,
        },
    ),
    (
        AppState::IdleManualOnly,
        TrayPresentation {
            icon_key: "paused",
            status: "Active",
            label: "Automatic Sync Is Off",
            user_paused: false,
        },
    ),
    (
        AppState::SyncQueued,
        TrayPresentation {
            icon_key: "syncing",
            status: "Active",
            label: "Synchronization Scheduled",
            user_paused: false,
        },
    ),
    (
        AppState::Syncing,
        TrayPresentation {
            icon_key: "syncing",
            status: "Active",
            label: "Synchronizing…",
            user_paused: false,
        },
    ),
    (
        AppState::PausedUser,
        TrayPresentation {
            icon_key: "paused",
            status: "Active",
            label: "Paused",
            user_paused: true,
        },
    ),
    (
        AppState::PausedBattery,
        TrayPresentation {
            icon_key: "battery",
            status: "Active",
            label: "Paused on Battery",
            user_paused: false,
        },
    ),
    (
        AppState::Offline,
        TrayPresentation {
            icon_key: "offline",
            status: "Active",
            label: "Offline",
            user_paused: false,
        },
    ),
    (
        AppState::Error,
        TrayPresentation {
            icon_key: "error",
            status: "NeedsAttention",
            label: "Synchronization Error",
            user_paused: false,
        },
    ),
    (
        AppState::AuthRequired,
        TrayPresentation {
            icon_key: "error",
            status: "NeedsAttention",
            label: "Account Needs Attention",
            user_paused: false,
        },
    ),
    (
        AppState::KeyringLocked,
        TrayPresentation {
            icon_key: "error",
            status: "NeedsAttention",
            label: "Password Keyring Locked",
            user_paused: false,
        },
    ),
    (
        AppState::DeleteReview,
        TrayPresentation {
            icon_key: "error",
            status: "NeedsAttention",
            label: "Review Deletions",
            user_paused: false,
        },
    ),
];

/// Return the complete tray presentation for an application state.
///
/// The table is exhaustive over [`AppState`]; an unknown state cannot be
/// requested.
pub fn presentation_for(state: AppState) -> TrayPresentation {
    STATE_PRESENTATIONS
        .iter()
        .find_map(|(candidate, presentation)| (*candidate == state).then_some(*presentation))
        .expect("every AppState has a tray presentation")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_table_covers_every_state_exactly_like_the_python() {
        let expected = [
            (
                AppState::Unconfigured,
                TrayPresentation {
                    icon_key: "offline",
                    status: "NeedsAttention",
                    label: "Not Configured",
                    user_paused: false,
                },
            ),
            (
                AppState::IdleOk,
                TrayPresentation {
                    icon_key: "ok",
                    status: "Active",
                    label: "Synchronized",
                    user_paused: false,
                },
            ),
            (
                AppState::IdleManualOnly,
                TrayPresentation {
                    icon_key: "paused",
                    status: "Active",
                    label: "Automatic Sync Is Off",
                    user_paused: false,
                },
            ),
            (
                AppState::SyncQueued,
                TrayPresentation {
                    icon_key: "syncing",
                    status: "Active",
                    label: "Synchronization Scheduled",
                    user_paused: false,
                },
            ),
            (
                AppState::Syncing,
                TrayPresentation {
                    icon_key: "syncing",
                    status: "Active",
                    label: "Synchronizing…",
                    user_paused: false,
                },
            ),
            (
                AppState::PausedUser,
                TrayPresentation {
                    icon_key: "paused",
                    status: "Active",
                    label: "Paused",
                    user_paused: true,
                },
            ),
            (
                AppState::PausedBattery,
                TrayPresentation {
                    icon_key: "battery",
                    status: "Active",
                    label: "Paused on Battery",
                    user_paused: false,
                },
            ),
            (
                AppState::Offline,
                TrayPresentation {
                    icon_key: "offline",
                    status: "Active",
                    label: "Offline",
                    user_paused: false,
                },
            ),
            (
                AppState::Error,
                TrayPresentation {
                    icon_key: "error",
                    status: "NeedsAttention",
                    label: "Synchronization Error",
                    user_paused: false,
                },
            ),
            (
                AppState::AuthRequired,
                TrayPresentation {
                    icon_key: "error",
                    status: "NeedsAttention",
                    label: "Account Needs Attention",
                    user_paused: false,
                },
            ),
            (
                AppState::KeyringLocked,
                TrayPresentation {
                    icon_key: "error",
                    status: "NeedsAttention",
                    label: "Password Keyring Locked",
                    user_paused: false,
                },
            ),
            (
                AppState::DeleteReview,
                TrayPresentation {
                    icon_key: "error",
                    status: "NeedsAttention",
                    label: "Review Deletions",
                    user_paused: false,
                },
            ),
        ];

        for (state, presentation) in expected {
            assert_eq!(presentation_for(state), presentation, "state {state:?}");
        }
    }

    #[test]
    fn the_table_defines_twelve_presentations() {
        assert_eq!(STATE_PRESENTATIONS.len(), 12);
    }
}
