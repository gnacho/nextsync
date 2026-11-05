//! `nextcloudcmd` command wrapper.
//!
//! Fase 2 (Task 2.3): builds the `std::process::Command` arguments
//! (`--non-interactive`, `--max-sync-retries`, `-h`, `--silent`, `--trust`,
//! `--httpproxy`, `--exclude`, `--path`) and classifies the process output
//! into the same categories the Python wrapper uses. Mirrors
//! `nextcloud/command.py`.
//!
//! The flag spellings were verified against `nextcloudcmd --version`
//! (Nextcloud 34.0.1, `/usr/bin/nextcloudcmd`): the CLI keeps `-h` as "sync
//! hidden files", `--max-sync-retries [n]` (default 3), `--httpproxy [proxy]`,
//! `--exclude [file]`, `--trust` and `--path`, all with space-separated
//! values. None of the Python assumptions had drifted.

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::nextcloud::driver::{DriverContext, NextcloudDriver, SyncDriver};
use crate::storage::config::{AccountConfig, FolderConfig, NetworkConfig};

/// Name of the wrapped binary (searched on `$PATH`).
pub const BINARY_NAME: &str = "nextcloudcmd";

/// Default line tail kept by [`BoundedOutputCapture`].
pub const DEFAULT_MAX_LINES: usize = 200;

/// Error raised while resolving or building the `nextcloudcmd` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// The binary was not found on `$PATH` (Python `NextcloudCmdMissingError`).
    MissingBinary,
    /// The remote path did not normalize to a valid `--path`/`--remote-folder` value.
    InvalidRemotePath(String),
    /// The OpenCloud account folder has no `space_id` configured.
    MissingSpaceId,
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingBinary => f.write_str(
                "the sync binary is not installed (nextcloudcmd: nextcloud-desktop-cmd; \
                 opencloudcmd: opencloud-desktop-git).",
            ),
            Self::InvalidRemotePath(message) => f.write_str(message),
            Self::MissingSpaceId => {
                f.write_str("OpenCloud folders require a space id to synchronize.")
            }
        }
    }
}

impl std::error::Error for CommandError {}

/// A fully resolved `nextcloudcmd` invocation: argv plus environment.
#[derive(Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Argument vector, first element is the executable.
    pub argv: Vec<String>,
    /// Environment overrides (`NC_USER`, `NC_PASSWORD`).
    pub environment: Vec<(String, String)>,
}

impl std::fmt::Debug for CommandSpec {
    /// Custom `Debug` so secret-bearing environment values never reach logs
    /// (issue #140): the argv is shown in full, but the password/token
    /// variables print as `[REDACTED]`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandSpec")
            .field("argv", &self.argv)
            .field(
                "environment",
                &self
                    .environment
                    .iter()
                    .map(|(key, value)| {
                        let shown = if SECRET_ENV_KEYS.iter().any(|secret| secret == key) {
                            "[REDACTED]".to_string()
                        } else {
                            value.clone()
                        };
                        (key.clone(), shown)
                    })
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Environment variable names whose values are account secrets and must be
/// redacted from `Debug` output (issue #140).
const SECRET_ENV_KEYS: [&str; 3] = ["NC_PASSWORD", "OPENCLOUD_TOKEN", "OPENCLOUD_PASSWORD"];

impl CommandSpec {
    /// Materialize this spec as a `std::process::Command`.
    pub fn to_command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.argv[0]);
        command.args(&self.argv[1..]);
        command.envs(self.environment.iter().cloned());
        command
    }

    /// Materialize this spec with reduced IO/CPU priority when the tools
    /// exist on `$PATH` (issue #39). Returns the plain command otherwise.
    ///
    /// `nextcloudcmd` has no bandwidth flag; lowering the process priority
    /// is the closest portable lever (idle IO class + low CPU niceness).
    pub fn to_command_low_impact(&self) -> std::process::Command {
        let ionice = find_binary("ionice");
        let nice = find_binary("nice");
        match (ionice, nice) {
            (Some(ionice), Some(nice)) => {
                let mut command = std::process::Command::new(&ionice);
                command.arg("-c").arg("3");
                command.arg(&nice);
                command.arg("-n").arg("10");
                command.arg(&self.argv[0]);
                command.args(&self.argv[1..]);
                command.envs(self.environment.iter().cloned());
                command
            }
            _ => self.to_command(),
        }
    }
}

/// Locate an executable on `$PATH` (mirror of `shutil.which`).
pub fn find_binary(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Locate the `nextcloudcmd` executable on `$PATH`.
pub fn find_nextcloudcmd() -> Option<PathBuf> {
    find_binary(BINARY_NAME)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Build the `nextcloudcmd` argument list and environment for one folder pair.
///
/// Mirrors `command.py::build_command`: one invocation per folder
/// (`local_root` + `remote_path`), account settings and network settings. The
/// password never appears on the command line, only in `NC_PASSWORD`.
///
/// This is the Nextcloud-only entry point kept for compatibility: it delegates
/// to [`NextcloudDriver`]. Provider-agnostic callers should ask
/// `driver_for(provider)` for the driver instead (see [`SyncDriver`]).
pub fn build_command(
    account: &AccountConfig,
    folder: &FolderConfig,
    network: &NetworkConfig,
    password: &str,
    exclude_file: Option<&Path>,
    executable: Option<&Path>,
) -> Result<CommandSpec, CommandError> {
    let ctx = DriverContext::from_folder(
        account,
        folder,
        network,
        password.to_string(),
        exclude_file.map(Path::to_path_buf),
        executable.map(Path::to_path_buf),
    );
    NextcloudDriver.build_command(&ctx)
}

/// Text markers that indicate an authentication failure, in lower case.
const AUTH_ERROR_MARKERS: [&str; 6] = [
    "authentication failed",
    "invalid credentials",
    "access forbidden",
    "unauthorized",
    "http error code 401",
    "server replied: unauthorized",
];

/// Text markers that indicate a conflicted copy was created, in lower case.
/// Deliberately no bare "conflict": with progress logging enabled, ordinary
/// file names can contain the word (a test fixture called
/// file_case_conflict.txt does). The discovery instruction token lives in
/// parsed progress lines and reaches the classifier through the explicit
/// conflict signal instead.
const CONFLICT_MARKERS: [&str; 2] = ["conflicted copy", "csync_exclude_conflict"];

/// Lines that mention the auth words but describe something else entirely.
/// The binary disables its push websocket a few seconds into a run when the
/// server's notify_push is unreachable, logging "Disable push notifications
/// object because authentication failed or connection lost"; a long sync
/// still running at that moment would otherwise classify as rejected
/// credentials. Real auth failures never match these shapes.
const AUTH_NOISE_PATTERNS: [&str; 1] = ["disable push notifications object"];

/// Outcome categories of a finished `nextcloudcmd` run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// The server rejected the account credentials.
    Authentication,
    /// The run succeeded but produced conflicted copies.
    Conflict,
    /// The run succeeded without conflicts.
    Success,
    /// The run failed for any other reason.
    SyncError,
}

impl Classification {
    /// Fold the engine's explicit conflict signal (parsed progress lines
    /// with the discovery instruction token or a created conflicted copy)
    /// into this classification: a signal upgrades a plain success to
    /// Conflict and never downgrades anything.
    #[must_use]
    pub const fn with_conflict_signal(self, signal: bool) -> Self {
        if signal && matches!(self, Self::Success) {
            Self::Conflict
        } else {
            self
        }
    }

    /// Stable machine-readable name (Python `BoundedOutputCapture` labels).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::Conflict => "conflict",
            Self::Success => "success",
            Self::SyncError => "sync_error",
        }
    }
}

/// Classify a command stream while retaining only a small diagnostic tail.
///
/// Mirrors `command.py::BoundedOutputCapture`: keeps the last `max_lines`
/// lines and remembers whether authentication or conflict markers appeared.
#[derive(Debug, Clone)]
pub struct BoundedOutputCapture {
    max_lines: usize,
    lines: VecDeque<String>,
    authentication_seen: bool,
    conflict_seen: bool,
}

impl BoundedOutputCapture {
    /// Create a capture keeping at most `max_lines` lines (`max_lines >= 1`).
    pub fn new(max_lines: usize) -> Self {
        assert!(max_lines >= 1, "max_lines must be positive");
        Self {
            max_lines,
            lines: VecDeque::new(),
            authentication_seen: false,
            conflict_seen: false,
        }
    }

    /// Feed one output line, updating the tail and the classification flags.
    pub fn feed(&mut self, text: &str) {
        let lowered = text.to_lowercase();
        // Auth-shaped noise (push websocket teardown) never counts as an
        // authentication signal.
        let auth_noise = AUTH_NOISE_PATTERNS
            .iter()
            .any(|pattern| lowered.contains(pattern));
        if self.lines.len() == self.max_lines {
            self.lines.pop_front();
        }
        self.lines.push_back(text.to_string());
        self.authentication_seen |= !auth_noise
            && AUTH_ERROR_MARKERS
                .iter()
                .any(|marker| lowered.contains(marker));
        self.conflict_seen |= CONFLICT_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker));
    }

    /// The retained diagnostic tail (`"\n"`-joined, no trailing newline).
    pub fn output(&self) -> String {
        let mut out = String::new();
        for (index, line) in self.lines.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push_str(line);
        }
        out
    }

    /// Number of retained lines.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether no line is retained.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Classify a finished run from its exit code and the seen markers.
    pub fn classification(&self, exit_code: i32) -> Classification {
        if self.authentication_seen {
            Classification::Authentication
        } else if exit_code == 0 && self.conflict_seen {
            Classification::Conflict
        } else if exit_code == 0 {
            Classification::Success
        } else {
            Classification::SyncError
        }
    }
}

/// Classify a single output string (helper mirroring `classify_output`).
pub fn classify_output(output: &str, exit_code: i32) -> Classification {
    let mut capture = BoundedOutputCapture::new(1);
    capture.feed(output);
    capture.classification(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nextcloud::driver::Provider;
    use crate::storage::config::{DeleteGuardConfig, RuntimeConfig, SyncConfig};

    fn account() -> AccountConfig {
        AccountConfig {
            server_url: "https://cloud.example.com".to_string(),
            login_name: "alice".to_string(),
            authentication_type: "manual".to_string(),
            provider: Provider::default(),
            folders: vec![folder()],
            sync: SyncConfig {
                max_sync_retries: 3,
                detailed_output: true,
                ..SyncConfig::default()
            },
            delete_guard: DeleteGuardConfig::default(),
            runtime: RuntimeConfig::default(),
            id: "test-account".to_string(),
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

    #[test]
    fn secret_is_only_in_environment() {
        let spec = build_command(
            &account(),
            &folder(),
            &NetworkConfig::default(),
            "very-secret",
            None,
            Some(Path::new("/bin/true")),
        )
        .expect("build should succeed");
        assert!(!spec.argv.iter().any(|arg| arg.contains("very-secret")));
        let password = spec
            .environment
            .iter()
            .find(|(key, _)| key == "NC_PASSWORD")
            .expect("NC_PASSWORD should be set");
        assert_eq!(password.1, "very-secret");
        assert!(spec.argv.iter().any(|arg| arg == "--non-interactive"));
        assert!(spec.argv.iter().any(|arg| arg == "-h"));
    }

    #[test]
    fn debug_redacts_secret_environment_values() {
        // Issue #140: formatting a spec for logs must never echo the secret.
        let spec = build_command(
            &account(),
            &folder(),
            &NetworkConfig::default(),
            "very-secret",
            None,
            Some(Path::new("/bin/true")),
        )
        .expect("build should succeed");
        let rendered = format!("{spec:?}");
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains("very-secret"));
        // The argv is still readable (the binary path matters for debugging).
        assert!(rendered.contains("/bin/true"));
    }

    #[test]
    fn options_map_without_shell() {
        let mut account = account();
        account.sync = SyncConfig {
            max_sync_retries: 7,
            detailed_output: false,
            ..SyncConfig::default()
        };
        let network = NetworkConfig {
            custom_proxy: Some("http://proxy:8080".to_string()),
            trust_invalid_certificates: true,
            reduce_transfer_impact: false,
            allowed_ssids: None,
        };
        let spec = build_command(
            &account,
            &folder(),
            &network,
            "secret",
            None,
            Some(Path::new("/bin/true")),
        )
        .expect("build should succeed");
        assert!(spec.argv.iter().any(|arg| arg == "--silent"));
        assert!(spec.argv.iter().any(|arg| arg == "--trust"));
        assert!(spec.argv.iter().any(|arg| arg == "--httpproxy"));
        assert!(spec.argv.iter().any(|arg| arg == "http://proxy:8080"));
        assert!(spec.argv.iter().any(|arg| arg == "--max-sync-retries"));
        assert!(spec.argv.iter().any(|arg| arg == "7"));
        assert!(!spec.argv.iter().any(|arg| arg == "--path"));
    }

    #[test]
    fn remote_path_adds_path_argument() {
        let mut folder = folder();
        folder.remote_path = "/Documents".to_string();
        let spec = build_command(
            &account(),
            &folder,
            &NetworkConfig::default(),
            "secret",
            None,
            Some(Path::new("/bin/true")),
        )
        .expect("build should succeed");
        let path_index = spec
            .argv
            .iter()
            .position(|arg| arg == "--path")
            .expect("--path should be present");
        assert_eq!(spec.argv[path_index + 1], "/Documents");
        let local_index = spec.argv.iter().position(|arg| arg == "/tmp/NextCloud");
        let server_index = spec
            .argv
            .iter()
            .position(|arg| arg == "https://cloud.example.com");
        assert!(local_index.is_some() && server_index.is_some());
        assert!(path_index < local_index.unwrap());
        assert!(path_index < server_index.unwrap());
    }

    #[test]
    fn root_remote_path_omits_path_argument() {
        for raw in ["", "/"] {
            let mut folder = folder();
            folder.remote_path = raw.to_string();
            let spec = build_command(
                &account(),
                &folder,
                &NetworkConfig::default(),
                "secret",
                None,
                Some(Path::new("/bin/true")),
            )
            .expect("build should succeed");
            assert!(!spec.argv.iter().any(|arg| arg == "--path"));
        }
    }

    #[test]
    fn exclude_file_and_ordering() {
        let spec = build_command(
            &account(),
            &folder(),
            &NetworkConfig::default(),
            "secret",
            Some(Path::new("/tmp/excludes.lst")),
            Some(Path::new("/bin/true")),
        )
        .expect("build should succeed");
        let exclude_index = spec
            .argv
            .iter()
            .position(|arg| arg == "--exclude")
            .expect("--exclude should be present");
        assert_eq!(spec.argv[exclude_index + 1], "/tmp/excludes.lst");
    }

    #[test]
    fn first_positional_arguments_follow_all_flags() {
        let spec = build_command(
            &account(),
            &folder(),
            &NetworkConfig::default(),
            "secret",
            None,
            Some(Path::new("/bin/true")),
        )
        .expect("build should succeed");
        let local_index = spec
            .argv
            .iter()
            .position(|arg| arg == "/tmp/NextCloud")
            .expect("local root should be present");
        let server_index = spec
            .argv
            .iter()
            .position(|arg| arg == "https://cloud.example.com")
            .expect("server url should be present");
        assert_eq!(server_index, local_index + 1);
        assert!(local_index > 0);
    }

    #[test]
    fn result_classification() {
        assert_eq!(classify_output("", 0), Classification::Success);
        assert_eq!(
            classify_output("authentication failed", 4),
            Classification::Authentication
        );
        assert_eq!(
            classify_output("created conflicted copy", 0),
            Classification::Conflict
        );
        assert_eq!(
            classify_output("server failure", 5),
            Classification::SyncError
        );
        assert_eq!(Classification::Success.as_str(), "success");
        assert_eq!(Classification::SyncError.as_str(), "sync_error");
    }

    #[test]
    fn push_teardown_noise_is_not_authentication() {
        // Real line from a long run against a server with unreachable
        // notify_push: it lands seconds in, mid-sync, and mentions the auth
        // words. Long folders (huge trees) are still running then and were
        // classified as rejected credentials; short ones finished first and
        // passed, which made it look random.
        let line =
            "Disable push notifications object because authentication failed or connection lost";
        assert_eq!(classify_output(line, 0), Classification::Success);
        // The genuine marker still trips.
        assert_eq!(
            classify_output("authentication failed", 0),
            Classification::Authentication
        );
    }

    #[test]
    fn authentication_wins_over_conflict_and_exit_code() {
        let mut capture = BoundedOutputCapture::new(20);
        capture.feed("authentication failed");
        capture.feed("created conflicted copy: /x");
        assert_eq!(capture.classification(0), Classification::Authentication);
        assert_eq!(capture.classification(4), Classification::Authentication);
    }

    #[test]
    fn large_output_keeps_a_bounded_tail_without_losing_classification() {
        let mut capture = BoundedOutputCapture::new(20);
        capture.feed("authentication failed");
        for index in 0..100_000 {
            capture.feed(&format!("ordinary output line {index}"));
        }
        assert_eq!(capture.classification(4), Classification::Authentication);
        assert!(!capture.output().contains("ordinary output line 0\n"));
        assert_eq!(capture.len(), 20);
        assert_eq!(capture.output().split('\n').count(), 20);
    }

    #[test]
    fn output_is_joined_without_trailing_newline() {
        let mut capture = BoundedOutputCapture::new(5);
        capture.feed("first");
        capture.feed("second");
        assert_eq!(capture.output(), "first\nsecond");
    }

    #[test]
    fn command_spec_materializes_into_command() {
        let spec = CommandSpec {
            argv: vec!["/bin/true".to_string(), "--trust".to_string()],
            environment: vec![("NC_USER".to_string(), "alice".to_string())],
        };
        let command = spec.to_command();
        assert_eq!(command.get_program().to_str(), Some("/bin/true"));
        let args: Vec<&str> = command
            .get_args()
            .map(|arg| arg.to_str().expect("argv is utf-8"))
            .collect();
        assert_eq!(args, vec!["--trust"]);
    }

    #[test]
    fn low_impact_wraps_with_ionice_and_nice_when_available() {
        let spec = CommandSpec {
            argv: vec!["/bin/syncengine".to_string(), "--flag".to_string()],
            environment: vec![("NC_PASSWORD".to_string(), "secret".to_string())],
        };
        let command = spec.to_command_low_impact();
        let program = command.get_program().to_string_lossy().to_string();
        // Either the wrapper chain (ionice found) or the plain argv fallback:
        // both must preserve the original argv and environment.
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        if program.ends_with("ionice") {
            assert!(args.windows(2).any(|pair| pair == ["-c", "3"]));
            assert!(args.iter().any(|arg| arg.ends_with("nice")));
            assert!(args.windows(2).any(|pair| pair == ["-n", "10"]));
            assert!(args.iter().any(|arg| arg == "/bin/syncengine"));
            assert!(args.iter().any(|arg| arg == "--flag"));
        } else {
            assert_eq!(program, "/bin/syncengine");
            assert_eq!(args, vec!["--flag".to_string()]);
        }
    }
}
