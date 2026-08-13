//! Name-based exclusion matcher.
//!
//! Fase 3 (Task 3.1): mirrors `core/exclusions.py` — a small `fnmatch`-style
//! matcher over file *names* (no paths) that decides which disposable files
//! the filesystem watcher and the deletion guard ignore, so both agree with
//! what `nextcloudcmd` sees via its `--exclude` file.
//!
//! Patterns are validated exactly like `exclusions.validate_pattern`: no path
//! separators, no `..`, and the three too-broad patterns (`*`, `.*`, `*.*`)
//! are rejected.

use std::path::Path;

use crate::storage::config::{validate_pattern, DEFAULT_PATTERNS};

/// A matcher over a validated list of glob patterns.
///
/// `enabled = false` (the `exclude_patterns_enabled` switch) makes every
/// [`matches_name`](Self::matches_name) call return `false`.
#[derive(Debug, Clone)]
pub struct ExclusionMatcher {
    enabled: bool,
    patterns: Vec<String>,
}

impl ExclusionMatcher {
    /// Build a matcher, validating every pattern (invalid ones are dropped).
    pub fn new(patterns: impl IntoIterator<Item = impl AsRef<str>>, enabled: bool) -> Self {
        let patterns = patterns
            .into_iter()
            .filter_map(|pattern| validate_pattern(pattern.as_ref()).ok())
            .collect();
        Self { enabled, patterns }
    }

    /// The default matcher used when the user has not configured patterns.
    pub fn defaults() -> Self {
        Self::new(DEFAULT_PATTERNS, true)
    }

    /// Whether any pattern was successfully validated.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Number of validated patterns.
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// Match a bare file name against every pattern (`fnmatch.fnmatchcase`).
    pub fn matches_name(&self, name: &str) -> bool {
        if !self.enabled {
            return false;
        }
        self.patterns
            .iter()
            .any(|pattern| glob_match(pattern.as_bytes(), name.as_bytes()))
    }

    /// Match the file name of a path (like the Python `matches_path`).
    pub fn matches_path(&self, path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| self.matches_name(name))
            .unwrap_or(false)
    }
}

/// Case-sensitive glob match over bytes, replicating the subset of
/// `fnmatch.fnmatchcase` the validated patterns can contain (`*`, `?` and
/// `[...]` character classes with `!` negation and ranges). A `[` without a
/// closing `]` is treated as a literal character, like fnmatch does.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let mut p = 0;
    let mut t = 0;
    // Backtracking positions: when a `*` stops matching we retry with the
    // star consuming one more byte of text (`t = ++star_t`, `p = star_p + 1`).
    let mut star_p = usize::MAX;
    let mut star_t = 0;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'[' {
            match match_class(pattern, p, text[t]) {
                Some((next, matched)) => {
                    if matched {
                        p = next;
                        t += 1;
                    } else if !backtrack(&mut p, &mut t, &mut star_p, &mut star_t) {
                        return false;
                    }
                }
                None => {
                    // Unclosed class: `[` is a literal.
                    if !backtrack(&mut p, &mut t, &mut star_p, &mut star_t) {
                        return false;
                    }
                }
            }
        } else if p < pattern.len() && pattern[p] == b'*' {
            star_p = p;
            star_t = t;
            p += 1;
        } else if !backtrack(&mut p, &mut t, &mut star_p, &mut star_t) {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// Let the most recent `*` consume one more text byte. Returns `false` when
/// there is no star to fall back to.
fn backtrack(p: &mut usize, t: &mut usize, star_p: &mut usize, star_t: &mut usize) -> bool {
    if *star_p == usize::MAX {
        return false;
    }
    *star_t += 1;
    *t = *star_t;
    *p = *star_p + 1;
    true
}

/// Try to parse the character class starting at `pattern[start]` (`[`), which
/// must match byte `text_byte`. Returns the position just past the class and
/// whether it matched, or `None` when the class is unterminated.
fn match_class(pattern: &[u8], start: usize, text_byte: u8) -> Option<(usize, bool)> {
    let mut p = start + 1;
    let negated = p < pattern.len() && (pattern[p] == b'!' || pattern[p] == b'^');
    if negated {
        p += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        if p >= pattern.len() {
            return None;
        }
        let byte = pattern[p];
        if byte == b']' && !first {
            break;
        }
        first = false;
        if p + 2 < pattern.len() && pattern[p + 1] == b'-' && pattern[p + 2] != b']' {
            let lo = byte;
            let hi = pattern[p + 2];
            if lo <= text_byte && text_byte <= hi {
                matched = true;
            }
            p += 3;
        } else {
            if byte == text_byte {
                matched = true;
            }
            p += 1;
        }
    }
    Some((p + 1, if negated { !matched } else { matched }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_exclude_disposable_files() {
        let matcher = ExclusionMatcher::defaults();
        assert!(matcher.matches_name(".DS_Store"));
        assert!(matcher.matches_name("Thumbs.db"));
        assert!(matcher.matches_name("~$presentation.docx"));
        assert!(matcher.matches_name("draft.swp"));
        assert!(matcher.matches_name("draft.swo"));
        assert!(matcher.matches_name("backup~"));
        assert!(matcher.matches_name(".nextcloudsync.log"));
        assert!(!matcher.matches_name("report.pdf"));
        assert!(!matcher.matches_name("draft.swp2"));
    }

    #[test]
    fn glob_supports_question_mark_and_classes() {
        let matcher = ExclusionMatcher::new(["file?.txt", "[ab]b.*", "log[0-9].txt"], true);
        assert!(matcher.matches_name("file1.txt"));
        assert!(matcher.matches_name("fileA.txt"));
        assert!(!matcher.matches_name("file10.txt"));
        assert!(matcher.matches_name("ab.zip"));
        assert!(matcher.matches_name("bb.tar"));
        assert!(!matcher.matches_name("cb.tar"));
        assert!(matcher.matches_name("log3.txt"));
        assert!(!matcher.matches_name("log12.txt"));
    }

    #[test]
    fn disabled_matcher_never_matches() {
        let matcher = ExclusionMatcher::new(["*.swp"], false);
        assert!(!matcher.matches_name("x.swp"));
    }

    #[test]
    fn matches_path_uses_the_file_name() {
        let matcher = ExclusionMatcher::new(["*.tmp"], true);
        assert!(matcher.matches_path(Path::new("/data/sub/file.tmp")));
        assert!(!matcher.matches_path(Path::new("/data/sub/file.txt")));
    }

    #[test]
    fn invalid_patterns_are_dropped() {
        let matcher = ExclusionMatcher::new(["a/b", "*", "ok.tmp"], true);
        assert_eq!(matcher.len(), 1);
        assert!(matcher.matches_name("ok.tmp"));
        assert!(!matcher.matches_name("x.tmp"));
    }

    #[test]
    fn unclosed_class_treats_bracket_as_literal() {
        let matcher = ExclusionMatcher::new(["[.txt"], true);
        assert!(matcher.matches_name("[.txt"));
    }

    #[test]
    fn validate_pattern_rejects_the_python_forbidden_set() {
        for bad in ["*", ".*", "*.*", "a/b", "a\\b", "..foo", "", "a\0b"] {
            assert!(
                validate_pattern(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        assert_eq!(validate_pattern(" *.tmp ").unwrap(), "*.tmp");
    }

    #[test]
    fn pattern_too_long_is_rejected() {
        use crate::storage::config::ConfigError;
        let long = format!("a{}", "b".repeat(260));
        let err = validate_pattern(&long).unwrap_err();
        assert_eq!(
            err,
            ConfigError {
                message: "Pattern is invalid or too long.".into()
            }
        );
    }
}
