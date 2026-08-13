//! Application state and state machine.
//!
//! Fase 2 (Task 2.1): the full state machine lives here. `StateController`
//! holds one snapshot and notifies subscribers (callback-based, synchronous,
//! matching `core/state.py`); `AggregateStateController` exposes the *worst*
//! state across several controllers so a multi-folder app shows the most
//! important problem. Reference implementation: `src/nextsync/core/state.py`.
//!
//! The app is single-threaded (GLib main loop), so `Rc<RefCell<...>>` is the
//! sharing mechanism. Snapshot, progress and listener lists live in separate
//! cells so a subscriber can safely call `set` again from inside a
//! notification (no borrow conflict, matching the Python recursion). Only
//! adding/removing subscribers from inside a notification is rejected by the
//! borrow checker at runtime.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;

use crate::nextcloud::sync_engine::SyncProgress;

/// Application-level state of one account (or folder) runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppState {
    /// No account configured yet.
    Unconfigured,
    /// Everything is fine and nothing is running.
    IdleOk,
    /// Idle, but only manual synchronizations are allowed.
    IdleManualOnly,
    /// A synchronization is queued and will start shortly.
    SyncQueued,
    /// A synchronization is running.
    Syncing,
    /// The user paused synchronization.
    PausedUser,
    /// Synchronization is paused because of battery.
    PausedBattery,
    /// No network connection.
    Offline,
    /// The last synchronization failed.
    Error,
    /// The account credentials were rejected.
    AuthRequired,
    /// The password keyring is locked.
    KeyringLocked,
    /// A mass deletion is awaiting review.
    DeleteReview,
}

impl AppState {
    /// Stable machine-readable name, matching `AppState.value` in Python.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unconfigured => "unconfigured",
            Self::IdleOk => "idle_ok",
            Self::IdleManualOnly => "idle_manual_only",
            Self::SyncQueued => "sync_queued",
            Self::Syncing => "syncing",
            Self::PausedUser => "paused_user",
            Self::PausedBattery => "paused_battery",
            Self::Offline => "offline",
            Self::Error => "error",
            Self::AuthRequired => "auth_required",
            Self::KeyringLocked => "keyring_locked",
            Self::DeleteReview => "delete_review",
        }
    }
}

impl fmt::Display for AppState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// State of the remote push channel, mirroring `PushState` in `state.py`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PushState {
    Disabled,
    Unsupported,
    Connecting,
    Connected,
    Reconnecting,
    AuthRequired,
}

impl PushState {
    /// Stable machine-readable name, matching `PushState.value` in Python.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Unsupported => "unsupported",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
            Self::AuthRequired => "authentication_required",
        }
    }
}

/// Immutable snapshot of the state plus a human-readable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSnapshot {
    pub state: AppState,
    pub message: String,
}

impl StateSnapshot {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            message: String::new(),
        }
    }
}

/// Severity ranking used to pick the worst state when aggregating.
/// Mirrors `_STATE_SEVERITY` in `state.py` (higher = worse).
pub fn severity(state: AppState) -> i32 {
    match state {
        AppState::DeleteReview => 100,
        AppState::Error => 90,
        AppState::AuthRequired => 80,
        AppState::KeyringLocked => 70,
        AppState::Offline => 60,
        AppState::Syncing => 50,
        AppState::SyncQueued => 40,
        AppState::PausedBattery => 30,
        AppState::PausedUser => 20,
        AppState::IdleManualOnly => 10,
        AppState::IdleOk => 0,
        AppState::Unconfigured => -10,
    }
}

/// Opaque handle that cancels a subscription when `unsubscribe` is called.
///
/// Dropping the handle without calling `unsubscribe` keeps the subscription
/// active, exactly like the Python unsubscribe callable.
pub struct Subscription {
    unsubscribe_fn: Option<Box<dyn FnOnce()>>,
}

impl Subscription {
    /// Stop receiving notifications. Idempotent.
    pub fn unsubscribe(&mut self) {
        if let Some(unsubscribe) = self.unsubscribe_fn.take() {
            unsubscribe();
        }
    }
}

/// A subscriber to state changes.
type StateListener = Box<dyn Fn(&StateSnapshot) + 'static>;
/// A subscriber to progress changes.
type ProgressListener = Box<dyn Fn(Option<&SyncProgress>) + 'static>;

/// Single runtime state holder with synchronous change notifications.
#[derive(Clone)]
pub struct StateController {
    snapshot: Rc<RefCell<StateSnapshot>>,
    listeners: Rc<RefCell<Vec<(usize, StateListener)>>>,
    progress: Rc<RefCell<Option<SyncProgress>>>,
    progress_listeners: Rc<RefCell<Vec<(usize, ProgressListener)>>>,
    next_id: Rc<Cell<usize>>,
}

impl StateController {
    /// Create a controller in the given initial state.
    pub fn new(initial: AppState) -> Self {
        Self {
            snapshot: Rc::new(RefCell::new(StateSnapshot::new(initial))),
            listeners: Rc::new(RefCell::new(Vec::new())),
            progress: Rc::new(RefCell::new(None)),
            progress_listeners: Rc::new(RefCell::new(Vec::new())),
            next_id: Rc::new(Cell::new(1)),
        }
    }

    /// The current snapshot (a clone, so callers can keep it).
    pub fn snapshot(&self) -> StateSnapshot {
        self.snapshot.borrow().clone()
    }

    /// The current progress event, if any.
    pub fn progress(&self) -> Option<SyncProgress> {
        self.progress.borrow().clone()
    }

    /// Subscribe to state changes; the callback is invoked immediately with
    /// the current snapshot (like the Python `subscribe`), then on every
    /// change. Returns a handle that stops the subscription.
    pub fn subscribe(&self, callback: impl Fn(&StateSnapshot) + 'static) -> Subscription {
        let id = {
            let id = self.next_id.get();
            self.next_id.set(id + 1);
            self.listeners.borrow_mut().push((id, Box::new(callback)));
            id
        };
        self.notify_state(&[id]);
        let listeners = Rc::clone(&self.listeners);
        Subscription {
            unsubscribe_fn: Some(Box::new(move || {
                listeners
                    .borrow_mut()
                    .retain(|(existing, _)| *existing != id);
            })),
        }
    }

    /// Change the state (and optional message). Notifies subscribers only when
    /// the snapshot actually changed.
    pub fn set(&self, state: AppState, message: impl Into<String>) {
        let updated = StateSnapshot {
            state,
            message: message.into(),
        };
        if *self.snapshot.borrow() == updated {
            return;
        }
        *self.snapshot.borrow_mut() = updated;
        let ids: Vec<usize> = self.listeners.borrow().iter().map(|(id, _)| *id).collect();
        self.notify_state(&ids);
    }

    /// Set the current progress event; `None` clears it. Subscribers are
    /// notified only when the value actually changed.
    pub fn set_progress(&self, progress: Option<SyncProgress>) {
        if *self.progress.borrow() == progress {
            return;
        }
        *self.progress.borrow_mut() = progress;
        let ids: Vec<usize> = self
            .progress_listeners
            .borrow()
            .iter()
            .map(|(id, _)| *id)
            .collect();
        self.notify_progress(&ids);
    }

    /// Subscribe to progress changes; the callback is invoked immediately with
    /// the current value, then on every change.
    pub fn subscribe_progress(
        &self,
        callback: impl Fn(Option<&SyncProgress>) + 'static,
    ) -> Subscription {
        let id = {
            let id = self.next_id.get();
            self.next_id.set(id + 1);
            self.progress_listeners
                .borrow_mut()
                .push((id, Box::new(callback)));
            id
        };
        self.notify_progress(&[id]);
        let progress_listeners = Rc::clone(&self.progress_listeners);
        Subscription {
            unsubscribe_fn: Some(Box::new(move || {
                progress_listeners
                    .borrow_mut()
                    .retain(|(existing, _)| *existing != id);
            })),
        }
    }

    /// Whether `self` and `other` are the same underlying controller.
    pub fn same(&self, other: &StateController) -> bool {
        Rc::ptr_eq(&self.snapshot, &other.snapshot)
    }

    fn notify_state(&self, ids: &[usize]) {
        let snapshot = self.snapshot.borrow().clone();
        for id in ids {
            let listeners = self.listeners.borrow();
            if let Some((_, callback)) = listeners.iter().find(|(existing, _)| existing == id) {
                callback(&snapshot);
            }
        }
    }

    fn notify_progress(&self, ids: &[usize]) {
        let progress = self.progress.borrow().clone();
        for id in ids {
            let listeners = self.progress_listeners.borrow();
            if let Some((_, callback)) = listeners.iter().find(|(existing, _)| existing == id) {
                callback(progress.as_ref());
            }
        }
    }
}

/// Aggregate several [`StateController`]s and expose the worst state.
#[derive(Clone)]
pub struct AggregateStateController {
    inner: Rc<RefCell<AggregateInner>>,
}

struct AggregateInner {
    controllers: Vec<StateController>,
    subscriptions: Vec<Subscription>,
    listeners: Rc<RefCell<Vec<(usize, StateListener)>>>,
    progress_listeners: Rc<RefCell<Vec<(usize, ProgressListener)>>>,
    snapshot: Rc<RefCell<StateSnapshot>>,
    next_id: Rc<Cell<usize>>,
}

impl AggregateStateController {
    /// Create an empty aggregate, which reports `Unconfigured`.
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(AggregateInner {
                controllers: Vec::new(),
                subscriptions: Vec::new(),
                listeners: Rc::new(RefCell::new(Vec::new())),
                progress_listeners: Rc::new(RefCell::new(Vec::new())),
                snapshot: Rc::new(RefCell::new(StateSnapshot::new(AppState::Unconfigured))),
                next_id: Rc::new(Cell::new(1)),
            })),
        }
    }

    /// The aggregated snapshot.
    pub fn snapshot(&self) -> StateSnapshot {
        self.inner.borrow().snapshot.borrow().clone()
    }

    /// The progress of the first controller that currently has any.
    pub fn progress(&self) -> Option<SyncProgress> {
        let inner = self.inner.borrow();
        inner
            .controllers
            .iter()
            .find_map(|controller| controller.progress())
    }

    /// Add a controller to the aggregate and subscribe to its changes.
    pub fn add(&self, controller: StateController) {
        let weak = Rc::downgrade(&self.inner);
        let state_sub = controller.subscribe({
            let weak = weak.clone();
            move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.borrow_mut().recompute();
                }
            }
        });
        let progress_sub = controller.subscribe_progress({
            let weak = weak.clone();
            move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.borrow_mut().recompute_progress();
                }
            }
        });
        let mut inner = self.inner.borrow_mut();
        inner.controllers.push(controller);
        inner.subscriptions.push(state_sub);
        inner.subscriptions.push(progress_sub);
        inner.recompute();
        inner.recompute_progress();
    }

    /// Remove a controller and recompute the aggregate.
    pub fn remove(&self, controller: &StateController) {
        let mut inner = self.inner.borrow_mut();
        let Some(index) = inner
            .controllers
            .iter()
            .position(|current| current.same(controller))
        else {
            return;
        };
        inner.controllers.remove(index);
        if index * 2 < inner.subscriptions.len() {
            let mut sub = inner.subscriptions.remove(index * 2);
            sub.unsubscribe();
            if index * 2 < inner.subscriptions.len() {
                let mut sub = inner.subscriptions.remove(index * 2);
                sub.unsubscribe();
            }
        }
        inner.recompute();
        inner.recompute_progress();
    }

    /// Remove every controller and reset to `Unconfigured`.
    pub fn clear(&self) {
        let mut inner = self.inner.borrow_mut();
        for mut sub in inner.subscriptions.drain(..) {
            sub.unsubscribe();
        }
        inner.controllers.clear();
        inner.recompute();
        inner.recompute_progress();
    }

    /// Subscribe to aggregate state changes (immediate callback included).
    pub fn subscribe(&self, callback: impl Fn(&StateSnapshot) + 'static) -> Subscription {
        let id = {
            let inner = self.inner.borrow();
            let id = inner.next_id.get();
            inner.next_id.set(id + 1);
            inner.listeners.borrow_mut().push((id, Box::new(callback)));
            id
        };
        let (listeners, snapshot) = {
            let inner = self.inner.borrow();
            let snapshot = inner.snapshot.borrow().clone();
            (Rc::clone(&inner.listeners), snapshot)
        };
        {
            let listeners = listeners.borrow();
            if let Some((_, cb)) = listeners.iter().find(|(existing, _)| *existing == id) {
                cb(&snapshot);
            }
        }
        Subscription {
            unsubscribe_fn: Some(Box::new(move || {
                listeners
                    .borrow_mut()
                    .retain(|(existing, _)| *existing != id);
            })),
        }
    }

    /// Subscribe to aggregate progress changes (immediate callback included).
    pub fn subscribe_progress(
        &self,
        callback: impl Fn(Option<&SyncProgress>) + 'static,
    ) -> Subscription {
        let id = {
            let inner = self.inner.borrow();
            let id = inner.next_id.get();
            inner.next_id.set(id + 1);
            inner
                .progress_listeners
                .borrow_mut()
                .push((id, Box::new(callback)));
            id
        };
        let progress = self.progress();
        {
            let inner = self.inner.borrow();
            let listeners = inner.progress_listeners.borrow();
            if let Some((_, cb)) = listeners.iter().find(|(existing, _)| *existing == id) {
                cb(progress.as_ref());
            }
        }
        let progress_listeners = {
            let inner = self.inner.borrow();
            Rc::clone(&inner.progress_listeners)
        };
        Subscription {
            unsubscribe_fn: Some(Box::new(move || {
                progress_listeners
                    .borrow_mut()
                    .retain(|(existing, _)| *existing != id);
            })),
        }
    }
}

impl Default for AggregateStateController {
    fn default() -> Self {
        Self::new()
    }
}

impl AggregateInner {
    fn recompute(&mut self) {
        let updated = if self.controllers.is_empty() {
            StateSnapshot::new(AppState::Unconfigured)
        } else {
            let mut worst = self.controllers[0].snapshot();
            for controller in &self.controllers[1..] {
                let snapshot = controller.snapshot();
                if severity(snapshot.state) > severity(worst.state) {
                    worst = snapshot;
                }
            }
            worst
        };
        if *self.snapshot.borrow() == updated {
            return;
        }
        *self.snapshot.borrow_mut() = updated.clone();
        let ids: Vec<usize> = self.listeners.borrow().iter().map(|(id, _)| *id).collect();
        for id in ids {
            let listeners = self.listeners.borrow();
            if let Some((_, callback)) = listeners.iter().find(|(existing, _)| *existing == id) {
                callback(&updated);
            }
        }
    }

    fn recompute_progress(&mut self) {
        let progress = self
            .controllers
            .iter()
            .find_map(|controller| controller.progress());
        let ids: Vec<usize> = self
            .progress_listeners
            .borrow()
            .iter()
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let listeners = self.progress_listeners.borrow();
            if let Some((_, callback)) = listeners.iter().find(|(existing, _)| *existing == id) {
                callback(progress.as_ref());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- StateController transitions ---------------------------------------

    #[test]
    fn initial_snapshot_is_the_constructor_state() {
        let controller = StateController::new(AppState::IdleOk);
        assert_eq!(controller.snapshot().state, AppState::IdleOk);
    }

    #[test]
    fn set_updates_the_snapshot_and_notifies_subscribers() {
        let controller = StateController::new(AppState::IdleOk);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let collect = {
            let seen = Rc::clone(&seen);
            move |snapshot: &StateSnapshot| seen.borrow_mut().push(snapshot.state)
        };
        let _sub = controller.subscribe(collect);
        assert_eq!(*seen.borrow(), vec![AppState::IdleOk]);

        controller.set(AppState::Syncing, "sync");
        assert_eq!(*seen.borrow(), vec![AppState::IdleOk, AppState::Syncing]);
        assert_eq!(controller.snapshot().state, AppState::Syncing);
        assert_eq!(controller.snapshot().message, "sync");
    }

    #[test]
    fn identical_snapshot_is_not_resent() {
        let controller = StateController::new(AppState::IdleOk);
        let count = Rc::new(RefCell::new(0));
        let bump = {
            let count = Rc::clone(&count);
            move |_: &StateSnapshot| *count.borrow_mut() += 1
        };
        let _sub = controller.subscribe(bump);
        controller.set(AppState::IdleOk, "");
        controller.set(AppState::IdleOk, "");
        // 1 immediate + 0 changes
        assert_eq!(*count.borrow(), 1);
    }

    #[test]
    fn changing_only_the_message_notifies() {
        let controller = StateController::new(AppState::Error);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let collect = {
            let seen = Rc::clone(&seen);
            move |snapshot: &StateSnapshot| seen.borrow_mut().push(snapshot.message.clone())
        };
        let _sub = controller.subscribe(collect);
        controller.set(AppState::Error, "first");
        controller.set(AppState::Error, "first");
        controller.set(AppState::Error, "second");
        assert_eq!(
            *seen.borrow(),
            vec![String::new(), "first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn unsubscribe_stops_notifications() {
        let controller = StateController::new(AppState::IdleOk);
        let count = Rc::new(RefCell::new(0));
        let bump = {
            let count = Rc::clone(&count);
            move |_: &StateSnapshot| *count.borrow_mut() += 1
        };
        let mut sub = controller.subscribe(bump);
        controller.set(AppState::Offline, "off");
        sub.unsubscribe();
        controller.set(AppState::Offline, "still off");
        controller.set(AppState::Error, "err");
        assert_eq!(*count.borrow(), 2);
    }

    #[test]
    fn duplicate_progress_is_not_resent() {
        let controller = StateController::new(AppState::IdleOk);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let collect = {
            let seen = Rc::clone(&seen);
            move |progress: Option<&SyncProgress>| {
                seen.borrow_mut().push(progress.map(|p| p.action.clone()))
            }
        };
        let _sub = controller.subscribe_progress(collect);
        let progress = SyncProgress::new("download", "/tmp/a.pdf");
        controller.set_progress(Some(progress.clone()));
        controller.set_progress(Some(progress));
        assert_eq!(*seen.borrow(), vec![None, Some("download".to_string())]);
    }

    #[test]
    fn progress_unsubscribe_keeps_count_stable() {
        let controller = StateController::new(AppState::IdleOk);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let collect = {
            let seen = Rc::clone(&seen);
            move |p: Option<&SyncProgress>| seen.borrow_mut().push(p.map(|p| p.action.clone()))
        };
        let mut sub = controller.subscribe_progress(collect);
        controller.set_progress(Some(SyncProgress::new("upload", "/tmp/b")));
        controller.set_progress(None);
        sub.unsubscribe();
        controller.set_progress(Some(SyncProgress::new("delete", "/tmp/c")));
        // immediate + upload + None
        assert_eq!(seen.borrow().len(), 3);
    }

    #[test]
    fn app_state_matches_python_value_names() {
        assert_eq!(AppState::DeleteReview.as_str(), "delete_review");
        assert_eq!(AppState::IdleManualOnly.as_str(), "idle_manual_only");
        assert_eq!(AppState::Unconfigured.as_str(), "unconfigured");
        assert_eq!(PushState::AuthRequired.as_str(), "authentication_required");
    }

    #[test]
    fn severity_ranks_according_to_python_table() {
        assert!(severity(AppState::DeleteReview) > severity(AppState::Error));
        assert!(severity(AppState::Error) > severity(AppState::AuthRequired));
        assert!(severity(AppState::Syncing) > severity(AppState::SyncQueued));
        assert!(severity(AppState::IdleOk) > severity(AppState::Unconfigured));
    }

    // ---- AggregateStateController -------------------------------------------

    #[test]
    fn empty_aggregate_is_unconfigured() {
        let aggregate = AggregateStateController::new();
        assert_eq!(aggregate.snapshot().state, AppState::Unconfigured);
    }

    #[test]
    fn worst_state_wins_across_controllers() {
        let first = StateController::new(AppState::IdleOk);
        let second = StateController::new(AppState::Offline);
        let third = StateController::new(AppState::Syncing);
        let aggregate = AggregateStateController::new();
        aggregate.add(first);
        aggregate.add(second.clone());
        assert_eq!(aggregate.snapshot().state, AppState::Offline);
        aggregate.add(third);
        assert_eq!(aggregate.snapshot().state, AppState::Offline);
        second.set(AppState::Error, "boom");
        assert_eq!(aggregate.snapshot().state, AppState::Error);
        assert_eq!(aggregate.snapshot().message, "boom");
    }

    #[test]
    fn removing_a_controller_recomputes() {
        let first = StateController::new(AppState::IdleOk);
        let second = StateController::new(AppState::Error);
        let aggregate = AggregateStateController::new();
        aggregate.add(first);
        aggregate.add(second.clone());
        assert_eq!(aggregate.snapshot().state, AppState::Error);
        aggregate.remove(&second);
        assert_eq!(aggregate.snapshot().state, AppState::IdleOk);
    }

    #[test]
    fn clear_resets_to_unconfigured() {
        let first = StateController::new(AppState::Error);
        let aggregate = AggregateStateController::new();
        aggregate.add(first);
        aggregate.clear();
        assert_eq!(aggregate.snapshot().state, AppState::Unconfigured);
    }

    #[test]
    fn subscribers_are_notified_on_changes() {
        let first = StateController::new(AppState::IdleOk);
        let aggregate = AggregateStateController::new();
        aggregate.add(first.clone());
        let seen = Rc::new(RefCell::new(Vec::new()));
        let collect = {
            let seen = Rc::clone(&seen);
            move |snapshot: &StateSnapshot| seen.borrow_mut().push(snapshot.state)
        };
        let _sub = aggregate.subscribe(collect);
        first.set(AppState::Syncing, "");
        assert_eq!(aggregate.snapshot().state, AppState::Syncing);
        assert_eq!(*seen.borrow(), vec![AppState::IdleOk, AppState::Syncing]);
    }

    #[test]
    fn idle_ok_outranks_unconfigured() {
        let configured = StateController::new(AppState::IdleOk);
        let fresh = StateController::new(AppState::Unconfigured);
        let aggregate = AggregateStateController::new();
        aggregate.add(configured);
        aggregate.add(fresh);
        assert_eq!(aggregate.snapshot().state, AppState::IdleOk);
    }

    #[test]
    fn message_comes_from_the_worst_controller() {
        let first = StateController::new(AppState::IdleOk);
        let second = StateController::new(AppState::Syncing);
        second.set(AppState::Syncing, "Synchronizing files…");
        let aggregate = AggregateStateController::new();
        aggregate.add(first);
        aggregate.add(second);
        assert_eq!(aggregate.snapshot().state, AppState::Syncing);
        assert_eq!(aggregate.snapshot().message, "Synchronizing files…");
    }

    #[test]
    fn aggregate_progress_uses_first_non_empty_controller() {
        let first = StateController::new(AppState::IdleOk);
        let second = StateController::new(AppState::IdleOk);
        let aggregate = AggregateStateController::new();
        aggregate.add(first.clone());
        aggregate.add(second.clone());
        second.set_progress(Some(SyncProgress::new("upload", "/tmp/b")));
        assert_eq!(
            aggregate.progress().map(|p| p.path),
            Some("/tmp/b".to_string())
        );
        first.set_progress(Some(SyncProgress::new("download", "/tmp/a")));
        assert_eq!(
            aggregate.progress().map(|p| p.path),
            Some("/tmp/a".to_string())
        );
        first.set_progress(None);
        assert_eq!(
            aggregate.progress().map(|p| p.path),
            Some("/tmp/b".to_string())
        );
    }

    #[test]
    fn aggregate_progress_subscribers_are_notified() {
        let controller = StateController::new(AppState::IdleOk);
        let aggregate = AggregateStateController::new();
        aggregate.add(controller.clone());
        let seen = Rc::new(RefCell::new(Vec::new()));
        let collect = {
            let seen = Rc::clone(&seen);
            move |p: Option<&SyncProgress>| seen.borrow_mut().push(p.map(|p| p.action.clone()))
        };
        let _sub = aggregate.subscribe_progress(collect);
        controller.set_progress(Some(SyncProgress::new("download", "/tmp/a.pdf")));
        assert_eq!(*seen.borrow(), vec![None, Some("download".to_string())]);
    }

    #[test]
    fn aggregate_unsubscribe_stops_updates() {
        let controller = StateController::new(AppState::IdleOk);
        let aggregate = AggregateStateController::new();
        aggregate.add(controller.clone());
        let count = Rc::new(RefCell::new(0));
        let bump = {
            let count = Rc::clone(&count);
            move |_: &StateSnapshot| *count.borrow_mut() += 1
        };
        let mut sub = aggregate.subscribe(bump);
        controller.set(AppState::Offline, "off");
        sub.unsubscribe();
        controller.set(AppState::Error, "err");
        assert_eq!(*count.borrow(), 2);
    }

    #[test]
    fn removed_controller_no_longer_influences_the_aggregate() {
        let first = StateController::new(AppState::IdleOk);
        let second = StateController::new(AppState::Error);
        let aggregate = AggregateStateController::new();
        aggregate.add(first.clone());
        aggregate.add(second.clone());
        assert_eq!(aggregate.snapshot().state, AppState::Error);
        aggregate.remove(&second);
        first.set(AppState::IdleOk, "fine");
        assert_eq!(aggregate.snapshot().state, AppState::IdleOk);
    }
}
