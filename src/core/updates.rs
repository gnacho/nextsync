//! Update check against the published version manifest.
//!
//! Fase 5 (Task 5.6): mirrors `core/updates.py` from the Python v0.4.0. A
//! hand-rolled SemVer type (no semver crate, std only), a strictly validated
//! JSON manifest and a synchronous [`UpdateChecker`] over the shared
//! [`HttpClient`]. The result is non-fatal by design: network, HTTP and
//! manifest failures come back as [`UpdateCheckResult::error`] instead of
//! panicking.
//!
//! Deviations from the Python (motivated):
//! - The URLs point at `gnacho/nextsync` (this rewrite) instead of
//!   `gnacho/nextsync-py`.
//! - `released_at` is kept as the validated UTC text (no `datetime` type in
//!   std); `released_at_utc_text` formats it exactly like the Python.
//! - The checker is synchronous and returns the result (the Python used a
//!   callback with cancellation); the caller runs it off the main thread.
//! - The production transport reuses the shared 30 s agent instead of the
//!   Python's dedicated 8 s client.

use std::fmt;

use crate::nextcloud::api::{HttpClient, UreqHttpClient};

/// Canonical version manifest served from the repository's default branch.
pub const VERSION_MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/gnacho/nextsync/main/version.json";
/// Human-facing landing page for the latest release.
pub const RELEASES_URL: &str = "https://github.com/gnacho/nextsync/releases/latest";
/// The manifest is tiny; anything larger is rejected before parsing.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_SUMMARY_CHARACTERS: usize = 8_000;
const MAX_CHANGELOG_ITEMS: usize = 100;
const MAX_CHANGELOG_ITEM_CHARACTERS: usize = 2_000;

/// The manifest is invalid or incomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateManifestError(pub String);

impl fmt::Display for UpdateManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UpdateManifestError {}

/// A strict SemVer 2.0.0 version.
#[derive(Debug, Clone, Eq)]
pub struct SemanticVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Vec<String>,
    pub build: Vec<String>,
}

impl SemanticVersion {
    /// Parse a version string, rejecting anything SemVer forbids (leading
    /// zeroes in numeric identifiers, empty prereleases, non-numeric core
    /// components, surrounding whitespace).
    pub fn parse(value: &str) -> Result<Self, UpdateManifestError> {
        let invalid = |reason: &str| UpdateManifestError(reason.to_string());
        if value.is_empty() || value.trim() != value {
            return Err(invalid("Version must be a non-empty SemVer string."));
        }
        let (core_and_suffix, plus, build_text) = split_once_any(value, '+');
        if !plus.is_empty() && (build_text.is_empty() || build_text.contains('+')) {
            return Err(invalid("Invalid SemVer build metadata."));
        }
        let (core_text, dash, prerelease_text) = split_once_any(core_and_suffix, '-');
        if !dash.is_empty() && prerelease_text.is_empty() {
            return Err(invalid("Invalid empty SemVer prerelease."));
        }
        let core: Vec<&str> = core_text.split('.').collect();
        if core.len() != 3 || !core.iter().all(|item| is_core_identifier(item)) {
            return Err(invalid(
                "Version must contain three numeric SemVer components.",
            ));
        }
        let prerelease =
            parse_identifiers(if dash.is_empty() { "" } else { prerelease_text }, true)?;
        let build = parse_identifiers(if plus.is_empty() { "" } else { build_text }, false)?;
        Ok(Self {
            major: core[0].parse().unwrap_or(0),
            minor: core[1].parse().unwrap_or(0),
            patch: core[2].parse().unwrap_or(0),
            prerelease,
            build,
        })
    }
}

impl PartialEq for SemanticVersion {
    /// Build metadata is ignored for equality (SemVer §10).
    fn eq(&self, other: &Self) -> bool {
        self.major == other.major
            && self.minor == other.minor
            && self.patch == other.patch
            && self.prerelease == other.prerelease
    }
}

impl PartialOrd for SemanticVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SemanticVersion {
    /// Precedence per SemVer §11: core numerically, then prerelease (a
    /// release outranks any prerelease; identifiers compare numerically when
    /// both are numeric, numeric beats alphanumeric, then lexically; a
    /// longer identifier list wins when all shared identifiers are equal).
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let own_core = (self.major, self.minor, self.patch);
        let other_core = (other.major, other.minor, other.patch);
        if own_core != other_core {
            return own_core.cmp(&other_core);
        }
        match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }
        for (own, theirs) in self.prerelease.iter().zip(other.prerelease.iter()) {
            if own == theirs {
                continue;
            }
            let own_numeric = own.chars().all(|c| c.is_ascii_digit());
            let theirs_numeric = theirs.chars().all(|c| c.is_ascii_digit());
            return match (own_numeric, theirs_numeric) {
                (true, true) => own
                    .parse::<u64>()
                    .unwrap_or(0)
                    .cmp(&theirs.parse::<u64>().unwrap_or(0)),
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => own.cmp(theirs),
            };
        }
        self.prerelease.len().cmp(&other.prerelease.len())
    }
}

/// The published update manifest (schema 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateManifest {
    pub version_text: String,
    pub version: SemanticVersion,
    pub mandatory: bool,
    pub summary: String,
    pub changelog: Vec<String>,
    /// The validated ISO 8601 UTC release timestamp (raw text).
    pub released_at: String,
}

impl UpdateManifest {
    /// The release date formatted as `YYYY-MM-DD HH:MM UTC`.
    pub fn released_at_utc_text(&self) -> String {
        // "2026-08-09T01:51:24[.f]Z" -> "2026-08-09 01:51 UTC".
        let text = &self.released_at;
        let date = &text[0..10];
        let time = &text[11..16];
        format!("{date} {time} UTC")
    }
}

/// The outcome of one update check. Failures are reported through `error`;
/// `latest` is only present when the manifest parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateCheckResult {
    pub latest: Option<UpdateManifest>,
    pub update_available: bool,
    pub error: Option<String>,
}

/// Split on the first occurrence of `separator`, returning the head, the
/// separator (empty when none) and the tail.
fn split_once_any(text: &str, separator: char) -> (&str, &str, &str) {
    match text.find(separator) {
        Some(index) => (
            &text[..index],
            &text[index..index + separator.len_utf8()],
            &text[index + separator.len_utf8()..],
        ),
        None => (text, "", ""),
    }
}

/// `0` or a multi-digit number without leading zeroes.
fn is_core_identifier(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if value.len() > 1 && value.starts_with('0') {
        return false;
    }
    value.chars().all(|c| c.is_ascii_digit())
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn parse_identifiers(value: &str, prerelease: bool) -> Result<Vec<String>, UpdateManifestError> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let identifiers: Vec<&str> = value.split('.').collect();
    if !identifiers.iter().all(|item| is_identifier(item)) {
        return Err(UpdateManifestError(
            "Invalid SemVer identifier.".to_string(),
        ));
    }
    if prerelease
        && identifiers.iter().any(|item| {
            item.chars().all(|c| c.is_ascii_digit()) && item.len() > 1 && item.starts_with('0')
        })
    {
        return Err(UpdateManifestError(
            "Numeric prerelease identifiers cannot have leading zeroes.".to_string(),
        ));
    }
    Ok(identifiers.iter().map(|item| item.to_string()).collect())
}

/// `[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]{1,6})?Z`
fn is_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20 {
        return false;
    }
    let digits_at = |range: std::ops::Range<usize>| {
        range
            .clone()
            .all(|index| bytes.get(index).is_some_and(u8::is_ascii_digit))
    };
    if !digits_at(0..4)
        || bytes.get(4) != Some(&b'-')
        || !digits_at(5..7)
        || bytes.get(7) != Some(&b'-')
        || !digits_at(8..10)
        || bytes.get(10) != Some(&b'T')
        || !digits_at(11..13)
        || bytes.get(13) != Some(&b':')
        || !digits_at(14..16)
        || bytes.get(16) != Some(&b':')
        || !digits_at(17..19)
    {
        return false;
    }
    let mut index = 19;
    if bytes.get(index) == Some(&b'.') {
        let fraction_start = index + 1;
        index = fraction_start;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        let fraction_len = index - fraction_start;
        if fraction_len == 0 || fraction_len > 6 {
            return false;
        }
    }
    index == bytes.len() - 1 && bytes.get(index) == Some(&b'Z')
}

/// Parse and strictly validate an update manifest.
pub fn parse_update_manifest(data: &[u8]) -> Result<UpdateManifest, UpdateManifestError> {
    if data.len() > MAX_MANIFEST_BYTES {
        return Err(UpdateManifestError(
            "The update manifest is too large.".to_string(),
        ));
    }
    let decoded = std::str::from_utf8(data)
        .map_err(|_| UpdateManifestError("The update manifest is not valid UTF-8 JSON.".into()))?;
    let payload: serde_json::Value = serde_json::from_str(decoded)
        .map_err(|_| UpdateManifestError("The update manifest is not valid UTF-8 JSON.".into()))?;
    let serde_json::Value::Object(object) = payload else {
        return Err(UpdateManifestError(
            "The update manifest root must be an object.".to_string(),
        ));
    };
    // `type(...) is not int` in the Python: booleans are not integers here
    // because serde_json keeps them distinct types.
    let schema = object
        .get("schema_version")
        .and_then(|value| value.as_i64());
    if schema != Some(1) {
        return Err(UpdateManifestError(
            "Unsupported or missing update manifest schema.".to_string(),
        ));
    }
    let version_text = object.get("version").and_then(|value| value.as_str());
    let version = SemanticVersion::parse(version_text.unwrap_or(""))
        .map_err(|_| UpdateManifestError("The update manifest version is invalid.".into()))?;
    let mandatory = object.get("mandatory").and_then(|value| value.as_bool());
    let Some(mandatory) = mandatory else {
        return Err(UpdateManifestError(
            "The mandatory field must be a boolean.".to_string(),
        ));
    };
    let summary = object.get("summary").and_then(|value| value.as_str());
    let Some(summary) = summary.filter(|text| !text.trim().is_empty()) else {
        return Err(UpdateManifestError(
            "The update summary is missing or invalid.".to_string(),
        ));
    };
    let summary = summary.trim();
    if summary.chars().count() > MAX_SUMMARY_CHARACTERS {
        return Err(UpdateManifestError(
            "The update summary is too long.".to_string(),
        ));
    }
    let changelog_value = object.get("changelog").and_then(|value| value.as_array());
    let Some(changelog_value) = changelog_value.filter(|items| !items.is_empty()) else {
        return Err(UpdateManifestError(
            "The update changelog is missing or invalid.".to_string(),
        ));
    };
    if changelog_value.len() > MAX_CHANGELOG_ITEMS {
        return Err(UpdateManifestError(
            "The update changelog contains too many items.".to_string(),
        ));
    }
    let mut changelog = Vec::with_capacity(changelog_value.len());
    for item in changelog_value {
        let Some(text) = item.as_str().filter(|text| !text.trim().is_empty()) else {
            return Err(UpdateManifestError(
                "The update changelog contains an invalid item.".to_string(),
            ));
        };
        let text = text.trim();
        if text.chars().count() > MAX_CHANGELOG_ITEM_CHARACTERS {
            return Err(UpdateManifestError(
                "An update changelog item is too long.".to_string(),
            ));
        }
        changelog.push(text.to_string());
    }
    let released_at = object.get("released_at").and_then(|value| value.as_str());
    let Some(released_at) = released_at.filter(|text| is_utc_timestamp(text)) else {
        return Err(UpdateManifestError(
            "The release date must be an ISO 8601 UTC value.".to_string(),
        ));
    };
    Ok(UpdateManifest {
        version_text: version_text.unwrap_or_default().to_string(),
        version,
        mandatory,
        summary: summary.to_string(),
        changelog,
        released_at: released_at.to_string(),
    })
}

/// Parse a manifest and compare it against the installed version.
pub fn evaluate_update(
    data: &[u8],
    current_version: &str,
) -> Result<UpdateCheckResult, UpdateManifestError> {
    let latest = parse_update_manifest(data)?;
    let installed = SemanticVersion::parse(current_version)
        .map_err(|_| UpdateManifestError("The installed application version is invalid.".into()))?;
    Ok(UpdateCheckResult {
        update_available: latest.version > installed,
        latest: Some(latest),
        error: None,
    })
}

/// Synchronous update checker over the shared HTTP transport.
pub struct UpdateChecker {
    http: Box<dyn HttpClient>,
    url: String,
}

impl UpdateChecker {
    /// Production checker against the canonical manifest URL.
    pub fn new() -> Self {
        Self {
            http: Box::new(UreqHttpClient::new()),
            url: VERSION_MANIFEST_URL.to_string(),
        }
    }

    /// Test/injected constructor.
    pub fn with_http(http: Box<dyn HttpClient>, url: String) -> Self {
        Self { http, url }
    }

    /// The manifest URL this checker queries.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Fetch and evaluate the manifest. Never panics: transport, HTTP and
    /// manifest failures land in `error`.
    pub fn check(&self, current_version: &str) -> UpdateCheckResult {
        let response = self.http.request(
            "GET",
            &self.url,
            &[
                ("Accept", "application/json"),
                ("Cache-Control", "no-cache"),
            ],
            None,
        );
        match response {
            Err(error) => UpdateCheckResult {
                error: Some(error.to_string()),
                ..UpdateCheckResult::default()
            },
            Ok(response) => {
                if response.status != 200 {
                    return UpdateCheckResult {
                        error: Some(format!("HTTP status {}", response.status)),
                        ..UpdateCheckResult::default()
                    };
                }
                match evaluate_update(&response.body, current_version) {
                    Ok(result) => result,
                    Err(error) => UpdateCheckResult {
                        error: Some(error.to_string()),
                        ..UpdateCheckResult::default()
                    },
                }
            }
        }
    }
}

impl Default for UpdateChecker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nextcloud::api::{ApiError, HttpResponse};

    fn manifest_bytes(changes: &[(&str, serde_json::Value)]) -> Vec<u8> {
        let mut payload = serde_json::json!({
            "schema_version": 1,
            "version": "1.2.0",
            "mandatory": false,
            "released_at": "2026-08-09T01:51:24Z",
            "summary": "Corrections and improvements.",
            "changelog": ["Fixed one issue.", "Improved one workflow."],
        });
        let serde_json::Value::Object(object) = &mut payload else {
            unreachable!();
        };
        for (key, value) in changes {
            object.insert(key.to_string(), value.clone());
        }
        serde_json::to_vec(&payload).unwrap()
    }

    #[test]
    fn numeric_components_are_not_compared_as_strings() {
        assert!(
            SemanticVersion::parse("1.10.0").unwrap() > SemanticVersion::parse("1.9.9").unwrap()
        );
    }

    #[test]
    fn semver_prerelease_precedence_and_build_metadata() {
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        let parsed: Vec<SemanticVersion> = ordered
            .iter()
            .map(|value| SemanticVersion::parse(value).unwrap())
            .collect();
        let mut sorted = parsed.clone();
        sorted.sort();
        assert_eq!(parsed, sorted);
        assert_eq!(
            SemanticVersion::parse("1.0.0+build.1").unwrap(),
            SemanticVersion::parse("1.0.0+build.2").unwrap()
        );
    }

    #[test]
    fn invalid_versions_are_rejected() {
        for value in ["1.2", "1.02.3", "1.2.3-", "1.2.3-01", "v1.2.3", ""] {
            assert!(
                SemanticVersion::parse(value).is_err(),
                "expected {value:?} to be rejected"
            );
        }
    }

    #[test]
    fn valid_manifest_contains_plain_summary_and_utc_release_date() {
        let manifest =
            parse_update_manifest(&manifest_bytes(&[("mandatory", serde_json::json!(true))]))
                .unwrap();
        assert_eq!(manifest.version_text, "1.2.0");
        assert!(manifest.mandatory);
        assert_eq!(manifest.summary, "Corrections and improvements.");
        assert_eq!(
            manifest.changelog,
            vec!["Fixed one issue.", "Improved one workflow."]
        );
        assert_eq!(manifest.released_at_utc_text(), "2026-08-09 01:51 UTC");
    }

    #[test]
    fn fractional_utc_timestamps_are_accepted() {
        let manifest = parse_update_manifest(&manifest_bytes(&[(
            "released_at",
            serde_json::json!("2026-08-09T01:51:24.512Z"),
        )]))
        .unwrap();
        assert_eq!(manifest.released_at_utc_text(), "2026-08-09 01:51 UTC");
    }

    #[test]
    fn newer_equal_and_older_versions_are_evaluated_safely() {
        assert!(
            evaluate_update(
                &manifest_bytes(&[("version", serde_json::json!("1.10.0"))]),
                "1.9.9"
            )
            .unwrap()
            .update_available
        );
        assert!(
            !evaluate_update(
                &manifest_bytes(&[("version", serde_json::json!("1.9.9"))]),
                "1.9.9"
            )
            .unwrap()
            .update_available
        );
        assert!(
            !evaluate_update(
                &manifest_bytes(&[("version", serde_json::json!("1.8.9"))]),
                "1.9.9"
            )
            .unwrap()
            .update_available
        );
    }

    #[test]
    fn invalid_or_incomplete_manifests_are_rejected() {
        let invalid: Vec<Vec<u8>> = vec![
            b"not-json".to_vec(),
            b"[]".to_vec(),
            manifest_bytes(&[("schema_version", serde_json::json!(2))]),
            manifest_bytes(&[("version", serde_json::json!("latest"))]),
            manifest_bytes(&[("mandatory", serde_json::json!("false"))]),
            manifest_bytes(&[("summary", serde_json::json!(""))]),
            manifest_bytes(&[("changelog", serde_json::json!([]))]),
            manifest_bytes(&[("changelog", serde_json::json!(["Valid", ""]))]),
            manifest_bytes(&[("released_at", serde_json::json!("2026-08-09Z"))]),
            manifest_bytes(&[(
                "released_at",
                serde_json::json!("2026-08-09T01:51:24-03:00"),
            )]),
        ];
        for value in &invalid {
            assert!(
                parse_update_manifest(value).is_err(),
                "expected {:?} to be rejected",
                String::from_utf8_lossy(&value[..value.len().min(60)])
            );
        }
    }

    #[test]
    fn manifest_download_is_bounded() {
        let mut oversized = vec![b'{'];
        oversized.resize(1 + MAX_MANIFEST_BYTES, b' ');
        oversized.push(b'}');
        assert!(parse_update_manifest(&oversized).is_err());
    }

    struct FakeHttp {
        status: u16,
        data: Vec<u8>,
        error: Option<String>,
    }

    impl HttpClient for FakeHttp {
        fn request(
            &self,
            _method: &str,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: Option<&[u8]>,
        ) -> Result<HttpResponse, ApiError> {
            if self.error.is_some() {
                return Err(ApiError::Transport);
            }
            Ok(HttpResponse {
                status: self.status,
                body: self.data.clone(),
            })
        }
    }

    #[test]
    fn network_and_http_failures_become_non_fatal_results() {
        let fakes = [
            FakeHttp {
                status: 200,
                data: Vec::new(),
                error: Some("offline".to_string()),
            },
            FakeHttp {
                status: 503,
                data: Vec::new(),
                error: None,
            },
            FakeHttp {
                status: 200,
                data: b"invalid".to_vec(),
                error: None,
            },
        ];
        for fake in &fakes {
            let checker = UpdateChecker::with_http(
                Box::new(FakeHttp {
                    status: fake.status,
                    data: fake.data.clone(),
                    error: fake.error.clone(),
                }),
                VERSION_MANIFEST_URL.to_string(),
            );
            let result = checker.check("1.0.0");
            assert!(result.error.is_some(), "expected an error result");
        }
    }

    #[test]
    fn repository_version_manifest_is_valid_and_matches_the_crate() {
        let data = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/version.json"))
            .expect("version.json ships with the repository");
        let manifest = parse_update_manifest(&data).unwrap();
        assert_eq!(
            manifest.version_text,
            env!("CARGO_PKG_VERSION"),
            "version.json and Cargo.toml must agree"
        );

        let metainfo = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/io.github.gnacho.nextsync.metainfo.xml"
        ))
        .expect("metainfo.xml ships with the repository");
        let release_tag = format!("<release version=\"{}\"", env!("CARGO_PKG_VERSION"));
        assert!(
            metainfo.contains(&release_tag),
            "metainfo.xml <release> must carry the current version"
        );
    }

    #[test]
    fn checker_uses_the_canonical_json_and_releases_urls() {
        let checker = UpdateChecker::with_http(
            Box::new(FakeHttp {
                status: 200,
                data: manifest_bytes(&[("version", serde_json::json!("0.1.17"))]),
                error: None,
            }),
            VERSION_MANIFEST_URL.to_string(),
        );
        assert_eq!(checker.url(), VERSION_MANIFEST_URL);
        assert!(checker
            .url()
            .contains("raw.githubusercontent.com/gnacho/nextsync"));
        assert_eq!(
            RELEASES_URL,
            "https://github.com/gnacho/nextsync/releases/latest"
        );
        let result = checker.check("0.1.17");
        assert!(result.error.is_none());
        assert!(!result.update_available);
    }
}
