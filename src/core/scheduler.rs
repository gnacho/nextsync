//! Sync scheduler.
//!
//! Fase 2 (Task 2.2): coalesces sync triggers, debounces local feedback, and
//! starts at most one reconciliation at a time (optionally gated by a shared
//! [`SyncPermit`]). Mirrors `core/scheduler.py`: the request gating (pause,
//! offline, keyring, deletion guard), the state transitions and the post-run
//! cooldown. The periodic interval triggers live in [`SyncTimers`] (the
//! Python `core/timers.py`).
//!
//! The actual `nextcloudcmd` run (credential lookup + process spawn) is
//! plugged in through the [`SyncRunner`] trait, which will be implemented by
//! the sync engine in Task 2.3. Timing goes through
//! `crate::core::debounce::TimeoutSource`, so tests run without a GLib loop.
//!
//! Re-entrancy note: methods borrow the inner `Rc<RefCell<_>>` while running,
//! so callbacks handed to [`SyncRunner`] or `on_completed` must not call back
//! into the same scheduler synchronously (they fire from the loop/other
//! threads). Permit waiters defer through an idle source for the same reason.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Duration;

use crate::core::debounce::{DebounceGate, TimeoutSource};
use crate::core::delete_guard::GuardCheck;
use crate::core::sync_permit::SyncPermit;
use crate::core::triggers::{manual_only, CoalescingQueue, Trigger, TriggerSettings};
use crate::state::{AppState, StateController};
use crate::storage::config::SyncConfig;

/// Quiet window before local feedback collapses into a start (ms).
pub const DEBOUNCE_MS: u64 = 2000;
/// Cooldown after a sync finishes before the next one may start (s).
pub const COOLDOWN_SECONDS: u64 = 4;

/// How a finished reconciliation turned out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Synchronization completed without conflicts.
    Success,
    /// Synchronization completed but produced conflicted copies.
    Conflict,
    /// The server rejected the account credentials.
    AuthFailed,
    /// The password keyring was locked during the credential lookup.
    KeyringLocked,
    /// The synchronization failed (any other error).
    Failed,
}

/// Executes a reconciliation. Implemented by the sync engine in Task 2.3.
pub trait SyncRunner {
    /// Begin a reconciliation for the given reasons. `on_finished` must be
    /// invoked exactly once when the run ends, and never synchronously from
    /// inside `start`.
    fn start(&mut self, reasons: &[Trigger], on_finished: Box<dyn FnOnce(SyncOutcome) + 'static>);

    /// Cancel a running reconciliation (best effort).
    fn cancel(&mut self) {}
}

/// A deletion-guard alert that blocks synchronization until reviewed.
#[derive(Debug, Clone, Default)]
pub struct DeleteAlert {
    /// Stable reason: `folder_missing`, `folder_emptied` or `mass_local_deletion`.
    pub reason: String,
    /// Human-readable message surfaced in the review state.
    pub message: String,
    /// Relative paths that disappeared from the local tree.
    pub missing_paths: Vec<String>,
    /// Number of files in the last manifest.
    pub previous_count: usize,
    /// Number of files found now.
    pub current_count: usize,
    /// Whether a one-time approval is allowed (only explicit deletions;
    /// structural failures like a missing folder never are).
    pub can_approve_once: bool,
}

/// Coalescing scheduler for one account runtime.
///
/// Clone to get another handle on the same scheduler.
#[derive(Clone)]
pub struct Scheduler {
    inner: Rc<RefCell<SchedulerInner>>,
}

/// Callback invoked once per finished run with its outcome.
type CompletedCallback = Box<dyn Fn(&SyncOutcome) + 'static>;

struct SchedulerInner {
    state: StateController,
    permit: Option<SyncPermit>,
    queue: CoalescingQueue,
    source: Rc<RefCell<dyn TimeoutSource>>,
    debounce: Option<DebounceGate>,
    runner: Box<dyn SyncRunner>,
    guard: Option<Box<dyn GuardCheck>>,
    on_completed: Option<CompletedCallback>,
    settings: TriggerSettings,
    self_ref: Weak<RefCell<SchedulerInner>>,
    online: bool,
    user_paused: bool,
    battery_paused: bool,
    local_dirty: bool,
    remote_pending: bool,
    start_source: Option<u64>,
    preparing: bool,
    running: bool,
    stopped: bool,
    inotify_during_sync: bool,
    feedback_followup_pending: bool,
    keyring_locked: bool,
    delete_alert: Option<DeleteAlert>,
    delete_bypass_once: bool,
}

impl Scheduler {
    /// Create a scheduler. `sync_permit` is optional (the Python default);
    /// `on_completed` is called once per finished run.
    pub fn new(
        state: StateController,
        settings: TriggerSettings,
        runner: Box<dyn SyncRunner>,
        source: Rc<RefCell<dyn TimeoutSource>>,
        sync_permit: Option<SyncPermit>,
        on_completed: Option<CompletedCallback>,
    ) -> Self {
        let inner = SchedulerInner {
            state,
            permit: sync_permit,
            queue: CoalescingQueue::new(),
            source: source.clone(),
            debounce: None,
            runner,
            guard: None,
            on_completed,
            settings,
            self_ref: Weak::new(),
            online: true,
            user_paused: false,
            battery_paused: false,
            local_dirty: false,
            remote_pending: false,
            start_source: None,
            preparing: false,
            running: false,
            stopped: false,
            inotify_during_sync: false,
            feedback_followup_pending: false,
            keyring_locked: false,
            delete_alert: None,
            delete_bypass_once: false,
        };
        let inner = Rc::new(RefCell::new(inner));
        {
            let weak = Rc::downgrade(&inner);
            let mut borrow = inner.borrow_mut();
            borrow.self_ref = weak.clone();
            borrow.debounce = Some(DebounceGate::new(
                DEBOUNCE_MS,
                COOLDOWN_SECONDS,
                move || {
                    if let Some(inner) = weak.upgrade() {
                        inner.borrow_mut().schedule_start();
                    }
                },
                source,
            ));
        }
        Self { inner }
    }

    /// Request a synchronization for the given reason.
    pub fn request(&self, trigger: Trigger) {
        self.inner.borrow_mut().request(trigger);
    }

    /// Pause or resume synchronization by user request.
    pub fn set_user_paused(&self, paused: bool) {
        self.inner.borrow_mut().set_user_paused(paused);
    }

    /// Pause or resume synchronization because of battery status.
    pub fn set_battery_paused(&self, paused: bool) {
        self.inner.borrow_mut().set_battery_paused(paused);
    }

    /// Report network availability.
    pub fn set_online(&self, online: bool) {
        self.inner.borrow_mut().set_online(online);
    }

    /// Block the scheduler with a deletion-guard alert.
    pub fn set_delete_alert(&self, alert: DeleteAlert) {
        self.inner.borrow_mut().set_delete_alert(alert);
    }

    /// Install (or remove) the deletion guard checked before every run. The
    /// guard is a filesystem walk, so it runs inline on the main loop.
    pub fn set_guard(&self, guard: Option<Box<dyn GuardCheck>>) {
        self.inner.borrow_mut().guard = guard;
    }

    /// Approve one synchronization despite a deletion alert.
    pub fn approve_delete_once(&self) {
        self.inner.borrow_mut().approve_delete_once();
    }

    /// Clear the deletion alert and restore the folder from the server.
    pub fn restore_from_server(&self) {
        self.inner.borrow_mut().restore_from_server();
    }

    /// Stop the scheduler, cancelling pending work and timers.
    pub fn stop(&self) {
        self.inner.borrow_mut().stop();
    }

    /// Whether synchronization is paused (user or battery).
    pub fn paused(&self) -> bool {
        self.inner.borrow().paused()
    }

    /// Whether the user has manually paused synchronization.
    pub fn user_paused(&self) -> bool {
        self.inner.borrow().user_paused
    }

    /// Whether synchronization is paused because of battery status.
    pub fn battery_paused(&self) -> bool {
        self.inner.borrow().battery_paused
    }

    /// The current deletion-guard alert, if the scheduler is blocked on one.
    pub fn delete_alert(&self) -> Option<DeleteAlert> {
        self.inner.borrow().delete_alert.clone()
    }

    /// Whether only manual synchronization is allowed.
    pub fn manual_only(&self) -> bool {
        self.inner.borrow().manual_only()
    }

    /// Whether the keyring is currently locked.
    pub fn keyring_locked(&self) -> bool {
        self.inner.borrow().keyring_locked
    }

    /// Whether a reconciliation is currently running.
    pub fn is_running(&self) -> bool {
        self.inner.borrow().running
    }

    /// A clone of the state controller driven by this scheduler.
    pub fn state(&self) -> StateController {
        self.inner.borrow().state.clone()
    }

    /// Number of pending trigger reasons.
    pub fn queue_len(&self) -> usize {
        self.inner.borrow().queue.len()
    }

    /// Periodic interval timers wired to this scheduler's `request`.
    pub fn timers(&self) -> SyncTimers {
        let weak = Rc::downgrade(&self.inner);
        let source = self.inner.borrow().source.clone();
        SyncTimers::new(
            move |trigger: Trigger| {
                if let Some(inner) = weak.upgrade() {
                    inner.borrow_mut().request(trigger);
                }
            },
            source,
        )
    }
}

impl SchedulerInner {
    fn paused(&self) -> bool {
        self.user_paused || self.battery_paused
    }

    fn manual_only(&self) -> bool {
        manual_only(&self.settings)
    }

    fn debounce(&self) -> &DebounceGate {
        self.debounce
            .as_ref()
            .expect("debounce is initialized by Scheduler::new")
    }

    fn request(&mut self, trigger: Trigger) {
        if self.stopped {
            return;
        }
        if self.delete_alert.is_some() && !self.delete_bypass_once {
            self.queue.add(trigger);
            let message = self
                .delete_alert
                .as_ref()
                .map(|alert| alert.message.clone())
                .unwrap_or_default();
            self.state.set(AppState::DeleteReview, message);
            return;
        }
        if self.running || self.preparing {
            if trigger == Trigger::LocalInotify {
                self.inotify_during_sync = true;
            }
            self.queue.add(trigger);
            return;
        }
        if self.paused() && trigger != Trigger::Manual {
            match trigger {
                Trigger::LocalInotify | Trigger::LocalInterval | Trigger::LocalRecovery => {
                    self.local_dirty = true;
                }
                _ => self.remote_pending = true,
            }
            return;
        }
        if !self.online {
            self.queue.add(trigger);
            self.state
                .set(AppState::Offline, "Waiting for a network connection");
            return;
        }
        if self.keyring_locked && trigger != Trigger::Manual {
            self.queue.add(trigger);
            self.state
                .set(AppState::KeyringLocked, "Password keyring is locked");
            return;
        }
        self.queue.add(trigger);
        if self.keyring_locked && trigger == Trigger::Manual {
            self.start();
        } else if trigger == Trigger::LocalInotify {
            self.schedule_debounce();
        } else {
            self.schedule_start();
        }
    }

    fn schedule_debounce(&mut self) {
        self.state
            .set(AppState::SyncQueued, "Waiting for local changes to settle");
        self.debounce().kick();
    }

    fn schedule_start(&mut self) {
        if self.stopped
            || self.debounce().in_cooldown()
            || self.start_source.is_some()
            || self.preparing
            || self.running
        {
            return;
        }
        self.state
            .set(AppState::SyncQueued, "Synchronization scheduled");
        let weak = self.self_ref.clone();
        let id = self.source.borrow_mut().add_idle(Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.borrow_mut().start();
            }
        }));
        self.start_source = Some(id);
    }

    fn start(&mut self) {
        self.start_source = None;
        if self.stopped || self.preparing || self.running || self.queue.is_empty() || !self.online {
            return;
        }
        let reasons = self.queue.take();
        if self.paused() && !reasons.contains(&Trigger::Manual) {
            self.local_dirty = true;
            return;
        }
        if let Some(alert) = &self.delete_alert {
            if !self.delete_bypass_once {
                self.queue.extend(reasons.iter().copied());
                self.state
                    .set(AppState::DeleteReview, alert.message.clone());
                return;
            }
        }
        self.delete_bypass_once = false;
        // Fase 3: the deletion guard runs before every reconciliation. When it
        // finds a mass deletion the run is blocked and the app must review it.
        if let Some(guard) = &mut self.guard {
            if let Some(alert) = guard.check() {
                self.queue.extend(reasons.iter().copied());
                self.set_delete_alert(alert);
                return;
            }
        }
        self.prepare_sync(reasons);
    }

    fn prepare_sync(&mut self, reasons: Vec<Trigger>) {
        self.state.set(AppState::Syncing, "Synchronizing files…");
        self.state.set_progress(None);
        self.preparing = true;
        self.begin_run(reasons);
    }

    fn begin_run(&mut self, reasons: Vec<Trigger>) {
        self.preparing = false;
        if let Some(permit) = &self.permit {
            if !permit.try_acquire() {
                self.queue.extend(reasons.iter().copied());
                self.state.set(
                    AppState::SyncQueued,
                    "Waiting for another account to finish…",
                );
                let source = self.source.clone();
                let weak = self.self_ref.clone();
                permit.wait_for_release(Box::new(move || {
                    // Defer through the loop so this never runs while the
                    // releasing scheduler holds its borrow. One idle hop runs
                    // `start` directly, like Python's `idle_add(self._start)`.
                    let _ = source.borrow_mut().add_idle(Box::new(move || {
                        if let Some(inner) = weak.upgrade() {
                            inner.borrow_mut().start();
                        }
                    }));
                }));
                return;
            }
        }
        let feedback_followup = self.feedback_followup_pending;
        self.feedback_followup_pending = false;
        self.running = true;
        let weak = self.self_ref.clone();
        self.runner.start(
            &reasons,
            Box::new(move |outcome| {
                if let Some(inner) = weak.upgrade() {
                    inner.borrow_mut().finished(outcome, feedback_followup);
                }
            }),
        );
    }

    fn finished(&mut self, outcome: SyncOutcome, feedback_followup: bool) {
        if self.stopped {
            return;
        }
        let ran = match outcome {
            SyncOutcome::Success => {
                self.keyring_locked = false;
                self.set_idle_state();
                true
            }
            SyncOutcome::Conflict => {
                self.keyring_locked = false;
                self.state.set(
                    AppState::IdleOk,
                    "Synchronized with conflicts — review the log",
                );
                true
            }
            SyncOutcome::AuthFailed => {
                self.keyring_locked = false;
                self.state.set(
                    AppState::AuthRequired,
                    "Your Nextcloud account needs attention",
                );
                false
            }
            SyncOutcome::KeyringLocked => {
                self.keyring_locked = true;
                self.state
                    .set(AppState::KeyringLocked, "Password keyring is locked");
                false
            }
            SyncOutcome::Failed => {
                self.keyring_locked = false;
                self.state
                    .set(AppState::Error, "Synchronization failed — view the log");
                true
            }
        };
        // Fase 3: after a successful run the guard baseline is refreshed so
        // it reflects what nextcloudcmd just reconciled (Python: only for
        // `result.successful`, which covers conflicts too).
        if matches!(outcome, SyncOutcome::Success | SyncOutcome::Conflict)
            && self.delete_alert.is_none()
        {
            if let Some(guard) = &mut self.guard {
                let _ = guard.record_current();
            }
        }
        if let Some(callback) = &self.on_completed {
            callback(&outcome);
        }
        self.running = false;
        if ran {
            let mut queued = !self.queue.is_empty();
            if self.inotify_during_sync {
                if feedback_followup {
                    // Suppress only the local feedback from the reconciliation
                    // itself; manual/remote triggers stay queued.
                    self.queue.discard(Trigger::LocalInotify);
                } else {
                    self.feedback_followup_pending = true;
                    self.queue.add(Trigger::LocalInotify);
                }
                queued = !self.queue.is_empty();
            }
            self.inotify_during_sync = false;
            self.state.set_progress(None);
            if let Some(permit) = &self.permit {
                permit.release();
            }
            let weak = self.self_ref.clone();
            self.debounce().begin_cooldown(move || {
                if let Some(inner) = weak.upgrade() {
                    inner.borrow_mut().cooldown_finished(queued);
                }
            });
        } else {
            self.state.set_progress(None);
            if let Some(permit) = &self.permit {
                permit.release();
            }
        }
    }

    fn cooldown_finished(&mut self, run_pending: bool) {
        if self.stopped {
            return;
        }
        if run_pending && !self.queue.is_empty() && self.online && !self.paused() {
            self.schedule_start();
        } else if !self.paused() {
            self.set_idle_state();
        }
    }

    fn set_user_paused(&mut self, paused: bool) {
        self.user_paused = paused;
        if paused {
            self.state
                .set(AppState::PausedUser, "Synchronization is paused");
        } else {
            let should_reconcile =
                self.local_dirty || self.remote_pending || !self.queue.is_empty();
            self.local_dirty = false;
            self.remote_pending = false;
            if should_reconcile && !self.manual_only() {
                self.request(Trigger::Resume);
            } else {
                self.set_idle_state();
            }
        }
    }

    fn set_battery_paused(&mut self, paused: bool) {
        let was_paused = self.battery_paused;
        self.battery_paused = paused;
        if paused {
            let message = if self.running {
                "Will pause after the current synchronization"
            } else {
                "Paused on battery"
            };
            self.state.set(AppState::PausedBattery, message);
        } else if was_paused && !self.user_paused {
            let should_reconcile =
                self.local_dirty || self.remote_pending || !self.queue.is_empty();
            self.local_dirty = false;
            self.remote_pending = false;
            if should_reconcile && !self.manual_only() {
                self.request(Trigger::Resume);
            } else {
                self.set_idle_state();
            }
        }
    }

    fn set_online(&mut self, online: bool) {
        let was_online = self.online;
        self.online = online;
        if !online {
            self.state
                .set(AppState::Offline, "Waiting for a network connection");
        } else if !was_online {
            if !self.queue.is_empty() || !self.manual_only() {
                self.request(Trigger::NetworkRestored);
            } else {
                self.set_idle_state();
            }
        }
    }

    fn set_idle_state(&mut self) {
        if let Some(alert) = &self.delete_alert {
            self.state
                .set(AppState::DeleteReview, alert.message.clone());
        } else if self.user_paused {
            self.state
                .set(AppState::PausedUser, "Synchronization is paused");
        } else if self.battery_paused {
            self.state.set(AppState::PausedBattery, "Paused on battery");
        } else if !self.online {
            self.state
                .set(AppState::Offline, "Waiting for a network connection");
        } else if self.manual_only() {
            self.state
                .set(AppState::IdleManualOnly, "Automatic synchronization is off");
        } else {
            self.state.set(AppState::IdleOk, "Synchronized");
        }
    }

    fn set_delete_alert(&mut self, alert: DeleteAlert) {
        self.state
            .set(AppState::DeleteReview, alert.message.clone());
        self.delete_alert = Some(alert);
    }

    fn approve_delete_once(&mut self) {
        let Some(alert) = &self.delete_alert else {
            return;
        };
        if !alert.can_approve_once {
            return;
        }
        self.delete_alert = None;
        self.delete_bypass_once = true;
        self.request(Trigger::Manual);
    }

    fn restore_from_server(&mut self) {
        if self.delete_alert.is_none() {
            return;
        }
        // Fase 3: remove the local sync journals and record a fresh deletion
        // guard baseline so the guard stops blocking. With the journal gone
        // nextcloudcmd treats the server as authoritative and re-downloads
        // the folder (delete_guard.rs).
        if let Some(guard) = &mut self.guard {
            let _ = guard.restore_from_server();
        }
        self.delete_alert = None;
        self.delete_bypass_once = false;
        self.request(Trigger::Manual);
    }

    fn stop(&mut self) {
        self.stopped = true;
        self.debounce().stop();
        if let Some(id) = self.start_source.take() {
            self.source.borrow_mut().cancel(id);
        }
        self.queue.clear();
        self.local_dirty = false;
        self.remote_pending = false;
        self.feedback_followup_pending = false;
        self.delete_alert = None;
        if self.running {
            self.runner.cancel();
        }
        self.running = false;
    }
}

/// Periodic interval timers that emit `LocalInterval`/`RemoteInterval`.
///
/// Mirrors `core/timers.py`: arming is driven by the same settings switches
/// and the period is `max(1, minutes) * 60` seconds.
pub struct SyncTimers {
    request: Rc<dyn Fn(Trigger)>,
    source: Rc<RefCell<dyn TimeoutSource>>,
    local_source: Option<u64>,
    remote_source: Option<u64>,
}

impl SyncTimers {
    pub fn new(
        request: impl Fn(Trigger) + 'static,
        source: Rc<RefCell<dyn TimeoutSource>>,
    ) -> Self {
        Self {
            request: Rc::new(request),
            source,
            local_source: None,
            remote_source: None,
        }
    }

    /// (Re)arm the interval timers from the trigger settings.
    pub fn configure(&mut self, settings: &TriggerSettings) {
        self.stop();
        if settings.local_interval_enabled {
            let seconds = settings.local_interval_minutes.max(1) * 60;
            let request = Rc::clone(&self.request);
            let id = self.source.borrow_mut().add_repeating(
                Duration::from_secs(seconds as u64),
                Box::new(move || request(Trigger::LocalInterval)),
            );
            self.local_source = Some(id);
        }
        if settings.remote_interval_enabled {
            let seconds = settings.remote_interval_minutes.max(1) * 60;
            let request = Rc::clone(&self.request);
            let id = self.source.borrow_mut().add_repeating(
                Duration::from_secs(seconds as u64),
                Box::new(move || request(Trigger::RemoteInterval)),
            );
            self.remote_source = Some(id);
        }
    }

    /// Cancel both interval timers.
    pub fn stop(&mut self) {
        if let Some(id) = self.local_source.take() {
            self.source.borrow_mut().cancel(id);
        }
        if let Some(id) = self.remote_source.take() {
            self.source.borrow_mut().cancel(id);
        }
    }

    /// Timer id of the armed local interval, if any.
    pub fn local_interval(&self) -> Option<u64> {
        self.local_source
    }

    /// Timer id of the armed remote interval, if any.
    pub fn remote_interval(&self) -> Option<u64> {
        self.remote_source
    }
}

impl From<&SyncConfig> for TriggerSettings {
    fn from(sync: &SyncConfig) -> Self {
        TriggerSettings {
            local_inotify_enabled: sync.local_inotify_enabled,
            local_interval_enabled: sync.local_interval_enabled,
            local_interval_minutes: sync.local_interval_minutes,
            remote_push_enabled: sync.remote_push_enabled,
            remote_interval_enabled: sync.remote_interval_enabled,
            remote_interval_minutes: sync.remote_interval_minutes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::debounce::{fire_timer, FakeTimeoutSource};

    #[derive(Clone, Default)]
    struct FakeRunner(Rc<RefCell<FakeRunnerState>>);

    #[derive(Default)]
    struct FakeRunnerState {
        start_calls: usize,
        reasons: Vec<Vec<Trigger>>,
        pending: Option<Box<dyn FnOnce(SyncOutcome)>>,
        cancelled: bool,
    }

    impl SyncRunner for FakeRunner {
        fn start(&mut self, reasons: &[Trigger], on_finished: Box<dyn FnOnce(SyncOutcome)>) {
            let mut state = self.0.borrow_mut();
            state.start_calls += 1;
            state.reasons.push(reasons.to_vec());
            state.pending = Some(on_finished);
        }

        fn cancel(&mut self) {
            self.0.borrow_mut().cancelled = true;
        }
    }

    fn auto_settings() -> TriggerSettings {
        TriggerSettings {
            local_inotify_enabled: true,
            remote_push_enabled: true,
            ..TriggerSettings::default()
        }
    }

    fn fake_source() -> Rc<RefCell<FakeTimeoutSource>> {
        Rc::new(RefCell::new(FakeTimeoutSource::default()))
    }

    fn make_scheduler_with(
        permit: Option<SyncPermit>,
        settings: TriggerSettings,
    ) -> (Scheduler, Rc<RefCell<FakeTimeoutSource>>, FakeRunner) {
        let source = fake_source();
        let source_dyn: Rc<RefCell<dyn TimeoutSource>> = source.clone();
        let runner = FakeRunner::default();
        let scheduler = Scheduler::new(
            StateController::new(AppState::IdleOk),
            settings,
            Box::new(runner.clone()),
            source_dyn,
            permit,
            None,
        );
        (scheduler, source, runner)
    }

    fn make_scheduler(
        permit: Option<SyncPermit>,
    ) -> (Scheduler, Rc<RefCell<FakeTimeoutSource>>, FakeRunner) {
        make_scheduler_with(permit, auto_settings())
    }

    fn run_idle(source: &Rc<RefCell<FakeTimeoutSource>>) {
        let id = source.borrow().only_id();
        fire_timer(source, id);
    }

    fn finish(runner: &FakeRunner, outcome: SyncOutcome) {
        let callback = runner.0.borrow_mut().pending.take().unwrap();
        callback(outcome);
    }

    #[test]
    fn preparation_coalesces_new_triggers() {
        let (scheduler, source, runner) = make_scheduler(None);
        scheduler.request(Trigger::Manual);
        assert_eq!(source.borrow().pending(), 1);
        scheduler.request(Trigger::RemoteInterval);
        assert_eq!(source.borrow().pending(), 1);
        run_idle(&source);
        assert_eq!(runner.0.borrow().start_calls, 1);
        // LOCAL_INOTIFY during the run is coalesced, no new start.
        scheduler.request(Trigger::LocalInotify);
        assert_eq!(runner.0.borrow().start_calls, 1);
        assert_eq!(scheduler.queue_len(), 1);
        // The run finishes; the feedback follow-up keeps the queue non-empty.
        finish(&runner, SyncOutcome::Success);
        assert_eq!(scheduler.state().snapshot().state, AppState::IdleOk);
        assert_eq!(scheduler.queue_len(), 1);
    }

    #[test]
    fn stop_removes_pending_sources_and_cancels_process() {
        let (scheduler, source, runner) = make_scheduler(None);
        scheduler.request(Trigger::Manual);
        assert_eq!(source.borrow().pending(), 1);
        run_idle(&source);
        assert_eq!(runner.0.borrow().start_calls, 1);
        assert!(scheduler.is_running());
        scheduler.stop();
        assert!(runner.0.borrow().cancelled);
        assert_eq!(source.borrow().pending(), 0);
        assert_eq!(scheduler.queue_len(), 0);
        assert!(!scheduler.is_running());
    }

    #[test]
    fn locked_keyring_defers_automatic_triggers_until_manual_unlock() {
        let (scheduler, source, runner) = make_scheduler(None);
        scheduler.request(Trigger::Startup);
        run_idle(&source);
        assert_eq!(runner.0.borrow().start_calls, 1);
        finish(&runner, SyncOutcome::KeyringLocked);
        assert!(scheduler.keyring_locked());
        assert_eq!(scheduler.state().snapshot().state, AppState::KeyringLocked);

        scheduler.request(Trigger::LocalInotify);
        scheduler.request(Trigger::RemoteInterval);
        assert_eq!(runner.0.borrow().start_calls, 1);
        assert_eq!(scheduler.queue_len(), 2);

        scheduler.request(Trigger::Manual);
        assert_eq!(runner.0.borrow().start_calls, 2);
        finish(&runner, SyncOutcome::Success);
        assert!(!scheduler.keyring_locked());
        assert_eq!(runner.0.borrow().start_calls, 2);
        assert_eq!(scheduler.state().snapshot().state, AppState::IdleOk);
    }

    #[test]
    fn shared_permit_queues_a_second_account_until_release() {
        let permit = SyncPermit::try_new(1).unwrap();
        let (first, source_first, runner_first) = make_scheduler(Some(permit.clone()));
        let (second, source_second, runner_second) = make_scheduler(Some(permit));

        first.request(Trigger::Manual);
        run_idle(&source_first);
        assert_eq!(runner_first.0.borrow().start_calls, 1);

        second.request(Trigger::Manual);
        run_idle(&source_second);
        assert_eq!(runner_second.0.borrow().start_calls, 0);
        assert_eq!(second.queue_len(), 1);

        finish(&runner_first, SyncOutcome::Success);
        // The waiter defers through an idle on the second scheduler.
        assert_eq!(source_second.borrow().pending(), 1);
        run_idle(&source_second);
        assert_eq!(runner_second.0.borrow().start_calls, 1);
    }

    #[test]
    fn local_inotify_is_debounced() {
        let (scheduler, source, runner) = make_scheduler(None);
        scheduler.request(Trigger::LocalInotify);
        assert_eq!(source.borrow().pending(), 1);
        assert_eq!(runner.0.borrow().start_calls, 0);
        assert_eq!(scheduler.state().snapshot().state, AppState::SyncQueued);
        // A second burst event restarts the window; still one timer.
        scheduler.request(Trigger::LocalInotify);
        assert_eq!(source.borrow().pending(), 1);
        run_idle(&source); // debounce elapsed → ready → idle armed
        assert_eq!(source.borrow().pending(), 1);
        run_idle(&source); // idle → start
        assert_eq!(runner.0.borrow().start_calls, 1);
    }

    #[test]
    fn offline_defers_requests_and_blocks_start() {
        let (scheduler, source, runner) = make_scheduler(None);
        scheduler.set_online(false);
        assert_eq!(scheduler.state().snapshot().state, AppState::Offline);
        scheduler.request(Trigger::RemotePush);
        assert_eq!(scheduler.queue_len(), 1);
        assert_eq!(runner.0.borrow().start_calls, 0);
        assert_eq!(source.borrow().pending(), 0);
        scheduler.set_online(true);
        assert_eq!(scheduler.queue_len(), 2);
        assert!(source.borrow().pending() >= 1);
    }

    #[test]
    fn paused_defers_local_triggers_and_resume_reconciles() {
        let (scheduler, source, runner) = make_scheduler(None);
        scheduler.set_user_paused(true);
        assert_eq!(scheduler.state().snapshot().state, AppState::PausedUser);
        scheduler.request(Trigger::LocalInotify);
        scheduler.request(Trigger::RemotePush);
        assert_eq!(scheduler.queue_len(), 0);
        assert_eq!(runner.0.borrow().start_calls, 0);
        scheduler.set_user_paused(false);
        assert_eq!(source.borrow().pending(), 1);
        run_idle(&source);
        assert_eq!(runner.0.borrow().start_calls, 1);
    }

    #[test]
    fn battery_pause_defers_until_resumed() {
        let (scheduler, source, runner) = make_scheduler(None);
        scheduler.set_battery_paused(true);
        assert_eq!(scheduler.state().snapshot().state, AppState::PausedBattery);
        scheduler.request(Trigger::RemotePush);
        assert_eq!(scheduler.queue_len(), 0);
        scheduler.set_battery_paused(false);
        run_idle(&source);
        assert_eq!(runner.0.borrow().start_calls, 1);
    }

    #[test]
    fn manual_only_settings_show_idle_manual_only() {
        let (scheduler, _, runner) = make_scheduler_with(None, TriggerSettings::default());
        assert!(scheduler.manual_only());
        assert_eq!(scheduler.state().snapshot().state, AppState::IdleOk);
        assert!(runner.0.borrow().start_calls == 0);
        // No automatic trigger fires, so the state stays idle-manual after the
        // first idle evaluation.
        scheduler.request(Trigger::Retry);
        assert!(scheduler.queue_len() >= 1);
    }

    #[test]
    fn cooldown_gates_the_next_run_and_sets_idle() {
        let (scheduler, source, runner) = make_scheduler(None);
        scheduler.request(Trigger::Manual);
        run_idle(&source);
        finish(&runner, SyncOutcome::Success);
        // A cooldown timer is now pending; a new request cannot start yet.
        assert!(source.borrow().pending() >= 1);
        assert_eq!(scheduler.state().snapshot().state, AppState::IdleOk);
        run_idle(&source); // cooldown elapses with nothing queued → idle
        assert_eq!(scheduler.state().snapshot().state, AppState::IdleOk);
    }

    #[test]
    fn delete_alert_blocks_and_approve_once_bypasses() {
        let (scheduler, source, runner) = make_scheduler(None);
        scheduler.set_delete_alert(DeleteAlert {
            reason: "mass_local_deletion".to_string(),
            message: "Many files were removed".to_string(),
            can_approve_once: true,
            ..DeleteAlert::default()
        });
        assert_eq!(scheduler.state().snapshot().state, AppState::DeleteReview);
        scheduler.request(Trigger::RemotePush);
        assert_eq!(runner.0.borrow().start_calls, 0);
        scheduler.approve_delete_once();
        assert!(source.borrow().pending() >= 1);
        run_idle(&source);
        assert_eq!(runner.0.borrow().start_calls, 1);
    }

    #[test]
    fn pause_and_delete_alert_getters_reflect_the_inner_state() {
        let (scheduler, _, _) = make_scheduler(None);
        assert!(!scheduler.user_paused());
        assert!(!scheduler.battery_paused());
        assert!(scheduler.delete_alert().is_none());

        scheduler.set_user_paused(true);
        assert!(scheduler.user_paused());
        scheduler.set_battery_paused(true);
        assert!(scheduler.battery_paused());

        scheduler.set_user_paused(false);
        scheduler.set_battery_paused(false);

        scheduler.set_delete_alert(DeleteAlert {
            reason: "folder_emptied".to_string(),
            message: "The local folder was emptied".to_string(),
            can_approve_once: false,
            ..DeleteAlert::default()
        });
        let alert = scheduler.delete_alert().expect("alert is present");
        assert_eq!(alert.reason, "folder_emptied");
    }

    #[test]
    fn failed_sync_sets_error_state() {
        let (scheduler, source, runner) = make_scheduler(None);
        scheduler.request(Trigger::Manual);
        run_idle(&source);
        finish(&runner, SyncOutcome::Failed);
        assert_eq!(scheduler.state().snapshot().state, AppState::Error);
    }

    #[test]
    fn auth_failure_sets_auth_required() {
        let (scheduler, source, runner) = make_scheduler(None);
        scheduler.request(Trigger::Manual);
        run_idle(&source);
        finish(&runner, SyncOutcome::AuthFailed);
        assert_eq!(scheduler.state().snapshot().state, AppState::AuthRequired);
    }

    #[test]
    fn on_completed_reports_the_outcome() {
        let source = fake_source();
        let source_dyn: Rc<RefCell<dyn TimeoutSource>> = source.clone();
        let runner = FakeRunner::default();
        let completed = Rc::new(RefCell::new(Vec::new()));
        let scheduler = Scheduler::new(
            StateController::new(AppState::IdleOk),
            auto_settings(),
            Box::new(runner.clone()),
            source_dyn,
            None,
            Some(Box::new({
                let completed = Rc::clone(&completed);
                move |outcome: &SyncOutcome| completed.borrow_mut().push(*outcome)
            })),
        );
        scheduler.request(Trigger::Manual);
        run_idle(&source);
        finish(&runner, SyncOutcome::Conflict);
        assert_eq!(*completed.borrow(), vec![SyncOutcome::Conflict]);
    }

    #[test]
    fn settings_convert_from_sync_config() {
        let sync = SyncConfig {
            local_inotify_enabled: false,
            remote_interval_minutes: 42,
            ..SyncConfig::default()
        };
        let settings = TriggerSettings::from(&sync);
        assert!(!settings.local_inotify_enabled);
        assert_eq!(settings.remote_interval_minutes, 42);
        assert!(sync.remote_push_enabled);
    }

    #[test]
    fn timers_arm_local_and_remote_intervals() {
        let source = fake_source();
        let source_dyn: Rc<RefCell<dyn TimeoutSource>> = source.clone();
        let requested = Rc::new(RefCell::new(Vec::new()));
        let mut timers = SyncTimers::new(
            {
                let requested = Rc::clone(&requested);
                move |trigger: Trigger| requested.borrow_mut().push(trigger)
            },
            source_dyn,
        );
        let settings = TriggerSettings {
            local_interval_enabled: true,
            local_interval_minutes: 5,
            remote_interval_enabled: true,
            remote_interval_minutes: 10,
            ..TriggerSettings::default()
        };
        timers.configure(&settings);
        assert_eq!(source.borrow().pending(), 2);
        assert!(timers.local_interval().is_some());
        assert!(timers.remote_interval().is_some());

        // Firing an interval emits the trigger and keeps the timer armed.
        let local_id = timers.local_interval().unwrap();
        fire_timer(&source, local_id);
        assert_eq!(*requested.borrow(), vec![Trigger::LocalInterval]);
        assert_eq!(source.borrow().pending(), 2);
        let remote_id = timers.remote_interval().unwrap();
        fire_timer(&source, remote_id);
        assert_eq!(
            *requested.borrow(),
            vec![Trigger::LocalInterval, Trigger::RemoteInterval]
        );

        timers.configure(&TriggerSettings::default());
        assert_eq!(source.borrow().pending(), 0);
        assert!(timers.local_interval().is_none());
    }

    #[test]
    fn interval_period_never_underflows_to_zero() {
        let source = fake_source();
        let source_dyn: Rc<RefCell<dyn TimeoutSource>> = source.clone();
        let mut timers = SyncTimers::new(move |_| {}, source_dyn);
        let settings = TriggerSettings {
            local_interval_enabled: true,
            local_interval_minutes: 0,
            ..TriggerSettings::default()
        };
        timers.configure(&settings);
        assert!(timers.local_interval().is_some());
        timers.stop();
    }

    // ---- deletion guard integration -----------------------------------------

    #[derive(Clone, Default)]
    struct FakeGuard(Rc<RefCell<FakeGuardState>>);

    #[derive(Default)]
    struct FakeGuardState {
        alert: Option<DeleteAlert>,
        check_calls: usize,
        record_calls: usize,
        restore_calls: usize,
    }

    impl GuardCheck for FakeGuard {
        fn check(&mut self) -> Option<DeleteAlert> {
            self.0.borrow_mut().check_calls += 1;
            self.0.borrow().alert.clone()
        }

        fn record_current(&mut self) -> bool {
            self.0.borrow_mut().record_calls += 1;
            true
        }

        fn restore_from_server(&mut self) -> usize {
            self.0.borrow_mut().restore_calls += 1;
            2
        }
    }

    #[test]
    fn guard_blocks_a_mass_deletion_and_approve_once_bypasses() {
        let (scheduler, source, runner) = make_scheduler(None);
        let guard = FakeGuard::default();
        guard.0.borrow_mut().alert = Some(DeleteAlert {
            reason: "mass_local_deletion".to_string(),
            message: "An unusual number of local files disappeared".to_string(),
            missing_paths: vec!["a.txt".to_string(), "b.txt".to_string()],
            previous_count: 4,
            current_count: 2,
            can_approve_once: true,
        });
        scheduler.set_guard(Some(Box::new(guard.clone())));

        scheduler.request(Trigger::Manual);
        run_idle(&source);
        assert_eq!(runner.0.borrow().start_calls, 0);
        assert_eq!(scheduler.state().snapshot().state, AppState::DeleteReview);
        assert_eq!(scheduler.queue_len(), 1);
        assert_eq!(guard.0.borrow().check_calls, 1);

        // The deletion is over; approve once and the run goes through.
        guard.0.borrow_mut().alert = None;
        scheduler.approve_delete_once();
        assert!(source.borrow().pending() >= 1);
        run_idle(&source);
        assert_eq!(runner.0.borrow().start_calls, 1);
    }

    #[test]
    fn guard_never_runs_while_an_alert_is_already_active() {
        let (scheduler, source, runner) = make_scheduler(None);
        let guard = FakeGuard::default();
        scheduler.set_guard(Some(Box::new(guard.clone())));
        scheduler.set_delete_alert(DeleteAlert {
            reason: "folder_emptied".to_string(),
            message: "The folder is empty".to_string(),
            can_approve_once: true,
            ..DeleteAlert::default()
        });
        scheduler.request(Trigger::RemotePush);
        // The request is queued and blocked; nothing is scheduled to start.
        assert_eq!(source.borrow().pending(), 0);
        assert_eq!(runner.0.borrow().start_calls, 0);
        assert_eq!(guard.0.borrow().check_calls, 0);
        assert_eq!(scheduler.queue_len(), 1);
    }

    #[test]
    fn restore_from_server_delegates_to_the_guard_and_resumes() {
        let (scheduler, source, runner) = make_scheduler(None);
        let guard = FakeGuard::default();
        scheduler.set_guard(Some(Box::new(guard.clone())));
        scheduler.set_delete_alert(DeleteAlert {
            reason: "folder_missing".to_string(),
            message: "The local folder is missing".to_string(),
            can_approve_once: false,
            ..DeleteAlert::default()
        });

        scheduler.restore_from_server();
        assert_eq!(guard.0.borrow().restore_calls, 1);
        assert_eq!(scheduler.state().snapshot().state, AppState::SyncQueued);
        run_idle(&source);
        assert_eq!(runner.0.borrow().start_calls, 1);
    }

    #[test]
    fn successful_run_records_the_guard_baseline() {
        let (scheduler, source, runner) = make_scheduler(None);
        let guard = FakeGuard::default();
        scheduler.set_guard(Some(Box::new(guard.clone())));

        scheduler.request(Trigger::Manual);
        run_idle(&source);
        assert_eq!(guard.0.borrow().record_calls, 0);
        finish(&runner, SyncOutcome::Success);
        assert_eq!(guard.0.borrow().record_calls, 1);
    }

    #[test]
    fn failed_run_does_not_touch_the_baseline() {
        let (scheduler, source, runner) = make_scheduler(None);
        let guard = FakeGuard::default();
        scheduler.set_guard(Some(Box::new(guard.clone())));

        scheduler.request(Trigger::Manual);
        run_idle(&source);
        finish(&runner, SyncOutcome::Failed);
        assert_eq!(guard.0.borrow().record_calls, 0);
    }

    #[test]
    fn conflict_run_records_the_baseline_like_the_python_successful_result() {
        let (scheduler, source, runner) = make_scheduler(None);
        let guard = FakeGuard::default();
        scheduler.set_guard(Some(Box::new(guard.clone())));

        scheduler.request(Trigger::Manual);
        run_idle(&source);
        finish(&runner, SyncOutcome::Conflict);
        assert_eq!(guard.0.borrow().record_calls, 1);
    }
}
