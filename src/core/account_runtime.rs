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

use crate::core::debounce::TimeoutSource;
use crate::core::delete_guard::DeleteGuard;
use crate::core::scheduler::{DeleteAlert, Scheduler, SyncRunner};
use crate::core::sync_permit::SyncPermit;
use crate::nextcloud::sync_engine::{SyncEngine, SyncProgress};
use crate::state::{AggregateStateController, AppState, StateController};
use crate::storage::config::{AccountConfig, Config, FolderConfig, NetworkConfig};

/// One running synchronization runtime for a single folder pair.
pub struct FolderRuntime {
    pub folder: FolderConfig,
    state: StateController,
    scheduler: Scheduler,
    timers: crate::core::scheduler::SyncTimers,
    progress_rx: Option<async_channel::Receiver<SyncProgress>>,
}

impl Clone for FolderRuntime {
    fn clone(&self) -> Self {
        // The clone keeps the live state and scheduler (shared by Rc); the
        // interval timers are rebuilt unarmed and the progress receiver is not
        // moved — the original runtime owns it.
        Self {
            folder: self.folder.clone(),
            state: self.state.clone(),
            scheduler: self.scheduler.clone(),
            timers: self.scheduler.timers(),
            progress_rx: None,
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
        }
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
        );
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
}

impl AccountRuntime {
    /// Create a runtime for one account. No folders are started yet; call
    /// [`start`](Self::start) or [`sync_folders`](Self::sync_folders).
    pub fn new(
        account: AccountConfig,
        network: NetworkConfig,
        source: Rc<RefCell<dyn TimeoutSource>>,
        sync_permit: Option<SyncPermit>,
    ) -> Self {
        Self {
            account,
            folders: HashMap::new(),
            aggregate: AggregateStateController::new(),
            idle: None,
            source,
            sync_permit,
            network,
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

    /// Start runtimes for every configured folder of this account.
    pub fn start(&mut self) {
        self.sync_folders();
        if self.folders.is_empty() && self.idle.is_none() {
            let idle = StateController::new(AppState::IdleOk);
            idle.set(AppState::IdleOk, "Connected. Add folders from Settings.");
            self.aggregate.add(idle.clone());
            self.idle = Some(idle);
        }
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

    /// Stop every folder runtime and reset the aggregate.
    pub fn stop(&mut self) {
        for (_, mut runtime) in self.folders.drain() {
            runtime.stop();
        }
        if let Some(idle) = self.idle.take() {
            self.aggregate.remove(&idle);
        }
        self.aggregate.clear();
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
        }
    }

    /// Start a runtime for every account in the configuration.
    pub fn start(&mut self, config: &Config) {
        for account in config.accounts.clone() {
            self.ensure_runtime(account);
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
        );
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
mod tests {
    use super::*;
    use crate::core::debounce::{fire_timer, FakeTimeoutSource};
    use crate::state::StateSnapshot;

    fn fake_source() -> Rc<RefCell<FakeTimeoutSource>> {
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
            }];
        }
        account
    }

    #[test]
    fn account_without_folders_aggregates_to_idle_ok() {
        let source = fake_source();
        let mut runtime = AccountRuntime::new(
            sample_account(false),
            NetworkConfig::default(),
            source,
            None,
        );
        runtime.start();
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
        );
        runtime.start();
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
        let mut runtime =
            AccountRuntime::new(sample_account(true), NetworkConfig::default(), source, None);
        runtime.start();
        runtime.scheduler().set_paused(true);
        assert!(runtime.scheduler().user_paused());
        assert_eq!(runtime.state().snapshot().state, AppState::PausedUser);
    }

    #[test]
    fn sync_folders_starts_and_removes_runtimes() {
        let source = fake_source();
        let mut runtime =
            AccountRuntime::new(sample_account(true), NetworkConfig::default(), source, None);
        runtime.start();
        assert_eq!(runtime.folders().len(), 1);

        let mut account = sample_account(true);
        account.folders.push(FolderConfig {
            id: "folder-2".to_string(),
            local_root: "/tmp/nsync-folder-2".to_string(),
            remote_path: "/photos".to_string(),
            space_id: None,
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
        let mut runtime =
            AccountRuntime::new(sample_account(true), NetworkConfig::default(), source, None);
        runtime.start();
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
}
