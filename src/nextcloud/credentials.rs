//! Credential storage in the Secret Service.
//!
//! Per-account passwords are stored in the default collection of the desktop
//! Secret Service (`org.freedesktop.secrets`) keyed by `account_id`, so they
//! survive a reinstall. Accounts created by the Python `nextsync` app stored
//! their secret with `{server, username}` attributes instead; those entries
//! are picked up through [`CredentialsStore::get_for_account`] and adopted
//! (re-stored under `account_id`, leaving the legacy item untouched for the
//! Python app).
//!
//! Uses `secret_service::blocking` (feature `rt-tokio-crypto-rust`, DH
//! encrypted session). Blocking calls must not run on the async UI loop.

use std::collections::HashMap;

use secret_service::blocking::SecretService;
use secret_service::EncryptionType;

/// Error produced by the credential store (a [`secret_service::Error`]).
pub type CredentialError = secret_service::Error;

/// Attribute key used to index items by account id.
const ATTR_ACCOUNT_ID: &str = "account_id";

/// Attribute keys used by the legacy Python `nextsync` entries.
const ATTR_SERVER: &str = "server";
const ATTR_USERNAME: &str = "username";

/// Secret content type used for stored passwords.
const CONTENT_TYPE: &str = "text/plain";

/// Stores and retrieves account passwords in the default Secret Service
/// collection.
pub struct CredentialsStore;

impl CredentialsStore {
    /// Save (or replace) the password for an account.
    pub fn set(account_id: &str, password: &str) -> Result<(), CredentialError> {
        let service = SecretService::connect(EncryptionType::Dh)?;
        let collection = service.get_default_collection()?;
        collection.create_item(
            &format!("nextsync-{account_id}"),
            HashMap::from([(ATTR_ACCOUNT_ID, account_id)]),
            password.as_bytes(),
            true,
            CONTENT_TYPE,
        )?;
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
            return Ok(None);
        };
        let secret = item.get_secret()?;
        Ok(Some(String::from_utf8_lossy(&secret).into_owned()))
    }

    /// Read the password for an account, falling back to the legacy entry.
    ///
    /// Tries the `account_id` item first; when absent, searches the legacy
    /// Python `nextsync` attributes (`{server, username}`). A legacy hit is
    /// adopted: the secret is re-stored under `account_id` so later lookups
    /// hit the fast path, while the legacy item itself is left in place for
    /// the Python app. Adoption failure is not fatal — the password is still
    /// returned.
    pub fn get_for_account(
        account_id: &str,
        server: &str,
        login: &str,
    ) -> Result<Option<String>, CredentialError> {
        if let Some(password) = Self::get(account_id)? {
            return Ok(Some(password));
        }
        let service = SecretService::connect(EncryptionType::Dh)?;
        let result = service.search_items(HashMap::from([
            (ATTR_SERVER, server),
            (ATTR_USERNAME, login),
        ]))?;
        let Some(item) = result.unlocked.first() else {
            return Ok(None);
        };
        let secret = item.get_secret()?;
        let password = String::from_utf8_lossy(&secret).into_owned();
        let _ = Self::set(account_id, &password);
        Ok(Some(password))
    }

    /// Delete the stored password for an account, if any.
    pub fn delete(account_id: &str) -> Result<(), CredentialError> {
        let service = SecretService::connect(EncryptionType::Dh)?;
        let result = service.search_items(HashMap::from([(ATTR_ACCOUNT_ID, account_id)]))?;
        for item in result.unlocked {
            item.delete()?;
        }
        for item in result.locked {
            item.delete()?;
        }
        Ok(())
    }
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
        let collection = service.get_default_collection().expect("collection");
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
}
