//! Path resolution.
//!
//! Fase 1 + Fase 3 + Task 5.6: resolve config and state directories,
//! mirroring the Python `util/paths.py` (`$XDG_CONFIG_HOME/nextsync` and
//! `$XDG_STATE_HOME/nextsync`, with `~/.config` / `~/.local/state`
//! fallbacks). `state_dir()` hosts the deletion-guard manifests; the Task 5.6
//! additions (`autostart_dir`, `gtk_bookmarks_path`, `user_data_dir` and
//! `desktop_dir`) are the XDG paths the autostart entry and the desktop
//! integration live in.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// The `nextsync` application directory name.
pub const APP_DIR: &str = "nextsync";

/// `$XDG_CONFIG_HOME/nextsync` (default `~/.config/nextsync`).
pub fn config_dir() -> PathBuf {
    config_home().join(APP_DIR)
}

/// `$XDG_CONFIG_HOME` (default `~/.config`), without the app directory.
fn config_home() -> PathBuf {
    xdg("XDG_CONFIG_HOME", &[".config"])
}

/// `$XDG_CONFIG_HOME/autostart` (default `~/.config/autostart`). Mirrors
/// `paths.py::autostart_dir`.
pub fn autostart_dir() -> PathBuf {
    config_home().join("autostart")
}

/// `$XDG_CONFIG_HOME/gtk-3.0/bookmarks` (default
/// `~/.config/gtk-3.0/bookmarks`). Mirrors `paths.py::gtk_bookmarks_path`.
pub fn gtk_bookmarks_path() -> PathBuf {
    config_home().join("gtk-3.0").join("bookmarks")
}

/// `$XDG_DATA_HOME` (default `~/.local/share`). Mirrors
/// `paths.py::user_data_dir`.
pub fn user_data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", &[".local", "share"])
}

/// The XDG desktop directory without invoking a shell command. Mirrors
/// `paths.py::desktop_dir`.
pub fn desktop_dir() -> PathBuf {
    desktop_dir_from(&config_home())
}

/// Resolve the XDG desktop directory against a given config home. Replicates
/// `paths.py::desktop_dir`: an `XDG_DESKTOP_DIR` override wins, then the
/// `XDG_DESKTOP_DIR="..."` line of `<config_home>/user-dirs.dirs` (with
/// `$HOME`/`${HOME}` expansion), then `~/Desktop`.
pub(crate) fn desktop_dir_from(config_home: &Path) -> PathBuf {
    match env::var_os("XDG_DESKTOP_DIR") {
        Some(value) if !value.is_empty() => return PathBuf::from(value),
        _ => {}
    }
    let user_dirs = config_home.join("user-dirs.dirs");
    let content = match fs::read_to_string(&user_dirs) {
        Ok(content) => content,
        Err(_) => return home_dir().join("Desktop"),
    };
    let home = home_dir();
    for line in content.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(inner) = line.strip_prefix("XDG_DESKTOP_DIR=\"") {
            if let Some(value) = inner.strip_suffix('"') {
                let home = home.to_string_lossy();
                let expanded = value.replace("${HOME}", &home).replace("$HOME", &home);
                let candidate = PathBuf::from(expanded);
                if candidate.is_absolute() {
                    return candidate;
                }
            }
        }
    }
    home.join("Desktop")
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
    use std::ffi::OsString;

    /// Restore an environment variable mutated by a test.
    fn restore_env(var: &str, previous: Option<OsString>) {
        match previous {
            Some(value) => env::set_var(var, value),
            None => env::remove_var(var),
        }
    }

    #[test]
    fn state_dir_uses_xdg_state_home_when_set() {
        let _env = crate::util::test_env::lock();
        let previous = env::var_os("XDG_STATE_HOME");
        env::set_var("XDG_STATE_HOME", "/tmp/nxs-state");
        let result = state_dir();
        restore_env("XDG_STATE_HOME", previous);
        assert_eq!(result, PathBuf::from("/tmp/nxs-state/nextsync"));
    }

    #[test]
    fn config_dir_uses_xdg_config_home_when_set() {
        let _env = crate::util::test_env::lock();
        let previous = env::var_os("XDG_CONFIG_HOME");
        env::set_var("XDG_CONFIG_HOME", "/tmp/nxs-config");
        let result = config_dir();
        restore_env("XDG_CONFIG_HOME", previous);
        assert_eq!(result, PathBuf::from("/tmp/nxs-config/nextsync"));
    }

    #[test]
    fn empty_variable_falls_back_to_home() {
        let _env = crate::util::test_env::lock();
        let previous = env::var_os("XDG_STATE_HOME");
        env::remove_var("XDG_STATE_HOME");
        let result = state_dir();
        restore_env("XDG_STATE_HOME", previous);
        assert_eq!(result, home_dir().join(".local/state/nextsync"));
    }

    #[test]
    fn autostart_dir_uses_config_home() {
        let _env = crate::util::test_env::lock();
        let previous = env::var_os("XDG_CONFIG_HOME");
        env::set_var("XDG_CONFIG_HOME", "/tmp/nxs-cfg");
        let result = autostart_dir();
        restore_env("XDG_CONFIG_HOME", previous);
        assert_eq!(result, PathBuf::from("/tmp/nxs-cfg/autostart"));
    }

    #[test]
    fn gtk_bookmarks_path_uses_config_home() {
        let _env = crate::util::test_env::lock();
        let previous = env::var_os("XDG_CONFIG_HOME");
        env::set_var("XDG_CONFIG_HOME", "/tmp/nxs-cfg");
        let result = gtk_bookmarks_path();
        restore_env("XDG_CONFIG_HOME", previous);
        assert_eq!(result, PathBuf::from("/tmp/nxs-cfg/gtk-3.0/bookmarks"));
    }

    #[test]
    fn user_data_dir_uses_xdg_data_home() {
        let _env = crate::util::test_env::lock();
        let previous = env::var_os("XDG_DATA_HOME");
        env::set_var("XDG_DATA_HOME", "/tmp/nxs-data");
        let result = user_data_dir();
        restore_env("XDG_DATA_HOME", previous);
        assert_eq!(result, PathBuf::from("/tmp/nxs-data"));
    }

    #[test]
    fn user_data_dir_falls_back_to_local_share() {
        let _env = crate::util::test_env::lock();
        let previous = env::var_os("XDG_DATA_HOME");
        env::remove_var("XDG_DATA_HOME");
        let result = user_data_dir();
        restore_env("XDG_DATA_HOME", previous);
        assert_eq!(result, home_dir().join(".local/share"));
    }

    #[test]
    fn desktop_dir_prefers_xdg_desktop_dir_override() {
        let _env = crate::util::test_env::lock();
        let previous = env::var_os("XDG_DESKTOP_DIR");
        env::set_var("XDG_DESKTOP_DIR", "/tmp/desktop");
        let result = desktop_dir();
        restore_env("XDG_DESKTOP_DIR", previous);
        assert_eq!(result, PathBuf::from("/tmp/desktop"));
    }

    #[test]
    fn desktop_dir_parses_user_dirs_dirs() {
        let config = tempfile::tempdir().unwrap();
        fs::create_dir_all(config.path()).unwrap();
        fs::write(
            config.path().join("user-dirs.dirs"),
            "XDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\nXDG_DESKTOP_DIR=\"${HOME}/Escritorio\"\n",
        )
        .unwrap();
        let result = desktop_dir_from(config.path());
        assert_eq!(result, home_dir().join("Escritorio"));
    }

    #[test]
    fn desktop_dir_falls_back_to_home_desktop() {
        let config = tempfile::tempdir().unwrap();
        let result = desktop_dir_from(config.path());
        assert_eq!(result, home_dir().join("Desktop"));
    }
}
