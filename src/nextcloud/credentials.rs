//! Credential storage.
//!
//! Fase 1 (Task 1.2): store/read/delete per-account credentials in the
//! Secret Service (default collection, attributes keyed by account id)
//! via `secret_service::connect(EncryptionType::Dh)`.

/// Placeholder for credential storage.
pub struct Credentials;

impl Credentials {
    /// Returns the number of stored credentials.
    ///
    /// Placeholder: always `0` until Fase 1 lands.
    pub fn stored_count() -> usize {
        0
    }
}
