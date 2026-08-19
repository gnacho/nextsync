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
            assert!(
                !starts_with_es(language),
                "{language} should not select Spanish"
            );
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

    /// Issue #57: every `t("...")` literal in the crate must have a
    /// non-empty Spanish translation in the embedded catalog, so a Spanish
    /// user never silently falls back to English.
    #[test]
    fn every_translatable_literal_has_a_spanish_translation() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // The two deliberate fallback fixtures in this module's tests.
        let allowed = ["untranslated string", "not in the catalog"];
        let mut scanned = 0usize;
        let mut missing: Vec<String> = Vec::new();

        let mut files = Vec::new();
        let mut stack = vec![manifest.join("src")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    files.push(path);
                }
            }
        }
        files.sort();
        assert!(!files.is_empty(), "the source tree must be found");

        for path in &files {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            // Doc and line comments may quote `t(...)` call shapes; drop
            // whole-line comments so they are not scanned as call sites.
            let text: String = text
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            for message in extract_translatable_literals(&text) {
                if message.is_empty() || allowed.contains(&message.as_str()) {
                    continue;
                }
                scanned += 1;
                let translated = es::CATALOG
                    .iter()
                    .any(|(key, value)| *key == message && !value.is_empty());
                if !translated {
                    missing.push(format!("{}: {message:?}", path.display()));
                }
            }
        }
        assert!(
            scanned > 300,
            "the scan must cover the whole crate (only {scanned} literals found)"
        );
        assert!(
            missing.is_empty(),
            "missing Spanish translations:\n{}",
            missing.join("\n")
        );
    }

    /// Extract the string literals passed to `t(...)` calls: one or more
    /// adjacent Rust string literals (implicit concatenation) between the
    /// opening parenthesis and the following `,` or `)`.
    fn extract_translatable_literals(text: &str) -> Vec<String> {
        fn is_ident(byte: u8) -> bool {
            byte.is_ascii_alphanumeric() || byte == b'_'
        }
        let bytes = text.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'('
                && i > 0
                && bytes[i - 1] == b't'
                && (i == 1 || !is_ident(bytes[i - 2]))
            {
                let mut cursor = i + 1;
                let mut message = String::new();
                let mut matched = false;
                loop {
                    // Skip whitespace between/around the literals.
                    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                        cursor += 1;
                    }
                    if cursor < bytes.len() && bytes[cursor] == b'"' {
                        cursor += 1;
                        let mut literal = String::new();
                        let mut closed = false;
                        while cursor < bytes.len() {
                            match bytes[cursor] {
                                b'\\' if cursor + 1 < bytes.len() => {
                                    let escaped = bytes[cursor + 1];
                                    if escaped == b'\n' {
                                        // Rust line continuation: swallow the
                                        // newline and the following whitespace.
                                        cursor += 2;
                                        while cursor < bytes.len()
                                            && bytes[cursor].is_ascii_whitespace()
                                        {
                                            cursor += 1;
                                        }
                                    } else {
                                        literal.push(match escaped {
                                            b'n' => '\n',
                                            b't' => '\t',
                                            b'r' => '\r',
                                            other => other as char,
                                        });
                                        cursor += 2;
                                    }
                                }
                                b'"' => {
                                    cursor += 1;
                                    closed = true;
                                    break;
                                }
                                _ => {
                                    // Advance over one full UTF-8 scalar.
                                    let start = cursor;
                                    cursor += 1;
                                    while cursor < bytes.len() && bytes[cursor] & 0xC0 == 0x80 {
                                        cursor += 1;
                                    }
                                    literal.push_str(&text[start..cursor]);
                                }
                            }
                        }
                        if !closed {
                            break;
                        }
                        message.push_str(&literal);
                        matched = true;
                    } else {
                        break;
                    }
                }
                if matched
                    && cursor < bytes.len()
                    && (bytes[cursor] == b',' || bytes[cursor] == b')')
                {
                    out.push(message);
                }
                i = cursor.max(i + 1);
            } else {
                i += 1;
            }
        }
        out
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
