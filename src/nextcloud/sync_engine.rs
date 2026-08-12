//! Sync engine.
//!
//! Fase 2 (Task 2.3): spawn `nextcloudcmd`, drain stdout+stderr in parallel
//! threads (anti-deadlock on the 64 KB pipe) and forward lines to the state
//! channel as `SyncProgress`.

/// Placeholder for the sync engine.
pub struct SyncEngine;

impl SyncEngine {
    /// Reports whether a sync is in progress.
    ///
    /// Placeholder: always `false` until Fase 2 lands.
    pub fn is_syncing() -> bool {
        false
    }
}
