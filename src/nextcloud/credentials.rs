//! Credential storage in the Secret Service.
//!
//! Per-account passwords are stored in the default collection of the desktop
//! Secret Service (`org.freedesktop.secrets`) keyed by `account_id`, so they
//! survive a reinstall and are shared with the Python `nextsync` v0.2.x.
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
}
