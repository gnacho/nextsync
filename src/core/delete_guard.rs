//! Lightweight mass-deletion guard.
//!
//! Fase 3 (Task 3.1): detect bulk local deletions (count/percent threshold
//! against the last-sync manifest) and gate them via
//! `approve_delete_once` / `restore_from_server`.

/// Placeholder for the deletion guard.
pub struct DeleteGuard;

impl DeleteGuard {
    /// Reports whether a deletion needs approval.
    ///
    /// Placeholder: always `false` until Fase 3 lands.
    pub fn needs_approval() -> bool {
        false
    }
}
