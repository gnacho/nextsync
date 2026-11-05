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
//! Issue #221: every operation below runs on ONE process-wide Secret Service
//! session, created lazily on first use. Each `SecretService::connect`
//! negotiates a fresh D-Bus connection plus DH session, and the upstream
//! gnome-keyring crash (issue #216, Ubuntu bug 2161749) is a race in that
//! negotiation that any client can trigger: fewer sessions per process means
//! fewer chances to hit it. On a retryable failure (transport or stale
//! session after a daemon restart) the cached session is dropped and the
//! operation reconnects exactly once; domain errors (locked, missing item)
//! never reconnect. All access is serialized through a Mutex.
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

/// Process-wide Secret Service session (issue #221).
///
/// Created lazily on first use and reused by every operation, so a process
/// pays one DH session negotiation instead of one per keyring call. The
/// service is leaked on purpose: the desktop process lives for the whole
/// session, and the crate ties returned collections/items to `&'a self` with
/// the same `'a` as the struct, so a process-wide instance must be
/// `'static` anyway. A reconnect (e.g. after the daemon restarted) leaks one
/// more instance, which is negligible.
static SERVICE: OnceLock<Mutex<Option<&'static SecretService<'static>>>> = OnceLock::new();

fn service_lock() -> MutexGuard<'static, Option<&'static SecretService<'static>>> {
    SERVICE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Connect a fresh session against the live daemon (negotiates DH).
fn connect_service() -> Result<&'static SecretService<'static>, CredentialError> {
    let service: SecretService<'static> = SecretService::connect(EncryptionType::Dh)?;
    Ok(Box::leak(Box::new(service)))
}

/// Whether a failed operation may succeed on a fresh session (issue #221).
///
/// Domain errors are properties of the data, not of the connection, so
/// reconnecting cannot change them: locked keyring (#98, #214), missing
/// object, dismissed prompt, unreadable secret (#139). Everything else
/// (crypto/session mismatch, zbus transport failure, name owner gone after
/// a daemon restart) is worth exactly one retry on a new session.
fn is_retryable(error: &CredentialError) -> bool {
    !matches!(
        error,
        CredentialError::Utf8
            | CredentialError::Service(
                secret_service::Error::Locked
                    | secret_service::Error::NoResult
                    | secret_service::Error::Prompt
            )
    )
}

/// Run `op` against the cached session, reconnecting once on retryable
/// failures (issue #221).
///
/// Pure policy over an injectable session slot and factory so the cache and
/// reconnect behaviour are testable without a Secret Service:
/// - the session is created lazily and reused until an operation fails;
/// - on a retryable failure the cached session is dropped and exactly one
///   fresh session is created for a single retry (no loops);
/// - domain failures and connect failures are returned as-is.
fn run_with_session<S: Copy, T, E>(
    slot: &mut Option<S>,
    connect: &impl Fn() -> Result<S, E>,
    retryable: &impl Fn(&E) -> bool,
    op: &impl Fn(S) -> Result<T, E>,
) -> Result<T, E> {
    if slot.is_none() {
        *slot = Some(connect()?);
    }
    let session = slot.unwrap();
    match op(session) {
        Ok(value) => Ok(value),
        Err(error) if retryable(&error) => {
            *slot = None;
            *slot = Some(connect()?);
            op(slot.unwrap())
        }
        Err(error) => Err(error),
    }
}

/// Run a credential operation on the process-wide session.
///
/// Serializes access (the session is shared by engine threads and UI) and
/// applies the reconnect-once policy. The lock is held for the whole
/// operation, including any unlock prompt: credential calls are fast, and
/// while the keyring prompts it is locked for everyone anyway.
fn with_service<T>(
    op: impl Fn(&'static SecretService<'static>) -> Result<T, CredentialError>,
) -> Result<T, CredentialError> {
    run_with_session(&mut service_lock(), &connect_service, &is_retryable, &op)
}

/// Drop the cached session so the next operation reconnects (tests only).
///
/// The pool is process-global and tests run in parallel threads: a test that
/// leaves the shared session in a bad state must reset the slot so it does
/// not poison unrelated tests.
#[cfg(test)]
pub(crate) fn reset_service_for_tests() {
    *service_lock() = None;
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
        with_service(|service| {
            let collection = collection(service)?;
            collection.create_item(
                &format!("nextsync-{account_id}"),
                HashMap::from([(ATTR_ACCOUNT_ID, account_id)]),
                password.as_bytes(),
                true,
                CONTENT_TYPE,
            )?;
            Ok(())
        })?;
        cache_lock().insert(account_id.to_string(), password.to_string());
        Ok(())
    }

    /// Read the password for an account, if stored.
    ///
    /// Returns `Ok(None)` when no item matches; requires the default collection
    /// to be unlocked (the normal state of a desktop session).
    pub fn get(account_id: &str) -> Result<Option<String>, CredentialError> {
        with_service(|service| {
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
        })
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
        let legacy = with_service(|service| {
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
            Ok(Some(
                String::from_utf8_lossy(&item.get_secret()?).into_owned(),
            ))
        })?;
        let Some(password) = legacy else {
            return Ok(None);
        };
        let _ = Self::set(account_id, &password);
        cache_lock().insert(account_id.to_string(), password.clone());
        Ok(Some(password))
    }

    /// Try to unlock every locked collection (issue #214).
    ///
    /// GNOME collections created by the desktop session usually share the
    /// login password: the Secret Service then completes the unlock prompt
    /// on its own as long as the client connection stays alive through it,
    /// so the common case returns without any user interaction. Returns
    /// `Ok(true)` when at least one collection was locked and got unlocked.
    /// Per-collection failures are skipped so one stubborn collection does
    /// not block the rest.
    pub fn unlock_locked_collections() -> Result<bool, CredentialError> {
        with_service(|service| {
            let mut unlocked_any = false;
            for collection in service.get_all_collections()? {
                let was_locked = collection.is_locked().unwrap_or(false);
                if was_locked && collection.unlock().is_ok() {
                    unlocked_any = true;
                }
            }
            Ok(unlocked_any)
        })
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
        with_service(|service| {
            let result = service.search_items(HashMap::from([(ATTR_ACCOUNT_ID, account_id)]))?;
            // Only the unlocked items are reachable; items in a locked collection
            // (the legacy default keyring) cannot be removed without unlocking.
            for item in result.unlocked {
                item.delete()?;
            }
            Ok(())
        })
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

    /// Issue #221: the session pool policy (pure, no Secret Service needed).
    /// `run_with_session` gets an injectable slot, factory and classifier,
    /// with a Copy fake standing in for the leaked `&'static SecretService`.
    mod session_pool {
        use super::run_with_session;
        use std::cell::Cell;

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        struct FakeSession(u32);

        #[derive(Debug, PartialEq, Eq)]
        enum FakeError {
            /// Transport-like: a fresh session may fix it.
            Disconnected,
            /// Domain-like: reconnecting cannot help.
            Locked,
        }

        fn is_retryable(error: &FakeError) -> bool {
            *error == FakeError::Disconnected
        }

        /// Second and later operations reuse the cached session: the factory
        /// runs exactly once.
        #[test]
        fn second_operation_reuses_the_session() {
            let mut slot = None;
            let connects = Cell::new(0u32);
            let connect = || {
                connects.set(connects.get() + 1);
                Ok::<_, FakeError>(FakeSession(connects.get()))
            };
            let op = |session: FakeSession| Ok::<_, FakeError>(session.0);

            let first = run_with_session(&mut slot, &connect, &is_retryable, &op);
            let second = run_with_session(&mut slot, &connect, &is_retryable, &op);
            assert_eq!(first, Ok(1));
            assert_eq!(second, Ok(1));
            assert_eq!(connects.get(), 1, "factory must run once until a failure");
        }

        /// A transport failure drops the cached session and reconnects
        /// exactly once; the retry and the following operations run on the
        /// new session without any further connect.
        #[test]
        fn transport_failure_reconnects_exactly_once() {
            let mut slot = None;
            let connects = Cell::new(0u32);
            let connect = || {
                connects.set(connects.get() + 1);
                Ok::<_, FakeError>(FakeSession(connects.get()))
            };
            let op = |session: FakeSession| {
                if session.0 == 1 {
                    Err(FakeError::Disconnected)
                } else {
                    Ok(session.0)
                }
            };

            let retried = run_with_session(&mut slot, &connect, &is_retryable, &op);
            assert_eq!(retried, Ok(2), "the same call retries on the new session");
            assert_eq!(connects.get(), 2, "one reconnect after the failure");

            let next = run_with_session(&mut slot, &connect, &is_retryable, &op);
            assert_eq!(next, Ok(2));
            assert_eq!(connects.get(), 2, "the new session is cached and reused");
        }

        /// When both the original attempt and the single retry fail, the
        /// error is returned: there is no reconnect loop.
        #[test]
        fn retry_does_not_loop() {
            let mut slot = None;
            let connects = Cell::new(0u32);
            let connect = || {
                connects.set(connects.get() + 1);
                Ok::<_, FakeError>(FakeSession(connects.get()))
            };
            let op = |_: FakeSession| Err::<u32, _>(FakeError::Disconnected);

            let result = run_with_session(&mut slot, &connect, &is_retryable, &op);
            assert_eq!(result, Err(FakeError::Disconnected));
            assert_eq!(connects.get(), 2, "original + one retry, then give up");
        }

        /// Domain errors (locked, missing…) never reconnect: the error
        /// propagates as-is and the next operation still uses the cached
        /// session.
        #[test]
        fn domain_error_does_not_reconnect() {
            let mut slot = None;
            let connects = Cell::new(0u32);
            let connect = || {
                connects.set(connects.get() + 1);
                Ok::<_, FakeError>(FakeSession(connects.get()))
            };

            let locked = run_with_session(&mut slot, &connect, &is_retryable, &|_: FakeSession| {
                Err::<u32, _>(FakeError::Locked)
            });
            assert_eq!(locked, Err(FakeError::Locked));
            assert_eq!(connects.get(), 1, "domain errors must not reconnect");

            let next = run_with_session(
                &mut slot,
                &connect,
                &is_retryable,
                &|session: FakeSession| Ok(session.0),
            );
            assert_eq!(next, Ok(1), "the cached session survives domain errors");
            assert_eq!(connects.get(), 1);
        }

        /// A connect failure propagates without calling the operation and
        /// without retrying.
        #[test]
        fn connect_failure_propagates_without_retry() {
            let mut slot = None;
            let connects = Cell::new(0u32);
            let connect = || {
                connects.set(connects.get() + 1);
                Err::<FakeSession, _>(FakeError::Disconnected)
            };
            let op = |_: FakeSession| -> Result<u32, FakeError> {
                panic!("op must not run when connect fails");
            };

            let result = run_with_session(&mut slot, &connect, &is_retryable, &op);
            assert_eq!(result, Err(FakeError::Disconnected));
            assert_eq!(connects.get(), 1);
            assert!(slot.is_none(), "no stale session is cached");
        }

        /// The test-only reset is callable and idempotent. Parallel keyring
        /// tests share the process-wide slot, so asserting on its content
        /// would be racy; the cache/reconnect policy itself is covered by
        /// the fake-world tests above.
        #[test]
        fn reset_for_tests_is_callable_and_idempotent() {
            super::super::reset_service_for_tests();
            super::super::reset_service_for_tests();
        }
    }
}
