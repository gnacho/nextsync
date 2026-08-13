//! Autostart registration (Task 5.6).
//!
//! Mirrors `core/autostart.py` (v0.4.0): a single `.desktop` entry in
//! `$XDG_CONFIG_HOME/autostart` (`~/.config/autostart` by default) asks GNOME
//! to launch the app at login with `--background`. The launcher binary comes
//! from `$NEXTSYNC_LAUNCHER` when set (shell-quoted, as the Python does with
//! `shlex.quote`), falling back to `nextsync`.
//!
//! # Deviations from `autostart.py` (motivated)
//!
//! - **`set_enabled` returns `io::Result<()>`** instead of raising; the
//!   idempotent unlink treats a missing file as success (the Python's
//!   `unlink(missing_ok=True)`).
//! - **`command()` is public** (the Python `_command` is private) so tests
//!   and the Settings UI can render the exact `Exec=` value.
//! - **POSIX shell quoting is implemented inline** (the repo avoids adding
//!   dependencies); it matches Python's `shlex.quote` for the safe charset
//!   and single-quotes everything else.
//! - **`is_enabled` reports `path.is_file()`**: the Python had no getter and
//!   the `.desktop` presence is the source of truth.

use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use crate::util::paths;

/// Application ID, used for the autostart file name and the desktop icon.
pub const APP_ID: &str = "io.github.gnacho.nextsync";

/// Owns the autostart `.desktop` entry for the current user.
pub struct AutostartManager {
    path: PathBuf,
}

impl AutostartManager {
    /// Create a manager for the given desktop file, defaulting to
    /// `~/.config/autostart/io.github.gnacho.nextsync.desktop`.
    pub fn new(desktop_path: Option<PathBuf>) -> Self {
        let path = desktop_path
            .unwrap_or_else(|| paths::autostart_dir().join(format!("{APP_ID}.desktop")));
        Self { path }
    }

    /// The `Exec=` value: the launcher from `$NEXTSYNC_LAUNCHER` (shell
    /// quoted) or `nextsync`, always with `--background`. Matches the
    /// Python `_command`.
    pub fn command(&self) -> String {
        match env::var("NEXTSYNC_LAUNCHER") {
            Ok(launcher) if !launcher.is_empty() => {
                format!("{} --background", shell_quote(&launcher))
            }
            _ => "nextsync --background".to_string(),
        }
    }

    /// Enable (write) or disable (unlink) the autostart entry. The file is
    /// written atomically (tmp + rename) with the exact fields of the
    /// Python `set_enabled`.
    pub fn set_enabled(&self, enabled: bool) -> io::Result<()> {
        if !enabled {
            return match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            };
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=NextSync\n\
             Exec={}\n\
             Icon={APP_ID}\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n\
             StartupNotify=false\n\
             \n",
            self.command()
        );
        let temporary = self.path.with_extension("tmp");
        fs::write(&temporary, content)?;
        fs::rename(&temporary, &self.path)
    }

    /// Whether the autostart entry currently exists.
    pub fn is_enabled(&self) -> bool {
        self.path.is_file()
    }
}

/// POSIX shell single-quoting equivalent to Python's `shlex.quote` for the
/// "safe" character set: alphanumerics plus `_@%+=:,./-` are left alone,
/// everything else is wrapped in single quotes with `'` escaped as `'\''`.
fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-')
        });
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn restore_env(previous: Option<OsString>) {
        match previous {
            Some(value) => env::set_var("NEXTSYNC_LAUNCHER", value),
            None => env::remove_var("NEXTSYNC_LAUNCHER"),
        }
    }

    #[test]
    fn command_defaults_to_nextsync_when_unset() {
        let previous = env::var_os("NEXTSYNC_LAUNCHER");
        env::remove_var("NEXTSYNC_LAUNCHER");
        let manager = AutostartManager::new(None);
        assert_eq!(manager.command(), "nextsync --background");
        restore_env(previous);
    }

    #[test]
    fn command_defaults_to_nextsync_when_empty() {
        let previous = env::var_os("NEXTSYNC_LAUNCHER");
        env::set_var("NEXTSYNC_LAUNCHER", "");
        let manager = AutostartManager::new(None);
        assert_eq!(manager.command(), "nextsync --background");
        restore_env(previous);
    }

    #[test]
    fn command_respects_launcher_env_var() {
        let previous = env::var_os("NEXTSYNC_LAUNCHER");
        env::set_var("NEXTSYNC_LAUNCHER", "/opt/nextsync/nextsync");
        let manager = AutostartManager::new(None);
        assert_eq!(manager.command(), "/opt/nextsync/nextsync --background");
        restore_env(previous);
    }

    #[test]
    fn command_quotes_launcher_with_spaces() {
        let previous = env::var_os("NEXTSYNC_LAUNCHER");
        env::set_var("NEXTSYNC_LAUNCHER", "/opt/next sync/run");
        let manager = AutostartManager::new(None);
        assert_eq!(manager.command(), "'/opt/next sync/run' --background");
        restore_env(previous);
    }

    #[test]
    fn set_enabled_true_writes_desktop_file() {
        let previous = env::var_os("NEXTSYNC_LAUNCHER");
        env::set_var("NEXTSYNC_LAUNCHER", "/opt/nextsync/nextsync");
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("autostart")
            .join("io.github.gnacho.nextsync.desktop");
        let manager = AutostartManager::new(Some(path.clone()));
        manager.set_enabled(true).unwrap();
        assert!(path.is_file());
        assert!(manager.is_enabled());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("[Desktop Entry]\n"));
        assert!(content.contains("Exec=/opt/nextsync/nextsync --background\n"));
        assert!(content.contains("Icon=io.github.gnacho.nextsync\n"));
        assert!(content.contains("X-GNOME-Autostart-enabled=true\n"));
        assert!(content.ends_with("StartupNotify=false\n\n"));
        restore_env(previous);
    }

    #[test]
    fn set_enabled_false_removes_desktop_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("io.github.gnacho.nextsync.desktop");
        let manager = AutostartManager::new(Some(path.clone()));
        manager.set_enabled(true).unwrap();
        manager.set_enabled(false).unwrap();
        assert!(!path.exists());
        assert!(!manager.is_enabled());
    }

    #[test]
    fn set_enabled_false_is_idempotent_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("io.github.gnacho.nextsync.desktop");
        let manager = AutostartManager::new(Some(path.clone()));
        manager.set_enabled(false).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn set_enabled_true_writes_atomically_via_tmp_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("io.github.gnacho.nextsync.desktop");
        let manager = AutostartManager::new(Some(path.clone()));
        manager.set_enabled(true).unwrap();
        assert!(!path.with_extension("tmp").exists());
        assert!(path.is_file());
    }

    #[test]
    fn default_path_lives_under_autostart_dir() {
        let previous = env::var_os("XDG_CONFIG_HOME");
        env::set_var("XDG_CONFIG_HOME", "/tmp/nxs-cfg");
        let manager = AutostartManager::new(None);
        assert!(manager
            .path
            .starts_with(PathBuf::from("/tmp/nxs-cfg/autostart")));
        match previous {
            Some(value) => env::set_var("XDG_CONFIG_HOME", value),
            None => env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}
