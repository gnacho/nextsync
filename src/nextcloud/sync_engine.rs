//! Sync engine.
//!
//! Fase 2 (Task 2.3): spawn `nextcloudcmd`, drain stdout+stderr in parallel
//! threads (anti-deadlock on the 64 KB pipe) and forward lines to the state
//! channel as `SyncProgress`.

/// One parsed progress event from `nextcloudcmd` output.
///
/// Mirrors `nextcloudcmd_progress.SyncProgress`: `processed` counts operations
/// reported so far in the current sync when a total is unavailable; it starts
/// at 1 for the first parsed line. The line parser itself lands in Task 2.3.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncProgress {
    /// Normalized action: `download`, `upload`, `delete`, `conflict`, ...
    pub action: String,
    /// Path of the file being operated on.
    pub path: String,
    /// Number of operations reported so far in the current sync.
    pub processed: u32,
}

impl SyncProgress {
    /// Create a progress event with a zero operation counter.
    pub fn new(action: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            path: path.into(),
            processed: 0,
        }
    }

    /// Whether this event describes a real file operation.
    pub fn is_operation(&self) -> bool {
        matches!(
            self.action.as_str(),
            "download" | "upload" | "delete" | "conflict"
        )
    }

    /// Short human label, mirroring `nextcloudcmd_progress.describe_progress`.
    pub fn describe(&self) -> String {
        if self.is_operation() && self.processed > 0 {
            format!("{}: {} ({})", self.action, self.path, self.processed)
        } else {
            format!("{}: {}", self.action, self.path)
        }
    }
}

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
