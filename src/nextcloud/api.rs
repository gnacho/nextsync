//! Nextcloud HTTP/WebDAV API client.
//!
//! Port of `src/nextsync/nextcloud/api.py` (v0.4.0): a synchronous client used
//! by the remote folder picker (#25) and the setup wizard. Three operations:
//!
//! - [`NextcloudApi::validate_credentials`]: OCS user lookup
//!   (`GET /ocs/v2.php/cloud/user?format=json`) to check a server/user/password
//!   triple and fetch the account display name.
//! - [`NextcloudApi::probe_remote`]: shallow WebDAV PROPFIND (Depth 1, no file
//!   bodies) to check a folder exists and holds at least one entry.
//! - [`NextcloudApi::list_remote_folders`]: PROPFIND against the account root
//!   to list existing top-level folders as normalized paths (`/Documents`).
//! - [`NextcloudApi::revoke_app_password`]: invalidate the app password used
//!   for the session (`DELETE /ocs/v2.php/core/apppassword`) when an account is
//!   removed.
//! - [`NextcloudApi::notifications`]: list the account's server notifications
//!   (shares, comments, mentions) for desktop notifications.
//!
//! # HTTP client choice (verified)
//!
//! The crate had no HTTP client before this module (Cargo.toml checked).
//! [`ureq`] 3.2.0 was chosen over `reqwest` blocking because it is:
//! - synchronous/blocking and pure Rust with a `rustls` TLS backend, matching
//!   the crate's existing TLS stack (`rustls` 0.23 + `rustls-native-certs`,
//!   already used by the `tungstenite` push client) — verified in the official
//!   docs (docs.rs/ureq/3.2.0) and in ureq's `Cargo.toml` (feature `rustls`,
//!   dependency `rustls ^0.23.22`),
//! - MSRV 1.71.1 (verified in `Cargo.toml` of the `3.2.0` tag), below the
//!   crate's declared `rust-version = "1.83"`. ureq 3.3+/3.4 raised the MSRV to
//!   1.85, so the dependency is pinned with `~3.2` to avoid silently breaking
//!   the project's declared MSRV on a future `cargo update`.
//! - able to issue arbitrary HTTP methods (WebDAV PROPFIND): ureq routes any
//!   `http::Request` through `Agent::run`, and non-standard methods are allowed
//!   with `allow_non_standard_methods(true)` — the same pattern as ureq's own
//!   `propfind_with_body` test (`src/lib.rs`, issue #1034).
//! - status codes are *not* turned into errors (`http_status_as_error(false)`),
//!   so the API layer maps 401/403 to `ApiError::AuthRejected` and other
//!   non-2xx codes itself, exactly like the Python.
//!
//! The concrete library stays an implementation detail: the module's public
//! surface only exposes the injectable [`HttpClient`] trait (the Rust analogue
//! of the Python `HttpClient`), so tests can use a fake backend or a local
//! server without touching the network or the real HTTP library.
//!
//! XML is parsed with [`roxmltree`] (0.21, MSRV 1.60, namespace-aware), the
//! Rust counterpart of Python's `xml.etree.ElementTree`: the `{DAV:}` expanded
//! names map to `("DAV:", name)` tuples.
//!
//! # Deviations from `api.py` (motivated)
//!
//! - The Python API is callback-asynchronous; this port is synchronous and
//!   returns `Result`, per the crate's module contract.
//! - `validate_credentials` returns `Ok(None)` when the payload has no
//!   `display-name`, matching the Python (which passes `None` to its callback).

use std::error::Error;
use std::fmt;
use std::time::Duration;

use data_encoding::BASE64;
use roxmltree::{Document, Node};

/// PROPFIND body requesting only `resourcetype` (no file bodies downloaded).
const PROPFIND_BODY: &[u8] = b"<?xml version=\"1.0\"?><d:propfind xmlns:d=\"DAV:\"><d:prop><d:resourcetype/></d:prop></d:propfind>";

/// Request timeout in seconds, mirroring the Python `HttpClient(timeout=30)`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// WebDAV namespace URI (the `{DAV:}` of the Python `xml.etree`).
const DAV_NS: &str = "DAV:";

/// Error returned by the Nextcloud API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The server rejected the credentials (HTTP 401/403).
    AuthRejected,
    /// The server answered with an unexpected HTTP status.
    Http { status: u16 },
    /// The response body was not valid XML or JSON.
    InvalidResponse,
    /// Transport-level failure (connection, DNS, TLS, timeout).
    Transport,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthRejected => f.write_str("The server rejected these credentials."),
            Self::Http { status } => write!(f, "Nextcloud returned HTTP {status}."),
            Self::InvalidResponse => f.write_str("Invalid Nextcloud response."),
            Self::Transport => f.write_str("Network error."),
        }
    }
}

impl Error for ApiError {}

/// A raw HTTP response: status code plus body bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code, including non-2xx (4xx/5xx are not errors).
    pub status: u16,
    /// Raw response body.
    pub body: Vec<u8>,
}

/// Display name plus storage quota for the account summary card.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountSummary {
    /// The server-side display name, when present.
    pub display_name: Option<String>,
    /// Storage used, in bytes. `None` when the server reports no quota.
    pub used: Option<u64>,
    /// Storage total, in bytes. `None` when unlimited.
    pub total: Option<u64>,
}

impl AccountSummary {
    /// Human-readable usage, e.g. `53.2 GB used · unlimited` or
    /// `53.2 GB of 100 GB used`.
    pub fn usage_label(&self) -> String {
        match (self.used, self.total) {
            (Some(used), Some(total)) => {
                format!("{} / {}", format_bytes(used), format_bytes(total))
            }
            (Some(used), None) => format!("{} · {}", format_bytes(used), "unlimited"),
            (None, _) => String::new(),
        }
    }
}

/// One server notification (shares, comments, mentions) from the OCS
/// notifications endpoint. Only the fields needed for a desktop notification
/// are parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerNotification {
    /// The numeric id used to deduplicate notifications across polls.
    pub notification_id: i64,
    /// The app that produced it (e.g. `files_sharing`, `comments`, `spreed`).
    pub app: String,
    /// The parsed subject, e.g. "Alice shared a file with you".
    pub subject: String,
    /// The parsed message (optional; many notifications only have a subject).
    pub message: Option<String>,
}

/// Format bytes with binary prefixes (KiB/MiB/GiB), one decimal.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNIT: f64 = 1024.0;
    let value = bytes as f64;
    if value >= UNIT * UNIT * UNIT {
        format!("{:.1} GiB", value / (UNIT * UNIT * UNIT))
    } else if value >= UNIT * UNIT {
        format!("{:.1} MiB", value / (UNIT * UNIT))
    } else if value >= UNIT {
        format!("{:.1} KiB", value / UNIT)
    } else {
        format!("{bytes} B")
    }
}

/// Minimal HTTP transport abstraction, mirror of the Python `HttpClient`.
///
/// `request` returns transport failures as `Err(ApiError::Transport)`; every
/// HTTP status — including 401/403 and 5xx — is delivered as `Ok` in
/// [`HttpResponse`] so callers can map it themselves (like the Python
/// `done(status, body, error)` callback).
pub trait HttpClient {
    /// Perform an HTTP request and return the status plus the raw body.
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<HttpResponse, ApiError>;
}

/// One `<d:response>` of a PROPFIND multistatus, pre-normalized.
struct PropfindEntry {
    /// `urlsplit(href).path` with the trailing slash stripped.
    href_path: String,
    /// Whether the resource is a collection (`<d:collection/>`).
    is_collection: bool,
}

/// Nextcloud HTTP API client (remote folder picker + setup wizard).
pub struct NextcloudApi {
    http: Box<dyn HttpClient>,
}

impl NextcloudApi {
    /// Create a client with the production HTTP backend (ureq + rustls).
    pub fn new() -> Self {
        Self::with_http(Box::new(UreqHttpClient::new()))
    }

    /// Create a client with a custom transport (tests inject a fake here).
    pub fn with_http(http: Box<dyn HttpClient>) -> Self {
        Self { http }
    }

    /// Validate credentials against the OCS user endpoint.
    ///
    /// Returns the account's `display-name`, or `Ok(None)` when the payload
    /// does not carry one. HTTP 401/403 map to [`ApiError::AuthRejected`].
    pub fn validate_credentials(
        &self,
        server: &str,
        username: &str,
        password: &str,
    ) -> Result<Option<String>, ApiError> {
        let url = format!(
            "{}/ocs/v2.php/cloud/user?format=json",
            server.trim_end_matches('/')
        );
        let authorization = basic_authorization(username, password);
        let headers = [
            ("Accept", "application/json"),
            ("OCS-APIREQUEST", "true"),
            ("Authorization", authorization.as_str()),
        ];
        let response = self.http.request("GET", &url, &headers, None)?;
        map_status(response.status)?;
        let payload: serde_json::Value =
            serde_json::from_slice(&response.body).map_err(|_| ApiError::InvalidResponse)?;
        let display_name = payload
            .get("ocs")
            .and_then(|ocs| ocs.get("data"))
            .and_then(|data| data.get("display-name"))
            .and_then(|name| name.as_str())
            .map(str::to_owned);
        Ok(display_name)
    }

    /// Account summary from the OCS user endpoint: display name plus quota.
    ///
    /// `used`/`total` are bytes; servers with no quota report negative
    /// values (e.g. `-3`), which map to `None` (render "unlimited").
    pub fn account_summary(
        &self,
        server: &str,
        username: &str,
        password: &str,
    ) -> Result<AccountSummary, ApiError> {
        let url = format!(
            "{}/ocs/v2.php/cloud/user?format=json",
            server.trim_end_matches('/')
        );
        let authorization = basic_authorization(username, password);
        let headers = [
            ("Accept", "application/json"),
            ("OCS-APIREQUEST", "true"),
            ("Authorization", authorization.as_str()),
        ];
        let response = self.http.request("GET", &url, &headers, None)?;
        map_status(response.status)?;
        let payload: serde_json::Value =
            serde_json::from_slice(&response.body).map_err(|_| ApiError::InvalidResponse)?;
        let data = payload
            .get("ocs")
            .and_then(|ocs| ocs.get("data"))
            .cloned()
            .unwrap_or_default();
        let display_name = data
            .get("display-name")
            .and_then(|name| name.as_str())
            .map(str::to_owned);
        let used = data
            .get("quota")
            .and_then(|quota| quota.get("used"))
            .and_then(|used| used.as_f64())
            .filter(|used| *used >= 0.0)
            .map(|used| used as u64);
        let total = data
            .get("quota")
            .and_then(|quota| quota.get("total"))
            .and_then(|total| total.as_f64())
            .filter(|total| *total > 0.0)
            .map(|total| total as u64);
        Ok(AccountSummary {
            display_name,
            used,
            total,
        })
    }

    /// Create the remote folder (and any missing parents) over WebDAV MKCOL.
    ///
    /// `nextcloudcmd` fails silently (exit 1, no output) when `--path` points
    /// at a folder that does not exist on the server, so the app must create
    /// the target itself before the first sync. Idempotent per segment:
    /// 201 (created) and 405 (already exists) both succeed; 401/403 map to
    /// [`ApiError::AuthRejected`]; an empty `remote_path` (the account root)
    /// is a no-op.
    pub fn ensure_remote_folder(
        &self,
        server: &str,
        username: &str,
        password: &str,
        remote_path: &str,
    ) -> Result<(), ApiError> {
        let path = remote_path.trim_matches('/');
        if path.is_empty() {
            return Ok(());
        }
        let base = dav_base(server, username);
        let authorization = basic_authorization(username, password);
        let mut accumulated = String::new();
        for segment in path.split('/') {
            if segment.is_empty() {
                continue;
            }
            if !accumulated.is_empty() {
                accumulated.push('/');
            }
            accumulated.push_str(segment);
            let url = format!("{base}/{accumulated}");
            let response = self.http.request(
                "MKCOL",
                &url,
                &[("Authorization", authorization.as_str())],
                None,
            )?;
            match response.status {
                201 | 405 => {}
                401 | 403 => return Err(ApiError::AuthRejected),
                status => return Err(ApiError::Http { status }),
            }
        }
        Ok(())
    }

    /// Probe whether a remote folder exists and holds at least one entry,
    /// using a shallow PROPFIND (Depth 1, no file bodies).
    ///
    /// `remote_path` uses the normalized config form (`""` for the account
    /// root, otherwise `/Documents`). A `true` result means the folder exists
    /// and has at least one child.
    pub fn probe_remote(
        &self,
        server: &str,
        username: &str,
        password: &str,
        remote_path: &str,
    ) -> Result<bool, ApiError> {
        let base = dav_base(server, username);
        let folder = format!("{base}{}/", remote_path.trim_end_matches('/'));
        let folder_path = href_path_of(&folder).to_owned();
        let entries = self.propfind(&folder, username, password)?;
        let children = entries
            .iter()
            .filter(|entry| entry.href_path != folder_path)
            .count();
        Ok(children > 0)
    }

    /// List the top-level folders that already exist for the account.
    ///
    /// Files and special collections (hidden, trash, versions) are excluded; a
    /// root without subfolders yields an empty list. Paths are normalized
    /// (`/Documents`, `/Photos`) and sorted.
    pub fn list_remote_folders(
        &self,
        server: &str,
        username: &str,
        password: &str,
    ) -> Result<Vec<String>, ApiError> {
        let base = dav_base(server, username);
        let url = format!("{base}/");
        let root_path = href_path_of(&url).to_owned();
        let entries = self.propfind(&url, username, password)?;
        let mut folders = Vec::new();
        for entry in entries {
            if entry.href_path == root_path {
                continue;
            }
            if !entry.is_collection {
                continue;
            }
            let segments: Vec<&str> = entry.href_path.split('/').collect();
            if segments.iter().any(|segment| {
                segment.starts_with('.')
                    || segment.contains("trashbin")
                    || segment.contains("trash")
                    || segment.contains("versions")
            }) {
                continue;
            }
            let name = entry.href_path.rsplit('/').next().unwrap_or_default();
            folders.push(format!("/{name}"));
        }
        folders.sort();
        Ok(folders)
    }

    /// Revoke the app password used for this session.
    ///
    /// Port of `revoke_app_password` (`api.py`): a `DELETE` against
    /// `/ocs/v2.php/core/apppassword` with the account credentials. The server
    /// invalidates the token currently in use, so a removed account can no
    /// longer authenticate. 401/403 map to [`ApiError::AuthRejected`].
    pub fn revoke_app_password(
        &self,
        server: &str,
        username: &str,
        password: &str,
    ) -> Result<(), ApiError> {
        let url = format!(
            "{}/ocs/v2.php/core/apppassword",
            server.trim_end_matches('/')
        );
        let authorization = basic_authorization(username, password);
        let headers = [
            ("Accept", "application/json"),
            ("OCS-APIREQUEST", "true"),
            ("Authorization", authorization.as_str()),
        ];
        let response = self.http.request("DELETE", &url, &headers, None)?;
        map_status(response.status)?;
        Ok(())
    }

    /// List the account's server notifications (issue #31).
    ///
    /// `GET /ocs/v2.php/apps/notifications/api/v1/notifications?format=json`
    /// with the OCS headers. A 204 (no app uses notifications) is an empty
    /// list, not an error; `subject`/`message` are the parsed strings.
    pub fn notifications(
        &self,
        server: &str,
        username: &str,
        password: &str,
    ) -> Result<Vec<ServerNotification>, ApiError> {
        let url = format!(
            "{}/ocs/v2.php/apps/notifications/api/v1/notifications?format=json",
            server.trim_end_matches('/')
        );
        let authorization = basic_authorization(username, password);
        let headers = [
            ("Accept", "application/json"),
            ("OCS-APIREQUEST", "true"),
            ("Authorization", authorization.as_str()),
        ];
        let response = self.http.request("GET", &url, &headers, None)?;
        if response.status == 204 {
            return Ok(Vec::new());
        }
        map_status(response.status)?;
        let payload: serde_json::Value =
            serde_json::from_slice(&response.body).map_err(|_| ApiError::InvalidResponse)?;
        let items = payload
            .get("ocs")
            .and_then(|ocs| ocs.get("data"))
            .and_then(|data| data.as_array())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut notifications = Vec::with_capacity(items.len());
        for item in items {
            let message = item
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .filter(|message| !message.is_empty());
            notifications.push(ServerNotification {
                notification_id: item
                    .get("notification_id")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
                app: item
                    .get("app")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                subject: item
                    .get("subject")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                message,
            });
        }
        Ok(notifications)
    }

    /// Run a Depth-1 PROPFIND and parse the multistatus response.
    fn propfind(
        &self,
        url: &str,
        username: &str,
        password: &str,
    ) -> Result<Vec<PropfindEntry>, ApiError> {
        let authorization = basic_authorization(username, password);
        let headers = [
            ("Depth", "1"),
            ("Content-Type", "application/xml; charset=utf-8"),
            ("Authorization", authorization.as_str()),
        ];
        let response = self
            .http
            .request("PROPFIND", url, &headers, Some(PROPFIND_BODY))?;
        map_status(response.status)?;
        parse_multistatus(&response.body)
    }
}

impl Default for NextcloudApi {
    fn default() -> Self {
        Self::new()
    }
}

/// Base URL of the per-user WebDAV root.
fn dav_base(server: &str, username: &str) -> String {
    format!(
        "{}/remote.php/dav/files/{username}",
        server.trim_end_matches('/')
    )
}

/// `Basic base64(user:pass)` header value (same as the Python
/// `basic_authorization`).
fn basic_authorization(username: &str, password: &str) -> String {
    let encoded = BASE64.encode(format!("{username}:{password}").as_bytes());
    format!("Basic {encoded}")
}

/// Map a status code to an error, replicating the Python `done` mapping.
fn map_status(status: u16) -> Result<(), ApiError> {
    if status == 401 || status == 403 {
        return Err(ApiError::AuthRejected);
    }
    if !(200..300).contains(&status) {
        return Err(ApiError::Http { status });
    }
    Ok(())
}

/// `urlsplit(value).path` with the trailing slash stripped (Python semantics).
///
/// Drops the scheme and authority when the href is an absolute URL, keeping
/// only the path component (e.g. `/remote.php/dav/files/alice/Documents`).
fn href_path_of(value: &str) -> &str {
    let without_scheme = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value);
    let path = match without_scheme.find('/') {
        Some(index) => &without_scheme[index..],
        None => "",
    };
    path.trim_end_matches('/')
}

/// Parse a PROPFIND multistatus body into normalized entries.
fn parse_multistatus(body: &[u8]) -> Result<Vec<PropfindEntry>, ApiError> {
    let text = std::str::from_utf8(body).map_err(|_| ApiError::InvalidResponse)?;
    let doc = Document::parse(text).map_err(|_| ApiError::InvalidResponse)?;
    let mut entries = Vec::new();
    for response in doc
        .descendants()
        .filter(|node| node.has_tag_name((DAV_NS, "response")))
    {
        let href = response
            .descendants()
            .find(|node| node.has_tag_name((DAV_NS, "href")))
            .and_then(|node| node.text())
            .unwrap_or("");
        let href_path = href_path_of(href).to_owned();
        let is_collection = find_resource_type(response)
            .map(|resource_type| {
                resource_type
                    .descendants()
                    .any(|node| node.has_tag_name((DAV_NS, "collection")))
            })
            .unwrap_or(false);
        entries.push(PropfindEntry {
            href_path,
            is_collection,
        });
    }
    Ok(entries)
}

/// Find the `<d:propstat>/<d:prop>/<d:resourcetype>` node of a response.
fn find_resource_type<'a, 'input>(response: Node<'a, 'input>) -> Option<Node<'a, 'input>> {
    response
        .descendants()
        .find(|node| node.has_tag_name((DAV_NS, "propstat")))
        .and_then(|propstat| {
            propstat
                .descendants()
                .find(|node| node.has_tag_name((DAV_NS, "prop")))
        })
        .and_then(|prop| {
            prop.descendants()
                .find(|node| node.has_tag_name((DAV_NS, "resourcetype")))
        })
}

/// Production HTTP transport backed by ureq 3.2 + rustls with system roots.
pub struct UreqHttpClient {
    agent: ureq::Agent,
}

impl Default for UreqHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl UreqHttpClient {
    /// Build the ureq agent:
    /// - status codes delivered as responses, not errors (the API maps them),
    /// - non-standard methods allowed (WebDAV PROPFIND),
    /// - 30 s global timeout (mirrors the Python),
    /// - system trust store via `rustls-native-certs` (same as the push client),
    /// - explicit `aws-lc-rs` CryptoProvider: ureq is built with
    ///   `rustls-no-provider` so its default `ring` provider never clashes with
    ///   the crate's `aws-lc-rs` (the process must expose exactly one provider).
    pub fn new() -> Self {
        let native = rustls_native_certs::load_native_certs();
        let certs: Vec<ureq::tls::Certificate<'static>> = native
            .certs
            .iter()
            .map(|cert| ureq::tls::Certificate::from_der(cert.as_ref()).to_owned())
            .collect();
        let crypto = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .unversioned_rustls_crypto_provider(crypto)
            .root_certs(ureq::tls::RootCerts::new_with_certs(&certs))
            .build();
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .tls_config(tls)
            .build()
            .new_agent();
        Self { agent }
    }
}

impl HttpClient for UreqHttpClient {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<HttpResponse, ApiError> {
        let method =
            ureq::http::Method::from_bytes(method.as_bytes()).map_err(|_| ApiError::Transport)?;
        let mut builder = ureq::http::Request::builder().method(method).uri(url);
        for (key, value) in headers {
            builder = builder.header(*key, *value);
        }
        let mut response = match body {
            Some(bytes) => {
                let request = builder
                    .body(bytes.to_vec())
                    .map_err(|_| ApiError::Transport)?;
                self.agent.run(request)
            }
            None => {
                let request = builder.body(()).map_err(|_| ApiError::Transport)?;
                self.agent.run(request)
            }
        }
        .map_err(|_| ApiError::Transport)?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_vec()
            .map_err(|_| ApiError::Transport)?;
        Ok(HttpResponse { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    const EMPTY_PROPFIND: &[u8] = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
  <d:response>
    <d:href>/remote.php/dav/files/alice/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    const POPULATED_PROPFIND: &[u8] = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
  <d:response>
    <d:href>/remote.php/dav/files/alice/</d:href>
    <d:propstat><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Documents/</d:href>
    <d:propstat><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/report.pdf</d:href>
    <d:propstat><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;

    const COLLECTIONS_PROPFIND: &[u8] = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
  <d:response>
    <d:href>/remote.php/dav/files/alice/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Documents/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Photos/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/report.pdf</d:href>
    <d:propstat>
      <d:prop><d:resourcetype></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    const SPECIAL_COLLECTIONS_PROPFIND: &[u8] = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
  <d:response>
    <d:href>/remote.php/dav/files/alice/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/.hidden/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/files_trashbin/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Documents/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    const USER_JSON: &[u8] = br#"{"ocs":{"data":{"id":"alice","display-name":"Alice Example"}}}"#;

    /// Deterministic fake transport, mirroring the Python `_FakeHttp`.
    #[derive(Default)]
    struct FakeHttp {
        status: u16,
        body: Vec<u8>,
        requests: Rc<RefCell<Vec<RecordedRequest>>>,
    }

    impl FakeHttp {
        fn new(status: u16, body: &[u8]) -> Self {
            Self {
                status,
                body: body.to_vec(),
                requests: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }

    #[derive(Debug)]
    struct RecordedRequest {
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    }

    impl HttpClient for FakeHttp {
        fn request(
            &self,
            method: &str,
            url: &str,
            headers: &[(&str, &str)],
            body: Option<&[u8]>,
        ) -> Result<HttpResponse, ApiError> {
            self.requests.borrow_mut().push(RecordedRequest {
                method: method.to_owned(),
                url: url.to_owned(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body: body.map(<[u8]>::to_vec),
            });
            Ok(HttpResponse {
                status: self.status,
                body: self.body.clone(),
            })
        }
    }

    fn header_value<'a>(request: &'a RecordedRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    // ---- probe_remote -------------------------------------------------------

    /// Fake returning a scripted status per request (MKCOL sequences).
    struct ScriptedHttp {
        statuses: Rc<RefCell<std::collections::VecDeque<u16>>>,
        requests: Rc<RefCell<Vec<RecordedRequest>>>,
    }

    impl ScriptedHttp {
        fn new(statuses: &[u16]) -> Self {
            Self {
                statuses: Rc::new(RefCell::new(statuses.iter().copied().collect())),
                requests: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }

    impl HttpClient for ScriptedHttp {
        fn request(
            &self,
            method: &str,
            url: &str,
            headers: &[(&str, &str)],
            body: Option<&[u8]>,
        ) -> Result<HttpResponse, ApiError> {
            self.requests.borrow_mut().push(RecordedRequest {
                method: method.to_owned(),
                url: url.to_owned(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body: body.map(<[u8]>::to_vec),
            });
            let status = self.statuses.borrow_mut().pop_front().unwrap_or(500);
            Ok(HttpResponse {
                status,
                body: Vec::new(),
            })
        }
    }

    #[test]
    fn ensure_remote_folder_noops_on_account_root() {
        let http = ScriptedHttp::new(&[]);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        api.ensure_remote_folder("https://cloud.example.com", "alice", "pw", "")
            .unwrap();
        assert!(requests.borrow().is_empty());
    }

    #[test]
    fn ensure_remote_folder_creates_each_segment() {
        let http = ScriptedHttp::new(&[201, 201]);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        api.ensure_remote_folder("https://cloud.example.com", "alice", "pw", "/a/b")
            .unwrap();
        let urls: Vec<String> = requests
            .borrow()
            .iter()
            .map(|request| request.url.clone())
            .collect();
        let base = "https://cloud.example.com/remote.php/dav/files/alice";
        assert_eq!(urls, vec![format!("{base}/a"), format!("{base}/a/b")]);
        assert!(requests.borrow().iter().all(|r| r.method == "MKCOL"));
    }

    #[test]
    fn ensure_remote_folder_treats_405_as_existing() {
        let http = ScriptedHttp::new(&[405]);
        let api = NextcloudApi::with_http(Box::new(http));
        api.ensure_remote_folder("https://cloud.example.com", "alice", "pw", "/docs")
            .unwrap();
    }

    #[test]
    fn ensure_remote_folder_maps_401_to_auth_rejected() {
        let http = ScriptedHttp::new(&[401]);
        let api = NextcloudApi::with_http(Box::new(http));
        assert!(matches!(
            api.ensure_remote_folder("https://cloud.example.com", "alice", "pw", "/docs"),
            Err(ApiError::AuthRejected)
        ));
    }

    #[test]
    fn ensure_remote_folder_surfaces_unexpected_status() {
        let http = ScriptedHttp::new(&[500]);
        let api = NextcloudApi::with_http(Box::new(http));
        assert!(matches!(
            api.ensure_remote_folder("https://cloud.example.com", "alice", "pw", "/docs"),
            Err(ApiError::Http { status: 500 })
        ));
    }

    #[test]
    fn probe_empty_folder_returns_false() {
        let http = FakeHttp::new(207, EMPTY_PROPFIND);
        let api = NextcloudApi::with_http(Box::new(http));
        assert!(!api
            .probe_remote("https://cloud.example.com", "alice", "secret", "")
            .unwrap());
    }

    #[test]
    fn probe_populated_folder_returns_true() {
        let http = FakeHttp::new(207, POPULATED_PROPFIND);
        let api = NextcloudApi::with_http(Box::new(http));
        assert!(api
            .probe_remote("https://cloud.example.com", "alice", "secret", "")
            .unwrap());
    }

    #[test]
    fn probe_uses_depth_one_with_auth_header() {
        let http = FakeHttp::new(207, EMPTY_PROPFIND);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        api.probe_remote("https://cloud.example.com", "alice", "secret", "")
            .unwrap();
        let request = &requests.borrow()[0];
        assert_eq!(request.method, "PROPFIND");
        assert_eq!(header_value(request, "Depth"), Some("1"));
        assert!(header_value(request, "Authorization")
            .unwrap()
            .starts_with("Basic "));
        assert!(request.url.ends_with("/remote.php/dav/files/alice/"));
        assert!(request.body.is_some());
    }

    #[test]
    fn probe_appends_remote_path_to_the_folder() {
        let http = FakeHttp::new(207, EMPTY_PROPFIND);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        api.probe_remote("https://cloud.example.com", "alice", "secret", "/Documents")
            .unwrap();
        let request = &requests.borrow()[0];
        assert!(request.url.ends_with("/Documents/"));
    }

    #[test]
    fn probe_http_error_surfaces() {
        let http = FakeHttp::new(500, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.probe_remote("https://cloud.example.com", "alice", "secret", ""),
            Err(ApiError::Http { status: 500 })
        );
    }

    #[test]
    fn probe_auth_rejection_surfaces() {
        let http = FakeHttp::new(401, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.probe_remote("https://cloud.example.com", "alice", "secret", ""),
            Err(ApiError::AuthRejected)
        );
    }

    #[test]
    fn probe_malformed_xml_surfaces() {
        let http = FakeHttp::new(207, b"not xml");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.probe_remote("https://cloud.example.com", "alice", "secret", ""),
            Err(ApiError::InvalidResponse)
        );
    }

    // ---- list_remote_folders ----------------------------------------------

    #[test]
    fn list_returns_existing_top_level_folders_only() {
        let http = FakeHttp::new(207, COLLECTIONS_PROPFIND);
        let api = NextcloudApi::with_http(Box::new(http));
        let folders = api
            .list_remote_folders("https://cloud.example.com", "alice", "secret")
            .unwrap();
        assert_eq!(folders, ["/Documents", "/Photos"]);
    }

    #[test]
    fn list_ignores_hidden_and_trash_collections() {
        let http = FakeHttp::new(207, SPECIAL_COLLECTIONS_PROPFIND);
        let api = NextcloudApi::with_http(Box::new(http));
        let folders = api
            .list_remote_folders("https://cloud.example.com", "alice", "secret")
            .unwrap();
        assert_eq!(folders, ["/Documents"]);
    }

    #[test]
    fn list_empty_root_returns_no_folders() {
        let http = FakeHttp::new(207, EMPTY_PROPFIND);
        let api = NextcloudApi::with_http(Box::new(http));
        let folders = api
            .list_remote_folders("https://cloud.example.com", "alice", "secret")
            .unwrap();
        assert!(folders.is_empty());
    }

    #[test]
    fn list_auth_rejection_surfaces() {
        let http = FakeHttp::new(403, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.list_remote_folders("https://cloud.example.com", "alice", "secret"),
            Err(ApiError::AuthRejected)
        );
    }

    #[test]
    fn list_malformed_xml_surfaces() {
        let http = FakeHttp::new(207, b"not xml");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.list_remote_folders("https://cloud.example.com", "alice", "secret"),
            Err(ApiError::InvalidResponse)
        );
    }

    #[test]
    fn list_probes_the_account_root_with_depth_one() {
        let http = FakeHttp::new(207, COLLECTIONS_PROPFIND);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        api.list_remote_folders("https://cloud.example.com", "alice", "secret")
            .unwrap();
        let request = &requests.borrow()[0];
        assert_eq!(request.method, "PROPFIND");
        assert_eq!(header_value(request, "Depth"), Some("1"));
        assert!(request.url.ends_with("/remote.php/dav/files/alice/"));
    }

    // ---- validate_credentials ----------------------------------------------

    #[test]
    fn validate_returns_display_name() {
        let http = FakeHttp::new(200, USER_JSON);
        let api = NextcloudApi::with_http(Box::new(http));
        let display = api
            .validate_credentials("https://cloud.example.com", "alice", "secret")
            .unwrap();
        assert_eq!(display.as_deref(), Some("Alice Example"));
    }

    #[test]
    fn validate_missing_display_name_returns_none() {
        let http = FakeHttp::new(200, br#"{"ocs":{"data":{"id":"alice"}}}"#);
        let api = NextcloudApi::with_http(Box::new(http));
        let display = api
            .validate_credentials("https://cloud.example.com", "alice", "secret")
            .unwrap();
        assert_eq!(display, None);
    }

    #[test]
    fn validate_sends_ocs_headers() {
        let http = FakeHttp::new(200, USER_JSON);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        api.validate_credentials("https://cloud.example.com", "alice", "secret")
            .unwrap();
        let request = &requests.borrow()[0];
        assert_eq!(request.method, "GET");
        assert_eq!(header_value(request, "Accept"), Some("application/json"));
        assert_eq!(header_value(request, "OCS-APIREQUEST"), Some("true"));
        assert!(header_value(request, "Authorization")
            .unwrap()
            .starts_with("Basic "));
        assert!(request.url.ends_with("/ocs/v2.php/cloud/user?format=json"));
    }

    #[test]
    fn validate_auth_rejection_surfaces() {
        for status in [401, 403] {
            let http = FakeHttp::new(status, b"");
            let api = NextcloudApi::with_http(Box::new(http));
            assert_eq!(
                api.validate_credentials("https://cloud.example.com", "alice", "secret"),
                Err(ApiError::AuthRejected)
            );
        }
    }

    #[test]
    fn validate_http_error_surfaces() {
        let http = FakeHttp::new(503, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.validate_credentials("https://cloud.example.com", "alice", "secret"),
            Err(ApiError::Http { status: 503 })
        );
    }

    #[test]
    fn validate_invalid_json_surfaces() {
        let http = FakeHttp::new(200, b"not json");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.validate_credentials("https://cloud.example.com", "alice", "secret"),
            Err(ApiError::InvalidResponse)
        );
    }

    #[test]
    fn transport_error_surfaces() {
        struct Failing;
        impl HttpClient for Failing {
            fn request(
                &self,
                _method: &str,
                _url: &str,
                _headers: &[(&str, &str)],
                _body: Option<&[u8]>,
            ) -> Result<HttpResponse, ApiError> {
                Err(ApiError::Transport)
            }
        }
        let api = NextcloudApi::with_http(Box::new(Failing));
        assert_eq!(
            api.validate_credentials("https://cloud.example.com", "alice", "secret"),
            Err(ApiError::Transport)
        );
    }

    // ---- revoke_app_password ----------------------------------------------

    #[test]
    fn revoke_issues_a_delete_against_core_apppassword() {
        let http = FakeHttp::new(200, b"");
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        api.revoke_app_password("https://cloud.example.com", "alice", "secret")
            .unwrap();
        let request = &requests.borrow()[0];
        assert_eq!(request.method, "DELETE");
        assert_eq!(header_value(request, "OCS-APIREQUEST"), Some("true"));
        assert!(header_value(request, "Authorization")
            .unwrap()
            .starts_with("Basic "));
        assert!(request.url.ends_with("/ocs/v2.php/core/apppassword"));
    }

    #[test]
    fn revoke_success_is_ok() {
        for status in [200, 204] {
            let http = FakeHttp::new(status, b"");
            let api = NextcloudApi::with_http(Box::new(http));
            api.revoke_app_password("https://cloud.example.com", "alice", "secret")
                .unwrap();
        }
    }

    #[test]
    fn revoke_auth_rejection_surfaces() {
        for status in [401, 403] {
            let http = FakeHttp::new(status, b"");
            let api = NextcloudApi::with_http(Box::new(http));
            assert_eq!(
                api.revoke_app_password("https://cloud.example.com", "alice", "secret"),
                Err(ApiError::AuthRejected)
            );
        }
    }

    #[test]
    fn revoke_http_error_surfaces() {
        let http = FakeHttp::new(500, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.revoke_app_password("https://cloud.example.com", "alice", "secret"),
            Err(ApiError::Http { status: 500 })
        );
    }

    #[test]
    fn revoke_transport_error_surfaces() {
        struct Failing;
        impl HttpClient for Failing {
            fn request(
                &self,
                _method: &str,
                _url: &str,
                _headers: &[(&str, &str)],
                _body: Option<&[u8]>,
            ) -> Result<HttpResponse, ApiError> {
                Err(ApiError::Transport)
            }
        }
        let api = NextcloudApi::with_http(Box::new(Failing));
        assert_eq!(
            api.revoke_app_password("https://cloud.example.com", "alice", "secret"),
            Err(ApiError::Transport)
        );
    }

    // ---- notifications -----------------------------------------------------

    const NOTIFICATIONS_JSON: &[u8] = br#"{"ocs":{"data":[
      {"notification_id":42,"app":"files_sharing","subject":"Alice shared a file with you","message":"Documents/report.pdf"},
      {"notification_id":43,"app":"comments","subject":"Bob commented on your post","message":""},
      {"notification_id":44,"app":"spreed","subject":"Call with Carol"}
    ]}}"#;

    #[test]
    fn notifications_parses_id_app_subject_and_message() {
        let http = FakeHttp::new(200, NOTIFICATIONS_JSON);
        let api = NextcloudApi::with_http(Box::new(http));
        let notifications = api
            .notifications("https://cloud.example.com", "alice", "secret")
            .unwrap();
        assert_eq!(notifications.len(), 3);
        assert_eq!(notifications[0].notification_id, 42);
        assert_eq!(notifications[0].app, "files_sharing");
        assert_eq!(notifications[0].subject, "Alice shared a file with you");
        assert_eq!(
            notifications[0].message.as_deref(),
            Some("Documents/report.pdf")
        );
        assert_eq!(notifications[1].notification_id, 43);
        assert_eq!(notifications[1].message, None, "empty message becomes None");
        assert_eq!(
            notifications[2].message, None,
            "missing message becomes None"
        );
    }

    #[test]
    fn notifications_sends_get_with_ocs_headers() {
        let http = FakeHttp::new(200, NOTIFICATIONS_JSON);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        api.notifications("https://cloud.example.com", "alice", "secret")
            .unwrap();
        let request = &requests.borrow()[0];
        assert_eq!(request.method, "GET");
        assert_eq!(header_value(request, "Accept"), Some("application/json"));
        assert_eq!(header_value(request, "OCS-APIREQUEST"), Some("true"));
        assert!(header_value(request, "Authorization")
            .unwrap()
            .starts_with("Basic "));
        assert!(request
            .url
            .ends_with("/ocs/v2.php/apps/notifications/api/v1/notifications?format=json"));
    }

    #[test]
    fn notifications_204_without_notifiers_is_empty() {
        let http = FakeHttp::new(204, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        let notifications = api
            .notifications("https://cloud.example.com", "alice", "secret")
            .unwrap();
        assert!(notifications.is_empty());
    }

    #[test]
    fn notifications_empty_payload_is_empty() {
        let http = FakeHttp::new(200, br#"{"ocs":{"data":[]}}"#);
        let api = NextcloudApi::with_http(Box::new(http));
        assert!(api
            .notifications("https://cloud.example.com", "alice", "secret")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn notifications_auth_rejection_surfaces() {
        let http = FakeHttp::new(403, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.notifications("https://cloud.example.com", "alice", "secret"),
            Err(ApiError::AuthRejected)
        );
    }

    #[test]
    fn notifications_invalid_json_surfaces() {
        let http = FakeHttp::new(200, b"not json");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.notifications("https://cloud.example.com", "alice", "secret"),
            Err(ApiError::InvalidResponse)
        );
    }

    // ---- integration with a real local server ------------------------------

    /// Runs a one-shot tiny_http server that answers a PROPFIND with a fixture,
    /// exercising the real ureq transport end to end (localhost only, no net).
    #[test]
    fn integration_list_against_local_server() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let base = format!("http://{addr}");
        let handle = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            assert_eq!(request.method().as_str(), "PROPFIND");
            request
                .respond(
                    tiny_http::Response::from_string(
                        String::from_utf8_lossy(COLLECTIONS_PROPFIND).into_owned(),
                    )
                    .with_status_code(207),
                )
                .unwrap();
        });

        let api = NextcloudApi::new();
        let folders = api.list_remote_folders(&base, "alice", "secret").unwrap();
        assert_eq!(folders, ["/Documents", "/Photos"]);
        handle.join().unwrap();
    }

    /// The real transport maps a local 401 to `AuthRejected`.
    #[test]
    fn integration_auth_rejection_against_local_server() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let base = format!("http://{addr}");
        let handle = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            request
                .respond(tiny_http::Response::from_string("denied").with_status_code(401))
                .unwrap();
        });

        let api = NextcloudApi::new();
        let result = api.list_remote_folders(&base, "alice", "secret");
        assert_eq!(result, Err(ApiError::AuthRejected));
        handle.join().unwrap();
    }
    #[test]
    fn account_summary_parses_quota_and_handles_unlimited() {
        let body = br#"{"ocs":{"meta":{"status":"ok"},"data":{"display-name":"Alice","quota":{"used":57077043830,"total":-3,"free":-3}}}}"#;
        let http = FakeHttp::new(200, body);
        let api = NextcloudApi::with_http(Box::new(http));
        let summary = api
            .account_summary("https://cloud.example.com", "alice", "pw")
            .unwrap();
        assert_eq!(summary.display_name.as_deref(), Some("Alice"));
        assert_eq!(summary.used, Some(57077043830));
        assert_eq!(summary.total, None, "negative total means unlimited");
        assert!(summary.usage_label().ends_with("· unlimited"));
        assert!(summary.usage_label().starts_with("53.2 GiB"));
    }

    #[test]
    fn usage_label_joins_used_and_total() {
        let summary = AccountSummary {
            display_name: None,
            used: Some(1024 * 1024 * 1024),
            total: Some(2 * 1024 * 1024 * 1024),
        };
        assert_eq!(summary.usage_label(), "1.0 GiB / 2.0 GiB");
    }

    #[test]
    fn usage_label_empty_without_quota() {
        let summary = AccountSummary::default();
        assert_eq!(summary.usage_label(), "");
    }

    #[test]
    fn format_bytes_uses_binary_prefixes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(1024 * 1024 * 5), "5.0 MiB");
    }
}
