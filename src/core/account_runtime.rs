//! Account runtimes: the glue between configuration and the UI.
//!
//! Fase 5 (Task 5.1): mirrors `core/account_manager.py`. One [`FolderRuntime`]
//! owns the [`Scheduler`], the deletion guard, the interval timers and the
//! [`StateController`] for a single local/remote folder pair. An
//! [`AccountRuntime`] groups them and exposes the *worst* state through an
//! [`AggregateStateController`] plus the neutral scheduler facade used by the
//! account controls when the account has no folders. The [`AccountManager`]
//! owns one runtime per configured account.
//!
//! The actual `nextcloudcmd` run is plugged in through a
//! `Box<dyn SyncRunner>` (a [`SyncEngine`] in production, a fake in tests) so
//! this module is testable without a GLib main loop. Timing goes through a
//! shared `Rc<RefCell<dyn TimeoutSource>>` (the app uses
//! [`GlibTimeoutSource`], tests use [`FakeTimeoutSource`]).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use crate::core::debounce::TimeoutSource;
use crate::core::delete_guard::DeleteGuard;
use crate::core::exclusions::ExclusionMatcher;
use crate::core::network::NetworkWatcher;
use crate::core::power::PowerWatcher;
use crate::core::scheduler::{DeleteAlert, Scheduler, SyncRunner};
use crate::core::suspend::SuspendWatcher;
use crate::core::sync_permit::SyncPermit;
use crate::core::triggers::Trigger;
use crate::core::watcher::{FsWatcher, WatcherEvent};
use crate::nextcloud::push::{remote_push_supported, NotifyPushClient};
use crate::nextcloud::sync_engine::{SyncEngine, SyncProgress};
use crate::state::{AggregateStateController, AppState, PushState, StateController};
use crate::storage::config::{AccountConfig, Config, FolderConfig, NetworkConfig};
use crate::util::i18n::t;

/// One set of system watchers built together by a factory.
///
/// The default factory wires the production GLib/sysfs/login1 backends; tests
/// install a factory that returns inert, fake-backed watchers so the runtimes
/// stay deterministic without a main loop or real hardware.
pub(crate) struct WatcherBundle {
    network: NetworkWatcher,
    power: PowerWatcher,
    suspend: SuspendWatcher,
}

/// Builds a fresh [`WatcherBundle`] on demand.
pub(crate) type WatcherFactory = Rc<dyn Fn() -> WatcherBundle>;

/// The default factory: GLib network monitor, ACPI sysfs power supply and the
/// login1 suspend signal — the same backends `network.py`/`power.py`/
/// `suspend.py` use.
fn default_watcher_factory() -> WatcherFactory {
    Rc::new(|| WatcherBundle {
        network: NetworkWatcher::gio(),
        power: PowerWatcher::sysfs(),
        suspend: SuspendWatcher::login1(),
    })
}

/// Live scheduler/push handles the system watchers fan out to.
///
/// Watcher callbacks arrive asynchronously (from the GLib main loop or the
/// probe backends) and must reflect the *current* folder set, which changes as
/// folders are added or removed. They therefore capture an
/// `Rc<RefCell<WatcherTargets>>` that [`AccountRuntime`] keeps in sync, instead
/// of a snapshot taken at start time.
#[derive(Default)]
struct WatcherTargets {
    schedulers: Vec<Scheduler>,
    push: Option<NotifyPushClient>,
    /// Global "pause on battery" preference (honored in addition to the live
    /// `on_battery` reading, like `application.py`'s `_power_changed`).
    pause_on_battery: bool,
    /// Last battery reading reported by the power watcher.
    on_battery: bool,
    /// Last notify_push state/message reported by the push client.
    push_state: Option<(PushState, String)>,
    /// App-layer hook for the push `notify_notification` hint (issue #31): set
    /// by the launcher to poke the server-notifications poller.
    on_server_notification: Option<Rc<dyn Fn()>>,
}

impl WatcherTargets {
    /// Network availability reaches every folder scheduler and the push client
    /// (mirrors `_network_changed`: `scheduler.set_online` + `push.set_online`).
    fn apply_online(targets: &Rc<RefCell<WatcherTargets>>, online: bool) {
        // Clone the handles and release the borrow before calling out: the
        // push client emits state synchronously through `on_state`, which
        // borrows `targets` again.
        let (schedulers, push) = {
            let current = targets.borrow();
            (current.schedulers.clone(), current.push.clone())
        };
        for scheduler in &schedulers {
            scheduler.set_online(online);
        }
        if let Some(push) = &push {
            push.set_online(online);
        }
    }

    /// Battery state gates sync only when the global preference is on, exactly
    /// like `_power_changed`'s `pause = general.pause_on_battery and on_battery`.
    fn apply_on_battery(targets: &Rc<RefCell<WatcherTargets>>, on_battery: bool) {
        let pause = {
            let mut current = targets.borrow_mut();
            current.on_battery = on_battery;
            current.pause_on_battery && on_battery
        };
        let schedulers = targets.borrow().schedulers.clone();
        for scheduler in &schedulers {
            scheduler.set_battery_paused(pause);
        }
    }

    /// On wake: drop the push socket so it reconnects and request a resume sync
    /// (mirrors `_resumed`). Push credentials are re-supplied by the app layer
    /// through [`NotifyPushClient::configure`].
    fn apply_resume(targets: &Rc<RefCell<WatcherTargets>>) {
        let (schedulers, push) = {
            let current = targets.borrow();
            (current.schedulers.clone(), current.push.clone())
        };
        if let Some(push) = &push {
            push.disconnect(true);
        }
        for scheduler in &schedulers {
            scheduler.request(Trigger::Resume);
        }
    }

    /// A remote file hint triggers a remote-push sync on every folder, like the
    /// Python `NotifyPushClient` callback `scheduler.request(REMOTE_PUSH)`.
    fn apply_remote_push(targets: &Rc<RefCell<WatcherTargets>>) {
        let schedulers = targets.borrow().schedulers.clone();
        for scheduler in &schedulers {
            scheduler.request(Trigger::RemotePush);
        }
    }

    /// Record the latest push state reported by the client (used for the UI).
    fn store_push_state(targets: &Rc<RefCell<WatcherTargets>>, state: PushState, message: String) {
        if let Ok(mut current) = targets.try_borrow_mut() {
            current.push_state = Some((state, message));
        }
    }

    /// A `notify_notification` hint from the push client pokes the app layer's
    /// server-notifications poller (issue #31), when one is installed.
    fn apply_server_notification(targets: &Rc<RefCell<WatcherTargets>>) {
        let callback = targets.borrow().on_server_notification.clone();
        if let Some(callback) = callback {
            callback();
        }
    }
}

/// One running synchronization runtime for a single folder pair.
pub struct FolderRuntime {
    pub folder: FolderConfig,
    state: StateController,
    scheduler: Scheduler,
    timers: crate::core::scheduler::SyncTimers,
    progress_rx: Option<async_channel::Receiver<SyncProgress>>,
    /// Live filesystem watcher feeding `Trigger::LocalInotify`, when enabled.
    ///
    /// The watcher itself is moved into [`Self::watcher_task`]'s closure (it
    /// is not `Clone`); the task owns it and keeps it alive.
    watcher_task: Option<glib::JoinHandle<()>>,
}

impl Clone for FolderRuntime {
    fn clone(&self) -> Self {
        // The clone keeps the live state and scheduler (shared by Rc); the
        // interval timers are rebuilt unarmed, the progress receiver and the
        // watcher task are not moved — the original runtime owns them.
        Self {
            folder: self.folder.clone(),
            state: self.state.clone(),
            scheduler: self.scheduler.clone(),
            timers: self.scheduler.timers(),
            progress_rx: None,
            watcher_task: None,
        }
    }
}

impl FolderRuntime {
    /// Build the runtime for one folder pair.
    ///
    /// `runner` performs the reconciliation, `progress_rx` (optional) forwards
    /// parsed progress events to `state` on the GLib main loop, and `source`
    /// is the shared timer backend.
    pub fn new(
        account: &AccountConfig,
        folder: FolderConfig,
        network: &NetworkConfig,
        runner: Box<dyn SyncRunner>,
        source: Rc<RefCell<dyn TimeoutSource>>,
        sync_permit: Option<SyncPermit>,
        progress_rx: Option<async_channel::Receiver<SyncProgress>>,
    ) -> Self {
        let state = StateController::new(AppState::IdleOk);
        let settings = crate::core::triggers::TriggerSettings::from(&account.sync);
        let scheduler = Scheduler::new(
            state.clone(),
            settings,
            runner,
            source.clone(),
            sync_permit,
            None,
        );
        // Issue #35: the scheduler knows which local tree it reconciles so
        // the permit can queue overlapping folders and the external-engine
        // scan can guard the same root.
        scheduler.set_local_root(Some(std::path::PathBuf::from(&folder.local_root)));
        if account.delete_guard.enabled {
            scheduler.set_guard(Some(Box::new(DeleteGuard::for_folder(account, &folder))));
        }
        let mut timers = scheduler.timers();
        timers.configure(&crate::core::triggers::TriggerSettings::from(&account.sync));
        let _ = network;
        Self {
            folder,
            state,
            scheduler,
            timers,
            progress_rx,
            watcher_task: None,
        }
    }

    /// Start the filesystem watcher for this folder, when local inotify is
    /// enabled, and forward its events to the scheduler on the main loop.
    ///
    /// Must be called once from the thread owning the main context (the app
    /// startup, never inside unit tests). `local_root` must exist; when it
    /// does not, the watcher is skipped (the scheduler still works through
    /// intervals/push/manual). `Change` events request a [`Trigger::LocalInotify`]
    /// sync; a degraded watcher is rebuilt through [`FsWatcher::rescan`], which
    /// re-registers the tree and emits a [`WatcherEvent::Rescan`].
    pub fn connect_watcher(&mut self, account: &AccountConfig) {
        if !account.sync.local_inotify_enabled {
            return;
        }
        let matcher = ExclusionMatcher::new(
            account.sync.exclude_patterns.clone(),
            account.sync.exclude_patterns_enabled,
        );
        let Ok((watcher, receiver)) = FsWatcher::start(&self.folder.local_root, matcher) else {
            // Folder not present yet (or backend unavailable): skip silently;
            // the scheduler still runs through the other triggers.
            return;
        };
        let scheduler = self.scheduler.clone();
        let task = glib::spawn_future_local(async move {
            let mut watcher = watcher;
            while let Ok(event) = receiver.recv().await {
                match event {
                    WatcherEvent::Change(_) | WatcherEvent::Rescan => {
                        scheduler.request(Trigger::LocalInotify);
                    }
                    WatcherEvent::Degraded(_) => {
                        watcher.rescan();
                    }
                }
            }
        });
        self.watcher_task = Some(task);
    }

    /// Forward parsed progress events to `state` on the GLib main loop.
    ///
    /// Must be called once from the thread owning the main context (the app
    /// startup, never inside unit tests); the receiver is moved and the
    /// forwarder runs until the engine closes the channel.
    pub fn connect_progress(&mut self) {
        let Some(progress_rx) = self.progress_rx.take() else {
            return;
        };
        let state_for_progress = self.state.clone();
        glib::spawn_future_local(async move {
            while let Ok(progress) = progress_rx.recv().await {
                state_for_progress.set_progress(Some(progress));
            }
        });
    }

    /// Build the production runner (a [`SyncEngine`]) and its progress channel
    /// for one folder pair.
    pub fn engine_for(
        account: &AccountConfig,
        folder: &FolderConfig,
        network: &NetworkConfig,
        exclude_file: Option<std::path::PathBuf>,
        executable: Option<std::path::PathBuf>,
    ) -> (Box<dyn SyncRunner>, async_channel::Receiver<SyncProgress>) {
        let (tx, rx) = async_channel::bounded(256);
        let engine = SyncEngine::new(
            account.clone(),
            folder.clone(),
            network.clone(),
            exclude_file,
            executable,
            tx,
        )
        .with_remote_ensurer(Arc::new(|account, folder, password| {
            crate::nextcloud::sync_engine::ProductionRemoteEnsurer::run(account, folder, password)
        }));
        (Box::new(engine), rx)
    }

    /// The state controller for this folder.
    pub fn state(&self) -> StateController {
        self.state.clone()
    }

    /// The scheduler driving this folder.
    pub fn scheduler(&self) -> Scheduler {
        self.scheduler.clone()
    }

    /// Request an immediate manual synchronization.
    pub fn sync_now(&self) {
        self.scheduler
            .request(crate::core::triggers::Trigger::Manual);
    }

    /// Pause or resume this folder by user request.
    pub fn set_paused(&self, paused: bool) {
        self.scheduler.set_user_paused(paused);
    }

    /// Whether the user has paused this folder.
    pub fn user_paused(&self) -> bool {
        self.scheduler.user_paused()
    }

    /// Reconfigure the interval timers from the account sync settings.
    pub fn reconfigure(&mut self, account: &AccountConfig) {
        self.timers
            .configure(&crate::core::triggers::TriggerSettings::from(&account.sync));
    }

    /// Stop the scheduler, cancelling pending work and timers.
    pub fn stop(&mut self) {
        self.scheduler.stop();
        self.timers.stop();
    }
}

/// Neutral scheduler facade for an account without sync folders.
///
/// The account controls read the scheduler state unconditionally; without
/// folders there is no real scheduler, so these return neutral values.
#[derive(Clone, Default)]
pub struct NeutralScheduler;

impl NeutralScheduler {
    pub fn user_paused(&self) -> bool {
        false
    }
    pub fn battery_paused(&self) -> bool {
        false
    }
    pub fn delete_alert(&self) -> Option<DeleteAlert> {
        None
    }
    pub fn sync_now(&self) {}
    pub fn set_paused(&self, _paused: bool) {}
    pub fn approve_delete_once(&self) {}
    pub fn restore_from_server(&self) {}
}

/// The account-level scheduler surface the view consumes.
#[derive(Clone)]
pub enum SchedulerFacade {
    Real(Scheduler),
    Neutral(NeutralScheduler),
}

/// One activity-log line for a finished run, translated to the active locale.
pub fn outcome_log_line(outcome: &crate::core::scheduler::SyncOutcome) -> &'static str {
    match outcome {
        crate::core::scheduler::SyncOutcome::Success => t("Synchronization completed"),
        crate::core::scheduler::SyncOutcome::Conflict => {
            t("Synchronization completed with conflicts")
        }
        crate::core::scheduler::SyncOutcome::AuthFailed => {
            t("Synchronization failed: credentials rejected")
        }
        crate::core::scheduler::SyncOutcome::KeyringLocked => {
            t("Synchronization blocked: password keyring is locked")
        }
        crate::core::scheduler::SyncOutcome::Failed => t("Synchronization failed — view the log"),
    }
}

impl SchedulerFacade {
    pub fn user_paused(&self) -> bool {
        match self {
            Self::Real(scheduler) => scheduler.user_paused(),
            Self::Neutral(neutral) => neutral.user_paused(),
        }
    }
    pub fn battery_paused(&self) -> bool {
        match self {
            Self::Real(scheduler) => scheduler.battery_paused(),
            Self::Neutral(neutral) => neutral.battery_paused(),
        }
    }
    pub fn delete_alert(&self) -> Option<DeleteAlert> {
        match self {
            Self::Real(scheduler) => scheduler.delete_alert(),
            Self::Neutral(neutral) => neutral.delete_alert(),
        }
    }
    pub fn sync_now(&self) {
        match self {
            Self::Real(scheduler) => scheduler.request(crate::core::triggers::Trigger::Manual),
            Self::Neutral(neutral) => neutral.sync_now(),
        }
    }
    pub fn set_paused(&self, paused: bool) {
        match self {
            Self::Real(scheduler) => scheduler.set_user_paused(paused),
            Self::Neutral(neutral) => neutral.set_paused(paused),
        }
    }
    pub fn approve_delete_once(&self) {
        match self {
            Self::Real(scheduler) => scheduler.approve_delete_once(),
            Self::Neutral(neutral) => neutral.approve_delete_once(),
        }
    }
    pub fn restore_from_server(&self) {
        match self {
            Self::Real(scheduler) => scheduler.restore_from_server(),
            Self::Neutral(neutral) => neutral.restore_from_server(),
        }
    }
}

/// One account with zero or more running folder runtimes.
#[derive(Clone)]
pub struct AccountRuntime {
    pub account: AccountConfig,
    folders: HashMap<String, FolderRuntime>,
    aggregate: AggregateStateController,
    idle: Option<StateController>,
    source: Rc<RefCell<dyn TimeoutSource>>,
    sync_permit: Option<SyncPermit>,
    network: NetworkConfig,
    /// System-wide watchers (network/power/suspend), owned per account like the
    /// Python `RuntimeController`. Started in [`Self::start`], stopped in
    /// [`Self::stop`].
    network_watcher: Option<NetworkWatcher>,
    power_watcher: Option<PowerWatcher>,
    suspend_watcher: Option<SuspendWatcher>,
    /// notify_push client; `None` for OpenCloud or when remote push is disabled.
    push: Option<NotifyPushClient>,
    /// Live handles the watcher callbacks fan out to (shared with the closures).
    targets: Rc<RefCell<WatcherTargets>>,
    /// Builds the production watchers on `start`; tests swap in fakes.
    watcher_factory: WatcherFactory,
}

/// One activity-log line for a finished run: a human readable account label
/// (`login@host`) and the folder's local path instead of the internal SHA-256
/// ids, so the Recent tab rows read naturally (issue #33).
pub fn activity_line(
    login_name: &str,
    server_url: &str,
    local_root: &str,
    outcome: &crate::core::scheduler::SyncOutcome,
) -> String {
    format!(
        "{}@{} · {}: {}",
        login_name,
        crate::util::url::server_host(server_url),
        local_root,
        outcome_log_line(outcome)
    )
}

impl AccountRuntime {
    /// Create a runtime for one account. No folders are started yet; call
    /// [`start`](Self::start) or [`sync_folders`](Self::sync_folders).
    ///
    /// `pause_on_battery` applies the global `general.pause_on_battery`
    /// preference at construction time (the Python applies it live in
    /// `_power_changed`; the runtime also exposes
    /// [`set_pause_on_battery`](Self::set_pause_on_battery) for changes).
    pub fn new(
        account: AccountConfig,
        network: NetworkConfig,
        source: Rc<RefCell<dyn TimeoutSource>>,
        sync_permit: Option<SyncPermit>,
        pause_on_battery: bool,
    ) -> Self {
        let targets = WatcherTargets {
            pause_on_battery,
            ..WatcherTargets::default()
        };
        Self {
            account,
            folders: HashMap::new(),
            aggregate: AggregateStateController::new(),
            idle: None,
            source,
            sync_permit,
            network,
            network_watcher: None,
            power_watcher: None,
            suspend_watcher: None,
            push: None,
            targets: Rc::new(RefCell::new(targets)),
            watcher_factory: default_watcher_factory(),
        }
    }

    pub fn account_id(&self) -> &str {
        &self.account.id
    }

    /// The aggregated (worst) state across this account's folders.
    pub fn state(&self) -> AggregateStateController {
        self.aggregate.clone()
    }

    /// The `StateController`s that feed this account's aggregate. The manager
    /// subscribes these to build its own global aggregate.
    pub fn state_sources(&self) -> Vec<StateController> {
        let mut sources: Vec<StateController> =
            self.folders.values().map(|folder| folder.state()).collect();
        if let Some(idle) = &self.idle {
            sources.push(idle.clone());
        }
        sources
    }

    /// The folder runtimes keyed by folder id.
    pub fn folders(&self) -> &HashMap<String, FolderRuntime> {
        &self.folders
    }

    /// The scheduler surface used by the account controls: the first folder's
    /// scheduler, or a neutral facade when there are no folders.
    pub fn scheduler(&self) -> SchedulerFacade {
        match self.folders.values().next() {
            Some(folder_runtime) => SchedulerFacade::Real(folder_runtime.scheduler()),
            None => SchedulerFacade::Neutral(NeutralScheduler),
        }
    }

    /// The last successful sync timestamp of the account, if any.
    pub fn last_successful_sync(&self) -> Option<&str> {
        self.account.runtime.last_successful_sync.as_deref()
    }

    /// The notify_push client for this account, if one was mounted.
    ///
    /// The app layer holds the account credentials and calls
    /// [`NotifyPushClient::configure`] with them (the Python does this in
    /// `_configure_push` once the keyring resolves the password). The runtime
    /// owns the client lifecycle (construction, online/offline and resume
    /// wiring) but not the credential handshake.
    pub fn push_client(&self) -> Option<NotifyPushClient> {
        self.push.clone()
    }

    /// Install the app-layer hook invoked when the push client reports a
    /// `notify_notification` server hint (issue #31). Set once at startup;
    /// the push client routes through the shared [`WatcherTargets`], so the
    /// hook keeps working even if the channel reconnects.
    pub fn set_on_server_notification(&self, callback: Rc<dyn Fn()>) {
        self.targets.borrow_mut().on_server_notification = Some(callback);
    }

    /// Connect an activity logger to every folder scheduler: one line per
    /// finished run with its outcome. The logger is shared (`LogBuffer` is
    /// `Clone` over a shared buffer), so the UI recent/log views see the same
    /// lines the engine writes. When `notifications` is enabled, problem
    /// outcomes also raise a desktop notification through `notifier`.
    pub fn connect_logger(
        &self,
        logger: &crate::core::log::LogBuffer,
        notifier: Option<Rc<dyn crate::core::notifications::DesktopNotifier>>,
        notifications_enabled: bool,
    ) {
        // Human readable account label (`login@host`) and folder local path
        // instead of the internal SHA-256 ids, so the Recent tab rows read
        // naturally (issue #33).
        let login_name = self.account.login_name.clone();
        let server_url = self.account.server_url.clone();
        for runtime in self.folders.values() {
            let login_name = login_name.clone();
            let server_url = server_url.clone();
            let logger = logger.clone();
            let notifier = notifier.clone();
            let folder_path = runtime.folder.local_root.clone();
            runtime
                .scheduler()
                .set_on_completed(Some(Box::new(move |outcome| {
                    logger.append(&activity_line(
                        &login_name,
                        &server_url,
                        &folder_path,
                        outcome,
                    ));
                    if let Some(notifier) = notifier.as_ref() {
                        crate::core::notifications::notify_for_outcome(
                            notifier,
                            notifications_enabled,
                            &folder_path,
                            outcome,
                        );
                    }
                })));
        }
    }

    /// The latest notify_push state and message reported by the client.
    pub fn push_state(&self) -> Option<(PushState, String)> {
        self.targets.borrow().push_state.clone()
    }

    /// Apply a change of the global `pause_on_battery` preference at runtime,
    /// re-evaluating the current battery state.
    pub fn set_pause_on_battery(&mut self, enabled: bool) {
        let pause = {
            let mut current = self.targets.borrow_mut();
            current.pause_on_battery = enabled;
            current.pause_on_battery && current.on_battery
        };
        let schedulers = self.targets.borrow().schedulers.clone();
        for scheduler in &schedulers {
            scheduler.set_battery_paused(pause);
        }
    }

    /// Start runtimes for every configured folder and wire the system watchers
    /// (network/power/suspend) and the notify_push client.
    pub fn start(&mut self) {
        self.prime();
        self.mount_watchers();
    }

    /// Bring the folder runtimes up without the system watchers (used by
    /// [`start`](Self::start) and by tests that drive the watchers themselves).
    fn prime(&mut self) {
        self.sync_folders();
        if self.folders.is_empty() && self.idle.is_none() {
            let idle = StateController::new(AppState::IdleOk);
            idle.set(AppState::IdleOk, t("Connected. Add folders from Settings."));
            self.aggregate.add(idle.clone());
            self.idle = Some(idle);
        }
    }

    /// Build the production watchers through [`Self::watcher_factory`] and the
    /// push client, then mount them.
    fn mount_watchers(&mut self) {
        let WatcherBundle {
            network,
            power,
            suspend,
        } = (self.watcher_factory)();
        let push = self.default_push();
        self.mount(network, power, suspend, push);
    }

    /// Wire the GLib main-loop consumers for every folder runtime: the local
    /// filesystem watcher and the live progress forwarder.
    ///
    /// Production only: called by the app after [`Self::start`], on the thread
    /// owning the main context. The unit tests drive the runtimes through
    /// [`start_without_watchers`](Self::start_without_watchers) and never call
    /// this, so they stay deterministic without a GLib loop.
    pub fn connect_glue(&mut self) {
        let account = self.account.clone();
        for runtime in self.folders.values_mut() {
            runtime.connect_watcher(&account);
            runtime.connect_progress();
        }
    }

    /// Wire callbacks to the given watchers, start them and keep them for
    /// [`Self::stop`]. Injecting the watchers keeps tests deterministic.
    fn mount(
        &mut self,
        mut network: NetworkWatcher,
        mut power: PowerWatcher,
        mut suspend: SuspendWatcher,
        push: Option<NotifyPushClient>,
    ) {
        {
            let mut targets = self.targets.borrow_mut();
            targets.push = push.clone();
        }
        let network_targets = Rc::clone(&self.targets);
        network.set_callback(move |online| WatcherTargets::apply_online(&network_targets, online));
        let power_targets = Rc::clone(&self.targets);
        power.set_callback(move |on_battery| {
            WatcherTargets::apply_on_battery(&power_targets, on_battery)
        });
        let suspend_targets = Rc::clone(&self.targets);
        suspend.set_on_resume(move || WatcherTargets::apply_resume(&suspend_targets));

        // `start` reports the current network/battery state synchronously, which
        // fans out to the schedulers through the callbacks above.
        network.start();
        power.start();
        suspend.start();

        self.network_watcher = Some(network);
        self.power_watcher = Some(power);
        self.suspend_watcher = Some(suspend);
        self.push = push;
    }

    /// Build the notify_push client for this account, or `None` when the
    /// provider has no push channel or the account disabled it.
    fn default_push(&self) -> Option<NotifyPushClient> {
        if !remote_push_supported(self.account.provider) || !self.account.sync.remote_push_enabled {
            return None;
        }
        let file_targets = Rc::clone(&self.targets);
        let notification_targets = Rc::clone(&self.targets);
        let state_targets = Rc::clone(&self.targets);
        Some(NotifyPushClient::new(
            self.account.provider,
            move || WatcherTargets::apply_remote_push(&file_targets),
            move || WatcherTargets::apply_server_notification(&notification_targets),
            move |state, message| WatcherTargets::store_push_state(&state_targets, state, message),
        ))
    }

    /// Refresh the shared scheduler list watched by the system callbacks from
    /// the current folder runtimes (folders can be added or removed at runtime).
    fn sync_targets(&self) {
        let schedulers: Vec<Scheduler> = self
            .folders
            .values()
            .map(|folder| folder.scheduler())
            .collect();
        self.targets.borrow_mut().schedulers = schedulers;
    }

    /// Reconcile the folder runtimes with the account's current folders.
    pub fn sync_folders(&mut self) {
        let desired = self.account.folders.clone();
        let desired_ids: HashSet<String> = desired.iter().map(|folder| folder.id.clone()).collect();
        for folder_id in self.folders.keys().cloned().collect::<Vec<_>>() {
            if !desired_ids.contains(&folder_id) {
                if let Some(mut runtime) = self.folders.remove(&folder_id) {
                    let state = runtime.state();
                    runtime.stop();
                    self.aggregate.remove(&state);
                }
            }
        }
        for folder in desired {
            self.ensure_folder(folder);
        }
        self.sync_targets();
    }

    fn ensure_folder(&mut self, folder: FolderConfig) {
        if self.folders.contains_key(&folder.id) {
            return;
        }
        let (runner, progress_rx) =
            FolderRuntime::engine_for(&self.account, &folder, &self.network, None, None);
        let runtime = FolderRuntime::new(
            &self.account,
            folder,
            &self.network,
            runner,
            self.source.clone(),
            self.sync_permit.clone(),
            Some(progress_rx),
        );
        let state = runtime.state();
        self.aggregate.add(state);
        self.folders.insert(runtime.folder.id.clone(), runtime);
    }

    /// Stop every folder runtime, the system watchers, the push client and
    /// reset the aggregate.
    pub fn stop(&mut self) {
        for (_, mut runtime) in self.folders.drain() {
            runtime.stop();
        }
        if let Some(idle) = self.idle.take() {
            self.aggregate.remove(&idle);
        }
        self.aggregate.clear();
        if let Some(mut network) = self.network_watcher.take() {
            network.stop();
        }
        if let Some(mut power) = self.power_watcher.take() {
            power.stop();
        }
        if let Some(mut suspend) = self.suspend_watcher.take() {
            suspend.stop();
        }
        if let Some(push) = self.push.take() {
            push.disconnect(false);
        }
        self.targets.borrow_mut().push = None;
    }
}

/// Owns one [`AccountRuntime`] per configured account.
#[derive(Clone)]
pub struct AccountManager {
    runtimes: HashMap<String, AccountRuntime>,
    aggregate: AggregateStateController,
    /// State controllers this manager subscribed for each account runtime, so
    /// a removed account leaves the global aggregate consistent.
    aggregate_sources: HashMap<String, Vec<StateController>>,
    source: Rc<RefCell<dyn TimeoutSource>>,
    sync_permit: SyncPermit,
    /// Global `general.pause_on_battery` preference, applied to new runtimes.
    pause_on_battery: bool,
    /// Builds the production watchers; tests swap in fakes.
    watcher_factory: WatcherFactory,
}

impl AccountManager {
    /// Create an empty manager. Populate it with [`start`](Self::start).
    pub fn new(source: Rc<RefCell<dyn TimeoutSource>>) -> Self {
        Self {
            runtimes: HashMap::new(),
            aggregate: AggregateStateController::new(),
            aggregate_sources: HashMap::new(),
            source,
            sync_permit: SyncPermit::try_new(1).expect("permit max 1"),
            pause_on_battery: false,
            watcher_factory: default_watcher_factory(),
        }
    }

    /// Start a runtime for every account in the configuration. The global
    /// `general.pause_on_battery` preference is captured for the runtimes.
    ///
    /// This wires the system watchers (network/power/suspend) but NOT the
    /// per-folder GLib consumers; call [`connect_all_glue`](Self::connect_all_glue)
    /// from the main thread afterwards to start the filesystem watchers and
    /// the progress forwarders.
    pub fn start(&mut self, config: &Config) {
        self.pause_on_battery = config.general.pause_on_battery;
        for account in config.accounts.clone() {
            self.ensure_runtime(account);
        }
    }

    /// Start the per-folder GLib main-loop consumers (filesystem watchers and
    /// progress forwarders) for every account runtime. Must be called from the
    /// thread owning the main context (the app startup), never in tests.
    pub fn connect_all_glue(&mut self) {
        for runtime in self.runtimes.values_mut() {
            runtime.connect_glue();
        }
    }

    /// The account runtimes keyed by account id.
    pub fn runtimes(&self) -> &HashMap<String, AccountRuntime> {
        &self.runtimes
    }

    /// The global aggregated state across all accounts.
    pub fn aggregate_state(&self) -> AggregateStateController {
        self.aggregate.clone()
    }

    /// Get one account runtime.
    pub fn get(&self, account_id: &str) -> Option<AccountRuntime> {
        self.runtimes.get(account_id).cloned()
    }

    /// Start the runtime for one account when it is not already running.
    pub fn ensure_account_runtime(&mut self, account: AccountConfig) {
        self.ensure_runtime(account);
    }

    /// Reconcile the folder runtimes of an existing account with a fresh
    /// account configuration (folders added or removed in Settings).
    ///
    /// The runtime keeps running; only the folder set is reconciled. Returns
    /// `false` when the account has no runtime yet.
    pub fn sync_folders(&mut self, account: &AccountConfig) -> bool {
        let Some(runtime) = self.runtimes.get_mut(&account.id) else {
            return false;
        };
        runtime.account = account.clone();
        runtime.sync_folders();
        true
    }

    /// Drop a single account runtime, leaving the rest running.
    pub fn remove(&mut self, account_id: &str) -> bool {
        let Some(mut runtime) = self.runtimes.remove(account_id) else {
            return false;
        };
        if let Some(sources) = self.aggregate_sources.remove(account_id) {
            for source in sources {
                self.aggregate.remove(&source);
            }
        }
        runtime.stop();
        true
    }

    fn ensure_runtime(&mut self, account: AccountConfig) {
        if self.runtimes.contains_key(&account.id) {
            return;
        }
        let mut runtime = AccountRuntime::new(
            account,
            NetworkConfig::default(),
            self.source.clone(),
            Some(self.sync_permit.clone()),
            self.pause_on_battery,
        );
        runtime.watcher_factory = self.watcher_factory.clone();
        runtime.start();
        let sources = runtime.state_sources();
        for source in &sources {
            self.aggregate.add(source.clone());
        }
        let account_id = runtime.account_id().to_string();
        self.aggregate_sources.insert(account_id, sources);
        self.runtimes.insert(runtime.account.id.clone(), runtime);
    }

    /// Stop every account runtime.
    pub fn stop(&mut self) {
        for (_, mut runtime) in self.runtimes.drain() {
            runtime.stop();
        }
        self.aggregate_sources.clear();
        self.aggregate.clear();
    }
}

#[cfg(test)]
impl AccountRuntime {
    /// Bring the folder runtimes up without the production system watchers, so
    /// the existing tests stay deterministic without GLib/sysfs/login1.
    pub(crate) fn start_without_watchers(&mut self) {
        self.prime();
    }

    /// Mount specific (fake-backed) watchers and an optional push client.
    pub(crate) fn mount_test(
        &mut self,
        network: NetworkWatcher,
        power: PowerWatcher,
        suspend: SuspendWatcher,
        push: Option<NotifyPushClient>,
    ) {
        self.mount(network, power, suspend, push);
    }

    /// Drive the notify_push file-notification fan-out (what the client's
    /// `on_file_notification` callback runs in production).
    pub(crate) fn simulate_remote_push(&self) {
        WatcherTargets::apply_remote_push(&self.targets);
    }

    /// Build the push client for the current account (test mirror of
    /// `default_push`).
    pub(crate) fn default_push_for_test(&self) -> Option<NotifyPushClient> {
        self.default_push()
    }
}

#[cfg(test)]
impl AccountManager {
    /// Install a watcher factory before [`Self::start`] (tests pass inerts).
    pub(crate) fn set_watcher_factory(&mut self, factory: WatcherFactory) {
        self.watcher_factory = factory;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::debounce::{fire_timer, FakeTimeoutSource};
    use crate::core::network::NetworkProbe;
    use crate::core::power::PowerProbe;
    use crate::core::suspend::SuspendProbe;
    use crate::nextcloud::driver::Provider;
    use crate::state::StateSnapshot;

    fn fake_source() -> Rc<RefCell<FakeTimeoutSource>> {
        // Pin English so the translated state messages stay deterministic
        // regardless of the ambient locale (LANG=es_ES on the dev machine).
        crate::util::i18n::set_locale(crate::util::i18n::Locale::English);
        Rc::new(RefCell::new(FakeTimeoutSource::default()))
    }

    fn sample_account(with_folders: bool) -> AccountConfig {
        let mut account = AccountConfig {
            id: "acc-1".to_string(),
            server_url: "https://cloud.example.com".to_string(),
            login_name: "alice".to_string(),
            ..AccountConfig::default()
        };
        // Keep the timer backend deterministic: disable interval triggers so
        // the only pending timer after `sync_now` is the start idle.
        account.sync.local_interval_enabled = false;
        account.sync.remote_interval_enabled = false;
        if with_folders {
            account.folders = vec![FolderConfig {
                id: "folder-1".to_string(),
                local_root: "/tmp/nsync-folder-1".to_string(),
                remote_path: "/docs".to_string(),
                space_id: None,
                size_confirmed: false,
            }];
        }
        account
    }

    // ---- fake system probes (deterministic, no GLib/sysfs/D-Bus) -----------

    #[derive(Clone, Default)]
    struct FakeNetProbe {
        inner: Rc<RefCell<FakeNetInner>>,
    }
    #[derive(Default)]
    struct FakeNetInner {
        available: bool,
        callback: Option<Rc<dyn Fn(bool)>>,
    }
    impl FakeNetProbe {
        fn set(&self, available: bool) {
            let callback = self.inner.borrow().callback.clone();
            if let Some(callback) = callback {
                callback(available);
            }
        }
    }
    impl NetworkProbe for FakeNetProbe {
        fn is_available(&self) -> bool {
            self.inner.borrow().available
        }
        fn subscribe(&self, callback: Rc<dyn Fn(bool)>) -> u64 {
            self.inner.borrow_mut().callback = Some(callback);
            1
        }
        fn unsubscribe(&self, _id: u64) {
            self.inner.borrow_mut().callback = None;
        }
    }

    #[derive(Clone, Default)]
    struct FakePowerProbe {
        inner: Rc<RefCell<FakePowerInner>>,
    }
    #[derive(Default)]
    struct FakePowerInner {
        on_battery: bool,
        callback: Option<Rc<dyn Fn(bool)>>,
    }
    impl FakePowerProbe {
        fn set(&self, on_battery: bool) {
            let callback = self.inner.borrow().callback.clone();
            if let Some(callback) = callback {
                callback(on_battery);
            }
        }
    }
    impl PowerProbe for FakePowerProbe {
        fn on_battery(&self) -> bool {
            self.inner.borrow().on_battery
        }
        fn subscribe(&self, callback: Rc<dyn Fn(bool)>) -> u64 {
            self.inner.borrow_mut().callback = Some(callback);
            1
        }
        fn unsubscribe(&self, _id: u64) {
            self.inner.borrow_mut().callback = None;
        }
    }

    #[derive(Clone, Default)]
    struct FakeSuspendProbe {
        inner: Rc<RefCell<FakeSuspendInner>>,
    }
    #[derive(Default)]
    struct FakeSuspendInner {
        on_resume: Option<Rc<dyn Fn()>>,
    }
    impl FakeSuspendProbe {
        fn fire_resume(&self) {
            let callback = self.inner.borrow().on_resume.clone();
            if let Some(callback) = callback {
                callback();
            }
        }
    }
    impl SuspendProbe for FakeSuspendProbe {
        fn subscribe(&self, on_resume: Rc<dyn Fn()>) -> u64 {
            self.inner.borrow_mut().on_resume = Some(on_resume);
            1
        }
        fn unsubscribe(&self, _id: u64) {
            self.inner.borrow_mut().on_resume = None;
        }
    }

    /// A factory whose watchers start online/on-mains and never push changes.
    fn inert_watcher_factory() -> WatcherFactory {
        Rc::new(|| {
            let net = FakeNetProbe::default();
            net.inner.borrow_mut().available = true;
            WatcherBundle {
                network: NetworkWatcher::new(Box::new(net)),
                power: PowerWatcher::new(Box::new(FakePowerProbe::default())),
                suspend: SuspendWatcher::new(Box::new(FakeSuspendProbe::default())),
            }
        })
    }

    /// A network watcher that reports "online" on start and never changes, for
    /// tests focused on power/suspend/push.
    fn online_network_watcher() -> NetworkWatcher {
        let net = FakeNetProbe::default();
        net.inner.borrow_mut().available = true;
        NetworkWatcher::new(Box::new(net))
    }

    // ---- existing runtime/manager tests (watchers kept inert) --------------

    #[test]
    fn account_without_folders_aggregates_to_idle_ok() {
        let source = fake_source();
        let mut runtime = AccountRuntime::new(
            sample_account(false),
            NetworkConfig::default(),
            source,
            None,
            false,
        );
        runtime.start_without_watchers();
        assert_eq!(runtime.state().snapshot().state, AppState::IdleOk);
        let scheduler = runtime.scheduler();
        assert!(!scheduler.user_paused());
        assert!(scheduler.delete_alert().is_none());
    }

    #[test]
    fn account_with_folder_creates_runtime_and_sync_now_requests() {
        let source = fake_source();
        let mut runtime = AccountRuntime::new(
            sample_account(true),
            NetworkConfig::default(),
            source.clone(),
            None,
            false,
        );
        runtime.start_without_watchers();
        assert_eq!(runtime.folders().len(), 1);
        let folder = runtime.folders().values().next().unwrap().clone();
        assert_eq!(folder.folder.id, "folder-1");
        assert_eq!(runtime.state().snapshot().state, AppState::IdleOk);

        folder.sync_now();
        // A queued manual sync schedules an idle source on the shared backend.
        assert_eq!(source.borrow().pending(), 1);
        let id = source.borrow().only_id();
        fire_timer(&source, id);
        assert_eq!(runtime.state().snapshot().state, AppState::Syncing);
    }

    #[test]
    fn set_paused_propagates_to_the_first_folder_scheduler() {
        let source = fake_source();
        let mut runtime = AccountRuntime::new(
            sample_account(true),
            NetworkConfig::default(),
            source,
            None,
            false,
        );
        runtime.start_without_watchers();
        runtime.scheduler().set_paused(true);
        assert!(runtime.scheduler().user_paused());
        assert_eq!(runtime.state().snapshot().state, AppState::PausedUser);
    }

    #[test]
    fn sync_folders_starts_and_removes_runtimes() {
        let source = fake_source();
        let mut runtime = AccountRuntime::new(
            sample_account(true),
            NetworkConfig::default(),
            source,
            None,
            false,
        );
        runtime.start_without_watchers();
        assert_eq!(runtime.folders().len(), 1);

        let mut account = sample_account(true);
        account.folders.push(FolderConfig {
            id: "folder-2".to_string(),
            local_root: "/tmp/nsync-folder-2".to_string(),
            remote_path: "/photos".to_string(),
            space_id: None,
            size_confirmed: false,
        });
        runtime.account = account;
        runtime.sync_folders();
        assert_eq!(runtime.folders().len(), 2);

        runtime.account.folders.clear();
        runtime.sync_folders();
        assert_eq!(runtime.folders().len(), 0);
        // With folders gone the aggregate falls back to unconfigured until
        // start() is called again to install the idle state.
        assert_eq!(runtime.state().snapshot().state, AppState::Unconfigured);
    }

    #[test]
    fn manager_starts_every_account_and_removes_one() {
        let source = fake_source();
        let mut manager = AccountManager::new(source);
        manager.set_watcher_factory(inert_watcher_factory());
        let mut config = Config {
            accounts: vec![sample_account(true), sample_account(false)],
            ..Config::default()
        };
        config.accounts[1].id = "acc-2".to_string();
        manager.start(&config);
        assert_eq!(manager.runtimes().len(), 2);
        assert!(manager.get("acc-1").is_some());

        assert!(manager.remove("acc-1"));
        assert_eq!(manager.runtimes().len(), 1);
        assert!(!manager.remove("acc-1"));
        assert!(manager.get("acc-2").is_some());
    }

    #[test]
    fn state_subscription_forwarded_through_the_aggregate() {
        let source = fake_source();
        let mut runtime = AccountRuntime::new(
            sample_account(true),
            NetworkConfig::default(),
            source,
            None,
            false,
        );
        runtime.start_without_watchers();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let collect = {
            let seen = Rc::clone(&seen);
            move |snapshot: &StateSnapshot| seen.borrow_mut().push(snapshot.state)
        };
        let _sub = runtime.state().subscribe(collect);
        let folder = runtime.folders().values().next().unwrap().clone();
        folder.set_paused(true);
        assert_eq!(*seen.borrow(), vec![AppState::IdleOk, AppState::PausedUser]);
    }

    #[test]
    fn manager_aggregate_tracks_removed_accounts() {
        let source = fake_source();
        let mut manager = AccountManager::new(source);
        manager.set_watcher_factory(inert_watcher_factory());
        let mut config = Config {
            accounts: vec![sample_account(false)],
            ..Config::default()
        };
        config.accounts[0].id = "acc-1".to_string();
        manager.start(&config);
        assert_eq!(manager.aggregate_state().snapshot().state, AppState::IdleOk);
        manager.remove("acc-1");
        assert_eq!(
            manager.aggregate_state().snapshot().state,
            AppState::Unconfigured
        );
    }

    #[test]
    fn neutral_facade_is_inert() {
        let facade = SchedulerFacade::Neutral(NeutralScheduler);
        assert!(!facade.user_paused());
        assert!(!facade.battery_paused());
        assert!(facade.delete_alert().is_none());
        facade.sync_now();
        facade.set_paused(true);
        facade.approve_delete_once();
        facade.restore_from_server();
    }

    // ---- network watcher wiring -------------------------------------------

    #[test]
    fn network_offline_then_online_drives_the_scheduler_state() {
        let source = fake_source();
        let net = FakeNetProbe::default();
        net.inner.borrow_mut().available = true;
        let mut runtime = AccountRuntime::new(
            sample_account(true),
            NetworkConfig::default(),
            source.clone(),
            None,
            false,
        );
        runtime.start_without_watchers();
        runtime.mount_test(
            NetworkWatcher::new(Box::new(net.clone())),
            PowerWatcher::new(Box::new(FakePowerProbe::default())),
            SuspendWatcher::new(Box::new(FakeSuspendProbe::default())),
            None,
        );
        // mount reports the initial online state, so the folder stays IdleOk.
        assert_eq!(runtime.state().snapshot().state, AppState::IdleOk);

        net.set(false);
        assert_eq!(runtime.state().snapshot().state, AppState::Offline);

        // Reconnecting requests a NetworkRestored sync because the default
        // account has inotify watching on (manual_only() is false), mirroring
        // the Python reconnect behaviour.
        net.set(true);
        assert_eq!(runtime.state().snapshot().state, AppState::SyncQueued);
    }

    #[test]
    fn network_reconnect_stays_idle_for_manual_only_accounts() {
        let source = fake_source();
        let net = FakeNetProbe::default();
        net.inner.borrow_mut().available = true;
        let mut account = sample_account(true);
        account.sync.local_inotify_enabled = false;
        account.sync.remote_push_enabled = false;
        let mut runtime =
            AccountRuntime::new(account, NetworkConfig::default(), source, None, false);
        runtime.start_without_watchers();
        runtime.mount_test(
            NetworkWatcher::new(Box::new(net.clone())),
            PowerWatcher::new(Box::new(FakePowerProbe::default())),
            SuspendWatcher::new(Box::new(FakeSuspendProbe::default())),
            None,
        );
        assert_eq!(runtime.state().snapshot().state, AppState::IdleOk);

        net.set(false);
        assert_eq!(runtime.state().snapshot().state, AppState::Offline);

        // With every automatic trigger off the queue stays empty, so the
        // scheduler returns to the manual-only idle instead of queueing a
        // reconnect sync.
        net.set(true);
        assert_eq!(runtime.state().snapshot().state, AppState::IdleManualOnly);
    }

    #[test]
    fn repeated_network_state_does_not_reemit() {
        let source = fake_source();
        let net = FakeNetProbe::default();
        net.inner.borrow_mut().available = true;
        let mut runtime = AccountRuntime::new(
            sample_account(true),
            NetworkConfig::default(),
            source,
            None,
            false,
        );
        runtime.start_without_watchers();
        runtime.mount_test(
            NetworkWatcher::new(Box::new(net.clone())),
            PowerWatcher::new(Box::new(FakePowerProbe::default())),
            SuspendWatcher::new(Box::new(FakeSuspendProbe::default())),
            None,
        );
        assert_eq!(runtime.state().snapshot().state, AppState::IdleOk);
        net.set(true);
        // Still online: no transition, no Offline state.
        assert_eq!(runtime.state().snapshot().state, AppState::IdleOk);
    }

    // ---- power watcher wiring ---------------------------------------------

    #[test]
    fn battery_pauses_only_when_the_preference_is_on() {
        let source = fake_source();
        let power = FakePowerProbe::default(); // starts on mains
        let mut runtime = AccountRuntime::new(
            sample_account(true),
            NetworkConfig::default(),
            source,
            None,
            true, // pause_on_battery enabled
        );
        runtime.start_without_watchers();
        runtime.mount_test(
            online_network_watcher(),
            PowerWatcher::new(Box::new(power.clone())),
            SuspendWatcher::new(Box::new(FakeSuspendProbe::default())),
            None,
        );

        power.set(true);
        assert_eq!(runtime.state().snapshot().state, AppState::PausedBattery);

        power.set(false);
        assert_eq!(runtime.state().snapshot().state, AppState::IdleOk);
    }

    #[test]
    fn battery_is_ignored_when_the_preference_is_off() {
        let source = fake_source();
        let power = FakePowerProbe::default();
        let mut runtime = AccountRuntime::new(
            sample_account(true),
            NetworkConfig::default(),
            source,
            None,
            false, // pause_on_battery disabled
        );
        runtime.start_without_watchers();
        runtime.mount_test(
            online_network_watcher(),
            PowerWatcher::new(Box::new(power.clone())),
            SuspendWatcher::new(Box::new(FakeSuspendProbe::default())),
            None,
        );

        power.set(true);
        assert_eq!(runtime.state().snapshot().state, AppState::IdleOk);
    }

    #[test]
    fn set_pause_on_battery_re_evaluates_the_current_state() {
        let source = fake_source();
        let power = FakePowerProbe::default();
        let mut runtime = AccountRuntime::new(
            sample_account(true),
            NetworkConfig::default(),
            source,
            None,
            false,
        );
        runtime.start_without_watchers();
        runtime.mount_test(
            online_network_watcher(),
            PowerWatcher::new(Box::new(power.clone())),
            SuspendWatcher::new(Box::new(FakeSuspendProbe::default())),
            None,
        );

        // On battery, but preference off: not paused.
        power.set(true);
        assert_eq!(runtime.state().snapshot().state, AppState::IdleOk);

        // Flip the preference while still on battery: now paused.
        runtime.set_pause_on_battery(true);
        assert_eq!(runtime.state().snapshot().state, AppState::PausedBattery);
    }

    // ---- suspend watcher wiring -------------------------------------------

    #[test]
    fn resume_requests_a_sync_trigger() {
        let source = fake_source();
        let suspend = FakeSuspendProbe::default();
        let mut runtime = AccountRuntime::new(
            sample_account(true),
            NetworkConfig::default(),
            source.clone(),
            None,
            false,
        );
        runtime.start_without_watchers();
        runtime.mount_test(
            online_network_watcher(),
            PowerWatcher::new(Box::new(FakePowerProbe::default())),
            SuspendWatcher::new(Box::new(suspend.clone())),
            None,
        );
        assert_eq!(source.borrow().pending(), 0);

        suspend.fire_resume();
        // The resume trigger schedules a start through the shared backend.
        assert!(source.borrow().pending() >= 1);
    }

    // ---- push wiring ------------------------------------------------------

    #[test]
    fn nextcloud_account_with_push_enabled_gets_a_client() {
        let source = fake_source();
        let mut account = sample_account(true);
        account.provider = Provider::Nextcloud;
        account.sync.remote_push_enabled = true;
        let mut runtime =
            AccountRuntime::new(account, NetworkConfig::default(), source, None, false);
        runtime.start_without_watchers();
        runtime.mount_test(
            online_network_watcher(),
            PowerWatcher::new(Box::new(FakePowerProbe::default())),
            SuspendWatcher::new(Box::new(FakeSuspendProbe::default())),
            runtime.default_push_for_test(),
        );
        assert!(runtime.push_client().is_some());
    }

    #[test]
    fn opencloud_account_never_gets_a_push_client() {
        let source = fake_source();
        let mut account = sample_account(true);
        account.provider = Provider::OpenCloud;
        account.sync.remote_push_enabled = true;
        let mut runtime =
            AccountRuntime::new(account, NetworkConfig::default(), source, None, false);
        runtime.start_without_watchers();
        runtime.mount_test(
            online_network_watcher(),
            PowerWatcher::new(Box::new(FakePowerProbe::default())),
            SuspendWatcher::new(Box::new(FakeSuspendProbe::default())),
            runtime.default_push_for_test(),
        );
        assert!(runtime.push_client().is_none());
    }

    #[test]
    fn disabled_push_yields_no_client() {
        let source = fake_source();
        let mut account = sample_account(true);
        account.sync.remote_push_enabled = false;
        let mut runtime =
            AccountRuntime::new(account, NetworkConfig::default(), source, None, false);
        runtime.start_without_watchers();
        runtime.mount_test(
            online_network_watcher(),
            PowerWatcher::new(Box::new(FakePowerProbe::default())),
            SuspendWatcher::new(Box::new(FakeSuspendProbe::default())),
            runtime.default_push_for_test(),
        );
        assert!(runtime.push_client().is_none());
    }

    #[test]
    fn incoming_push_message_requests_a_remote_sync() {
        let source = fake_source();
        let mut runtime = AccountRuntime::new(
            sample_account(true),
            NetworkConfig::default(),
            source.clone(),
            None,
            false,
        );
        runtime.start_without_watchers();
        assert_eq!(source.borrow().pending(), 0);

        // `simulate_remote_push` runs exactly what the NotifyPushClient
        // `on_file_notification` callback runs in production.
        runtime.simulate_remote_push();
        assert!(source.borrow().pending() >= 1);
    }

    // ---- activity log wiring ----------------------------------------------

    #[test]
    fn outcome_log_line_covers_every_outcome_in_english_and_spanish() {
        use crate::core::scheduler::SyncOutcome;
        crate::util::i18n::set_locale(crate::util::i18n::Locale::English);
        for outcome in [
            SyncOutcome::Success,
            SyncOutcome::Conflict,
            SyncOutcome::AuthFailed,
            SyncOutcome::KeyringLocked,
            SyncOutcome::Failed,
        ] {
            let line = outcome_log_line(&outcome);
            assert!(!line.is_empty(), "English label for {outcome:?}");
            assert!(
                line.starts_with("Synchronization"),
                "EN line for {outcome:?}"
            );
        }
        crate::util::i18n::set_locale(crate::util::i18n::Locale::Spanish);
        assert_eq!(
            outcome_log_line(&SyncOutcome::Success),
            "Sincronización completada"
        );
        crate::util::i18n::reset_locale();
    }

    #[test]
    fn activity_line_uses_readable_account_and_folder_identifiers() {
        use crate::core::scheduler::SyncOutcome;
        crate::util::i18n::set_locale(crate::util::i18n::Locale::English);
        let line = activity_line(
            "alice",
            "https://cloud.example.com/",
            "/home/alice/Nextcloud",
            &SyncOutcome::Success,
        );
        assert_eq!(
            line,
            "alice@cloud.example.com · /home/alice/Nextcloud: Synchronization completed"
        );
        assert!(
            !line.contains("  "),
            "single spaces only, no double separator"
        );
        crate::util::i18n::reset_locale();
    }
}
