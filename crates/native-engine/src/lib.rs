//! Native sync engine for Nextcloud and OpenCloud.
//!
//! Replaces `nextcloudcmd` / `opencloudcmd` with a Rust implementation that
//! speaks WebDAV directly, keeps its own SQLite journal and reports per-file
//! progress through a sink.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

pub mod db;
pub mod engine;
pub mod http;
pub mod local;
pub mod reconcile;
pub mod transfer;

pub use db::{DbEntry, StateDb};
pub use engine::{Engine, SyncSummary};
pub use http::HttpClient;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Nextcloud,
    OpenCloud,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Nextcloud => "nextcloud",
            Provider::OpenCloud => "opencloud",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeleteGuardConfig {
    pub enabled: bool,
    pub max_delete_ratio: f64,
    pub min_group: usize,
}

impl Default for DeleteGuardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_delete_ratio: 0.2,
            min_group: 5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub provider: Provider,
    /// Scheme, host and port, no trailing slash, e.g. `https://cloud.example.com`.
    pub base_url: String,
    pub username: String,
    pub password: String,
    pub local_root: PathBuf,
    /// Path inside the account, e.g. `/Docs` or empty for the account root.
    pub remote_path: String,
    /// OpenCloud space id. Ignored by Nextcloud.
    pub space_id: Option<String>,
    pub trust_invalid_certificates: bool,
    pub proxy: Option<String>,
    pub max_retries: u32,
    pub sync_hidden_files: bool,
    pub exclude_patterns: Vec<String>,
    pub delete_guard: DeleteGuardConfig,
    pub dry_run: bool,
    pub timeout: Duration,
    /// Files at or above this size use chunked upload (Nextcloud chunked v2,
    /// OpenCloud TUS).
    pub chunk_threshold: u64,
    /// Journal location. Defaults to `<local_root>/.nextsync/state.db`.
    pub db_path: Option<PathBuf>,
}

impl EngineConfig {
    pub fn new(
        provider: Provider,
        base_url: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
        local_root: impl Into<PathBuf>,
        remote_path: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            username: username.into(),
            password: password.into(),
            local_root: local_root.into(),
            remote_path: remote_path.into(),
            space_id: None,
            trust_invalid_certificates: false,
            proxy: None,
            max_retries: 3,
            sync_hidden_files: false,
            exclude_patterns: Vec::new(),
            delete_guard: DeleteGuardConfig::default(),
            dry_run: false,
            timeout: Duration::from_secs(60),
            chunk_threshold: 10 * 1024 * 1024,
            db_path: None,
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.db_path
            .clone()
            .unwrap_or_else(|| self.local_root.join(".nextsync").join("state.db"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitState {
    Success,
    Conflict,
    AuthFailed,
    Failed,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub action: String,
    pub path: String,
    pub processed: u32,
    /// Bytes already transferred for this file.
    pub bytes_done: u64,
    /// Total bytes of the file being transferred, 0 when unknown.
    pub bytes_total: u64,
}

impl Progress {
    pub fn new(action: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            path: path.into(),
            processed: 0,
            bytes_done: 0,
            bytes_total: 0,
        }
    }

    /// Attach byte counters so the UI can show copied and remaining size.
    pub fn with_bytes(mut self, done: u64, total: u64) -> Self {
        self.bytes_done = done;
        self.bytes_total = total;
        self
    }
}

pub trait ProgressSink: Send + Sync {
    fn event(&self, progress: Progress);
}

/// Sink that drops every event. Useful for tests and CLI runs.
pub struct NullSink;

impl ProgressSink for NullSink {
    fn event(&self, _progress: Progress) {}
}

#[derive(Debug, Clone)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum Error {
    Auth(String),
    NotFound(String),
    Http {
        status: u16,
        url: String,
        body: String,
    },
    Transport(String),
    Io(std::io::Error),
    Db(rusqlite::Error),
    Xml(String),
    Cancelled,
    Safety(String),
    Config(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Auth(m) => write!(f, "authentication failed: {m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
            Error::Http { status, url, body } => {
                write!(f, "http {status} for {url}: {body}")
            }
            Error::Transport(m) => write!(f, "transport error: {m}"),
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Db(e) => write!(f, "database error: {e}"),
            Error::Xml(m) => write!(f, "invalid xml: {m}"),
            Error::Cancelled => write!(f, "cancelled"),
            Error::Safety(m) => write!(f, "safety abort: {m}"),
            Error::Config(m) => write!(f, "configuration error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e)
    }
}

impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::Timeout(_) => Error::Transport("timeout".to_string()),
            ureq::Error::Io(io) => Error::Transport(io.to_string()),
            other => Error::Transport(other.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
