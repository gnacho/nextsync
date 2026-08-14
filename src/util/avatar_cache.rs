//! Disk cache for account avatars (issue #50).
//!
//! Avatars live under `$XDG_STATE_HOME/nextsync/avatars/<account_id>` so a
//! cached image renders instantly at startup while the network refresh runs
//! in the background. The directory is created `0700` because avatars are
//! account data.

use std::fs;
use std::path::PathBuf;

use crate::util::paths::state_dir;

/// `<state_dir>/avatars/<account_id>` for one account.
pub fn avatar_path(account_id: &str) -> PathBuf {
    state_dir().join("avatars").join(account_id)
}

/// The cached avatar bytes, or `None` when nothing usable is stored.
pub fn read_cached_avatar(account_id: &str) -> Option<Vec<u8>> {
    fs::read(avatar_path(account_id))
        .ok()
        .filter(|bytes| !bytes.is_empty())
}

/// Persist the avatar for an account (creating the directory `0700` on
/// first use). Failures are the caller's to log.
pub fn store_avatar(account_id: &str, bytes: &[u8]) -> std::io::Result<()> {
    let path = avatar_path(account_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    fs::write(&path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avatar_round_trips_through_the_state_directory() {
        let _env = crate::util::test_env::lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", dir.path());
        assert_eq!(read_cached_avatar("acct-1"), None);
        store_avatar("acct-1", b"png-bytes").unwrap();
        assert_eq!(read_cached_avatar("acct-1"), Some(b"png-bytes".to_vec()));
        // A fresh store of another account does not disturb the first one.
        store_avatar("acct-2", b"other").unwrap();
        assert_eq!(read_cached_avatar("acct-2"), Some(b"other".to_vec()));
        // Empty files are treated as absent.
        fs::write(avatar_path("acct-3"), b"").unwrap();
        assert_eq!(read_cached_avatar("acct-3"), None);
        std::env::remove_var("XDG_STATE_HOME");
    }

    #[test]
    fn avatar_directory_is_created_user_private() {
        #[cfg(unix)]
        {
            let _env = crate::util::test_env::lock();
            let dir = tempfile::tempdir().unwrap();
            std::env::set_var("XDG_STATE_HOME", dir.path());
            store_avatar("acct-perm", b"png").unwrap();
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join("nextsync/avatars"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
            std::env::remove_var("XDG_STATE_HOME");
        }
    }
}
