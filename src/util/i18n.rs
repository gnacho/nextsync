//! Internationalization: English source strings with an embedded Spanish
//! catalog.
//!
//! Fase 6 (Task 6.1). The UI is written in English and translated through
//! [`t`], which looks up the active catalog and falls back to the source
//! string itself. The Spanish catalog is generated from the project's gettext
//! `po/es.po` by `tools/gen-translations.py` (compile-time embedded, no
//! runtime catalog files and no extra dependencies — the same zero-dep style
//! as the rest of the crate). PT-BR support is intentionally dropped
//! (decision, 12-Aug).
//!
//! Locale detection mirrors `util/i18n.py`: `LANGUAGE` (colon-separated
//! list), then `LC_ALL`, `LC_MESSAGES` and `LANG`; the first `es` prefix
//! selects the Spanish catalog. A process-wide override
//! ([`set_locale`]) wins over the environment, which keeps deterministic
//! tests and leaves room for a future in-app language setting.

use std::cell::Cell;
use std::env;

#[path = "translations/es.rs"]
mod es;

/// The selectable catalogs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    /// Source strings, no translation.
    English,
    /// The embedded Spanish catalog.
    Spanish,
}

thread_local! {
    static OVERRIDE: Cell<Option<Locale>> = const { Cell::new(None) };
}

/// Set the active locale explicitly (overrides the environment).
pub fn set_locale(locale: Locale) {
    OVERRIDE.with(|cell| cell.set(Some(locale)));
}

/// Clear an explicit override and go back to environment detection.
pub fn reset_locale() {
    OVERRIDE.with(|cell| cell.set(None));
}

/// The locale `t` currently translates into.
pub fn locale() -> Locale {
    if let Some(locale) = OVERRIDE.with(Cell::get) {
        return locale;
    }
    if requested_languages()
        .iter()
        .any(|language| starts_with_es(language))
    {
        return Locale::Spanish;
    }
    Locale::English
}

/// Translate `msgid` into the active catalog, falling back to `msgid`.
pub fn t(msgid: &str) -> &str {
    match locale() {
        Locale::English => msgid,
        Locale::Spanish => lookup_spanish(msgid).unwrap_or(msgid),
    }
}

/// Binary-search the sorted Spanish catalog.
fn lookup_spanish(msgid: &str) -> Option<&str> {
    es::CATALOG
        .binary_search_by(|(key, _)| (*key).cmp(msgid))
        .ok()
        .map(|index| es::CATALOG[index].1)
}

/// The requested language list, mirroring `_requested_languages` in the
/// Python: `LANGUAGE` (colon-separated) first, then the first of
/// `LC_ALL`/`LC_MESSAGES`/`LANG`.
fn requested_languages() -> Vec<String> {
    if let Some(value) = non_empty_env("LANGUAGE") {
        return value.split(':').map(str::to_string).collect();
    }
    for variable in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(value) = non_empty_env(variable) {
            return vec![value];
        }
    }
    Vec::new()
}

fn non_empty_env(variable: &str) -> Option<String> {
    let value = env::var(variable).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "C" || trimmed == "POSIX" {
        return None;
    }
    Some(trimmed.to_string())
}

/// `es`, `es_ES.UTF-8` and friends select Spanish.
fn starts_with_es(language: &str) -> bool {
    language
        .split('.')
        .next()
        .unwrap_or_default()
        .split('@')
        .next()
        .unwrap_or_default()
        .starts_with("es")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `t` with an override set is deterministic regardless of the ambient
    /// environment, so these tests need no environment lock.
    #[test]
    fn english_is_the_identity() {
        set_locale(Locale::English);
        assert_eq!(t("Synchronizing…"), "Synchronizing…");
        assert_eq!(t("untranslated string"), "untranslated string");
        reset_locale();
    }

    #[test]
    fn spanish_translates_known_strings() {
        set_locale(Locale::Spanish);
        assert_eq!(t("Synchronizing…"), "Sincronizando…");
        assert_eq!(
            t("Start NextSync when I sign in"),
            "Iniciar NextSync al iniciar sesión"
        );
        reset_locale();
    }

    #[test]
    fn spanish_falls_back_to_the_source_for_unknown_strings() {
        set_locale(Locale::Spanish);
        assert_eq!(t("not in the catalog"), "not in the catalog");
        reset_locale();
    }

    #[test]
    fn locale_codes_select_spanish() {
        for language in ["es", "es_ES.UTF-8", "es-ES", "es_AR"] {
            assert!(starts_with_es(language), "{language} should select Spanish");
        }
        for language in ["en", "en_US.UTF-8", "C", "POSIX", "pt_BR", "gl", ""] {
            assert!(!starts_with_es(language), "{language} should not select Spanish");
        }
    }

    #[test]
    fn every_catalog_entry_round_trips_through_lookup() {
        for (key, value) in es::CATALOG {
            set_locale(Locale::Spanish);
            assert_eq!(t(key), *value);
            reset_locale();
        }
    }

    #[test]
    fn environment_detection_selects_spanish() {
        let _env = crate::util::test_env::lock();
        let language = env::var_os("LANGUAGE");
        let lang = env::var_os("LANG");
        env::set_var("LANGUAGE", "es_ES.UTF-8:en");
        reset_locale();
        assert_eq!(locale(), Locale::Spanish);
        env::remove_var("LANGUAGE");
        env::set_var("LANG", "en_US.UTF-8");
        assert_eq!(locale(), Locale::English);
        match language {
            Some(value) => env::set_var("LANGUAGE", value),
            None => env::remove_var("LANGUAGE"),
        }
        match lang {
            Some(value) => env::set_var("LANG", value),
            None => env::remove_var("LANG"),
        }
    }
}
