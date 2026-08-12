//! Configuration storage.
//!
//! Fase 1 (Task 1.1): serde model of schema v6
//! (`Config { general, accounts }`, `AccountConfig { id, server, login,
//! folders }`, `FolderConfig { local_root, remote_path }`) saved under
//! `~/.config/nextsync/` or `~/.local/share/nextsync/`.

/// Placeholder for the configuration store.
pub struct Config;

impl Config {
    /// Returns the number of configured accounts.
    ///
    /// Placeholder: always `0` until Fase 1 lands.
    pub fn account_count() -> usize {
        0
    }
}
