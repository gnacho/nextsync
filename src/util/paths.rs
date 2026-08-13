//! Path resolution.
//!
//! Fase 1 + Fase 3: resolve config and state directories, mirroring the
//! Python `util/paths.py` (`$XDG_CONFIG_HOME/nextsync` and
//! `$XDG_STATE_HOME/nextsync`, with `~/.config` / `~/.local/state` fallbacks).
//! `state_dir()` hosts the deletion-guard manifests.

use std::env;
use std::path::PathBuf;

/// The `nextsync` application directory name.
pub const APP_DIR: &str = "nextsync";

/// `$XDG_CONFIG_HOME/nextsync` (default `~/.config/nextsync`).
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", &[".config"]).join(APP_DIR)
}

/// `$XDG_STATE_HOME/nextsync` (default `~/.local/state/nextsync`).
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", &[".local", "state"]).join(APP_DIR)
}

/// `$HOME` with a `/` fallback, like the other XDG helpers.
fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Resolve one XDG base directory from its environment variable, falling back
/// to a `$HOME`-relative default when unset or empty.
fn xdg(variable: &str, fallback_segments: &[&str]) -> PathBuf {
    match env::var_os(variable) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => {
            let mut base = home_dir();
            for segment in fallback_segments {
                base.push(segment);
            }
            base
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_uses_xdg_state_home_when_set() {
        let previous = env::var_os("XDG_STATE_HOME");
        env::set_var("XDG_STATE_HOME", "/tmp/nxs-state");
        let result = state_dir();
        if let Some(value) = previous {
            env::set_var("XDG_STATE_HOME", value);
        } else {
            env::remove_var("XDG_STATE_HOME");
        }
        assert_eq!(result, PathBuf::from("/tmp/nxs-state/nextsync"));
    }

    #[test]
    fn config_dir_uses_xdg_config_home_when_set() {
        let previous = env::var_os("XDG_CONFIG_HOME");
        env::set_var("XDG_CONFIG_HOME", "/tmp/nxs-config");
        let result = config_dir();
        if let Some(value) = previous {
            env::set_var("XDG_CONFIG_HOME", value);
        } else {
            env::remove_var("XDG_CONFIG_HOME");
        }
        assert_eq!(result, PathBuf::from("/tmp/nxs-config/nextsync"));
    }

    #[test]
    fn empty_variable_falls_back_to_home() {
        let previous = env::var_os("XDG_STATE_HOME");
        env::remove_var("XDG_STATE_HOME");
        let result = state_dir();
        if let Some(value) = previous {
            env::set_var("XDG_STATE_HOME", value);
        }
        assert_eq!(result, home_dir().join(".local/state/nextsync"));
    }
}
