//! Sync engine.
//!
//! Fase 2 (Task 2.3): spawns the provider's sync binary (resolved through the
//! [`SyncDriver`] of `account.provider`), drains stdout and stderr in
//! parallel threads (anti-deadlock on the 64 KB pipe, a spike finding),
//! forwards progress lines to a channel and reports the [`SyncOutcome`] to
//! the scheduler. The engine never branches on the provider: the driver
//! produces the [`CommandSpec`] and the progress parser is shared (both
//! binaries are forks of the same Qt codebase).
//!
//! Mirrors `core/sync_engine.py`: stdout drives live progress, both streams
//! feed a bounded diagnostic tail, and the final classification
//! (authentication / conflict / success / error) maps to the scheduler
//! outcomes.
//!
//! Threading: [`SyncRunner::start`] returns immediately. The heavy work
//! (credential lookup, spawn, draining) runs on the Gio blocking thread pool
//! through [`gio::spawn_blocking`]; its result is handed to a
//! `glib::spawn_future_local` future on the main context, which invokes
//! `on_finished`. The callback therefore always fires on the main thread
//! (the scheduler's `SyncRunner` contract), never synchronously from inside
//! `start`, and the main thread is never blocked.
//!
//! Progress delivery: [`SyncEngine::new`] takes an
//! `async_channel::Sender<SyncProgress>`. Connect its receiver on the main
//! loop (e.g. `glib::spawn_future_local`) and forward events to
//! `StateController::set_progress`; the scheduler clears progress itself when
//! the run finishes.

pub use crate::nextcloud::nextcloudcmd_progress::{
    describe_progress, parse_progress_line, SyncProgress,
};

use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::core::scheduler::{SyncOutcome, SyncRunner};
use crate::core::triggers::Trigger;
use crate::nextcloud::api::{ApiError, NextcloudApi};
use crate::nextcloud::command::{BoundedOutputCapture, Classification, DEFAULT_MAX_LINES};
use crate::nextcloud::credentials::CredentialsStore;
use crate::nextcloud::driver::{driver_for, DriverContext, Provider};
use crate::storage::config::{AccountConfig, FolderConfig, NetworkConfig};
use crate::util::redact::Redactor;

/// Result of looking up the account password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialLookup {
    /// A usable password was retrieved.
    Found(String),
    /// The keyring has no item for the account.
    Missing,
    /// The keyring is locked and could not answer.
    Locked,
    /// The lookup failed for any other reason.
    Unavailable,
}

impl CredentialLookup {
    /// Whether the failure is transient infrastructure trouble (issue #85):
    /// the session bus or the secret service is not ready yet, the
    /// collection is locked, or the agent answered something unexpected.
    /// Retrying later is meaningful; nothing here says the password is bad.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            CredentialLookup::Locked | CredentialLookup::Unavailable
        )
    }
}

/// Supplies the account password to the engine.
///
/// Kept behind a trait so tests can inject deterministic lookups instead of
/// talking to the Secret Service.
pub trait CredentialSource: Send + Sync + 'static {
    /// Resolve the password for the given account.
    fn lookup(&self, account: &AccountConfig) -> CredentialLookup;
}

/// [`CredentialSource`] backed by the desktop Secret Service.
pub struct KeyringCredentialSource;

impl CredentialSource for KeyringCredentialSource {
    fn lookup(&self, account: &AccountConfig) -> CredentialLookup {
        match CredentialsStore::get_for_account(
            &account.id,
            &account.server_url,
            &account.login_name,
        ) {
            Ok(Some(password)) => CredentialLookup::Found(password),
            Ok(None) => CredentialLookup::Missing,
            Err(crate::nextcloud::credentials::CredentialError::Service(
                secret_service::Error::Locked,
            )) => CredentialLookup::Locked,
            Err(_) => CredentialLookup::Unavailable,
        }
    }
}

/// Outcome of a finished `nextcloudcmd` run, mirroring `SyncResult`.
#[derive(Debug, Clone)]
pub struct SyncResult {
    /// Process exit code (`1` when the process was killed by a signal).
    pub exit_code: i32,
    /// Wall time of the run.
    pub duration: Duration,
    /// Retained diagnostic tail of stdout+stderr (last lines).
    pub output: String,
    /// Stream classification (authentication / conflict / success / error).
    pub classification: Classification,
}

impl SyncResult {
    /// Whether the process exited successfully.
    pub fn successful(&self) -> bool {
        self.exit_code == 0
    }

    /// Map the classification onto the scheduler's [`SyncOutcome`].
    pub fn outcome(&self) -> SyncOutcome {
        match self.classification {
            Classification::Authentication => SyncOutcome::AuthFailed,
            Classification::Conflict => SyncOutcome::Conflict,
            Classification::Success => SyncOutcome::Success,
            Classification::SyncError => SyncOutcome::Failed,
        }
    }
}

/// Creates the remote folder before a sync when one is configured.
///
/// Production installs [`ProductionRemoteEnsurer`] (WebDAV MKCOL through
/// [`NextcloudApi`](crate::nextcloud::api::NextcloudApi)); tests leave it
/// unset to stay hermetic.
pub type RemoteEnsurer =
    Arc<dyn Fn(&AccountConfig, &FolderConfig, &str) -> Result<(), ApiError> + Send + Sync>;

/// Confirms the account server is alive and answering HTTP.
///
/// Issue #179: a `Failed` outcome alone cannot distinguish a broken folder
/// from an unreachable server (a reverse proxy answering 502, a backend that
/// stopped responding). When a run fails, the engine asks this probe "is the
/// server actually up?"; a dead answer upgrades the outcome to
/// [`SyncOutcome::NetworkError`] so the scheduler parks the account Offline
/// instead of retrying a dead server forever.
pub type HealthProbe = Arc<dyn Fn(&AccountConfig, &str) -> Result<(), ApiError> + Send + Sync>;

/// Reads the root ETag of a folder to decide whether a periodic interval
/// reconciliation can be skipped (issue #189).
///
/// Returns `Ok(Some(etag))` when the server reported the folder's ETag,
/// `Ok(None)` when it is absent, and `Err` on transport/auth problems. A
/// mismatch against the recorded value means the remote tree changed; an
/// error means "do not skip" (a real change might go unseen).
pub type EtagProbe = Arc<
    dyn Fn(&AccountConfig, &FolderConfig, &str) -> Result<Option<String>, ApiError> + Send + Sync,
>;

/// Production [`EtagProbe`]: a `PROPFIND Depth:0` for `<getetag/>` on the
/// folder root (mirrors the official client's `RequestEtagJob`).
#[derive(Default)]
pub struct ProductionEtagProbe;

impl ProductionEtagProbe {
    /// Read the folder root ETag for one folder pair.
    pub fn run(
        account: &AccountConfig,
        folder: &FolderConfig,
        password: &str,
    ) -> Result<Option<String>, ApiError> {
        NextcloudApi::new().root_etag(
            &account.server_url,
            &account.login_name,
            password,
            &folder.remote_path,
        )
    }
}

/// Production [`HealthProbe`]: a short GET to the server's status endpoint.
///
/// Nextcloud answers `/status.php`; OpenCloud answers `/` with a status
/// payload. Any 2xx/3xx means the server is up; a 5xx (proxy dead, backend
/// down) or a transport error means it is not. Auth responses (401/403) are
/// not part of the health probe - the account's own credential flow reports
/// those.
#[derive(Default)]
pub struct ProductionHealthProbe;

impl ProductionHealthProbe {
    /// Run the health check for one account.
    pub fn run(account: &AccountConfig) -> Result<(), ApiError> {
        NextcloudApi::new().server_status(&account.server_url)
    }
}

/// Production [`RemoteEnsurer`]: MKCOL the folder's remote path before the
/// engine runs. Nextcloud creates it under the per-user files tree; OpenCloud
/// under the folder's space (issue #55; both verified against real
/// deployments). Root targets (empty remote path) stay a no-op.
#[derive(Default)]
pub struct ProductionRemoteEnsurer;

impl ProductionRemoteEnsurer {
    /// Run the ensure step for one folder pair.
    pub fn run(
        account: &AccountConfig,
        folder: &FolderConfig,
        password: &str,
    ) -> Result<(), ApiError> {
        let remote = folder.remote_path.trim_matches('/');
        if remote.is_empty() {
            return Ok(());
        }
        match account.provider {
            Provider::Nextcloud => NextcloudApi::new().ensure_remote_folder(
                &account.server_url,
                &account.login_name,
                password,
                &folder.remote_path,
            ),
            Provider::OpenCloud => {
                let Some(space_id) = folder.space_id.as_deref().map(str::trim) else {
                    return Ok(());
                };
                if space_id.is_empty() {
                    return Ok(());
                }
                NextcloudApi::new().ensure_opencloud_folder(
                    &account.server_url,
                    &account.login_name,
                    password,
                    space_id,
                    &folder.remote_path,
                )
            }
        }
    }
}

/// Spawns and drains `nextcloudcmd`, reporting [`SyncProgress`] and the final
/// [`SyncOutcome`]. Implements the scheduler's [`SyncRunner`].
pub struct SyncEngine {
    account: AccountConfig,
    folder: FolderConfig,
    network: NetworkConfig,
    exclude_file: Option<PathBuf>,
    executable: Option<PathBuf>,
    progress: async_channel::Sender<SyncProgress>,
    credentials: Arc<dyn CredentialSource>,
    process: Arc<Mutex<Option<Child>>>,
    remote_ensurer: Option<RemoteEnsurer>,
    health_probe: Option<HealthProbe>,
    /// Issue #189: last observed root ETag of this folder, shared between the
    /// main thread (captured when a run starts) and the worker. Comparing it
    /// against a fresh `PROPFIND` lets the periodic remote-interval skip a
    /// full `nextcloudcmd` reconciliation when nothing changed.
    etag_slot: Arc<Mutex<Option<String>>>,
    /// Issue #189: reads the folder root ETag before a periodic interval run.
    etag_probe: Option<EtagProbe>,
    /// Issue #195: called on the main thread when a periodic interval run
    /// records a new root ETag for this folder, so the caller can persist it
    /// across restarts (avoiding the first-run re-scan). Best-effort: the
    /// engine never blocks on this.
    on_etag_change: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

impl SyncEngine {
    /// Create an engine for one folder pair.
    ///
    /// `progress` carries parsed progress events; connect the receiver on the
    /// main loop and forward to `StateController::set_progress`. `executable`
    /// overrides the `nextcloudcmd` lookup (used by tests).
    pub fn new(
        account: AccountConfig,
        folder: FolderConfig,
        network: NetworkConfig,
        exclude_file: Option<PathBuf>,
        executable: Option<PathBuf>,
        progress: async_channel::Sender<SyncProgress>,
    ) -> Self {
        // Issue #195: seed the ETag slot from the persisted value so the first
        // periodic interval after a restart skips the reconciliation when the
        // remote tree is unchanged (avoids the full re-scan).
        let folder_id = folder.id.clone();
        let seeded_etag = crate::core::etag_store::read_etag(&folder_id);
        Self {
            account,
            folder,
            network,
            exclude_file,
            executable,
            progress,
            credentials: Arc::new(KeyringCredentialSource),
            process: Arc::new(Mutex::new(None)),
            remote_ensurer: None,
            health_probe: None,
            etag_slot: Arc::new(Mutex::new(seeded_etag)),
            etag_probe: None,
            on_etag_change: None,
        }
    }

    /// Replace the credential source (used by tests).
    pub fn with_credentials(mut self, credentials: Arc<dyn CredentialSource>) -> Self {
        self.credentials = credentials;
        self
    }

    /// Install the remote-folder ensure step (production wiring).
    pub fn with_remote_ensurer(mut self, ensurer: RemoteEnsurer) -> Self {
        self.remote_ensurer = Some(ensurer);
        self
    }

    /// Install the server health probe (issue #179). Without it a Failed run
    /// is never upgraded to NetworkError.
    pub fn with_health_probe(mut self, probe: HealthProbe) -> Self {
        self.health_probe = Some(probe);
        self
    }

    /// Install the folder ETag probe (issue #189). Without it the periodic
    /// remote-interval always reconciles (no skip).
    pub fn with_etag_probe(mut self, probe: EtagProbe) -> Self {
        self.etag_probe = Some(probe);
        self
    }

    /// Install a callback invoked (best-effort) when a periodic interval run
    /// records a new root ETag (issue #195). The caller persists it so a
    /// restart does not re-scan a folder whose remote tree is unchanged.
    pub fn with_on_etag_change(mut self, callback: Arc<dyn Fn(String) + Send + Sync>) -> Self {
        self.on_etag_change = Some(callback);
        self
    }

    /// Whether a reconciliation is currently running.
    pub fn is_running(&self) -> bool {
        self.process
            .lock()
            .expect("process mutex poisoned")
            .is_some()
    }
}

impl SyncRunner for SyncEngine {
    fn start(&mut self, reasons: &[Trigger], on_finished: Box<dyn FnOnce(SyncOutcome) + 'static>) {
        let account_id = self.account.id.clone();
        let account = self.account.clone();
        let folder = self.folder.clone();
        let network = self.network.clone();
        let exclude_file = self.exclude_file.clone();
        let executable = self.executable.clone();
        let progress = self.progress.clone();
        let credentials = Arc::clone(&self.credentials);
        let process = Arc::clone(&self.process);
        let remote_ensurer = self.remote_ensurer.clone();
        let health_probe = self.health_probe.clone();
        let etag_slot = Arc::clone(&self.etag_slot);
        let etag_probe = self.etag_probe.clone();
        let on_etag_change = self.on_etag_change.clone();
        let inputs = EngineInputs {
            account,
            folder,
            network,
            exclude_file,
            executable,
            remote_ensurer,
            health_probe,
            reasons: reasons.to_vec(),
            etag_slot,
            etag_probe,
            on_etag_change,
        };
        glib::spawn_future_local(async move {
            let run =
                gio::spawn_blocking(move || engine_thread(inputs, progress, credentials, process))
                    .await;
            let run = match run {
                Ok(run) => run,
                Err(_) => EngineRun::Direct(SyncOutcome::Failed),
            };
            match run {
                EngineRun::Result(result) => {
                    let outcome = result.outcome();
                    // Issue #178: a proven-dead password must leave the
                    // in-memory cache so the next lookup re-reads the
                    // keyring (e.g. after the user signs in again).
                    if matches!(outcome, SyncOutcome::AuthFailed) {
                        CredentialsStore::invalidate(&account_id);
                    }
                    on_finished(outcome);
                }
                EngineRun::Direct(outcome) => {
                    if matches!(outcome, SyncOutcome::AuthFailed) {
                        CredentialsStore::invalidate(&account_id);
                    }
                    on_finished(outcome);
                }
            }
        });
    }

    fn cancel(&mut self) {
        // `Child::kill` sends SIGKILL (the Python used SIGTERM with a hard
        // kill fallback). Best effort: the sync journal uses SQLite WAL, so a
        // killed engine reconciles cleanly on the next run.
        if let Some(child) = self
            .process
            .lock()
            .expect("process mutex poisoned")
            .as_mut()
        {
            let _ = child.kill();
        }
    }
}

/// What the blocking half produced for the main thread to deliver.
enum EngineRun {
    /// A real process ran to completion.
    Result(SyncResult),
    /// The run ended before (or without) a process: direct outcome.
    Direct(SyncOutcome),
}

/// Everything the blocking thread needs to build the command.
struct EngineInputs {
    account: AccountConfig,
    folder: FolderConfig,
    network: NetworkConfig,
    exclude_file: Option<PathBuf>,
    executable: Option<PathBuf>,
    remote_ensurer: Option<RemoteEnsurer>,
    health_probe: Option<HealthProbe>,
    /// Issue #189: the triggers that requested this run (used to decide whether
    /// the ETag gate applies, i.e. a periodic interval that may be skipped).
    reasons: Vec<Trigger>,
    /// Issue #189: shared last-observed root ETag (see [`SyncEngine::etag_slot`]).
    etag_slot: Arc<Mutex<Option<String>>>,
    /// Issue #189: reads the folder root ETag before a periodic interval run.
    etag_probe: Option<EtagProbe>,
    /// Issue #195: best-effort callback to persist a newly recorded ETag.
    on_etag_change: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

/// Run the whole reconciliation on the blocking thread pool.
fn engine_thread(
    inputs: EngineInputs,
    progress: async_channel::Sender<SyncProgress>,
    credentials: Arc<dyn CredentialSource>,
    process: Arc<Mutex<Option<Child>>>,
) -> EngineRun {
    let started = Instant::now();
    let password = match credentials.lookup(&inputs.account) {
        CredentialLookup::Found(password) => password,
        // Transient secret-service trouble (bus not ready at startup, locked
        // collection, agent hiccup) must not arm the credential gate
        // (issue #85): report the keyring as locked so the periodic triggers
        // retry instead of parking the account in needs-attention. Only a
        // truly missing item keeps the auth-rejected meaning.
        CredentialLookup::Locked | CredentialLookup::Unavailable => {
            return EngineRun::Direct(SyncOutcome::KeyringLocked)
        }
        CredentialLookup::Missing => return EngineRun::Direct(SyncOutcome::NoCredentials),
    };
    // Scrub the known secret from the captured output: nextcloudcmd can echo
    // the password/token on its diagnostics, and the structural passes alone
    // would leave it verbatim in the run tail (issue #126).
    let redactor = Arc::new(Redactor::from_secrets([password.clone()]));
    // `nextcloudcmd` exits 1 with no output when the remote folder does not
    // exist; create it (and its parents) first. Auth rejection surfaces as
    // such; anything else falls through and lets nextcloudcmd report.
    // Issue #162: a transport failure (the server itself does not answer)
    // must not launch nextcloudcmd against an unreachable host. Return a
    // NetworkError outcome so the scheduler marks this folder Offline and the
    // account stops reading as Connected.
    if let Some(ensure) = inputs.remote_ensurer.as_ref() {
        match ensure(&inputs.account, &inputs.folder, &password) {
            Ok(()) => {}
            Err(ApiError::AuthRejected) => return EngineRun::Direct(SyncOutcome::AuthFailed),
            Err(ApiError::Transport) => return EngineRun::Direct(SyncOutcome::NetworkError),
            // Issue #179: a 5xx from the server/proxy (a reverse proxy
            // answering 502 because the backend is down) is not a folder
            // problem - it is the account being unreachable. Confirm with a
            // health probe before launching nextcloudcmd; a dead probe means
            // the server is not answering and the run must not proceed.
            Err(ApiError::Http { status }) if (500..600).contains(&status) => {
                if let Some(probe) = inputs.health_probe.as_ref() {
                    if probe(&inputs.account, &password).is_err() {
                        return EngineRun::Direct(SyncOutcome::NetworkError);
                    }
                }
            }
            Err(_) => {}
        }
    }
    // Issue #189: for a pure periodic remote-interval run, a cheap root-ETag
    // check tells us whether the remote tree changed at all. If it has not,
    // skip the whole `nextcloudcmd` reconciliation (it scans the trees and
    // emits a huge number of progress events for nothing). This is exactly
    // what the official client does with `RequestEtagJob`/`Folder::etagRetrieved`:
    // only a changed ETag triggers a full sync. Non-interval runs (manual,
    // inotify, startup, remote push, network-restored, resume, retry, local
    // recovery) always reconcile.
    if etag_gate_applies(&inputs.reasons) {
        if let Some(probe) = inputs.etag_probe.as_ref() {
            let fresh = probe(&inputs.account, &inputs.folder, &password);
            if let Ok(Some(fresh_etag)) = fresh {
                let previous = inputs.etag_slot.lock().unwrap().clone();
                if previous.as_deref() == Some(fresh_etag.as_str()) {
                    // Nothing changed remotely: report a clean success and keep
                    // the recorded ETag so the next interval also skips.
                    return EngineRun::Direct(SyncOutcome::Success);
                }
                // Changed (or first run): record the new ETag and reconcile.
                *inputs.etag_slot.lock().unwrap() = Some(fresh_etag.clone());
                // Issue #195: persist best-effort so the next restart can skip
                // the first no-change reconciliation.
                crate::core::etag_store::write_etag(&inputs.folder.id, &fresh_etag);
                // Issue #195: also surface the change to an optional callback.
                if let Some(callback) = inputs.on_etag_change.as_ref() {
                    callback(fresh_etag);
                }
            }
            // Ok(None) or Err(_): the server did not answer or the ETag is
            // unavailable - do NOT skip the reconciliation (a real change
            // might be missed).
        }
    }
    let driver = driver_for(inputs.account.provider);
    let ctx = DriverContext::from_folder(
        &inputs.account,
        &inputs.folder,
        &inputs.network,
        password.clone(),
        inputs.exclude_file.clone(),
        inputs.executable.clone(),
    );
    let spec = match driver.build_command(&ctx) {
        Ok(spec) => spec,
        Err(_) => return EngineRun::Direct(SyncOutcome::Failed),
    };
    let mut command = if inputs.network.reduce_transfer_impact {
        spec.to_command_low_impact()
    } else {
        spec.to_command()
    };
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    // Under systemd Qt sends its debug output straight to the journal via
    // sd_journal_send, bypassing the pipes, and without the journal those
    // debug categories are off by default. Force text logging and enable
    // exactly the two categories that carry per-file progress; the rest stay
    // off so benign lines (a file named "file_case_conflict.txt", a push
    // notification probe saying "authentication failed") cannot trip the
    // output classifiers.
    command.env("QT_FORCE_STDERR_LOGGING", "1");
    command.env(
        "QT_LOGGING_RULES",
        "nextcloud.sync.discovery.debug=true;nextcloud.sync.propagator.debug=true",
    );
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return EngineRun::Direct(SyncOutcome::Failed),
    };
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    *process.lock().expect("process mutex poisoned") = Some(child);
    let capture = Arc::new(Mutex::new(BoundedOutputCapture::new(DEFAULT_MAX_LINES)));
    let processed = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let conflict_signal = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stdout_handle = drain_stream(
        stdout,
        progress.clone(),
        Arc::clone(&capture),
        Arc::clone(&processed),
        Arc::clone(&conflict_signal),
        Arc::clone(&redactor),
    );
    let stderr_handle = drain_stream(
        stderr,
        progress,
        capture.clone(),
        processed.clone(),
        conflict_signal.clone(),
        redactor,
    );
    let exit_code = wait_for_exit(&process);
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();
    // Ops diagnostics: how many progress events the run produced (visible in
    // the user journal through the app's stderr).
    eprintln!(
        "[nextsync] progress events parsed: {}",
        processed.load(std::sync::atomic::Ordering::Relaxed)
    );
    let output = capture.lock().expect("capture mutex poisoned").output();
    let classification = capture
        .lock()
        .expect("capture mutex poisoned")
        .classification(exit_code)
        .with_conflict_signal(conflict_signal.load(std::sync::atomic::Ordering::Relaxed));
    let result = SyncResult {
        exit_code,
        duration: started.elapsed(),
        output,
        classification,
    };
    // Issue #179: a run that failed while the server does not answer a health
    // probe means the account is unreachable (a proxy answering 502, the
    // backend gone), not that the folder is broken. Upgrade to NetworkError so
    // the scheduler parks the folder Offline and stops retrying a dead server
    // on every trigger. A live probe keeps the folder-level Failed.
    if result.classification == Classification::SyncError {
        if let Some(probe) = inputs.health_probe.as_ref() {
            if probe(&inputs.account, &password).is_err() {
                return EngineRun::Direct(SyncOutcome::NetworkError);
            }
        }
    }
    EngineRun::Result(result)
}

/// Whether the request reasons consist solely of the periodic remote interval
/// (issue #189). Only then is the root-ETag gate applied; user-triggered and
/// change-driven runs (inotify, startup, remote push, manual, recovery) must
/// always reconcile, otherwise a real change could be missed.
fn etag_gate_applies(reasons: &[Trigger]) -> bool {
    matches!(reasons, [Trigger::RemoteInterval])
}

/// Drain one process stream: redact, retain the tail and emit parsed
/// progress. Both streams carry progress under `QT_FORCE_STDERR_LOGGING`
/// (transfers on stderr, summaries on stdout), so both parse; the
/// operation counter is shared across them.
fn drain_stream(
    stream: impl Read + Send + 'static,
    progress: async_channel::Sender<SyncProgress>,
    capture: Arc<Mutex<BoundedOutputCapture>>,
    processed: Arc<std::sync::atomic::AtomicU32>,
    conflict_signal: Arc<std::sync::atomic::AtomicBool>,
    redactor: Arc<Redactor>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let line = redactor.redact(&line);
            match parse_progress_line(&line) {
                Some(parsed) => {
                    let processed =
                        processed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    let _ = progress.try_send(SyncProgress {
                        action: parsed.action,
                        path: parsed.path,
                        processed,
                    });
                    // The real conflict signal for a run lives in the parsed
                    // lines: the discovery instruction token and the
                    // conflicted-copy file names the propagator creates.
                    if line.contains("CSYNC_INSTRUCTION_CONFLICT")
                        || line.contains("(conflicted copy ")
                    {
                        conflict_signal.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    // Progress lines stay out of the diagnostic tail: the
                    // discovery flood (14k+ lines on a first sync) would
                    // drown the end-of-run markers, and their file names
                    // could trip the classifiers (a file literally named
                    // "file_case_conflict.txt" exists in the wild).
                }
                None => {
                    capture.lock().expect("capture mutex poisoned").feed(&line);
                }
            }
        }
    })
}

/// Poll the child until it exits and return its code (`1` when signalled).
fn wait_for_exit(process: &Arc<Mutex<Option<Child>>>) -> i32 {
    loop {
        {
            let mut guard = process.lock().expect("process mutex poisoned");
            let Some(child) = guard.as_mut() else {
                return 1;
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    *guard = None;
                    return status.code().unwrap_or(1);
                }
                Ok(None) => {}
                Err(_) => return 1,
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use crate::nextcloud::driver::Provider;

    /// Credential source with a fixed answer.
    struct FakeCredentials(CredentialLookup);

    impl CredentialSource for FakeCredentials {
        fn lookup(&self, _account: &AccountConfig) -> CredentialLookup {
            self.0.clone()
        }
    }

    fn account() -> AccountConfig {
        AccountConfig {
            id: "test-account".to_string(),
            server_url: "https://cloud.example.com".to_string(),
            login_name: "alice".to_string(),
            authentication_type: "manual".to_string(),
            provider: Default::default(),
            folders: Vec::new(),
            sync: Default::default(),
            delete_guard: Default::default(),
            runtime: Default::default(),
            custom_proxy: None,
            trust_invalid_certificates: false,
        }
    }

    fn folder() -> FolderConfig {
        FolderConfig {
            id: "test-folder".to_string(),
            local_root: "/tmp/NextCloud".to_string(),
            remote_path: String::new(),
            space_id: None,
            size_confirmed: false,
        }
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Run one reconciliation with its own isolated main context, so parallel
    /// tests never share (and drop each other's) GLib sources.
    fn run_engine(
        mut engine: SyncEngine,
        progress_rx: &async_channel::Receiver<SyncProgress>,
    ) -> (SyncOutcome, Vec<SyncProgress>) {
        let context = glib::MainContext::new();
        let (outcome_tx, outcome_rx) = std::sync::mpsc::channel();
        let outcome = context
            .with_thread_default(|| {
                engine.start(
                    &[Trigger::Manual],
                    Box::new(move |outcome| {
                        let _ = outcome_tx.send(outcome);
                    }),
                );
                pump_outcome(&context, &outcome_rx)
            })
            .expect("the test main context is available");
        let mut events = Vec::new();
        while let Ok(event) = progress_rx.try_recv() {
            events.push(event);
        }
        (outcome, events)
    }

    /// Iterate a main context until the outcome arrives.
    fn pump_outcome(
        context: &glib::MainContext,
        outcome_rx: &std::sync::mpsc::Receiver<SyncOutcome>,
    ) -> SyncOutcome {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Ok(outcome) = outcome_rx.try_recv() {
                return outcome;
            }
            let _ = context.iteration(false);
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("timed out waiting for the engine outcome");
    }

    /// Run an engine with a periodic remote-interval reason (the only trigger
    /// the ETag gate applies to), returning the outcome. A skipped run still
    /// resolves (success) without ever spawning `nextcloudcmd`.
    fn run_engine_interval(
        mut engine: SyncEngine,
        progress_rx: &async_channel::Receiver<SyncProgress>,
    ) -> SyncOutcome {
        let context = glib::MainContext::new();
        let (outcome_tx, outcome_rx) = std::sync::mpsc::channel();
        let outcome = context
            .with_thread_default(|| {
                engine.start(
                    &[Trigger::RemoteInterval],
                    Box::new(move |outcome| {
                        let _ = outcome_tx.send(outcome);
                    }),
                );
                pump_outcome(&context, &outcome_rx)
            })
            .expect("the test main context is available");
        while progress_rx.try_recv().is_ok() {}
        outcome
    }

    #[test]
    fn emits_progress_and_reports_success() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "fake-nextcloudcmd",
            "#!/bin/sh\n\
             echo 'Downloading: /home/user/NextCloud/a.pdf'\n\
             echo 'Uploading: docs/report.txt'\n\
             echo 'Synced  : /home/user/NextCloud/file.txt'\n\
             exit 0\n",
        );
        let (progress_tx, progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(script),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))));
        let (outcome, events) = run_engine(engine, &progress_rx);
        assert_eq!(outcome, SyncOutcome::Success);
        let actions: Vec<&str> = events.iter().map(|event| event.action.as_str()).collect();
        assert_eq!(actions, vec!["download", "upload", "synced"]);
        assert_eq!(events[0].path, "/home/user/NextCloud/a.pdf");
        let counters: Vec<u32> = events.iter().map(|event| event.processed).collect();
        assert_eq!(counters, vec![1, 2, 3]);
    }

    #[test]
    fn authentication_failure_maps_to_auth_failed() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "fake-auth-fail",
            "#!/bin/sh\n\
             echo 'authentication failed' >&2\n\
             exit 4\n",
        );
        let (progress_tx, progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(script),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))));
        let (outcome, events) = run_engine(engine, &progress_rx);
        assert_eq!(outcome, SyncOutcome::AuthFailed);
        assert!(events.is_empty());
    }

    #[test]
    fn conflicted_copy_reports_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "fake-conflict",
            "#!/bin/sh\n\
             echo 'Created conflicted copy of /x' \n\
             exit 0\n",
        );
        let (progress_tx, progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(script),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))));
        let (outcome, events) = run_engine(engine, &progress_rx);
        assert_eq!(outcome, SyncOutcome::Conflict);
        assert!(events.is_empty());
    }

    #[test]
    fn missing_binary_maps_to_failed() {
        let (progress_tx, progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some("/nonexistent/nextcloudcmd".into()),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))));
        let (outcome, _events) = run_engine(engine, &progress_rx);
        assert_eq!(outcome, SyncOutcome::Failed);
    }

    #[test]
    fn locked_keyring_maps_to_keyring_locked() {
        let (progress_tx, progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            None,
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Locked)));
        let (outcome, events) = run_engine(engine, &progress_rx);
        assert_eq!(outcome, SyncOutcome::KeyringLocked);
        assert!(events.is_empty());
    }

    #[test]
    fn unreachable_secret_service_maps_to_keyring_locked() {
        // Issue #85: at startup the session bus may not be ready yet; that
        // is transient infrastructure trouble, not rejected credentials,
        // so it must not arm the auth gate.
        let (progress_tx, progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            None,
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Unavailable)));
        let (outcome, events) = run_engine(engine, &progress_rx);
        assert_eq!(outcome, SyncOutcome::KeyringLocked);
        assert!(events.is_empty());
    }

    #[test]
    fn missing_credential_maps_to_no_credentials() {
        let (progress_tx, progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            None,
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Missing)));
        let (outcome, events) = run_engine(engine, &progress_rx);
        assert_eq!(outcome, SyncOutcome::NoCredentials);
        assert!(events.is_empty());
    }

    #[test]
    fn opencloud_provider_syncs_with_space_id() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "fake-opencloudcmd",
            "#!/bin/sh\n\
             [ -n \"$OPENCLOUD_TOKEN\" ] || exit 3\n\
             [ \"$1\" = \"https://cloud.example.com\" ] || exit 4\n\
             [ \"$2\" = \"space:abcd\" ] || exit 5\n\
             [ \"$3\" = \"/tmp/NextCloud/\" ] || exit 6\n\
             [ \"$4\" = \"--user\" ] || exit 7\n\
             [ \"$5\" = \"alice\" ] || exit 8\n\
             echo 'Downloading: /home/user/NextCloud/a.pdf'\n\
             exit 0\n",
        );
        let mut account = account();
        account.provider = Provider::OpenCloud;
        let mut folder = folder();
        folder.space_id = Some("space:abcd".to_string());
        let (progress_tx, progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account,
            folder,
            NetworkConfig::default(),
            None,
            Some(script),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))));
        let (outcome, events) = run_engine(engine, &progress_rx);
        assert_eq!(outcome, SyncOutcome::Success);
        assert_eq!(events[0].action, "download");
    }

    #[test]
    fn engine_output_has_the_password_redacted() {
        // Issue #126: the engine can echo the password/token on its own
        // diagnostics; the run tail must scrub it with the known-secret
        // redactor, not only the structural passes.
        let capture = Arc::new(Mutex::new(BoundedOutputCapture::new(DEFAULT_MAX_LINES)));
        let processed = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let conflict_signal = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (progress_tx, progress_rx) = async_channel::unbounded();
        let redactor = Arc::new(Redactor::from_secrets(["s3cr3t-t0ken-here"]));
        let line = "Downloading using token s3cr3t-t0ken-here\n".to_string();
        let handle = drain_stream(
            std::io::Cursor::new(line),
            progress_tx,
            Arc::clone(&capture),
            Arc::clone(&processed),
            Arc::clone(&conflict_signal),
            Arc::clone(&redactor),
        );
        handle.join().expect("drain thread joined");
        let output = capture.lock().expect("capture mutex poisoned").output();
        assert!(!output.contains("s3cr3t-t0ken-here"));
        assert!(output.contains("[REDACTED]"));
        assert!(progress_rx.try_recv().is_err());
    }

    #[test]
    fn opencloud_missing_space_id_maps_to_failed() {
        let mut account = account();
        account.provider = Provider::OpenCloud;
        let (progress_tx, progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account,
            folder(),
            NetworkConfig::default(),
            None,
            Some("/bin/true".into()),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))));
        let (outcome, events) = run_engine(engine, &progress_rx);
        assert_eq!(outcome, SyncOutcome::Failed);
        assert!(events.is_empty());
    }

    #[test]
    fn cancel_kills_a_long_running_process() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "fake-slow", "#!/bin/sh\nexec sleep 30\n");
        let (progress_tx, progress_rx) = async_channel::unbounded();
        let mut engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(script),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))));
        let context = glib::MainContext::new();
        let (outcome_tx, outcome_rx) = std::sync::mpsc::channel();
        let outcome = context
            .with_thread_default(|| {
                engine.start(
                    &[Trigger::Manual],
                    Box::new(move |outcome| {
                        let _ = outcome_tx.send(outcome);
                    }),
                );
                let deadline = Instant::now() + Duration::from_secs(5);
                while !engine.is_running() && Instant::now() < deadline {
                    let _ = context.iteration(false);
                    std::thread::sleep(Duration::from_millis(20));
                }
                assert!(engine.is_running(), "the process should have started");
                engine.cancel();
                pump_outcome(&context, &outcome_rx)
            })
            .expect("the test main context is available");
        assert_eq!(outcome, SyncOutcome::Failed);
        assert!(!engine.is_running());
        let _ = progress_rx;
    }

    #[test]
    fn outcome_maps_each_classification() {
        let cases = [
            (Classification::Success, SyncOutcome::Success),
            (Classification::Conflict, SyncOutcome::Conflict),
            (Classification::Authentication, SyncOutcome::AuthFailed),
            (Classification::SyncError, SyncOutcome::Failed),
        ];
        for (classification, expected) in cases {
            let result = SyncResult {
                exit_code: 0,
                duration: Duration::ZERO,
                output: String::new(),
                classification,
            };
            assert_eq!(result.outcome(), expected);
            assert!(result.successful());
        }
        let failed = SyncResult {
            exit_code: 3,
            duration: Duration::ZERO,
            output: String::new(),
            classification: Classification::SyncError,
        };
        assert!(!failed.successful());
    }

    #[test]
    fn reexports_match_the_parser_module() {
        assert_eq!(
            parse_progress_line("Downloading: /x")
                .expect("parses")
                .action,
            "download"
        );
        let progress = SyncProgress::new("upload", "/a");
        assert_eq!(describe_progress(Some(&progress)), "upload: /a");
    }

    #[test]
    fn remote_ensurer_auth_rejection_short_circuits_to_auth_failed() {
        // The fake "binary" is /bin/false: if the engine spawned it, the run
        // would end Failed. An AuthRejected ensurer must prevent the spawn
        // entirely and classify the outcome as AuthFailed.
        let (progress_tx, _progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(PathBuf::from("/bin/false")),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))))
        .with_remote_ensurer(Arc::new(|_account, _folder, _password| {
            Err(ApiError::AuthRejected)
        }));
        let (outcome, _) = run_engine(engine, &async_channel::unbounded().1);
        assert_eq!(outcome, SyncOutcome::AuthFailed);
    }

    /// Issue #178: when a run proves the password dead, the cached copy must
    /// be dropped so the next lookup re-reads the keyring instead of
    /// replaying the stale secret (e.g. after the user signs in again).
    #[test]
    fn auth_failed_outcome_invalidates_the_cached_password() {
        let account = account();
        crate::nextcloud::credentials::seed_cache_for_tests(&account.id, "stale-secret");
        let (progress_tx, _progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account,
            folder(),
            NetworkConfig::default(),
            None,
            Some(PathBuf::from("/bin/false")),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "stale-secret".to_string(),
        ))))
        .with_remote_ensurer(Arc::new(|_account, _folder, _password| {
            Err(ApiError::AuthRejected)
        }));
        let (outcome, _) = run_engine(engine, &async_channel::unbounded().1);
        assert_eq!(outcome, SyncOutcome::AuthFailed);
        assert!(crate::nextcloud::credentials::cached_for_tests("test-account").is_none());
    }

    /// Issue #162: a transport failure (the server itself does not answer)
    /// must short-circuit to NetworkError without spawning nextcloudcmd. The
    /// fake binary is /bin/false, so if the engine spawned it the run would
    /// end Failed instead; Transport must prevent the spawn entirely.
    #[test]
    fn remote_ensurer_transport_failure_short_circuits_to_network_error() {
        let (progress_tx, _progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(PathBuf::from("/bin/false")),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))))
        .with_remote_ensurer(Arc::new(|_account, _folder, _password| {
            Err(ApiError::Transport)
        }));
        let (outcome, _) = run_engine(engine, &async_channel::unbounded().1);
        assert_eq!(outcome, SyncOutcome::NetworkError);
    }

    #[test]
    fn remote_ensurer_non_auth_error_does_not_block_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "fake-ensure-ok", "#!/bin/sh\nexit 0\n");
        let (progress_tx, _progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(script),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))))
        .with_remote_ensurer(Arc::new(|_account, _folder, _password| {
            Err(ApiError::Http { status: 500 })
        }));
        let (outcome, _) = run_engine(engine, &async_channel::unbounded().1);
        assert_eq!(outcome, SyncOutcome::Success);
    }

    /// Issue #179: a 5xx from the remote ensurer with a server that does not
    /// answer a health probe must short-circuit to NetworkError WITHOUT
    /// launching nextcloudcmd (a dead server should not be spawned against).
    /// The fake binary writes a marker file, so its presence would prove the
    /// engine ran; a dead health probe must prevent the spawn entirely.
    #[test]
    fn ensurer_http_5xx_with_dead_health_probe_short_circuits_to_network_error() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("engine-ran");
        let script = write_script(
            dir.path(),
            "fake-engine-5xx-dead",
            &format!("#!/bin/sh\ntouch {}\nexit 0\n", marker.display()),
        );
        let (progress_tx, _progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(script),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))))
        .with_remote_ensurer(Arc::new(|_account, _folder, _password| {
            Err(ApiError::Http { status: 502 })
        }))
        .with_health_probe(Arc::new(|_account, _password| Err(ApiError::Transport)));
        let (outcome, _) = run_engine(engine, &async_channel::unbounded().1);
        assert_eq!(outcome, SyncOutcome::NetworkError);
        assert!(!marker.exists(), "nextcloudcmd must not be spawned");
    }

    /// Issue #179: a 5xx from the ensurer with a live health probe is a
    /// folder-specific error, not a connectivity failure - the run proceeds.
    #[test]
    fn ensurer_http_5xx_with_live_health_probe_keeps_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "fake-ensure-5xx-live", "#!/bin/sh\nexit 0\n");
        let (progress_tx, _progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(script),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))))
        .with_remote_ensurer(Arc::new(|_account, _folder, _password| {
            Err(ApiError::Http { status: 500 })
        }))
        .with_health_probe(Arc::new(|_account, _password| Ok(())));
        let (outcome, _) = run_engine(engine, &async_channel::unbounded().1);
        assert_eq!(outcome, SyncOutcome::Success);
    }

    /// Issue #189: an unchanged root ETag on a periodic interval skips the
    /// reconciliation - `nextcloudcmd` is never spawned and the run reports a
    /// clean success (the folder is already up to date).
    #[test]
    fn unchanged_etag_skips_the_interval_run() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("engine-ran");
        let script = write_script(
            dir.path(),
            "fake-engine-etag-skip",
            &format!("#!/bin/sh\ntouch {}\nexit 0\n", marker.display()),
        );
        let (etag_tx, _etag_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(script),
            etag_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))))
        .with_etag_probe(Arc::new(|_account, _folder, _password| {
            Ok(Some("\"abc\"".to_string()))
        }));
        // Seed the slot with the same ETag the probe returns (a prior run
        // recorded it), then a pure interval must not reconcile.
        *engine.etag_slot.lock().unwrap() = Some("\"abc\"".to_string());
        let outcome = run_engine_interval(engine, &async_channel::unbounded().1);
        assert_eq!(outcome, SyncOutcome::Success);
        assert!(!marker.exists(), "nextcloudcmd must not be spawned");
    }

    /// Issue #189: a changed root ETag (or no recorded ETag yet, e.g. first
    /// run) must reconcile.
    #[test]
    fn changed_etag_reconciles_the_interval_run() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("engine-ran");
        let script = write_script(
            dir.path(),
            "fake-engine-etag-changed",
            &format!("#!/bin/sh\ntouch {}\nexit 0\n", marker.display()),
        );
        let (etag_tx, _etag_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(script),
            etag_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))))
        .with_etag_probe(Arc::new(|_account, _folder, _password| {
            Ok(Some("\"new-etag\"".to_string()))
        }));
        // Previous recorded ETag differs from the fresh one -> must sync.
        *engine.etag_slot.lock().unwrap() = Some("\"old-etag\"".to_string());
        let outcome = run_engine_interval(engine, &async_channel::unbounded().1);
        assert_eq!(outcome, SyncOutcome::Success);
        assert!(marker.exists(), "nextcloudcmd must be spawned");
    }

    /// Issue #189: the gate only applies to a pure remote-interval run; a
    /// manual run must always reconcile even if the ETag is unchanged.
    #[test]
    fn manual_run_ignores_the_etag_gate() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("engine-ran");
        let script = write_script(
            dir.path(),
            "fake-engine-etag-manual",
            &format!("#!/bin/sh\ntouch {}\nexit 0\n", marker.display()),
        );
        let (etag_tx, _etag_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(script),
            etag_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))))
        .with_etag_probe(Arc::new(|_account, _folder, _password| {
            Ok(Some("\"abc\"".to_string()))
        }));
        *engine.etag_slot.lock().unwrap() = Some("\"abc\"".to_string());
        let (outcome, _) = run_engine(engine, &async_channel::unbounded().1); // Manual
        assert_eq!(outcome, SyncOutcome::Success);
        assert!(marker.exists(), "manual run must reconcile");
    }

    /// Issue #179: a run that ends Failed while the server does not answer a
    /// health probe must be reported as NetworkError (the account is
    /// unreachable, not the folder broken).
    #[test]
    fn failed_run_with_dead_health_probe_becomes_network_error() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "fake-fail", "#!/bin/sh\nexit 1\n");
        let (progress_tx, _progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(script),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))))
        .with_health_probe(Arc::new(|_account, _password| Err(ApiError::Transport)));
        let (outcome, _) = run_engine(engine, &async_channel::unbounded().1);
        assert_eq!(outcome, SyncOutcome::NetworkError);
    }

    /// Issue #179: a Failed run with a live server stays a folder error.
    #[test]
    fn failed_run_with_live_health_probe_stays_failed() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "fake-fail-live", "#!/bin/sh\nexit 1\n");
        let (progress_tx, _progress_rx) = async_channel::unbounded();
        let engine = SyncEngine::new(
            account(),
            folder(),
            NetworkConfig::default(),
            None,
            Some(script),
            progress_tx,
        )
        .with_credentials(Arc::new(FakeCredentials(CredentialLookup::Found(
            "secret".to_string(),
        ))))
        .with_health_probe(Arc::new(|_account, _password| Ok(())));
        let (outcome, _) = run_engine(engine, &async_channel::unbounded().1);
        assert_eq!(outcome, SyncOutcome::Failed);
    }

    #[test]
    fn production_ensurer_skips_root_and_spaceless_targets() {
        // Root-of-account (Nextcloud) and root-of-space (OpenCloud) targets
        // are no-ops: neither touches the network.
        let mut root_folder = folder();
        root_folder.remote_path = String::new();
        assert!(ProductionRemoteEnsurer::run(&account(), &root_folder, "pw").is_ok());
        let mut opencloud = account();
        opencloud.provider = Provider::OpenCloud;
        assert!(ProductionRemoteEnsurer::run(&opencloud, &root_folder, "pw").is_ok());
        // An OpenCloud folder without a space id cannot be ensured either.
        let mut spaceless = folder();
        spaceless.remote_path = "/cloud".to_string();
        spaceless.space_id = None;
        assert!(ProductionRemoteEnsurer::run(&opencloud, &spaceless, "pw").is_ok());
    }

    #[test]
    fn production_ensurer_hits_a_real_server_for_nextcloud_paths() {
        // cloud.example.com does not resolve: a network-touching run must
        // surface a Transport error (proof the MKCOL path is reached).
        let mut remote = folder();
        remote.remote_path = "/docs".to_string();
        assert!(matches!(
            ProductionRemoteEnsurer::run(&account(), &remote, "pw"),
            Err(ApiError::Transport)
        ));
    }

    #[test]
    fn production_ensurer_hits_a_real_server_for_opencloud_subpaths() {
        // Same proof for the OpenCloud branch: a non-empty remote path with
        // a space id must reach the network (Transport here).
        let mut opencloud = account();
        opencloud.provider = Provider::OpenCloud;
        let mut remote = folder();
        remote.remote_path = "/cloud".to_string();
        remote.space_id = Some("7d443b01$9bc084a7".to_string());
        assert!(matches!(
            ProductionRemoteEnsurer::run(&opencloud, &remote, "pw"),
            Err(ApiError::Transport)
        ));
    }
}
