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

/// Paths coming from the server must stay inside the synced folder. A href
/// carrying `..` (or an absolute path) would otherwise let a malicious or
/// broken server write outside `local_root`.
pub fn is_safe_rel(rel: &str) -> bool {
    if rel.is_empty() {
        return true;
    }
    if rel.starts_with('/') || rel.contains('\\') || rel.contains('\0') {
        return false;
    }
    rel.split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

#[derive(Clone)]
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
    /// Files at or above this size download in parallel ranges (issue #272).
    pub multipart_threshold: u64,
    /// Parallel range requests per download; one disables the feature.
    pub multipart_streams: u32,
    /// Journal location. Defaults to `<local_root>/.nextsync/state.db`.
    pub db_path: Option<PathBuf>,
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The password must never reach a log line, a panic message or a
        // crash report. Same rule as the app (issue #140).
        f.debug_struct("EngineConfig")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .field("local_root", &self.local_root)
            .field("remote_path", &self.remote_path)
            .field("space_id", &self.space_id)
            .field(
                "trust_invalid_certificates",
                &self.trust_invalid_certificates,
            )
            .field("proxy", &self.proxy)
            .field("max_retries", &self.max_retries)
            .field("sync_hidden_files", &self.sync_hidden_files)
            .field("exclude_patterns", &self.exclude_patterns)
            .field("delete_guard", &self.delete_guard)
            .field("dry_run", &self.dry_run)
            .field("timeout", &self.timeout)
            .field("chunk_threshold", &self.chunk_threshold)
            .field("multipart_threshold", &self.multipart_threshold)
            .field("multipart_streams", &self.multipart_streams)
            .field("db_path", &self.db_path)
            .finish()
    }
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
            multipart_threshold: 64 * 1024 * 1024,
            multipart_streams: default_multipart_streams(),
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

/// Parallel download streams, overridable with `NATIVE_DOWNLOAD_WORKERS`
/// (issue #272). Unparsable values fall back to four and the result is
/// clamped so a typo cannot open a hundred sockets.
fn default_multipart_streams() -> u32 {
    parallel_streams(std::env::var("NATIVE_DOWNLOAD_WORKERS").ok().as_deref())
}

fn parallel_streams(raw: Option<&str>) -> u32 {
    raw.and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(4)
        .clamp(1, 8)
}

#[cfg(test)]
mod multipart_tests {
    use super::parallel_streams;

    #[test]
    fn streams_default_to_four() {
        assert_eq!(parallel_streams(None), 4);
    }

    #[test]
    fn the_environment_can_pick_the_stream_count() {
        assert_eq!(parallel_streams(Some("2")), 2);
        assert_eq!(parallel_streams(Some(" 6 ")), 6);
    }

    #[test]
    fn nonsense_and_extremes_are_clamped() {
        assert_eq!(parallel_streams(Some("many")), 4);
        assert_eq!(parallel_streams(Some("0")), 1);
        assert_eq!(parallel_streams(Some("99")), 8);
    }
}
