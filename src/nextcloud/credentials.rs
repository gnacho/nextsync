//! Credential storage in the Secret Service.
//!
//! Per-account passwords are stored in the `login` collection (the one the
//! desktop session unlocks at sign-in, issue #58) falling back to the
//! default collection, keyed by `account_id`, so they survive a reinstall.
//! Accounts created by the Python `nextsync` app stored their secret with
//! `{server, username}` attributes instead; those entries are picked up
//! through [`CredentialsStore::get_for_account`] and adopted (re-stored
//! under `account_id`, leaving the legacy item untouched for the Python
//! app).
//!
//! Uses `secret_service::blocking` (feature `rt-tokio-crypto-rust`, DH
//! encrypted session). Blocking calls must not run on the async UI loop.
//!
//! Issue #178: resolved passwords are cached in process memory, so each
//! account costs one Secret Service session negotiation per process instead
//! of one per sync run (the desktop reference clients, e.g. Iotas, do the
//! same). The cache is written through on [`CredentialsStore::set`], evicted
//! on [`CredentialsStore::delete`], and invalidated when a sync run ends in
//! authentication failure so the next lookup re-reads the keyring. Secrets
//! therefore live in process memory for the whole session; a revoked
//! password is noticed on the next 401, not before.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

use secret_service::blocking::SecretService;
use secret_service::EncryptionType;

/// Error produced by the credential store: a wrapped [`secret_service::Error`]
/// or an unreadable stored secret (issue #139).
#[derive(Debug)]
pub enum CredentialError {
    /// The Secret Service call failed (locked, unavailable, transport…).
    Service(secret_service::Error),
    /// The stored secret is not valid UTF-8 and cannot be used as a password.
    Utf8,
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialError::Service(error) => write!(f, "{error}"),
            CredentialError::Utf8 => write!(f, "stored secret is not valid UTF-8"),
        }
    }
}

impl std::error::Error for CredentialError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CredentialError::Service(error) => Some(error),
            CredentialError::Utf8 => None,
        }
    }
}

impl From<secret_service::Error> for CredentialError {
    fn from(error: secret_service::Error) -> Self {
        CredentialError::Service(error)
    }
}

/// Attribute key used to index items by account id.
const ATTR_ACCOUNT_ID: &str = "account_id";

/// Attribute keys used by the legacy Python `nextsync` entries.
const ATTR_SERVER: &str = "server";
const ATTR_USERNAME: &str = "username";

/// Secret content type used for stored passwords.
const CONTENT_TYPE: &str = "text/plain";

/// Process-local password cache, keyed by account id (issue #178).
static PASSWORD_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn cache_lock() -> MutexGuard<'static, HashMap<String, String>> {
    PASSWORD_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Prefer the `login` collection (unlocked automatically by the desktop
/// session) and fall back to the default collection (issue #58).
///
/// The default collection is not always the login keyring: on GNOME it can
/// point at a separate, password-less keyring that never gets unlocked, in
/// which case every write fails with a locked error.
fn collection<'a>(
    service: &'a SecretService<'_>,
) -> Result<secret_service::blocking::Collection<'a>, CredentialError> {
    service
        .get_collection_by_alias("login")
        .or_else(|_| service.get_default_collection())
        .map_err(CredentialError::from)
}

/// Stores and retrieves account passwords in the Secret Service collection
/// the desktop session unlocks (`login`, falling back to default).
pub struct CredentialsStore;

impl CredentialsStore {
    /// Save (or replace) the password for an account.
    pub fn set(account_id: &str, password: &str) -> Result<(), CredentialError> {
        let service = SecretService::connect(EncryptionType::Dh)?;
        let collection = collection(&service)?;
        collection.create_item(
            &format!("nextsync-{account_id}"),
            HashMap::from([(ATTR_ACCOUNT_ID, account_id)]),
            password.as_bytes(),
            true,
            CONTENT_TYPE,
        )?;
        cache_lock().insert(account_id.to_string(), password.to_string());
        Ok(())
    }

    /// Read the password for an account, if stored.
    ///
    /// Returns `Ok(None)` when no item matches; requires the default collection
    /// to be unlocked (the normal state of a desktop session).
    pub fn get(account_id: &str) -> Result<Option<String>, CredentialError> {
        let service = SecretService::connect(EncryptionType::Dh)?;
        let result = service.search_items(HashMap::from([(ATTR_ACCOUNT_ID, account_id)]))?;
        let Some(item) = result.unlocked.first() else {
            // Distinguish "no secret at all" from "the keyring is locked":
            // the latter must surface as an error so callers do not treat it
            // as missing credentials and demand re-authentication (issue #98).
            return if result.locked.is_empty() {
                Ok(None)
            } else {
                Err(CredentialError::Service(secret_service::Error::Locked))
            };
        };
        let secret = item.get_secret()?;
        match std::str::from_utf8(&secret) {
            Ok(password) => Ok(Some(password.to_string())),
            // The stored bytes are not a usable password (issue #139): a
            // silent lossy substitution would authenticate with a different
            // string and fail opaquely. Surface it as an error instead.
            Err(_) => Err(CredentialError::Utf8),
        }
    }

    /// Read the password for an account, falling back to the legacy entry.
    ///
    /// Serves the in-memory cache first (issue #178); on a miss, tries the
    /// `account_id` item first; when absent, searches the legacy
    /// Python `nextsync` attributes (`{server, username}`). A legacy hit is
    /// adopted: the secret is re-stored under `account_id` so later lookups
    /// hit the fast path, while the legacy item itself is left in place for
    /// the Python app. Adoption failure is not fatal — the password is still
    /// returned. Successful keyring resolutions populate the cache.
    pub fn get_for_account(
        account_id: &str,
        server: &str,
        login: &str,
    ) -> Result<Option<String>, CredentialError> {
        if let Some(cached) = cache_lock().get(account_id) {
            return Ok(Some(cached.clone()));
        }
        if let Some(password) = Self::get(account_id)? {
            cache_lock().insert(account_id.to_string(), password.clone());
            return Ok(Some(password));
        }
        let service = SecretService::connect(EncryptionType::Dh)?;
        let result = service.search_items(HashMap::from([
            (ATTR_SERVER, server),
            (ATTR_USERNAME, login),
        ]))?;
        let Some(item) = result.unlocked.first() else {
            // Same locked-vs-missing distinction as in `get` (issue #98).
            return if result.locked.is_empty() {
                Ok(None)
            } else {
                Err(CredentialError::Service(secret_service::Error::Locked))
            };
        };
        let secret = item.get_secret()?;
        let password = String::from_utf8_lossy(&secret).into_owned();
        let _ = Self::set(account_id, &password);
        cache_lock().insert(account_id.to_string(), password.clone());
        Ok(Some(password))
    }

    /// Drop the cached password for an account (issue #178).
    ///
    /// Called when a sync run proves the credential wrong (authentication
    /// failure), so the next lookup re-reads the keyring instead of
    /// replaying the stale secret.
    pub fn invalidate(account_id: &str) {
        cache_lock().remove(account_id);
    }

    /// Delete the stored password for an account, if any.
    pub fn delete(account_id: &str) -> Result<(), CredentialError> {
        cache_lock().remove(account_id);
        let service = SecretService::connect(EncryptionType::Dh)?;
        let result = service.search_items(HashMap::from([(ATTR_ACCOUNT_ID, account_id)]))?;
        // Only the unlocked items are reachable; items in a locked collection
        // (the legacy default keyring) cannot be removed without unlocking.
        for item in result.unlocked {
            item.delete()?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn seed_cache_for_tests(account_id: &str, password: &str) {
    cache_lock().insert(account_id.to_string(), password.to_string());
}

#[cfg(test)]
pub(crate) fn cached_for_tests(account_id: &str) -> Option<String> {
    cache_lock().get(account_id).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_ACCOUNT: &str = "5fcc57b6eeae77370e1f1b1a1a608d97511bab8cf29c0e02beabeb3e9a393592";
    const TEST_PASSWORD: &str = "correct-horse-battery-staple";
    const TEST_LEGACY_SERVER: &str = "https://legacy-unit-test.example.net";
    const TEST_LEGACY_LOGIN: &str = "legacy-unit-test@example.net";

    /// Removes the test item even if the test panics.
    struct Cleanup;

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = CredentialsStore::delete(TEST_ACCOUNT);
        }
    }

    #[test]
    fn roundtrip_set_get_delete() {
        match SecretService::connect(EncryptionType::Dh) {
            Ok(_) => {}
            Err(_) => {
                eprintln!("no Secret Service session bus available; skipping");
                return;
            }
        }
        let _guard = Cleanup;

        let _ = CredentialsStore::delete(TEST_ACCOUNT);

        CredentialsStore::set(TEST_ACCOUNT, TEST_PASSWORD).expect("set should succeed");
        let stored = CredentialsStore::get(TEST_ACCOUNT).expect("get should succeed");
        assert_eq!(stored.as_deref(), Some(TEST_PASSWORD));

        // Replacing an existing item must not create a duplicate.
        CredentialsStore::set(TEST_ACCOUNT, TEST_PASSWORD).expect("set should succeed");

        let missing = CredentialsStore::get(
            "00003196e1aae95b1ef0cd15afaae5394c80144fcc6dd24f056d49bac25c6f3a",
        )
        .expect("get should succeed");
        assert!(missing.is_none());

        CredentialsStore::delete(TEST_ACCOUNT).expect("delete should succeed");
        let gone = CredentialsStore::get(TEST_ACCOUNT).expect("get should succeed");
        assert!(gone.is_none());
    }

    /// Removes the legacy-attribute test item even if the test panics.
    struct LegacyCleanup;

    impl Drop for LegacyCleanup {
        fn drop(&mut self) {
            if let Ok(service) = SecretService::connect(EncryptionType::Dh) {
                if let Ok(result) = service.search_items(HashMap::from([
                    (ATTR_SERVER, TEST_LEGACY_SERVER),
                    (ATTR_USERNAME, TEST_LEGACY_LOGIN),
                ])) {
                    for item in result.unlocked.iter().chain(result.locked.iter()) {
                        let _ = item.delete();
                    }
                }
            }
            let _ = CredentialsStore::delete(TEST_ACCOUNT);
        }
    }

    /// A legacy Python-era entry (`{server, username}` attributes) must be
    /// found by `get_for_account`, returned, and adopted under `account_id`.
    #[test]
    fn legacy_python_entry_is_found_and_adopted() {
        match SecretService::connect(EncryptionType::Dh) {
            Ok(_) => {}
            Err(_) => {
                eprintln!("no Secret Service session bus available; skipping");
                return;
            }
        }
        let _guard = LegacyCleanup;

        let _ = CredentialsStore::delete(TEST_ACCOUNT);
        let service = SecretService::connect(EncryptionType::Dh).expect("connect");
        let collection = collection(&service).expect("collection");
        collection
            .create_item(
                "NextSync — legacy test entry",
                HashMap::from([
                    (ATTR_SERVER, TEST_LEGACY_SERVER),
                    (ATTR_USERNAME, TEST_LEGACY_LOGIN),
                ]),
                TEST_PASSWORD.as_bytes(),
                true,
                CONTENT_TYPE,
            )
            .expect("legacy item stored");

        // No Rust-key item yet: the fallback must find the legacy secret.
        let resolved =
            CredentialsStore::get_for_account(TEST_ACCOUNT, TEST_LEGACY_SERVER, TEST_LEGACY_LOGIN)
                .expect("get_for_account should succeed");
        assert_eq!(resolved.as_deref(), Some(TEST_PASSWORD));

        // The legacy hit must have been adopted under `account_id`.
        let adopted = CredentialsStore::get(TEST_ACCOUNT).expect("get should succeed");
        assert_eq!(adopted.as_deref(), Some(TEST_PASSWORD));

        // The legacy item itself must survive for the Python app.
        let still_there = service
            .search_items(HashMap::from([
                (ATTR_SERVER, TEST_LEGACY_SERVER),
                (ATTR_USERNAME, TEST_LEGACY_LOGIN),
            ]))
            .expect("legacy search");
        assert!(
            !still_there.unlocked.is_empty() || !still_there.locked.is_empty(),
            "legacy item must be left in place"
        );

        // Without any match (random account, unknown server) → Ok(None).
        let missing = CredentialsStore::get_for_account(
            "00003196e1aae95b1ef0cd15afaae5394c80144fcc6dd24f056d49bac25c6f3a",
            "https://nonexistent.example.net",
            "nobody@example.net",
        )
        .expect("get_for_account should succeed");
        assert!(missing.is_none());
    }

    #[test]
    fn utf8_error_is_describable_and_does_not_expose_the_secret() {
        // Issue #139: the Utf8 variant is constructible without a bus and
        // its message never echoes the bytes.
        let error = CredentialError::Utf8;
        let message = error.to_string();
        assert!(message.contains("UTF-8"));
        assert!(!message.contains("0xFF"));
    }

    const TEST_CACHE_ACCOUNT: &str =
        "7e3a9c1f5b28d4e6a0f2c8b1d3e5a7c9b0d2e4f6a8c0b2d4e6f8a0c2e4b6d8a0c2";
    // Distinct id per cache test: the cache is process-global and tests run
    // in parallel threads.
    const TEST_CACHE_ACCOUNT_WT: &str =
        "1a2b3c4d5e6f708192a3b4c5d6e7f80192a3b4c5d6e7f8091a2b3c4d5e6f7a8b9";

    /// Issue #178: a seeded cache entry is served without touching the
    /// Secret Service, and `invalidate` forces the next lookup back to the
    /// keyring.
    #[test]
    fn cache_serves_until_invalidated() {
        CredentialsStore::invalidate(TEST_CACHE_ACCOUNT);
        seed_cache_for_tests(TEST_CACHE_ACCOUNT, TEST_PASSWORD);

        // Served from memory: works even though nothing is stored under
        // this account in any keyring.
        let resolved = CredentialsStore::get_for_account(
            TEST_CACHE_ACCOUNT,
            "https://cache-unit-test.example.net",
            "cache-unit-test@example.net",
        )
        .expect("cached lookup should succeed");
        assert_eq!(resolved.as_deref(), Some(TEST_PASSWORD));

        CredentialsStore::invalidate(TEST_CACHE_ACCOUNT);
        assert!(cached_for_tests(TEST_CACHE_ACCOUNT).is_none());
        // After invalidation the cache no longer answers: the result now
        // depends on the real keyring (Ok(None) when available, an error
        // when there is no session bus at all).
        let after = CredentialsStore::get_for_account(
            TEST_CACHE_ACCOUNT,
            "https://cache-unit-test.example.net",
            "cache-unit-test@example.net",
        );
        assert!(after.map(|password| password.is_none()).unwrap_or(true));
    }

    /// Issue #178: `set` writes through to the cache and `delete` evicts it.
    #[test]
    fn set_writes_through_and_delete_evicts() {
        match SecretService::connect(EncryptionType::Dh) {
            Ok(_) => {}
            Err(_) => {
                eprintln!("no Secret Service session bus available; skipping");
                return;
            }
        }
        struct CacheCleanup;
        impl Drop for CacheCleanup {
            fn drop(&mut self) {
                let _ = CredentialsStore::delete(TEST_CACHE_ACCOUNT_WT);
            }
        }
        let _guard = CacheCleanup;

        let _ = CredentialsStore::delete(TEST_CACHE_ACCOUNT_WT);
        CredentialsStore::set(TEST_CACHE_ACCOUNT_WT, TEST_PASSWORD).expect("set should succeed");
        assert_eq!(
            cached_for_tests(TEST_CACHE_ACCOUNT_WT).as_deref(),
            Some(TEST_PASSWORD)
        );

        // Remove the keyring item directly, bypassing the store: the cache
        // must keep serving the password regardless.
        let service = SecretService::connect(EncryptionType::Dh).expect("connect");
        let result = service
            .search_items(HashMap::from([(ATTR_ACCOUNT_ID, TEST_CACHE_ACCOUNT_WT)]))
            .expect("search");
        for item in result.unlocked {
            item.delete().expect("direct delete");
        }
        let resolved = CredentialsStore::get_for_account(
            TEST_CACHE_ACCOUNT_WT,
            "https://cache-unit-test.example.net",
            "cache-unit-test@example.net",
        )
        .expect("cached lookup should succeed");
        assert_eq!(resolved.as_deref(), Some(TEST_PASSWORD));

        CredentialsStore::delete(TEST_CACHE_ACCOUNT_WT).expect("delete should succeed");
        assert!(cached_for_tests(TEST_CACHE_ACCOUNT_WT).is_none());
    }
}
