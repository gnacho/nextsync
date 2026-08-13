//! Log redaction.
//!
//! Port of `nextsync/util/redact.py` (v0.4.0): centralized secret removal for
//! logs and diagnostic text. Three structural patterns are scrubbed without a
//! regex crate (the project keeps the dependency list regex-free), and an
//! optional set of known secrets (passwords/tokens resolved at runtime) is
//! erased verbatim, longest first.
//!
//! [`Redact::redact_line`] is the stateless entry point wired into the log
//! buffer ([`crate::core::log`]) and the sync-engine output capture
//! ([`crate::nextcloud::sync_engine`]); it runs only the three structural
//! passes. [`Redactor`] adds the known-secret scrub and mirrors the Python
//! `Redactor` API (`new`/`add_secret`/`redact`) so the app can register the
//! credentials it just looked up before emitting diagnostics.

/// The placeholder substituted for every redacted value, matching the Python
/// constant.
pub const REDACTED: &str = "[REDACTED]";

/// Stateless redaction helpers (the structural passes only).
pub struct Redact;

impl Redact {
    /// Redacts a line of log output.
    ///
    /// Runs the three structural passes (URL userinfo, query-string secrets and
    /// `Authorization` headers). Known-secret scrubbing needs instance state
    /// and lives on [`Redactor`]; the log buffer adopts it when it owns a
    /// `Redactor` instance.
    pub fn redact_line(line: &str) -> String {
        redact_patterns(line)
    }
}

/// Stateful redactor with an optional set of known secrets.
///
/// Mirrors `nextsync.util.redact.Redactor`: the three structural passes always
/// run, then every registered secret of three or more characters is replaced
/// verbatim (longest first, so a secret that contains another is erased in a
/// single pass).
pub struct Redactor {
    secrets: Vec<String>,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

impl Redactor {
    /// Create an empty redactor.
    pub fn new() -> Self {
        Self {
            secrets: Vec::new(),
        }
    }

    /// Create a redactor preloaded with a set of secrets.
    pub fn from_secrets<I, S>(secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            secrets: secrets
                .into_iter()
                .map(|s| s.as_ref().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }

    /// Register an additional secret to scrub. Empty values are ignored, like
    /// the Python `add_secret`.
    pub fn add_secret(&mut self, secret: &str) {
        if !secret.is_empty() {
            self.secrets.push(secret.to_string());
        }
    }

    /// Redact `value`: structural patterns first, then every known secret.
    pub fn redact(&self, value: &str) -> String {
        let text = redact_patterns(value);
        self.redact_secrets(text)
    }

    fn redact_secrets(&self, mut text: String) -> String {
        // Python sorts by `len` descending; character count keeps the order
        // correct for non-ASCII secrets too. Secrets shorter than three
        // characters are skipped to avoid clobbering common substrings.
        let mut secrets: Vec<&str> = self
            .secrets
            .iter()
            .map(String::as_str)
            .filter(|s| s.chars().count() >= 3)
            .collect();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.chars().count()));
        for secret in secrets {
            text = text.replace(secret, REDACTED);
        }
        text
    }
}

/// Apply the three structural redaction passes in Python order.
fn redact_patterns(text: &str) -> String {
    let text = redact_userinfo(text);
    let text = redact_query(&text);
    redact_authorization(&text)
}

/// `https?://user:pass@` -> `https?://[REDACTED]@`.
///
/// The whole `user:pass` pair collapses into [`REDACTED`] (the Python regex
/// keeps only the scheme). User cannot contain `:`; the password may.
fn redact_userinfo(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < n {
        let scheme = if starts_with_ci(&chars, i, "https://") {
            Some(8)
        } else if starts_with_ci(&chars, i, "http://") {
            Some(7)
        } else {
            None
        };
        if let Some(scheme_len) = scheme {
            let after_scheme = i + scheme_len;
            match match_userinfo(&chars, after_scheme) {
                Some(end) => {
                    out.extend(&chars[i..after_scheme]);
                    out.push_str(REDACTED);
                    out.push('@');
                    i = end;
                }
                None => {
                    out.extend(&chars[i..after_scheme]);
                    i = after_scheme;
                }
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Try to match a minimal `user:pass@` starting at `start`.
///
/// Returns the index past the closing `@`, or `None` when the run does not look
/// like credentials. Mirrors `(https?://)([^/@\s:]+):([^/@\s]+)@`.
fn match_userinfo(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start;
    let user_start = i;
    while i < chars.len() && !is_userinfo_stop(chars[i]) && chars[i] != ':' {
        i += 1;
    }
    if i == user_start || i >= chars.len() || chars[i] != ':' {
        return None;
    }
    i += 1; // consume ':'
    let pass_start = i;
    while i < chars.len() && !is_userinfo_stop(chars[i]) {
        i += 1;
    }
    if i == pass_start || i >= chars.len() || chars[i] != '@' {
        return None;
    }
    Some(i + 1)
}

fn is_userinfo_stop(c: char) -> bool {
    c.is_ascii_whitespace() || c == '/' || c == '@'
}

/// `[?&](token|password|appPassword|access_token)=value` -> value redacted.
fn redact_query(input: &str) -> String {
    const KEYS: [&str; 4] = ["token", "password", "appPassword", "access_token"];
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < n {
        if chars[i] == '?' || chars[i] == '&' {
            if let Some((value_start, value_end)) = match_query_secret(&chars, i, &KEYS) {
                out.extend(&chars[i..value_start]);
                out.push_str(REDACTED);
                i = value_end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Match one of `keys` followed by `=` and a non-empty value at `?`/`&`.
///
/// Returns `(value_start, value_end)` where the slice `[i..value_start]` is the
/// prefix to keep (`?key=`) and `[value_start..value_end]` is the secret.
fn match_query_secret(chars: &[char], i: usize, keys: &[&str]) -> Option<(usize, usize)> {
    let key_start = i + 1;
    for key in keys {
        let after_key = key_start + key.chars().count();
        if starts_with_ci(chars, key_start, key)
            && after_key < chars.len()
            && chars[after_key] == '='
        {
            let value_start = after_key + 1;
            let mut j = value_start;
            while j < chars.len() && chars[j] != '&' && !chars[j].is_ascii_whitespace() {
                j += 1;
            }
            if j > value_start {
                return Some((value_start, j));
            }
        }
    }
    None
}

/// `Authorization: (Basic|Bearer) <token>` -> token redacted.
fn redact_authorization(input: &str) -> String {
    const PREFIX: &str = "authorization:";
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < n {
        if starts_with_ci(&chars, i, PREFIX) {
            let mut j = i + PREFIX.chars().count();
            while j < n && chars[j].is_ascii_whitespace() {
                j += 1;
            }
            let scheme_len = if starts_with_ci(&chars, j, "basic") {
                5
            } else if starts_with_ci(&chars, j, "bearer") {
                6
            } else {
                0
            };
            if scheme_len > 0 {
                let mut k = j + scheme_len;
                let whitespace_start = k;
                while k < n && chars[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k > whitespace_start {
                    let token_start = k;
                    while k < n && !chars[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    if k > token_start {
                        out.extend(&chars[i..token_start]);
                        out.push_str(REDACTED);
                        i = k;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Case-insensitive prefix match for an ASCII literal against `chars[pos..]`.
fn starts_with_ci(chars: &[char], pos: usize, lit: &str) -> bool {
    for (i, expected) in (pos..).zip(lit.chars()) {
        if i >= chars.len() || !chars[i].eq_ignore_ascii_case(&expected) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- URL userinfo -------------------------------------------------------

    #[test]
    fn redacts_userinfo_password() {
        assert_eq!(
            Redact::redact_line("https://alice:secret@cloud.example.com/path"),
            "https://[REDACTED]@cloud.example.com/path"
        );
    }

    #[test]
    fn redacts_userinfo_for_plain_http() {
        assert_eq!(
            Redact::redact_line("http://bob:hunter2@cloud.example.com"),
            "http://[REDACTED]@cloud.example.com"
        );
    }

    #[test]
    fn redacts_userinfo_regardless_of_case() {
        assert_eq!(
            Redact::redact_line("HTTPS://Alice:Secret@cloud.example.com"),
            "HTTPS://[REDACTED]@cloud.example.com"
        );
    }

    #[test]
    fn leaves_url_without_credentials_alone() {
        assert_eq!(
            Redact::redact_line("https://cloud.example.com/path?x=1"),
            "https://cloud.example.com/path?x=1"
        );
    }

    #[test]
    fn password_segment_may_contain_colons() {
        // `[^/@\s]+` allows ':' in the password.
        assert_eq!(
            Redact::redact_line("https://u:p:a:b@cloud.example.com"),
            "https://[REDACTED]@cloud.example.com"
        );
    }

    #[test]
    fn empty_user_or_password_is_not_redacted() {
        // No user, or empty password, does not match the userinfo pattern.
        assert_eq!(
            Redact::redact_line("https://@cloud.example.com"),
            "https://@cloud.example.com"
        );
        assert_eq!(
            Redact::redact_line("https://u:@cloud.example.com"),
            "https://u:@cloud.example.com"
        );
    }

    // ---- query-string secrets ----------------------------------------------

    #[test]
    fn redacts_token_query_parameter() {
        assert_eq!(
            Redact::redact_line("/remote.php?token=abc123&foo=bar"),
            "/remote.php?token=[REDACTED]&foo=bar"
        );
    }

    #[test]
    fn redacts_known_query_secret_keys() {
        assert_eq!(
            Redact::redact_line("?password=p&appPassword=a&access_token=t"),
            "?password=[REDACTED]&appPassword=[REDACTED]&access_token=[REDACTED]"
        );
    }

    #[test]
    fn redacts_query_secrets_regardless_of_case() {
        assert_eq!(
            Redact::redact_line("?TOKEN=ABC&Access_Token=DEF"),
            "?TOKEN=[REDACTED]&Access_Token=[REDACTED]"
        );
    }

    #[test]
    fn unrelated_query_parameters_are_kept() {
        assert_eq!(
            Redact::redact_line("?file=report.pdf&user=alice"),
            "?file=report.pdf&user=alice"
        );
    }

    // ---- Authorization header ----------------------------------------------

    #[test]
    fn redacts_basic_authorization_header() {
        assert_eq!(
            Redact::redact_line("Authorization: Basic dXNlcjpwYXNz"),
            "Authorization: Basic [REDACTED]"
        );
    }

    #[test]
    fn redacts_bearer_authorization_header() {
        assert_eq!(
            Redact::redact_line("authorization: Bearer abc.def.ghi tail"),
            "authorization: Bearer [REDACTED] tail"
        );
    }

    #[test]
    fn authorization_without_scheme_is_kept() {
        assert_eq!(
            Redact::redact_line("Authorization: Custom value"),
            "Authorization: Custom value"
        );
    }

    // ---- known secrets ------------------------------------------------------

    #[test]
    fn redactor_scrubs_registered_secrets_longest_first() {
        let redactor = Redactor::from_secrets(["super-secret", "secret"]);
        // "super-secret" must be replaced first so "secret" does not leave the
        // "super-" prefix behind.
        assert_eq!(
            redactor.redact("login=super-secret fallback=secret"),
            "login=[REDACTED] fallback=[REDACTED]"
        );
    }

    #[test]
    fn redactor_ignores_secrets_shorter_than_three_chars() {
        let redactor = Redactor::from_secrets(["ab", "longenough"]);
        assert_eq!(redactor.redact("ab longenough ab"), "ab [REDACTED] ab");
    }

    #[test]
    fn add_secret_accumulates() {
        let mut redactor = Redactor::new();
        redactor.add_secret("alpha-bravo");
        redactor.add_secret("");
        assert_eq!(redactor.redact("alpha-bravo"), "[REDACTED]");
    }

    // ---- Python parity (tests/unit/test_redaction.py) -----------------------

    #[test]
    fn python_redaction_parity() {
        let redactor = Redactor::from_secrets(["secret-value"]);
        let text = redactor.redact(
            "secret-value https://alice:password@example.com?a=1&token=abc Authorization: Bearer xyz",
        );
        for secret in ["secret-value", "password", "abc", "xyz"] {
            assert!(!text.contains(secret), "leaked {secret}: {text}");
        }
        assert!(text.contains("[REDACTED]"), "no redaction marker: {text}");
    }

    #[test]
    fn plain_text_without_secrets_is_unchanged() {
        assert_eq!(
            Redact::redact_line("Synchronization completed for /docs"),
            "Synchronization completed for /docs"
        );
    }
}
