//! Sync triggers.
//!
//! Fase 2 (Task 2.2): local inotify, local interval, remote push and remote
//! interval (the four configurable kinds), plus the internal triggers the
//! scheduler uses. Mirrors `core/triggers.py`: the `Trigger` enum, the
//! `manual_only` predicate and the reason-preserving [`CoalescingQueue`].

use std::collections::HashSet;
use std::fmt;

/// The reason a synchronization was requested.
///
/// The four user-facing triggers (`LocalInotify`, `LocalInterval`,
/// `RemotePush`, `RemoteInterval`) map to the four settings switches; the
/// rest are produced internally by the app (startup, resume, network
/// restored, retry, manual, local recovery).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Trigger {
    LocalInotify,
    LocalInterval,
    LocalRecovery,
    RemotePush,
    RemoteInterval,
    Manual,
    Startup,
    NetworkRestored,
    Resume,
    Retry,
}

impl Trigger {
    /// Stable machine-readable name, matching `Trigger.value` in Python.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalInotify => "LOCAL_INOTIFY",
            Self::LocalInterval => "LOCAL_INTERVAL",
            Self::LocalRecovery => "LOCAL_RECOVERY",
            Self::RemotePush => "REMOTE_PUSH",
            Self::RemoteInterval => "REMOTE_INTERVAL",
            Self::Manual => "MANUAL",
            Self::Startup => "STARTUP",
            Self::NetworkRestored => "NETWORK_RESTORED",
            Self::Resume => "RESUME",
            Self::Retry => "RETRY",
        }
    }
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The four configurable trigger switches (mirrors the `sync` settings dict).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TriggerSettings {
    pub local_inotify_enabled: bool,
    pub local_interval_enabled: bool,
    pub local_interval_minutes: i64,
    pub remote_push_enabled: bool,
    pub remote_interval_enabled: bool,
    pub remote_interval_minutes: i64,
}

/// True when none of the four automatic triggers is enabled, meaning only
/// manual synchronizations are allowed. Mirrors `triggers.manual_only`.
pub fn manual_only(settings: &TriggerSettings) -> bool {
    !(settings.local_inotify_enabled
        || settings.local_interval_enabled
        || settings.remote_push_enabled
        || settings.remote_interval_enabled)
}

/// A path-free, reason-preserving queue for full reconciliations.
///
/// Every trigger pushed here is kept as a set entry, so duplicate reasons
/// coalesce and `take` drains everything at once.
#[derive(Debug, Clone, Default)]
pub struct CoalescingQueue {
    reasons: HashSet<Trigger>,
}

impl CoalescingQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one trigger reason (duplicates coalesce).
    pub fn add(&mut self, trigger: Trigger) {
        self.reasons.insert(trigger);
    }

    /// Add several trigger reasons at once.
    pub fn extend(&mut self, triggers: impl IntoIterator<Item = Trigger>) {
        self.reasons.extend(triggers);
    }

    /// Remove and return all pending reasons, emptying the queue.
    pub fn take(&mut self) -> Vec<Trigger> {
        let reasons = self.reasons.iter().copied().collect();
        self.reasons.clear();
        reasons
    }

    /// Remove a single reason, leaving the others untouched.
    pub fn discard(&mut self, trigger: Trigger) {
        self.reasons.remove(&trigger);
    }

    /// Drop every pending reason.
    pub fn clear(&mut self) {
        self.reasons.clear();
    }

    /// Whether the queue holds at least one pending reason.
    pub fn is_empty(&self) -> bool {
        self.reasons.is_empty()
    }

    /// Number of pending reasons.
    pub fn len(&self) -> usize {
        self.reasons.len()
    }

    /// Whether a given reason is currently pending.
    pub fn contains(&self, trigger: Trigger) -> bool {
        self.reasons.contains(&trigger)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_coalesces_duplicate_reasons() {
        let mut queue = CoalescingQueue::new();
        queue.add(Trigger::LocalInotify);
        queue.add(Trigger::LocalInotify);
        queue.add(Trigger::Manual);
        assert_eq!(queue.len(), 2);
        let taken = queue.take();
        assert!(taken.contains(&Trigger::LocalInotify));
        assert!(taken.contains(&Trigger::Manual));
        assert!(queue.is_empty());
    }

    #[test]
    fn discard_removes_only_the_selected_reason() {
        let mut queue = CoalescingQueue::new();
        queue.add(Trigger::LocalInotify);
        queue.add(Trigger::RemotePush);
        queue.discard(Trigger::LocalInotify);
        assert_eq!(queue.take(), vec![Trigger::RemotePush]);
    }

    #[test]
    fn extend_and_clear() {
        let mut queue = CoalescingQueue::new();
        queue.extend([Trigger::Retry, Trigger::Startup]);
        assert_eq!(queue.len(), 2);
        queue.clear();
        assert!(queue.is_empty());
    }

    #[test]
    fn manual_only_requires_all_four_triggers_off() {
        let base = TriggerSettings {
            local_inotify_enabled: false,
            local_interval_enabled: false,
            remote_push_enabled: false,
            remote_interval_enabled: false,
            ..TriggerSettings::default()
        };
        assert!(manual_only(&base));
        for enabled in [
            (
                "local_inotify_enabled",
                TriggerSettings {
                    local_inotify_enabled: true,
                    ..base
                },
            ),
            (
                "local_interval_enabled",
                TriggerSettings {
                    local_interval_enabled: true,
                    ..base
                },
            ),
            (
                "remote_push_enabled",
                TriggerSettings {
                    remote_push_enabled: true,
                    ..base
                },
            ),
            (
                "remote_interval_enabled",
                TriggerSettings {
                    remote_interval_enabled: true,
                    ..base
                },
            ),
        ] {
            assert!(!manual_only(&enabled.1), "{}", enabled.0);
        }
    }

    #[test]
    fn trigger_names_match_python_values() {
        assert_eq!(Trigger::LocalInotify.as_str(), "LOCAL_INOTIFY");
        assert_eq!(Trigger::LocalInterval.as_str(), "LOCAL_INTERVAL");
        assert_eq!(Trigger::RemotePush.as_str(), "REMOTE_PUSH");
        assert_eq!(Trigger::RemoteInterval.as_str(), "REMOTE_INTERVAL");
        assert_eq!(Trigger::Manual.as_str(), "MANUAL");
        assert_eq!(Trigger::NetworkRestored.as_str(), "NETWORK_RESTORED");
    }
}
