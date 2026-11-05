//! Pure tray presentation logic.
//!
//! Fase 5 (Task 5.5): maps an [`AppState`] to the visual presentation of the
//! tray icon and menu, replicating `ui/tray_state.py` of the Python client
//! (v0.4.0). GTK-free on purpose so the full table is unit-testable without a
//! display or a DBus session.

use crate::state::AppState;
use crate::util::i18n::t;

/// Complete tray presentation for an application state.
///
/// Mirrors the `TrayPresentation` dataclass of `tray_state.py`. `label` is
/// translated by [`presentation_for`]; `icon_key` and `status` are machine
/// identifiers and stay untranslated.
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
/// cannot drift apart. The `label` fields are English msgids; they are
/// translated at read time in [`presentation_for`].
const STATE_PRESENTATIONS: [(AppState, TrayPresentation); 13] = [
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
        AppState::IdleNotSynced,
        TrayPresentation {
            icon_key: "paused",
            status: "Active",
            label: "Not Synchronized Yet",
            user_paused: false,
        },
    ),
    (
        AppState::SyncQueued,
        TrayPresentation {
            icon_key: "syncing",
            status: "Active",
            label: "Waiting to synchronize",
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
/// The `label` is translated through the active catalog (falling back to the
/// English msgid); `icon_key` and `status` are identifiers and never change.
/// The table is exhaustive over [`AppState`]; an unknown state cannot be
/// requested.
pub fn presentation_for(state: AppState) -> TrayPresentation {
    STATE_PRESENTATIONS
        .iter()
        .find_map(|(candidate, presentation)| (*candidate == state).then_some(*presentation))
        .expect("every AppState has a tray presentation")
        .translated()
}

/// Translate the label of a presentation in place.
impl TrayPresentation {
    fn translated(mut self) -> Self {
        self.label = t(self.label);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::i18n::{reset_locale, set_locale, Locale};

    #[test]
    fn presentation_table_covers_every_state_exactly_like_the_python() {
        set_locale(Locale::English);
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
                    label: "Waiting to synchronize",
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
        reset_locale();
    }

    #[test]
    fn the_table_defines_thirteen_presentations() {
        assert_eq!(STATE_PRESENTATIONS.len(), 13);
    }

    #[test]
    fn presentation_translates_labels_but_not_identifiers() {
        set_locale(Locale::Spanish);
        let presentation = presentation_for(AppState::Syncing);
        assert_eq!(presentation.label, "Sincronizando…");
        assert_eq!(presentation.icon_key, "syncing");
        assert_eq!(presentation.status, "Active");
        assert_eq!(presentation_for(AppState::Offline).label, "Sin conexión");
        reset_locale();
    }

    #[test]
    fn presentation_falls_back_to_the_english_label() {
        set_locale(Locale::Spanish);
        let mut unknown = TrayPresentation {
            icon_key: "error",
            status: "Active",
            label: "not in the catalog",
            user_paused: false,
        };
        unknown = unknown.translated();
        assert_eq!(unknown.label, "not in the catalog");
        reset_locale();
    }
}
