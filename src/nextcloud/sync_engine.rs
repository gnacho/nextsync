//! Sync engine.
//!
//! Fase 2 (Task 2.3): spawns `nextcloudcmd`, drains stdout and stderr in
//! parallel threads (anti-deadlock on the 64 KB pipe, a spike finding),
//! forwards progress lines to a channel and reports the [`SyncOutcome`] to
//! the scheduler.
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
use crate::nextcloud::command::{
    build_command, BoundedOutputCapture, Classification, DEFAULT_MAX_LINES,
};
use crate::nextcloud::credentials::CredentialsStore;
use crate::storage::config::{AccountConfig, FolderConfig, NetworkConfig};
use crate::util::redact::Redact;

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
        match CredentialsStore::get(&account.id) {
            Ok(Some(password)) => CredentialLookup::Found(password),
            Ok(None) => CredentialLookup::Missing,
            Err(secret_service::Error::Locked) => CredentialLookup::Locked,
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
        Self {
            account,
            folder,
            network,
            exclude_file,
            executable,
            progress,
            credentials: Arc::new(KeyringCredentialSource),
            process: Arc::new(Mutex::new(None)),
        }
    }

    /// Replace the credential source (used by tests).
    pub fn with_credentials(mut self, credentials: Arc<dyn CredentialSource>) -> Self {
        self.credentials = credentials;
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
    fn start(&mut self, _reasons: &[Trigger], on_finished: Box<dyn FnOnce(SyncOutcome) + 'static>) {
        let account = self.account.clone();
        let folder = self.folder.clone();
        let network = self.network.clone();
        let exclude_file = self.exclude_file.clone();
        let executable = self.executable.clone();
        let progress = self.progress.clone();
        let credentials = Arc::clone(&self.credentials);
        let process = Arc::clone(&self.process);
        let inputs = EngineInputs {
            account,
            folder,
            network,
            exclude_file,
            executable,
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
                EngineRun::Result(result) => on_finished(result.outcome()),
                EngineRun::Direct(outcome) => on_finished(outcome),
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
        CredentialLookup::Locked => return EngineRun::Direct(SyncOutcome::KeyringLocked),
        CredentialLookup::Missing | CredentialLookup::Unavailable => {
            return EngineRun::Direct(SyncOutcome::AuthFailed);
        }
    };
    let spec = match build_command(
        &inputs.account,
        &inputs.folder,
        &inputs.network,
        &password,
        inputs.exclude_file.as_deref(),
        inputs.executable.as_deref(),
    ) {
        Ok(spec) => spec,
        Err(_) => return EngineRun::Direct(SyncOutcome::Failed),
    };
    let mut command = spec.to_command();
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return EngineRun::Direct(SyncOutcome::Failed),
    };
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    *process.lock().expect("process mutex poisoned") = Some(child);
    let capture = Arc::new(Mutex::new(BoundedOutputCapture::new(DEFAULT_MAX_LINES)));
    let stdout_handle = drain_stdout(stdout, progress, Arc::clone(&capture));
    let stderr_handle = drain_stderr(stderr, Arc::clone(&capture));
    let exit_code = wait_for_exit(&process);
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();
    let output = capture.lock().expect("capture mutex poisoned").output();
    let classification = capture
        .lock()
        .expect("capture mutex poisoned")
        .classification(exit_code);
    let result = SyncResult {
        exit_code,
        duration: started.elapsed(),
        output,
        classification,
    };
    EngineRun::Result(result)
}

/// Drain the process stdout: redact, retain the tail and emit parsed progress.
fn drain_stdout(
    stream: impl Read + Send + 'static,
    progress: async_channel::Sender<SyncProgress>,
    capture: Arc<Mutex<BoundedOutputCapture>>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stream);
        let mut processed: u32 = 0;
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let line = Redact::redact_line(&line);
            capture.lock().expect("capture mutex poisoned").feed(&line);
            if let Some(parsed) = parse_progress_line(&line) {
                processed += 1;
                let _ = progress.try_send(SyncProgress {
                    action: parsed.action,
                    path: parsed.path,
                    processed,
                });
            }
        }
    })
}

/// Drain the process stderr: redact and retain the tail, no progress.
fn drain_stderr(
    stream: impl Read + Send + 'static,
    capture: Arc<Mutex<BoundedOutputCapture>>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let line = Redact::redact_line(&line);
            capture.lock().expect("capture mutex poisoned").feed(&line);
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
            folders: Vec::new(),
            sync: Default::default(),
            delete_guard: Default::default(),
            runtime: Default::default(),
        }
    }

    fn folder() -> FolderConfig {
        FolderConfig {
            id: "test-folder".to_string(),
            local_root: "/tmp/NextCloud".to_string(),
            remote_path: String::new(),
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
    fn missing_credential_maps_to_auth_failed() {
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
        assert_eq!(outcome, SyncOutcome::AuthFailed);
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
}
