//! Internationalization.
//!
//! Fase 6 (Task 6.1): EN + ES catalogs via `gettext-rs` (or `fluent`).
//! PT-BR support is dropped (decision 12-Aug).

/// Placeholder for the i18n layer.
pub struct I18n;

impl I18n {
    /// Returns the active locale code.
    ///
    /// Placeholder: `en` until Fase 6 lands.
    pub fn locale() -> &'static str {
        "en"
    }
}
