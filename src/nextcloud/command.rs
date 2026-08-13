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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Argument vector, first element is the executable.
    pub argv: Vec<String>,
    /// Environment overrides (`NC_USER`, `NC_PASSWORD`).
    pub environment: Vec<(String, String)>,
}

impl CommandSpec {
    /// Materialize this spec as a `std::process::Command`.
    pub fn to_command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.argv[0]);
        command.args(&self.argv[1..]);
        command.envs(self.environment.iter().cloned());
        command
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
const CONFLICT_MARKERS: [&str; 3] = ["conflict", "conflicted copy", "csync_exclude_conflict"];

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
        if self.lines.len() == self.max_lines {
            self.lines.pop_front();
        }
        self.lines.push_back(text.to_string());
        let lowered = text.to_lowercase();
        self.authentication_seen |= AUTH_ERROR_MARKERS
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
        }
    }

    fn folder() -> FolderConfig {
        FolderConfig {
            id: "test-folder".to_string(),
            local_root: "/tmp/NextCloud".to_string(),
            remote_path: String::new(),
            space_id: None,
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
}
