//! Configuration storage: schema v7 model, validation and atomic file IO.
//!
//! The on-disk format is byte-compatible with the Python `nextsync` v0.2.x
//! (`~/.config/nextsync/settings.json`, `schema_version: 6`), plus the v7
//! additions (account `provider`, folder `space_id`). Reading tolerates legacy
//! v1-v6 files (migrating them in order) and missing keys (merging with
//! defaults); writing always emits a clean v7 payload.
//!
//! Reference implementation: `src/nextsync/storage/config.py` (v0.2.5).

use std::collections::HashSet;
use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::nextcloud::driver::Provider;

/// Current configuration schema version.
pub const SCHEMA_VERSION: u32 = 7;

/// Default exclusion patterns, mirroring `nextsync.core.exclusions`.
pub const DEFAULT_PATTERNS: [&str; 10] = [
    ".DS_Store",
    "Thumbs.db",
    "ehthumbs.db",
    "Desktop.ini",
    "desktop.ini",
    "~$*",
    "*.swp",
    "*.swo",
    "*~",
    ".nextcloudsync.log",
];

/// Fields of the v3/v4 safety subsystem removed by the v5 migration.
const DROP_SAFETY_KEYS: [&str; 5] = [
    "bootstrap_complete",
    "bootstrap_completed_at",
    "guard_enabled",
    "deletion_count_threshold",
    "deletion_percent_threshold",
];

/// An error raised while validating or persisting configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub message: String,
}

impl ConfigError {
    /// Build an error with a human-readable message (public for the UI layer,
    /// which constructs validation errors such as "Account not found.").
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Top-level configuration document (schema v6).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub schema_version: u32,
    pub accounts: Vec<AccountConfig>,
    pub general: GeneralConfig,
    pub logging: LoggingConfig,
    pub network: NetworkConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            accounts: Vec::new(),
            general: GeneralConfig::default(),
            logging: LoggingConfig::default(),
            network: NetworkConfig::default(),
        }
    }
}

/// One configured account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AccountConfig {
    pub id: String,
    pub server_url: String,
    pub login_name: String,
    pub authentication_type: String,
    /// The sync engine this account uses (defaults to Nextcloud).
    pub provider: Provider,
    pub folders: Vec<FolderConfig>,
    pub sync: SyncConfig,
    pub delete_guard: DeleteGuardConfig,
    pub runtime: RuntimeConfig,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            server_url: String::new(),
            login_name: String::new(),
            authentication_type: "manual".to_string(),
            provider: Provider::default(),
            folders: Vec::new(),
            sync: SyncConfig::default(),
            delete_guard: DeleteGuardConfig::default(),
            runtime: RuntimeConfig::default(),
        }
    }
}

/// One local/remote folder pair of an account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FolderConfig {
    pub id: String,
    pub local_root: String,
    pub remote_path: String,
    /// OpenCloud space id; unused by Nextcloud folders.
    #[serde(default)]
    pub space_id: Option<String>,
}

/// Per-account sync triggers and exclusions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SyncConfig {
    pub local_inotify_enabled: bool,
    pub local_interval_enabled: bool,
    pub local_interval_minutes: i64,
    pub remote_push_enabled: bool,
    pub remote_interval_enabled: bool,
    pub remote_interval_minutes: i64,
    pub max_sync_retries: i64,
    pub detailed_output: bool,
    pub exclude_patterns_enabled: bool,
    pub exclude_patterns: Vec<String>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            local_inotify_enabled: true,
            local_interval_enabled: false,
            local_interval_minutes: 5,
            remote_push_enabled: true,
            remote_interval_enabled: true,
            remote_interval_minutes: 10,
            max_sync_retries: 3,
            detailed_output: true,
            exclude_patterns_enabled: true,
            exclude_patterns: DEFAULT_PATTERNS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Per-account deletion guard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DeleteGuardConfig {
    pub enabled: bool,
    pub count_threshold: i64,
    pub percent_threshold: i64,
}

impl Default for DeleteGuardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            count_threshold: 10,
            percent_threshold: 20,
        }
    }
}

/// Per-account runtime bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct RuntimeConfig {
    pub last_successful_sync: Option<String>,
    pub last_exit_code: Option<i64>,
}

/// General (non-account) settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GeneralConfig {
    pub autostart: bool,
    /// Battery pause preference. The UI switch was removed by user decision
    /// (issue #22); the field stays (default false, neutral behavior) so the
    /// runtime plumbing and older config files keep working.
    #[serde(default)]
    pub pause_on_battery: bool,
    /// Color scheme preference: `"system"`, `"light"` or `"dark"`.
    #[serde(default = "default_color_scheme")]
    pub color_scheme: String,
    /// Whether desktop notifications are sent for sync/auth failures.
    #[serde(default = "yes")]
    pub show_notifications: bool,
    /// Whether the account's server notifications (shares, comments,
    /// mentions) are raised as desktop notifications (issue #31).
    #[serde(default)]
    pub show_server_notifications: bool,
}

/// Default color scheme (follow the desktop).
fn default_color_scheme() -> String {
    "system".to_string()
}

/// Default for `show_notifications` (on).
fn yes() -> bool {
    true
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            autostart: true,
            pause_on_battery: false,
            color_scheme: default_color_scheme(),
            show_notifications: yes(),
            show_server_notifications: false,
        }
    }
}

/// Logging settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LoggingConfig {
    pub save_logs: bool,
    pub retention_days: i64,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            save_logs: true,
            retention_days: 30,
        }
    }
}

/// Network settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct NetworkConfig {
    pub custom_proxy: Option<String>,
    pub trust_invalid_certificates: bool,
    /// Whether sync processes run with reduced IO/CPU priority
    /// (`ionice -c 3` + `nice -n 10`) so transfers do not saturate the
    /// machine (issue #39). It is a priority hint, not a bandwidth cap.
    #[serde(default)]
    pub reduce_transfer_impact: bool,
    /// Wi-Fi SSIDs synchronization is restricted to (comma separated).
    /// Empty or `None` means any network (issue #41).
    #[serde(default)]
    pub allowed_ssids: Option<String>,
}

/// Normalize a Nextcloud server URL: scheme lowered, no embedded credentials,
/// path without trailing slash, query/fragment dropped.
pub fn normalize_server_url(value: &str) -> Result<String, ConfigError> {
    let candidate = value.trim();
    let (scheme, netloc, path) = split_url(candidate);
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") || netloc.is_empty() {
        return Err(ConfigError::new(
            "Enter a complete HTTP or HTTPS Nextcloud URL.",
        ));
    }
    if has_credentials(&netloc) {
        return Err(ConfigError::new(
            "Credentials must not be included in the server URL.",
        ));
    }
    let path = path.trim_end_matches('/');
    Ok(format!("{scheme}://{netloc}{path}"))
}

/// Normalize a remote folder path; the account root is the empty string.
pub fn normalize_remote_path(value: &str) -> Result<String, ConfigError> {
    let raw = value.trim();
    if raw.is_empty() || raw == "/" {
        return Ok(String::new());
    }
    if raw.contains('\\') || raw.contains('\0') {
        return Err(ConfigError::new(
            "The remote folder may not contain backslashes or null bytes.",
        ));
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Err(ConfigError::new(
            "The remote folder must be a path, not a full URL.",
        ));
    }
    if raw.contains('?') || raw.contains('#') {
        return Err(ConfigError::new(
            "The remote folder may not include query parameters or fragments.",
        ));
    }
    let raw = if raw.starts_with('/') {
        raw
    } else {
        return normalize_remote_path(&format!("/{raw}"));
    };
    let mut segments = Vec::new();
    for segment in raw.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return Err(ConfigError::new(
                "The remote folder may not contain parent directory references.",
            ));
        }
        segments.push(segment);
    }
    if segments.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!("/{}", segments.join("/")))
    }
}

/// Stable account identity: SHA-256 of `server\nlogin`, both casefolded.
pub fn account_id(server_url: &str, login_name: &str) -> String {
    let identity = format!(
        "{}\n{}",
        server_url.trim_end_matches('/').to_lowercase(),
        login_name.to_lowercase()
    );
    sha256_hex(identity.as_bytes())
}

/// Stable identity of one folder pair within an account.
pub fn folder_fingerprint(
    server_url: &str,
    login_name: &str,
    local_root: &str,
    remote_path: &str,
) -> String {
    let local =
        std::path::absolute(expanduser(local_root)).unwrap_or_else(|_| PathBuf::from(local_root));
    let identity = format!(
        "{}\n{}\n{}\n{}",
        server_url.trim_end_matches('/').to_lowercase(),
        login_name.to_lowercase(),
        local.to_string_lossy(),
        remote_path
    );
    sha256_hex(identity.as_bytes())
}

/// Choose the remote folder path for the Add Folder dialog.
///
/// A literally empty remote field (whitespace only) maps to a remote folder
/// named after the local folder, e.g. `/home/user/NextCloud` becomes
/// `/NextCloud`. An explicit `/` keeps the account-root mapping (`""`); any
/// other value is normalized as typed.
pub fn remote_path_for(local_root: &str, remote_text: &str) -> Result<String, ConfigError> {
    let text = remote_text.trim();
    if text.is_empty() {
        let expanded = expanduser(local_root);
        let name = expanded
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        return normalize_remote_path(&format!("/{name}"));
    }
    normalize_remote_path(text)
}

/// Validate an exclusion pattern (mirror of `exclusions.validate_pattern`).
pub fn validate_pattern(pattern: &str) -> Result<String, ConfigError> {
    let candidate = pattern.trim();
    if candidate.is_empty() {
        return Err(ConfigError::new("Pattern cannot be empty."));
    }
    if candidate.contains('/') || candidate.contains('\\') || candidate.contains("..") {
        return Err(ConfigError::new(
            "Folder and path patterns are not supported.",
        ));
    }
    if matches!(candidate, "*" | ".*" | "*.*") {
        return Err(ConfigError::new(
            "This pattern is too broad and could hide user files.",
        ));
    }
    if candidate.contains('\0') || candidate.len() > 255 {
        return Err(ConfigError::new("Pattern is invalid or too long."));
    }
    Ok(candidate.to_string())
}

/// Validate and normalize a raw JSON document into the v6 model.
pub fn validate_config(value: Value) -> Result<Config, ConfigError> {
    let mut data = value
        .as_object()
        .cloned()
        .ok_or_else(|| ConfigError::new("Configuration root must be an object."))?;

    let version = match data.get("schema_version") {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
        _ => 1,
    };
    if version > SCHEMA_VERSION as i64 {
        return Err(ConfigError::new(format!(
            "Configuration schema {} is newer than this application supports.",
            version
        )));
    }

    migrate_to_v3(&mut data);
    migrate_to_v5(&mut data);
    migrate_to_v6(&mut data);
    migrate_to_v7(&mut data);
    data.insert("schema_version".to_string(), Value::from(SCHEMA_VERSION));

    let mut accounts = Vec::new();
    if let Some(Value::Array(raw_accounts)) = data.get("accounts") {
        for account in raw_accounts {
            accounts.push(validate_account(account)?);
        }
    }

    let general = validate_general(data.get("general"));
    let logging = validate_logging(data.get("logging"))?;
    let network = validate_network(data.get("network"))?;

    Ok(Config {
        schema_version: SCHEMA_VERSION,
        accounts,
        general,
        logging,
        network,
    })
}

/// Default local sync root (`$HOME/NextCloud`), as in `util/paths.py`.
pub fn default_sync_root() -> PathBuf {
    home_dir().join("NextCloud")
}

/// Resolve `~`-prefixed paths against `$HOME` (no `~user` support).
pub fn expanduser(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
}

/// Default settings file location (`$XDG_CONFIG_HOME/nextsync/settings.json`).
pub fn default_config_path() -> Result<PathBuf, ConfigError> {
    let base = match env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => home_dir().join(".config"),
    };
    Ok(base.join("nextsync").join("settings.json"))
}

/// Ensure a directory exists with 0700 permissions.
pub fn ensure_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Reads and writes the settings file atomically with 0600 permissions.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    /// Create a store for the default settings location.
    pub fn new() -> Result<Self, ConfigError> {
        Ok(Self {
            path: default_config_path()?,
        })
    }

    /// Create a store for an explicit path (used by tests).
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// The settings file path this store operates on.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load and validate the settings file, falling back to defaults when the
    /// file does not exist.
    pub fn load(&self) -> Result<Config, ConfigError> {
        if !self.path.exists() {
            return validate_config(Value::Object(Map::new()));
        }
        let text = fs::read_to_string(&self.path).map_err(|error| {
            ConfigError::new(format!("Could not load {}: {}", self.path.display(), error))
        })?;
        let value: Value = serde_json::from_str(&text).map_err(|error| {
            ConfigError::new(format!("Could not load {}: {}", self.path.display(), error))
        })?;
        validate_config(value)
    }

    /// Atomically write the configuration (tmp file + rename, mode 0600).
    pub fn save(&self, config: &Config) -> Result<(), ConfigError> {
        let mut payload = config.clone();
        payload.schema_version = SCHEMA_VERSION;

        let parent = self
            .path
            .parent()
            .ok_or_else(|| ConfigError::new("Configuration path has no parent directory."))?;
        ensure_private_directory(parent).map_err(|error| {
            ConfigError::new(format!(
                "Could not create directory for {}: {}",
                self.path.display(),
                error
            ))
        })?;

        let content = serde_json::to_string_pretty(&payload).map_err(|error| {
            ConfigError::new(format!("Could not serialize configuration: {error}"))
        })? + "\n";

        let temporary = self.path.with_extension("tmp");
        let write_result = (|| -> io::Result<()> {
            let mut handle = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            handle.write_all(content.as_bytes())?;
            handle.sync_all()?;
            drop(handle);
            fs::rename(&temporary, &self.path)
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(ConfigError::new(format!(
                "Could not save {}: {}",
                self.path.display(),
                error
            )));
        }
        Ok(())
    }

    /// Add a validated account, persisting immediately. The account is
    /// re-validated and normalized (server URL, login name, folder
    /// fingerprints) exactly like a freshly loaded file, mirroring the Python
    /// `add_account` which runs `_validate_account`. Returns the account id.
    pub fn add_account(&self, account: &AccountConfig) -> Result<String, ConfigError> {
        let value = serde_json::to_value(account)
            .map_err(|error| ConfigError::new(format!("Could not serialize account: {error}")))?;
        let validated = validate_account(&value)?;
        let mut config = self.load()?;
        if config.accounts.iter().any(|item| item.id == validated.id) {
            return Err(ConfigError::new(
                "An account with the same server and username already exists.",
            ));
        }
        let id = validated.id.clone();
        config.accounts.push(validated);
        self.save(&config)?;
        Ok(id)
    }

    /// Remove an account by id, returning `false` when it did not exist.
    pub fn remove_account(&self, account_id: &str) -> Result<bool, ConfigError> {
        let mut config = self.load()?;
        let before = config.accounts.len();
        config.accounts.retain(|item| item.id != account_id);
        if config.accounts.len() == before {
            return Ok(false);
        }
        self.save(&config)?;
        Ok(true)
    }

    /// Fetch one account by id.
    pub fn account(&self, account_id: &str) -> Result<Option<AccountConfig>, ConfigError> {
        let config = self.load()?;
        Ok(config
            .accounts
            .into_iter()
            .find(|item| item.id == account_id))
    }

    /// Add a folder to an account, recomputing the fingerprint over the
    /// normalized local root and remote path (mirrors the Python `add_folder`).
    pub fn add_folder(
        &self,
        account_id: &str,
        folder: &FolderConfig,
    ) -> Result<String, ConfigError> {
        let mut config = self.load()?;
        let account = config
            .accounts
            .iter_mut()
            .find(|item| item.id == account_id)
            .ok_or_else(|| ConfigError::new("Account not found."))?;
        let root = expanduser(&folder.local_root);
        if !root.is_absolute() {
            return Err(ConfigError::new(
                "The local synchronization folder must be absolute.",
            ));
        }
        let local_root = root.to_string_lossy().into_owned();
        let remote_path = normalize_remote_path(&folder.remote_path)?;
        let id = folder_fingerprint(
            &account.server_url,
            &account.login_name,
            &local_root,
            &remote_path,
        );
        if account.folders.iter().any(|item| item.id == id) {
            return Err(ConfigError::new("This local folder is already configured."));
        }
        account.folders.push(FolderConfig {
            id: id.clone(),
            local_root,
            remote_path,
            space_id: folder.space_id.clone(),
        });
        self.save(&config)?;
        Ok(id)
    }

    /// Remove a folder from an account, returning `false` when it (or the
    /// account) did not exist.
    pub fn remove_folder(&self, account_id: &str, folder_id: &str) -> Result<bool, ConfigError> {
        let mut config = self.load()?;
        let account = config
            .accounts
            .iter_mut()
            .find(|item| item.id == account_id);
        let Some(account) = account else {
            return Ok(false);
        };
        let before = account.folders.len();
        account.folders.retain(|item| item.id != folder_id);
        if account.folders.len() == before {
            return Ok(false);
        }
        self.save(&config)?;
        Ok(true)
    }

    /// Replace the stored account that matches `account.id` and persist
    /// (mirrors the Python `_sync_back` + `save` used by Settings). When the
    /// incoming account carries no id, it is recomputed from its identity.
    pub fn update_account(&self, account: &AccountConfig) -> Result<(), ConfigError> {
        let lookup_id = if account.id.is_empty() {
            account_id(&account.server_url, &account.login_name)
        } else {
            account.id.clone()
        };
        let mut config = self.load()?;
        let index = config
            .accounts
            .iter()
            .position(|item| item.id == lookup_id)
            .ok_or_else(|| ConfigError::new("Account not found."))?;
        let mut updated = account.clone();
        if updated.id.is_empty() {
            updated.id = lookup_id;
        }
        config.accounts[index] = updated;
        self.save(&config)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Schema migrations (order: v3, v5, v6), mirroring `config.py`.
// ---------------------------------------------------------------------------

/// Move a single legacy `account` into `accounts[0]` (schema v3).
fn migrate_to_v3(raw: &mut Map<String, Value>) {
    if raw.contains_key("accounts") || !raw.contains_key("account") {
        return;
    }
    let account = raw.get("account").cloned().unwrap_or(Value::Null);
    let accounts = match account {
        Value::Object(account) => {
            let sync = raw.get("sync").cloned().unwrap_or_else(default_sync_value);
            let runtime = raw
                .get("runtime")
                .cloned()
                .unwrap_or_else(default_runtime_value);
            let authentication_type = match account.get("authentication_type") {
                Some(Value::String(value)) => value.clone(),
                _ => "manual".to_string(),
            };
            vec![json!({
                "server_url": account.get("server_url").map(json_str).unwrap_or_default(),
                "login_name": account.get("login_name").map(json_str).unwrap_or_default(),
                "authentication_type": authentication_type,
                "local_root": account.get("local_root").map(json_str).unwrap_or_default(),
                "remote_path": account.get("remote_path").map(json_str).unwrap_or_default(),
                "sync": sync,
                "runtime": runtime,
            })]
        }
        _ => Vec::new(),
    };
    raw.insert("accounts".to_string(), Value::Array(accounts));
    raw.remove("account");
    raw.remove("sync");
    raw.remove("runtime");
}

/// Drop the safety-subsystem fields introduced in schema v3/v4 (schema v5).
fn migrate_to_v5(raw: &mut Map<String, Value>) {
    if let Some(Value::Array(accounts)) = raw.get_mut("accounts") {
        for account in accounts {
            if let Value::Object(map) = account {
                map.remove("safety");
                for key in DROP_SAFETY_KEYS {
                    map.remove(key);
                }
            }
        }
    }
    raw.remove("safety");
    for key in DROP_SAFETY_KEYS {
        raw.remove(key);
    }
}

/// Move the single `local_root`/`remote_path` pair into `folders[0]` and
/// compute every account id (schema v6).
fn migrate_to_v6(raw: &mut Map<String, Value>) {
    let accounts = match raw.get("accounts").cloned() {
        Some(Value::Array(accounts)) => accounts,
        _ => Vec::new(),
    };
    let migrated: Vec<Value> = accounts
        .into_iter()
        .map(|account| {
            let Value::Object(mut map) = account else {
                return account;
            };
            let folders_empty = match map.get("folders") {
                None | Some(Value::Null) => true,
                Some(Value::Array(folders)) => folders.is_empty(),
                _ => false,
            };
            let has_local_root = map.get("local_root").is_some_and(json_truthy);
            if folders_empty && has_local_root {
                map.insert(
                    "folders".to_string(),
                    json!([{
                        "local_root": map.get("local_root").map(json_str).unwrap_or_default(),
                        "remote_path": map.get("remote_path").map(json_str).unwrap_or_default(),
                    }]),
                );
            }
            map.remove("local_root");
            map.remove("remote_path");
            let server_url = map.get("server_url").map(json_str).unwrap_or_default();
            let login_name = map.get("login_name").map(json_str).unwrap_or_default();
            map.insert(
                "id".to_string(),
                Value::String(account_id(&server_url, &login_name)),
            );
            Value::Object(map)
        })
        .collect();
    raw.insert("accounts".to_string(), Value::Array(migrated));
}

/// Add the account `provider` and the folder `space_id` (schema v7).
///
/// This is a metadata-only migration: legacy files carry neither key, and both
/// default at validation time (`Provider` defaults to `Nextcloud`, `space_id`
/// to `None`). The step exists to mark the version bump in the same place as
/// the other migrations.
fn migrate_to_v7(raw: &mut Map<String, Value>) {
    let _ = raw;
}

// ---------------------------------------------------------------------------
// Per-section validators, mirroring `config.py`.
// ---------------------------------------------------------------------------

fn validate_account(raw: &Value) -> Result<AccountConfig, ConfigError> {
    let obj = raw
        .as_object()
        .ok_or_else(|| ConfigError::new("Account configuration is invalid."))?;

    let server_url =
        normalize_server_url(&obj.get("server_url").map(json_str).unwrap_or_default())?;
    let login_name = obj
        .get("login_name")
        .map(json_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if login_name.is_empty() {
        return Err(ConfigError::new("Account username is missing."));
    }
    let authentication_type = match obj.get("authentication_type") {
        Some(Value::String(value)) => value.clone(),
        _ => "manual".to_string(),
    };

    let mut folders = Vec::new();
    let mut seen = HashSet::new();
    for folder in obj
        .get("folders")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let folder = validate_folder(&server_url, &login_name, folder)?;
        if !seen.insert(folder.id.clone()) {
            return Err(ConfigError::new(
                "The same local folder and remote path are configured more than once.",
            ));
        }
        folders.push(folder);
    }

    let provider = match obj.get("provider") {
        Some(Value::String(value)) => Provider::from_str(value).unwrap_or_default(),
        _ => Provider::default(),
    };
    let sync = validate_sync(obj.get("sync"))?;
    let delete_guard = validate_delete_guard(obj.get("delete_guard"))?;
    let runtime = validate_runtime(obj.get("runtime"));
    let id = account_id(&server_url, &login_name);

    Ok(AccountConfig {
        id,
        server_url,
        login_name,
        authentication_type,
        provider,
        folders,
        sync,
        delete_guard,
        runtime,
    })
}

fn validate_folder(
    server_url: &str,
    login_name: &str,
    raw: &Value,
) -> Result<FolderConfig, ConfigError> {
    let obj = raw
        .as_object()
        .ok_or_else(|| ConfigError::new("Folder configuration is invalid."))?;
    let local_root_default = default_sync_root().to_string_lossy().into_owned();
    let root = expanduser(
        &obj.get("local_root")
            .map(json_str)
            .unwrap_or(local_root_default),
    );
    if !root.is_absolute() {
        return Err(ConfigError::new(
            "The local synchronization folder must be absolute.",
        ));
    }
    let local_root = root.to_string_lossy().into_owned();
    let remote_path =
        normalize_remote_path(&obj.get("remote_path").map(json_str).unwrap_or_default())?;
    let space_id = match obj.get("space_id") {
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => None,
    };
    let id = folder_fingerprint(server_url, login_name, &local_root, &remote_path);
    Ok(FolderConfig {
        id,
        local_root,
        remote_path,
        space_id,
    })
}

fn validate_sync(raw: Option<&Value>) -> Result<SyncConfig, ConfigError> {
    let mut merged = serde_json::to_value(SyncConfig::default()).expect("sync defaults serialize");
    if let Some(incoming) = raw {
        merged = deep_merge(&merged, incoming);
    }
    let obj = merged.as_object().expect("sync defaults are an object");

    let local_interval_minutes = read_int_value(
        obj,
        "local_interval_minutes",
        5,
        1,
        1440,
        "Invalid setting: local_interval_minutes",
        "local_interval_minutes must be between 1 and 1440.",
    )?;
    let remote_interval_minutes = read_int_value(
        obj,
        "remote_interval_minutes",
        10,
        1,
        1440,
        "Invalid setting: remote_interval_minutes",
        "remote_interval_minutes must be between 1 and 1440.",
    )?;
    let max_sync_retries = read_int_value(
        obj,
        "max_sync_retries",
        3,
        1,
        10,
        "Invalid setting: max_sync_retries",
        "max_sync_retries must be between 1 and 10.",
    )?;

    let exclude_patterns = match merged.get("exclude_patterns") {
        Some(Value::Array(patterns)) => patterns
            .iter()
            .map(|pattern| validate_pattern(&json_str(pattern)))
            .collect::<Result<Vec<_>, _>>()?,
        _ => DEFAULT_PATTERNS.iter().map(|s| s.to_string()).collect(),
    };

    Ok(SyncConfig {
        local_inotify_enabled: get_bool(obj, "local_inotify_enabled", true),
        local_interval_enabled: get_bool(obj, "local_interval_enabled", false),
        local_interval_minutes,
        remote_push_enabled: get_bool(obj, "remote_push_enabled", true),
        remote_interval_enabled: get_bool(obj, "remote_interval_enabled", true),
        remote_interval_minutes,
        max_sync_retries,
        detailed_output: get_bool(obj, "detailed_output", true),
        exclude_patterns_enabled: get_bool(obj, "exclude_patterns_enabled", true),
        exclude_patterns,
    })
}

fn validate_delete_guard(raw: Option<&Value>) -> Result<DeleteGuardConfig, ConfigError> {
    let mut merged =
        serde_json::to_value(DeleteGuardConfig::default()).expect("guard defaults serialize");
    if let Some(incoming) = raw {
        merged = deep_merge(&merged, incoming);
    }
    let obj = merged.as_object().expect("guard defaults are an object");

    let count_threshold = read_int_value(
        obj,
        "count_threshold",
        10,
        1,
        100_000,
        "Invalid deletion guard threshold.",
        "count_threshold must be between 1 and 100000.",
    )?;
    let percent_threshold = read_int_value(
        obj,
        "percent_threshold",
        20,
        1,
        100,
        "Invalid deletion guard threshold.",
        "percent_threshold must be between 1 and 100.",
    )?;

    Ok(DeleteGuardConfig {
        enabled: get_bool(obj, "enabled", true),
        count_threshold,
        percent_threshold,
    })
}

fn validate_runtime(raw: Option<&Value>) -> RuntimeConfig {
    let mut merged =
        serde_json::to_value(RuntimeConfig::default()).expect("runtime defaults serialize");
    if let Some(incoming) = raw {
        merged = deep_merge(&merged, incoming);
    }
    RuntimeConfig {
        last_successful_sync: merged
            .get("last_successful_sync")
            .and_then(Value::as_str)
            .map(str::to_string),
        last_exit_code: merged.get("last_exit_code").and_then(Value::as_i64),
    }
}

fn validate_general(raw: Option<&Value>) -> GeneralConfig {
    let mut merged = json!({ "autostart": true, "pause_on_battery": false });
    if let Some(incoming) = raw {
        merged = deep_merge(&merged, incoming);
    }
    let obj = merged.as_object().expect("general defaults are an object");
    GeneralConfig {
        autostart: get_bool(obj, "autostart", true),
        pause_on_battery: get_bool(obj, "pause_on_battery", false),
        color_scheme: get_string(obj, "color_scheme", &default_color_scheme()),
        show_notifications: get_bool(obj, "show_notifications", true),
        show_server_notifications: get_bool(obj, "show_server_notifications", false),
    }
}

fn validate_logging(raw: Option<&Value>) -> Result<LoggingConfig, ConfigError> {
    let mut merged = json!({ "save_logs": true, "retention_days": 30 });
    if let Some(incoming) = raw {
        merged = deep_merge(&merged, incoming);
    }
    let obj = merged.as_object().expect("logging defaults are an object");
    let retention_days = read_int_value(
        obj,
        "retention_days",
        30,
        1,
        365,
        "Invalid setting: retention_days",
        "retention_days must be between 1 and 365.",
    )?;
    Ok(LoggingConfig {
        save_logs: get_bool(obj, "save_logs", true),
        retention_days,
    })
}

fn validate_network(raw: Option<&Value>) -> Result<NetworkConfig, ConfigError> {
    let mut merged = json!({ "custom_proxy": Value::Null, "trust_invalid_certificates": false });
    if let Some(incoming) = raw {
        merged = deep_merge(&merged, incoming);
    }
    let proxy = merged.get("custom_proxy").cloned().unwrap_or(Value::Null);
    let custom_proxy = if json_truthy(&proxy) {
        let text = json_str(&proxy);
        validate_proxy_url(&text)?;
        Some(text)
    } else {
        None
    };
    let obj = merged.as_object().expect("network defaults are an object");
    Ok(NetworkConfig {
        custom_proxy,
        trust_invalid_certificates: get_bool(obj, "trust_invalid_certificates", false),
        reduce_transfer_impact: get_bool(obj, "reduce_transfer_impact", false),
        allowed_ssids: match obj.get("allowed_ssids") {
            Some(Value::String(text)) if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        },
    })
}

fn validate_proxy_url(value: &str) -> Result<(), ConfigError> {
    let (scheme, netloc, _) = split_url(value.trim());
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") || netloc.is_empty() || has_credentials(&netloc)
    {
        return Err(ConfigError::new(
            "The custom proxy must be an HTTP(S) URL without embedded credentials.",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Small helpers.
// ---------------------------------------------------------------------------

/// Split an URL into `(scheme, netloc, path)` approximating `urllib.parse.urlsplit`
/// for http/https inputs: netloc runs to the first `/`, `?` or `#`.
fn split_url(value: &str) -> (String, String, String) {
    let (scheme, rest) = match value.find("://") {
        Some(idx) => (value[..idx].to_string(), &value[idx + 3..]),
        None => (String::new(), value),
    };
    let (netloc, tail) = match rest.find(&['/', '?', '#'][..]) {
        Some(idx) => (rest[..idx].to_string(), &rest[idx..]),
        None => (rest.to_string(), ""),
    };
    let path = match tail.find(&['?', '#'][..]) {
        Some(idx) => &tail[..idx],
        None => tail,
    };
    (scheme, netloc, path.to_string())
}

/// Whether the netloc embeds a `user[:password]@` userinfo.
fn has_credentials(netloc: &str) -> bool {
    let Some(at) = netloc.rfind('@') else {
        return false;
    };
    let userinfo = &netloc[..at];
    match userinfo.find(':') {
        Some(idx) => !userinfo[..idx].is_empty() || !userinfo[idx + 1..].is_empty(),
        None => !userinfo.is_empty(),
    }
}

/// Coerce a JSON value to a string like Python's `str(value)`.
fn json_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Python-like truthiness for JSON values.
fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_i64() != Some(0) && number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Recursive merge of `default` with `incoming` (incoming wins).
fn deep_merge(default: &Value, incoming: &Value) -> Value {
    match (default, incoming) {
        (Value::Object(base), Value::Object(extra)) => {
            let mut merged = base.clone();
            for (key, value) in extra {
                if let Some(existing) = merged.get(key) {
                    merged.insert(key.clone(), deep_merge(existing, value));
                } else {
                    merged.insert(key.clone(), value.clone());
                }
            }
            Value::Object(merged)
        }
        _ => incoming.clone(),
    }
}

/// Read a JSON number as `i64` (accepting numeric strings), validating range.
fn read_int_value(
    obj: &Map<String, Value>,
    key: &str,
    default: i64,
    lower: i64,
    upper: i64,
    invalid_message: &str,
    range_message: &str,
) -> Result<i64, ConfigError> {
    let Some(value) = obj.get(key) else {
        return Ok(default);
    };
    let parsed = match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|u| i64::try_from(u).ok())),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    };
    let Some(value) = parsed else {
        return Err(ConfigError::new(invalid_message));
    };
    if value < lower || value > upper {
        return Err(ConfigError::new(range_message));
    }
    Ok(value)
}

/// Read a JSON boolean, tolerating `"true"`/`"false"` strings.
fn get_bool(obj: &Map<String, Value>, key: &str, default: bool) -> bool {
    match obj.get(key) {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => match text.to_ascii_lowercase().as_str() {
            "true" => true,
            "false" => false,
            _ => default,
        },
        _ => default,
    }
}

fn get_string(obj: &Map<String, Value>, key: &str, default: &str) -> String {
    match obj.get(key) {
        Some(Value::String(text)) => text.clone(),
        _ => default.to_string(),
    }
}

fn sha256_hex(input: &[u8]) -> String {
    let digest = Sha256::digest(input);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn default_sync_value() -> Value {
    serde_json::to_value(SyncConfig::default()).expect("sync defaults serialize")
}

fn default_runtime_value() -> Value {
    serde_json::to_value(RuntimeConfig::default()).expect("runtime defaults serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    // ---- normalize_server_url -------------------------------------------------

    #[test]
    fn url_normalizes_scheme_path_and_drops_query_fragment() {
        assert_eq!(
            normalize_server_url("  HTTPS://Host.example.com/nextcloud/  ").unwrap(),
            "https://Host.example.com/nextcloud"
        );
        assert_eq!(
            normalize_server_url("http://example.com/").unwrap(),
            "http://example.com"
        );
        assert_eq!(
            normalize_server_url("https://example.com/path?query=1#frag").unwrap(),
            "https://example.com/path"
        );
        assert_eq!(
            normalize_server_url("https://example.com").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn url_rejects_invalid_inputs() {
        for bad in [
            "example.com",
            "ftp://example.com",
            "https://",
            "",
            "https://user@example.com/",
            "https://user:pass@example.com/",
        ] {
            assert!(
                normalize_server_url(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        let err = normalize_server_url("example.com").unwrap_err();
        assert!(err.message.contains("complete HTTP or HTTPS"));
        let err = normalize_server_url("https://user:pass@example.com").unwrap_err();
        assert!(err.message.contains("Credentials must not be included"));
    }

    // ---- normalize_remote_path ------------------------------------------------

    #[test]
    fn remote_root_is_empty_string() {
        assert_eq!(normalize_remote_path("").unwrap(), "");
        assert_eq!(normalize_remote_path("/").unwrap(), "");
    }

    #[test]
    fn remote_trailing_slash_and_dot_segments_removed() {
        assert_eq!(normalize_remote_path("/Documents/").unwrap(), "/Documents");
        assert_eq!(normalize_remote_path("/./a//b/.").unwrap(), "/a/b");
        assert_eq!(normalize_remote_path("Documents").unwrap(), "/Documents");
    }

    #[test]
    fn remote_rejects_parent_references() {
        let err = normalize_remote_path("/a/../b").unwrap_err();
        assert!(err.message.contains("parent directory references"));
    }

    #[test]
    fn remote_rejects_urls() {
        let err = normalize_remote_path("https://example.com/x").unwrap_err();
        assert!(err.message.contains("must be a path"));
    }

    #[test]
    fn remote_rejects_backslash_query_and_fragment() {
        assert!(normalize_remote_path("\\foo").is_err());
        assert!(normalize_remote_path("/a?b").is_err());
        assert!(normalize_remote_path("/a#b").is_err());
        assert!(normalize_remote_path("\0").is_err());
    }

    // ---- identities -----------------------------------------------------------

    #[test]
    fn account_id_is_deterministic_and_casefolded() {
        let a = account_id("https://cloud.example.com/nextcloud", "alice@example.com");
        let b = account_id("https://CLOUD.EXAMPLE.COM/nextcloud/", "ALICE@example.com");
        let c = account_id("https://example.com", "bob");
        assert_eq!(
            a,
            "5fcc57b6eeae77370e1f1b1a1a608d97511bab8cf29c0e02beabeb3e9a393592"
        );
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(
            a,
            account_id("https://cloud.example.com/nextcloud", "alice@example.com")
        );
    }

    #[test]
    fn account_id_matches_real_python_value() {
        assert_eq!(
            account_id("https://hub.domatix.com", "nacho@domatix.com"),
            "f4d9b6644792ad9e662e29d0acd15ce2afc779f849ba10a189d6052a125042e9"
        );
    }

    #[test]
    fn folder_fingerprint_is_deterministic() {
        let a = folder_fingerprint(
            "https://cloud.example.com",
            "alice@example.com",
            "/home/alice/NextCloud",
            "/Documents",
        );
        let b = folder_fingerprint(
            "https://cloud.example.com/",
            "alice@example.com",
            "/home/alice/NextCloud",
            "/Documents",
        );
        let c = folder_fingerprint(
            "https://cloud.example.com",
            "alice@example.com",
            "/home/alice/NextCloud",
            "",
        );
        assert_eq!(
            a,
            "5f41be4bf49f372e3885159f5e6923e8769b5d59ee64d09b1b85c8fb8ce9b0a7"
        );
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // ---- validation -----------------------------------------------------------

    #[test]
    fn account_requires_login_name() {
        let cfg = json!({
            "schema_version": 6,
            "accounts": [{ "server_url": "https://cloud.example.com", "login_name": "" }]
        });
        let err = validate_config(cfg).unwrap_err();
        assert!(err.message.contains("Account username is missing"));
    }

    #[test]
    fn account_requires_absolute_local_root() {
        let cfg = json!({
            "schema_version": 6,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "folders": [{ "local_root": "relative/path", "remote_path": "" }]
            }]
        });
        let err = validate_config(cfg).unwrap_err();
        assert!(err.message.contains("must be absolute"));
    }

    #[test]
    fn duplicate_folder_rejected() {
        let cfg = json!({
            "schema_version": 6,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "folders": [
                    { "local_root": "/home/alice/NC", "remote_path": "/A" },
                    { "local_root": "/home/alice/NC", "remote_path": "/A" }
                ]
            }]
        });
        let err = validate_config(cfg).unwrap_err();
        assert!(err.message.contains("configured more than once"));
    }

    #[test]
    fn sync_interval_validation() {
        let bad_minutes = json!({
            "schema_version": 6,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "sync": { "local_interval_minutes": 0 }
            }]
        });
        let err = validate_config(bad_minutes).unwrap_err();
        assert!(err
            .message
            .contains("local_interval_minutes must be between 1 and 1440"));

        let bad_retries = json!({
            "schema_version": 6,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "sync": { "max_sync_retries": 11 }
            }]
        });
        let err = validate_config(bad_retries).unwrap_err();
        assert!(err
            .message
            .contains("max_sync_retries must be between 1 and 10"));
    }

    #[test]
    fn delete_guard_validation() {
        let bad_count = json!({
            "schema_version": 6,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "delete_guard": { "count_threshold": 0 }
            }]
        });
        let err = validate_config(bad_count).unwrap_err();
        assert!(err
            .message
            .contains("count_threshold must be between 1 and 100000"));

        let bad_percent = json!({
            "schema_version": 6,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "delete_guard": { "percent_threshold": 101 }
            }]
        });
        let err = validate_config(bad_percent).unwrap_err();
        assert!(err
            .message
            .contains("percent_threshold must be between 1 and 100"));
    }

    #[test]
    fn future_schema_rejected() {
        let cfg = json!({ "schema_version": 8 });
        let err = validate_config(cfg).unwrap_err();
        assert!(err
            .message
            .contains("Configuration schema 8 is newer than this application supports"));
    }

    #[test]
    fn retention_days_and_proxy_validation() {
        let err = validate_config(json!({ "logging": { "retention_days": 0 } })).unwrap_err();
        assert!(err
            .message
            .contains("retention_days must be between 1 and 365"));

        assert!(validate_config(json!({
            "network": { "custom_proxy": "https://proxy.example.com" }
        }))
        .is_ok());
        let err = validate_config(json!({
            "network": { "custom_proxy": "ftp://proxy.example.com" }
        }))
        .unwrap_err();
        assert!(err.message.contains("custom proxy"));
    }

    // ---- migrations -----------------------------------------------------------

    #[test]
    fn migrate_v5_to_v6_moves_local_root_into_folders() {
        let cfg = json!({
            "schema_version": 5,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "authentication_type": "manual",
                "local_root": "/home/alice/NC",
                "remote_path": "/Docs"
            }]
        });
        let config = validate_config(cfg).unwrap();
        assert_eq!(config.schema_version, 7);
        assert_eq!(config.accounts.len(), 1);
        let account = &config.accounts[0];
        assert_eq!(account.folders.len(), 1);
        assert_eq!(account.folders[0].local_root, "/home/alice/NC");
        assert_eq!(account.folders[0].remote_path, "/Docs");
        assert_eq!(
            account.folders[0].id,
            folder_fingerprint(
                "https://cloud.example.com",
                "alice",
                "/home/alice/NC",
                "/Docs"
            )
        );
        assert_eq!(account.id, account_id("https://cloud.example.com", "alice"));
    }

    #[test]
    fn migrate_v3_moves_root_account_into_accounts() {
        let cfg = json!({
            "schema_version": 3,
            "account": {
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "local_root": "/home/alice/NC",
                "remote_path": "/"
            },
            "sync": { "local_interval_minutes": 15 }
        });
        let config = validate_config(cfg).unwrap();
        let account = &config.accounts[0];
        assert_eq!(account.login_name, "alice");
        assert_eq!(account.folders[0].local_root, "/home/alice/NC");
        assert_eq!(account.folders[0].remote_path, "");
        assert_eq!(account.sync.local_interval_minutes, 15);
    }

    #[test]
    fn migrate_v6_preserves_existing_folders() {
        let cfg = json!({
            "schema_version": 6,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "local_root": "/ignored",
                "folders": [{ "local_root": "/home/alice/NC", "remote_path": "/A" }]
            }]
        });
        let config = validate_config(cfg).unwrap();
        let account = &config.accounts[0];
        assert_eq!(account.folders.len(), 1);
        assert_eq!(account.folders[0].local_root, "/home/alice/NC");
        assert_eq!(account.folders[0].remote_path, "/A");
    }

    #[test]
    fn v6_account_without_local_root_keeps_empty_folders() {
        let cfg = json!({
            "schema_version": 6,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice"
            }]
        });
        let config = validate_config(cfg).unwrap();
        assert!(config.accounts[0].folders.is_empty());
    }

    #[test]
    fn migrate_v6_to_v7_adds_provider_default() {
        let cfg = json!({
            "schema_version": 6,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice"
            }]
        });
        let config = validate_config(cfg).unwrap();
        assert_eq!(config.schema_version, 7);
        assert_eq!(config.accounts[0].provider, Provider::Nextcloud);
    }

    #[test]
    fn provider_parsed_from_account() {
        let cfg = json!({
            "schema_version": 7,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "provider": "opencloud"
            }]
        });
        let config = validate_config(cfg).unwrap();
        assert_eq!(config.accounts[0].provider, Provider::OpenCloud);
    }

    #[test]
    fn unknown_provider_falls_back_to_nextcloud() {
        let cfg = json!({
            "schema_version": 7,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "provider": "kde"
            }]
        });
        let config = validate_config(cfg).unwrap();
        assert_eq!(config.accounts[0].provider, Provider::Nextcloud);
    }

    #[test]
    fn space_id_optional_and_parsed() {
        let cfg = json!({
            "schema_version": 7,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "provider": "opencloud",
                "folders": [
                    { "local_root": "/home/alice/OC", "remote_path": "", "space_id": "space:abcd" },
                    { "local_root": "/home/alice/OC2", "remote_path": "" }
                ]
            }]
        });
        let config = validate_config(cfg).unwrap();
        let folders = &config.accounts[0].folders;
        assert_eq!(folders[0].space_id.as_deref(), Some("space:abcd"));
        assert_eq!(folders[1].space_id, None);
    }

    #[test]
    fn drop_v4_safety_fields() {
        let cfg = json!({
            "schema_version": 4,
            "safety": { "baseline": "x" },
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "local_root": "/home/alice/NC",
                "remote_path": "",
                "safety": { "baseline": "y" },
                "bootstrap_complete": true
            }]
        });
        let config = validate_config(cfg).unwrap();
        assert_eq!(config.accounts[0].folders.len(), 1);
    }

    #[test]
    fn reads_legacy_python_file_with_root_level_aliases() {
        // The Python v0.2.x also writes `account`, `folders`, `sync` and
        // `runtime` at the root level; those are ignored when reading.
        let cfg = json!({
            "schema_version": 6,
            "accounts": [{
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "authentication_type": "browser",
                "folders": [],
                "id": "old-id",
                "sync": { "max_sync_retries": 2 },
                "delete_guard": { "enabled": false, "count_threshold": 50, "percent_threshold": 30 }
            }],
            "general": { "autostart": false },
            "account": {
                "id": "old-id",
                "server_url": "https://cloud.example.com",
                "login_name": "alice",
                "local_root": "/home/alice/NC",
                "remote_path": ""
            },
            "sync": { "max_sync_retries": 99 },
            "runtime": { "last_exit_code": 1 }
        });
        let config = validate_config(cfg).unwrap();
        let account = &config.accounts[0];
        assert_eq!(account.id, account_id("https://cloud.example.com", "alice"));
        assert!(account.folders.is_empty());
        assert_eq!(account.sync.max_sync_retries, 2);
        assert!(account.sync.local_inotify_enabled);
        assert!(!config.general.autostart);
        assert!(!config.general.pause_on_battery);
        assert!(!config.general.show_server_notifications);
        assert_eq!(account.delete_guard.count_threshold, 50);
    }

    // ---- file IO --------------------------------------------------------------

    #[test]
    fn save_load_roundtrip_with_0600_permissions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let store = ConfigStore::with_path(path.clone());

        let mut config = Config::default();
        config.general.autostart = false;
        config.accounts.push(AccountConfig {
            id: account_id("https://cloud.example.com", "alice"),
            server_url: "https://cloud.example.com".to_string(),
            login_name: "alice".to_string(),
            authentication_type: "manual".to_string(),
            provider: Provider::OpenCloud,
            folders: vec![FolderConfig {
                id: folder_fingerprint(
                    "https://cloud.example.com",
                    "alice",
                    "/home/alice/NC",
                    "/Docs",
                ),
                local_root: "/home/alice/NC".to_string(),
                remote_path: "/Docs".to_string(),
                space_id: Some("space:abcd".to_string()),
            }],
            sync: SyncConfig::default(),
            delete_guard: DeleteGuardConfig::default(),
            runtime: RuntimeConfig::default(),
        });

        store.save(&config).unwrap();
        assert!(path.exists());

        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);

        let loaded = store.load().unwrap();
        assert_eq!(loaded, config);

        let raw: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let obj = raw.as_object().unwrap();
        assert_eq!(obj.get("schema_version"), Some(&json!(7)));
        assert!(!obj.contains_key("account"));
        assert!(!obj.contains_key("sync"));
        assert!(!obj.contains_key("runtime"));
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::with_path(dir.path().join("settings.json"));
        let config = store.load().unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.schema_version, 7);
        assert!(config.accounts.is_empty());
    }

    #[test]
    fn server_notifications_preference_roundtrips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let store = ConfigStore::with_path(path.clone());

        let mut config = Config::default();
        config.general.show_server_notifications = true;
        store.save(&config).unwrap();
        let loaded = store.load().unwrap();
        assert!(loaded.general.show_server_notifications);

        // Older config files without the key keep the (off) default.
        let dir = tempdir().unwrap();
        let legacy = dir.path().join("settings.json");
        std::fs::write(&legacy, r#"{"general":{"show_notifications":true}}"#).unwrap();
        let store = ConfigStore::with_path(legacy);
        assert!(!store.load().unwrap().general.show_server_notifications);
    }

    #[test]
    fn load_rejects_future_schema_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, json!({ "schema_version": 99 }).to_string()).unwrap();
        let store = ConfigStore::with_path(path);
        let err = store.load().unwrap_err();
        assert!(err.message.contains("newer than this application supports"));
    }

    #[test]
    fn empty_document_validates_to_defaults() {
        let config = validate_config(json!({})).unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.schema_version, 7);
        assert!(config.accounts.is_empty());
    }

    #[test]
    fn validate_pattern_rejects_broad_and_path_patterns() {
        for bad in ["*", ".*", "*.*", "a/b", "..foo", ""] {
            assert!(
                validate_pattern(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        assert_eq!(validate_pattern("  *.tmp ").unwrap(), "*.tmp");
    }

    // ---- remote_path_for --------------------------------------------------

    #[test]
    fn remote_path_for_blank_uses_local_folder_name() {
        assert_eq!(
            remote_path_for("/home/user/NextCloud", "").unwrap(),
            "/NextCloud"
        );
        assert_eq!(
            remote_path_for("/home/user/NextCloud", "   ").unwrap(),
            "/NextCloud"
        );
        assert_eq!(remote_path_for("~/NextCloud", "").unwrap(), "/NextCloud");
    }

    #[test]
    fn remote_path_for_explicit_root_keeps_account_root() {
        assert_eq!(remote_path_for("/home/user/NextCloud", "/").unwrap(), "");
    }

    #[test]
    fn remote_path_for_typed_value_is_normalized() {
        assert_eq!(
            remote_path_for("/home/user/NextCloud", "Documents").unwrap(),
            "/Documents"
        );
        assert_eq!(
            remote_path_for("/home/user/NextCloud", "/Documents/").unwrap(),
            "/Documents"
        );
    }

    // ---- account mutations -------------------------------------------------

    fn account_fixture(server_url: &str, login_name: &str) -> AccountConfig {
        AccountConfig {
            id: account_id(server_url, login_name),
            server_url: server_url.to_string(),
            login_name: login_name.to_string(),
            authentication_type: "browser".to_string(),
            provider: Provider::Nextcloud,
            folders: Vec::new(),
            sync: SyncConfig::default(),
            delete_guard: DeleteGuardConfig::default(),
            runtime: RuntimeConfig::default(),
        }
    }

    fn folder_fixture(local_root: &str, remote_path: &str) -> FolderConfig {
        FolderConfig {
            id: "bogus-id".to_string(),
            local_root: local_root.to_string(),
            remote_path: remote_path.to_string(),
            space_id: None,
        }
    }

    #[test]
    fn add_and_remove_account_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let store = ConfigStore::with_path(path.clone());

        let first_id = store
            .add_account(&account_fixture("https://cloud.example.com", "alice"))
            .unwrap();
        let second_id = store
            .add_account(&account_fixture("https://work.example.com", "bob"))
            .unwrap();
        assert_ne!(first_id, second_id);
        assert_eq!(
            store.account(&first_id).unwrap().unwrap().login_name,
            "alice"
        );

        assert!(store.remove_account(&second_id).unwrap());
        assert!(store.account(&second_id).unwrap().is_none());
        assert!(!store.remove_account(&second_id).unwrap());

        let reloaded = ConfigStore::with_path(path);
        let config = reloaded.load().unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].login_name, "alice");
    }

    #[test]
    fn add_duplicate_account_is_rejected() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::with_path(dir.path().join("settings.json"));
        store
            .add_account(&account_fixture("https://cloud.example.com", "alice"))
            .unwrap();
        let err = store
            .add_account(&account_fixture("https://cloud.example.com", "alice"))
            .unwrap_err();
        assert!(err.message.contains("same server and username"));

        let err = store
            .add_account(&account_fixture("https://cloud.example.com", "ALICE"))
            .unwrap_err();
        assert!(err.message.contains("same server and username"));
    }

    #[test]
    fn add_account_normalizes_server_url() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::with_path(dir.path().join("settings.json"));
        let id = store
            .add_account(&account_fixture("HTTPS://cloud.example.com/", "alice"))
            .unwrap();
        let stored = store.account(&id).unwrap().unwrap();
        assert_eq!(stored.server_url, "https://cloud.example.com");
    }

    #[test]
    fn account_getter_returns_none_for_missing() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::with_path(dir.path().join("settings.json"));
        assert!(store.account("missing").unwrap().is_none());
    }

    // ---- folder mutations -------------------------------------------------

    #[test]
    fn add_and_remove_folder_idempotent() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::with_path(dir.path().join("settings.json"));
        let account_id = store
            .add_account(&account_fixture("https://cloud.example.com", "alice"))
            .unwrap();

        let first_id = store
            .add_folder(&account_id, &folder_fixture("/tmp/NextCloud", ""))
            .unwrap();
        // The id is recomputed over normalized values, ignoring the caller's.
        assert_eq!(
            first_id,
            folder_fingerprint("https://cloud.example.com", "alice", "/tmp/NextCloud", "")
        );

        let second_id = store
            .add_folder(&account_id, &folder_fixture("/tmp/Second", "Docs"))
            .unwrap();
        let account = store.account(&account_id).unwrap().unwrap();
        assert_eq!(account.folders.len(), 2);
        assert_eq!(account.folders[1].local_root, "/tmp/Second");
        assert_eq!(account.folders[1].remote_path, "/Docs");
        assert_eq!(account.folders[1].id, second_id);

        let err = store
            .add_folder(&account_id, &folder_fixture("/tmp/NextCloud", ""))
            .unwrap_err();
        assert!(err.message.contains("already configured"));

        assert!(store.remove_folder(&account_id, &second_id).unwrap());
        assert_eq!(
            store.account(&account_id).unwrap().unwrap().folders.len(),
            1
        );
        assert!(!store.remove_folder(&account_id, &second_id).unwrap());
        assert!(!store.remove_folder("missing", &second_id).unwrap());
    }

    #[test]
    fn add_folder_rejects_relative_local_root_and_missing_account() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::with_path(dir.path().join("settings.json"));
        let account_id = store
            .add_account(&account_fixture("https://cloud.example.com", "alice"))
            .unwrap();

        let err = store
            .add_folder(&account_id, &folder_fixture("relative/path", ""))
            .unwrap_err();
        assert!(err.message.contains("must be absolute"));

        let err = store
            .add_folder("missing", &folder_fixture("/tmp/X", ""))
            .unwrap_err();
        assert!(err.message.contains("Account not found."));
    }

    // ---- update_account ---------------------------------------------------

    #[test]
    fn update_account_persists_changes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let store = ConfigStore::with_path(path.clone());
        let id = store
            .add_account(&account_fixture("https://cloud.example.com", "alice"))
            .unwrap();

        let mut account = store.account(&id).unwrap().unwrap();
        account.sync.local_interval_minutes = 42;
        account.delete_guard.enabled = false;
        account.runtime.last_exit_code = Some(7);
        store.update_account(&account).unwrap();

        let reloaded = ConfigStore::with_path(path);
        let stored = &reloaded.load().unwrap().accounts[0];
        assert_eq!(stored.sync.local_interval_minutes, 42);
        assert!(!stored.delete_guard.enabled);
        assert_eq!(stored.runtime.last_exit_code, Some(7));
        assert_eq!(stored.id, id);
    }

    #[test]
    fn update_account_missing_is_an_error() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::with_path(dir.path().join("settings.json"));
        let account = account_fixture("https://cloud.example.com", "alice");
        let err = store.update_account(&account).unwrap_err();
        assert!(err.message.contains("Account not found."));
    }

    // ---- persistence ------------------------------------------------------

    #[test]
    fn mutations_persist_across_stores() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let writer = ConfigStore::with_path(path.clone());
        let id = writer
            .add_account(&account_fixture("https://cloud.example.com", "alice"))
            .unwrap();
        writer
            .add_folder(&id, &folder_fixture("/tmp/NextCloud", ""))
            .unwrap();
        writer
            .add_folder(&id, &folder_fixture("/tmp/Docs", "/Documents"))
            .unwrap();

        let reader = ConfigStore::with_path(path);
        let config = reader.load().unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].folders.len(), 2);
        let account = reader.account(&id).unwrap().unwrap();
        assert_eq!(account.folders[1].remote_path, "/Documents");
    }
}
