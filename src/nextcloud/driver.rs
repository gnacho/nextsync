//! Sync driver abstraction.
//!
//! Task 0.4: a thin layer that turns an account/folder pair into the exact
//! CLI invocation for the provider's sync binary. `NextcloudDriver` wraps
//! `nextcloudcmd` (credentials via `NC_USER`/`NC_PASSWORD`), `OpenCloudDriver`
//! wraps `opencloudcmd` (credentials via `--user` + the `OPENCLOUD_TOKEN`
//! environment variable). The rest of the engine only talks to the
//! [`SyncDriver`] trait, so it never needs to know which provider a run uses.
//!
//! # Token handling
//!
//! `opencloudcmd` accepts its token either as `--token <value>` or through the
//! `OPENCLOUD_TOKEN` environment variable. The CLI itself resolves it as
//! `QByteArray token = qgetenv("OPENCLOUD_TOKEN")` in `CmdOptions` and then, in
//! `parseOptions`, uses the `--token` flag only when it was set, otherwise
//! printing `Token not set` when the environment is also empty
//! (`opencloud-eu/desktop` `src/cmd/cmd.cpp`). We always use the environment
//! variable so the secret never appears in `argv` (and therefore never in
//! `/proc/<pid>/cmdline`); `--user` is passed as a flag, as the CLI requires.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::nextcloud::command::{find_binary, CommandError, CommandSpec};
use crate::storage::config::{normalize_remote_path, AccountConfig, FolderConfig, NetworkConfig};

/// The sync provider an account is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// The `nextcloudcmd` sync engine (the historical default).
    #[default]
    Nextcloud,
    /// The `opencloudcmd` sync engine (OpenCloud server).
    OpenCloud,
}

impl Provider {
    /// Stable lowercase name (`"nextcloud"` or `"opencloud"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nextcloud => "nextcloud",
            Self::OpenCloud => "opencloud",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error produced when an unknown provider name is parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError(pub String);

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown provider: {}", self.0)
    }
}

impl std::error::Error for ProviderError {}

impl FromStr for Provider {
    type Err = ProviderError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "nextcloud" => Ok(Self::Nextcloud),
            "opencloud" => Ok(Self::OpenCloud),
            other => Err(ProviderError(other.to_string())),
        }
    }
}

/// Everything a driver needs to build the final command for one folder pair.
///
/// Most fields mirror the account/folder/network configuration (see
/// [`DriverContext::from_folder`]); `password` is the secret the provider
/// expects (`NC_PASSWORD` for Nextcloud, the app password / token for
/// OpenCloud) and `sync_hidden_files` follows the OpenCloud flag that is
/// opt-in by default (the Nextcloud driver always syncs hidden files via `-h`).
#[derive(Debug, Clone, PartialEq)]
pub struct DriverContext {
    /// Account server URL, already normalized.
    pub server_url: String,
    /// Account login name (Nextcloud) or OpenCloud username.
    pub user: String,
    /// Secret: account password (Nextcloud) or app token (OpenCloud).
    pub password: String,
    /// Local sync root of the folder.
    pub local_root: String,
    /// Remote path: Nextcloud `--path` / OpenCloud `--remote-folder`.
    pub remote_path: String,
    /// OpenCloud space id (`None` for Nextcloud, mandatory for OpenCloud).
    pub space_id: Option<String>,
    /// Network settings (proxy, certificate trust).
    pub network: NetworkConfig,
    /// `--max-sync-retries` value.
    pub retries: i64,
    /// Nextcloud `--silent` (omitted when detailed output is enabled).
    pub detailed_output: bool,
    /// OpenCloud `--sync-hidden-files` (opt-in; off by default).
    pub sync_hidden_files: bool,
    /// Path to an exclude file, when configured.
    pub exclude_file: Option<PathBuf>,
    /// Override of the provider binary (used by tests).
    pub executable: Option<PathBuf>,
}

impl DriverContext {
    /// Build a context from the configuration types of one folder pair.
    pub fn from_folder(
        account: &AccountConfig,
        folder: &FolderConfig,
        network: &NetworkConfig,
        password: String,
        exclude_file: Option<PathBuf>,
        executable: Option<PathBuf>,
    ) -> Self {
        Self {
            server_url: account.server_url.clone(),
            user: account.login_name.clone(),
            password,
            local_root: folder.local_root.clone(),
            remote_path: folder.remote_path.clone(),
            space_id: folder.space_id.clone(),
            network: network.clone(),
            retries: account.sync.max_sync_retries,
            detailed_output: account.sync.detailed_output,
            sync_hidden_files: false,
            exclude_file,
            executable,
        }
    }
}

/// Produces the [`CommandSpec`] for a sync provider.
///
/// The engine asks this trait for the final invocation and never branches on
/// the provider itself; the progress parser is shared between both drivers
/// (both binaries are forks of the same Qt codebase).
pub trait SyncDriver: Send + Sync {
    /// Name of the wrapped binary (`nextcloudcmd` / `opencloudcmd`).
    fn binary_name(&self) -> &'static str;

    /// Whether the provider binary is available on `$PATH`.
    fn binary_exists(&self) -> bool;

    /// Build the exact argv + environment for one folder pair.
    fn build_command(&self, ctx: &DriverContext) -> Result<CommandSpec, CommandError>;
}

/// [`SyncDriver`] for `nextcloudcmd`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NextcloudDriver;

/// [`SyncDriver`] for `opencloudcmd`.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenCloudDriver;

/// The driver matching a [`Provider`].
pub fn driver_for(provider: Provider) -> Box<dyn SyncDriver> {
    match provider {
        Provider::Nextcloud => Box::new(NextcloudDriver),
        Provider::OpenCloud => Box::new(OpenCloudDriver),
    }
}

impl SyncDriver for NextcloudDriver {
    fn binary_name(&self) -> &'static str {
        "nextcloudcmd"
    }

    fn binary_exists(&self) -> bool {
        find_binary(self.binary_name()).is_some()
    }

    fn build_command(&self, ctx: &DriverContext) -> Result<CommandSpec, CommandError> {
        let binary = resolve_binary(self.binary_name(), &ctx.executable)?;
        let mut argv = vec![
            binary,
            "--non-interactive".to_string(),
            "--max-sync-retries".to_string(),
            ctx.retries.max(1).to_string(),
            "-h".to_string(),
        ];
        if !ctx.detailed_output {
            argv.push("--silent".to_string());
        }
        if ctx.network.trust_invalid_certificates {
            argv.push("--trust".to_string());
        }
        if let Some(proxy) = &ctx.network.custom_proxy {
            argv.push("--httpproxy".to_string());
            argv.push(proxy.clone());
        }
        if let Some(path) = &ctx.exclude_file {
            argv.push("--exclude".to_string());
            argv.push(path.to_string_lossy().into_owned());
        }
        let remote_path = normalize_remote_path(&ctx.remote_path)
            .map_err(|error| CommandError::InvalidRemotePath(error.message))?;
        if !remote_path.is_empty() {
            argv.push("--path".to_string());
            argv.push(remote_path);
        }
        argv.push(ctx.local_root.clone());
        argv.push(ctx.server_url.clone());
        Ok(CommandSpec {
            argv,
            environment: vec![
                ("NC_USER".to_string(), ctx.user.clone()),
                ("NC_PASSWORD".to_string(), ctx.password.clone()),
            ],
        })
    }
}

impl SyncDriver for OpenCloudDriver {
    fn binary_name(&self) -> &'static str {
        "opencloudcmd"
    }

    fn binary_exists(&self) -> bool {
        find_binary(self.binary_name()).is_some()
    }

    fn build_command(&self, ctx: &DriverContext) -> Result<CommandSpec, CommandError> {
        let binary = resolve_binary(self.binary_name(), &ctx.executable)?;
        let space_id = ctx.space_id.clone().ok_or(CommandError::MissingSpaceId)?;
        let source_dir = if ctx.local_root.ends_with('/') {
            ctx.local_root.clone()
        } else {
            format!("{}/", ctx.local_root)
        };
        let mut argv = vec![
            binary,
            ctx.server_url.clone(),
            space_id,
            source_dir,
            "--user".to_string(),
            ctx.user.clone(),
        ];
        let remote_folder = normalize_remote_path(&ctx.remote_path)
            .map_err(|error| CommandError::InvalidRemotePath(error.message))?;
        if !remote_folder.is_empty() {
            argv.push("--remote-folder".to_string());
            argv.push(remote_folder.trim_start_matches('/').to_string());
        }
        if let Some(path) = &ctx.exclude_file {
            argv.push("--exclude".to_string());
            argv.push(path.to_string_lossy().into_owned());
        }
        argv.push("--max-sync-retries".to_string());
        argv.push(ctx.retries.max(1).to_string());
        argv.push("--non-interactive".to_string());
        if ctx.network.trust_invalid_certificates {
            argv.push("--trust".to_string());
        }
        if ctx.sync_hidden_files {
            argv.push("--sync-hidden-files".to_string());
        }
        Ok(CommandSpec {
            argv,
            environment: vec![("OPENCLOUD_TOKEN".to_string(), ctx.password.clone())],
        })
    }
}

/// The provider binary: the explicit override, or the `$PATH` lookup.
fn resolve_binary(name: &str, executable: &Option<PathBuf>) -> Result<String, CommandError> {
    match executable {
        Some(path) => Ok(path.to_string_lossy().into_owned()),
        None => find_binary(name)
            .map(|path| path.to_string_lossy().into_owned())
            .ok_or(CommandError::MissingBinary),
    }
}

/// Discover the spaces of an OpenCloud server (query mode).
///
/// Running `opencloudcmd <server_url> --user <user>` without a space id or
/// source dir makes the CLI print a "Listing spaces:" table on stdout. Only
/// stdout is captured; parsing the table is left to the caller.
pub fn opencloud_list_spaces(
    url: &str,
    user: &str,
    token: &str,
    executable: Option<&Path>,
) -> Result<String, CommandError> {
    let binary = match executable {
        Some(path) => path.to_string_lossy().into_owned(),
        None => find_binary("opencloudcmd")
            .map(|path| path.to_string_lossy().into_owned())
            .ok_or(CommandError::MissingBinary)?,
    };
    let output = std::process::Command::new(&binary)
        .arg(url)
        .arg("--user")
        .arg(user)
        .env("OPENCLOUD_TOKEN", token)
        .output()
        .map_err(|_| CommandError::MissingBinary)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::nextcloud::command::CommandError;
    use crate::storage::config::NetworkConfig;

    fn ctx() -> DriverContext {
        DriverContext {
            server_url: "https://cloud.example.com".to_string(),
            user: "alice".to_string(),
            password: "very-secret".to_string(),
            local_root: "/tmp/NextCloud".to_string(),
            remote_path: String::new(),
            space_id: None,
            network: NetworkConfig::default(),
            retries: 3,
            detailed_output: true,
            sync_hidden_files: false,
            exclude_file: None,
            executable: Some(PathBuf::from("/bin/true")),
        }
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    // ---- Provider ----------------------------------------------------------

    #[test]
    fn provider_string_forms_and_defaults() {
        assert_eq!(Provider::Nextcloud.as_str(), "nextcloud");
        assert_eq!(Provider::OpenCloud.as_str(), "opencloud");
        assert_eq!(Provider::default(), Provider::Nextcloud);
    }

    #[test]
    fn provider_serde_roundtrip_lowercase() {
        assert_eq!(
            serde_json::to_string(&Provider::OpenCloud).unwrap(),
            "\"opencloud\""
        );
        assert_eq!(
            serde_json::from_str::<Provider>("\"opencloud\"").unwrap(),
            Provider::OpenCloud
        );
        assert_eq!(
            serde_json::from_str::<Provider>("\"nextcloud\"").unwrap(),
            Provider::Nextcloud
        );
    }

    #[test]
    fn provider_from_str_is_case_insensitive_and_rejects_unknown() {
        assert_eq!(
            "nextcloud".parse::<Provider>().unwrap(),
            Provider::Nextcloud
        );
        assert_eq!(
            "OpenCloud".parse::<Provider>().unwrap(),
            Provider::OpenCloud
        );
        assert!("kde".parse::<Provider>().is_err());
    }

    // ---- NextcloudDriver ---------------------------------------------------

    #[test]
    fn nextcloud_password_only_in_environment() {
        let spec = NextcloudDriver
            .build_command(&ctx())
            .expect("build succeeds");
        assert!(!spec.argv.iter().any(|arg| arg.contains("very-secret")));
        let password = spec
            .environment
            .iter()
            .find(|(key, _)| key == "NC_PASSWORD")
            .expect("NC_PASSWORD should be set");
        assert_eq!(password.1, "very-secret");
        let user = spec
            .environment
            .iter()
            .find(|(key, _)| key == "NC_USER")
            .expect("NC_USER should be set");
        assert_eq!(user.1, "alice");
    }

    #[test]
    fn nextcloud_options_proxy_retries_and_positionals() {
        let mut ctx = ctx();
        ctx.network = NetworkConfig {
            custom_proxy: Some("http://proxy:8080".to_string()),
            trust_invalid_certificates: true,
            reduce_transfer_impact: false,
            allowed_ssids: None,
        };
        ctx.retries = 7;
        ctx.exclude_file = Some(PathBuf::from("/tmp/excludes.lst"));
        ctx.remote_path = "/Documents".to_string();
        let spec = NextcloudDriver.build_command(&ctx).expect("build succeeds");
        for flag in [
            "--non-interactive",
            "--max-sync-retries",
            "-h",
            "--trust",
            "--httpproxy",
            "--exclude",
            "--path",
        ] {
            assert!(
                spec.argv.iter().any(|arg| arg == flag),
                "expected {flag} in argv"
            );
        }
        assert!(spec.argv.iter().any(|arg| arg == "http://proxy:8080"));
        assert!(spec.argv.iter().any(|arg| arg == "7"));
        let local_index = spec
            .argv
            .iter()
            .position(|arg| arg == "/tmp/NextCloud")
            .expect("local root present");
        let server_index = spec
            .argv
            .iter()
            .position(|arg| arg == "https://cloud.example.com")
            .expect("server url present");
        assert_eq!(server_index, local_index + 1);
    }

    // ---- OpenCloudDriver ---------------------------------------------------

    #[test]
    fn opencloud_positionals_are_url_space_and_source() {
        let mut ctx = ctx();
        ctx.space_id = Some("space:abcd".to_string());
        let spec = OpenCloudDriver.build_command(&ctx).expect("build succeeds");
        assert_eq!(spec.argv[0], "/bin/true");
        assert_eq!(spec.argv[1], "https://cloud.example.com");
        assert_eq!(spec.argv[2], "space:abcd");
        assert_eq!(spec.argv[3], "/tmp/NextCloud/");
    }

    #[test]
    fn opencloud_user_by_flag_and_token_via_environment() {
        let mut ctx = ctx();
        ctx.space_id = Some("space:abcd".to_string());
        let spec = OpenCloudDriver.build_command(&ctx).expect("build succeeds");
        assert!(spec.argv.iter().any(|arg| arg == "--user"));
        assert!(spec.argv.iter().any(|arg| arg == "alice"));
        assert!(!spec.argv.iter().any(|arg| arg.contains("very-secret")));
        assert!(!spec.argv.iter().any(|arg| arg == "--token"));
        let token = spec
            .environment
            .iter()
            .find(|(key, _)| key == "OPENCLOUD_TOKEN")
            .expect("OPENCLOUD_TOKEN should be set");
        assert_eq!(token.1, "very-secret");
        assert!(!spec
            .environment
            .iter()
            .any(|(key, _)| key.starts_with("NC_")));
    }

    #[test]
    fn opencloud_has_no_httpproxy_flag() {
        let mut ctx = ctx();
        ctx.space_id = Some("space:abcd".to_string());
        ctx.network = NetworkConfig {
            custom_proxy: Some("http://proxy:8080".to_string()),
            trust_invalid_certificates: true,
            reduce_transfer_impact: false,
            allowed_ssids: None,
        };
        let spec = OpenCloudDriver.build_command(&ctx).expect("build succeeds");
        assert!(!spec.argv.iter().any(|arg| arg == "--httpproxy"));
        assert!(spec.argv.iter().any(|arg| arg == "--trust"));
    }

    #[test]
    fn opencloud_remote_folder_flag_maps_to_subdirectory() {
        let mut ctx = ctx();
        ctx.space_id = Some("space:abcd".to_string());
        ctx.remote_path = "/Documents/".to_string();
        let spec = OpenCloudDriver.build_command(&ctx).expect("build succeeds");
        let index = spec
            .argv
            .iter()
            .position(|arg| arg == "--remote-folder")
            .expect("--remote-folder should be present");
        assert_eq!(spec.argv[index + 1], "Documents");

        let mut root = ctx.clone();
        root.remote_path = String::new();
        let spec = OpenCloudDriver
            .build_command(&root)
            .expect("build succeeds");
        assert!(!spec.argv.iter().any(|arg| arg == "--remote-folder"));
    }

    #[test]
    fn opencloud_requires_space_id() {
        assert_eq!(
            OpenCloudDriver.build_command(&ctx()).unwrap_err(),
            CommandError::MissingSpaceId
        );
    }

    #[test]
    fn opencloud_exclude_retries_non_interactive_and_hidden_flags() {
        let mut ctx = ctx();
        ctx.space_id = Some("space:abcd".to_string());
        ctx.exclude_file = Some(PathBuf::from("/tmp/excludes.lst"));
        ctx.retries = 7;
        ctx.sync_hidden_files = true;
        let spec = OpenCloudDriver.build_command(&ctx).expect("build succeeds");
        for flag in [
            "--exclude",
            "--max-sync-retries",
            "--non-interactive",
            "--sync-hidden-files",
        ] {
            assert!(
                spec.argv.iter().any(|arg| arg == flag),
                "expected {flag} in argv"
            );
        }
        assert!(spec.argv.iter().any(|arg| arg == "/tmp/excludes.lst"));
        assert!(spec.argv.iter().any(|arg| arg == "7"));

        let mut hidden_off = ctx.clone();
        hidden_off.sync_hidden_files = false;
        let spec = OpenCloudDriver
            .build_command(&hidden_off)
            .expect("build succeeds");
        assert!(!spec.argv.iter().any(|arg| arg == "--sync-hidden-files"));
    }

    // ---- factory and binary lookup ----------------------------------------

    #[test]
    fn driver_for_provider_binary_names() {
        assert_eq!(
            driver_for(Provider::Nextcloud).binary_name(),
            "nextcloudcmd"
        );
        assert_eq!(
            driver_for(Provider::OpenCloud).binary_name(),
            "opencloudcmd"
        );
    }

    #[test]
    fn find_binary_misses_unknown_names() {
        assert!(
            find_binary("definitely-not-a-real-binary-xyz").is_none(),
            "unknown binary must not resolve"
        );
    }

    // ---- spaces discovery --------------------------------------------------

    #[test]
    fn opencloud_list_spaces_captures_stdout_table() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "fake-opencloudcmd",
            "#!/bin/sh\n\
             [ -n \"$OPENCLOUD_TOKEN\" ] || exit 3\n\
             [ \"$1\" = \"https://cloud.example.com\" ] || exit 4\n\
             [ \"$2\" = \"--user\" ] || exit 5\n\
             [ \"$3\" = \"alice\" ] || exit 6\n\
             echo 'Listing spaces:'\n\
             echo 'Personal | space:abcd'\n\
             exit 0\n",
        );
        let output = opencloud_list_spaces(
            "https://cloud.example.com",
            "alice",
            "very-secret",
            Some(&script),
        )
        .expect("listing succeeds");
        assert!(output.contains("Listing spaces:"));
        assert!(output.contains("space:abcd"));
    }

    #[test]
    fn opencloud_list_spaces_missing_binary_errors() {
        assert_eq!(
            opencloud_list_spaces(
                "https://cloud.example.com",
                "alice",
                "secret",
                Some(Path::new("/nonexistent/opencloudcmd")),
            )
            .unwrap_err(),
            CommandError::MissingBinary
        );
    }
}
