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
//! - [`NextcloudApi::validate_opencloud_credentials`] /
//!   [`NextcloudApi::list_opencloud_spaces`] /
//!   [`NextcloudApi::probe_opencloud_space`]: OpenCloud has no OCS API, so its
//!   helpers talk to the LibreGraph API (`/graph/v1.0/me`, `/graph/v1.0/drives`)
//!   with `Basic user:app-token` for validation and space listing (verified
//!   against a real OpenCloud deployment: the WebDAV spaces root rejects
//!   PROPFIND with 405, so only Graph can list), and to
//!   `/remote.php/dav/spaces/<id>/` for the space probe.
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

/// PROPFIND body requesting `resourcetype` and `getcontentlength` (no file
/// bodies are ever downloaded).
const PROPFIND_BODY: &[u8] = b"<?xml version=\"1.0\"?><d:propfind xmlns:d=\"DAV:\"><d:prop><d:resourcetype/><d:getcontentlength/></d:prop></d:propfind>";

/// PROPFIND body requesting only `<getetag/>`, used for the cheap root ETag
/// poll that gates the periodic remote reconciliation (issue #189). Unlike
/// [`PROPFIND_BODY`] it asks for a single property so the response is small
/// and the comparison is just the folder's own ETag.
const PROPFIND_ETAG_BODY: &[u8] = b"<?xml version=\"1.0\"?><d:propfind xmlns:d=\"DAV:\"><d:prop><d:getetag/></d:prop></d:propfind>";

/// PROPFIND body for the trashbin listing (issue #38): the Nextcloud
/// trash properties plus the resource type.
const TRASH_PROPFIND_BODY: &[u8] = b"<?xml version=\"1.0\"?><d:propfind xmlns:d=\"DAV:\" xmlns:nc=\"http://nextcloud.org/ns\"><d:prop><d:resourcetype/><nc:trashbin-filename/><nc:trashbin-original-location/><nc:trashbin-deletion-time/></d:prop></d:propfind>";

/// Nextcloud namespace of the trashbin properties.
const NC_NS: &str = "http://nextcloud.org/ns";

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

/// One OpenCloud space discovered over WebDAV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCloudSpace {
    /// The space id (`--space` argument of `opencloudcmd`, a UUID).
    pub id: String,
    /// The space display name (`<d:displayname>`), when the server sends it.
    pub display_name: Option<String>,
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
    /// `<d:getcontentlength>` of a file response, when present.
    content_length: Option<u64>,
}

/// One item in the user's server-side trashbin (issue #38).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashItem {
    /// Trashbin name (`nc:trashbin-filename`, e.g. `a.txt.d1678901234`).
    pub filename: String,
    /// Original path relative to the account root
    /// (`nc:trashbin-original-location`), when the server reports it.
    pub original_location: Option<String>,
    /// Deletion time as a unix timestamp in seconds
    /// (`nc:trashbin-deletion-time`).
    pub deletion_time: Option<i64>,
    /// Whether the trashed item is a folder.
    pub is_collection: bool,
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

    /// Server health probe (issue #179): a short GET to the server root.
    ///
    /// Any HTTP response (2xx/3xx/4xx) means the server is up and answering;
    /// a transport failure or a 5xx (a reverse proxy whose backend is down,
    /// a dead upstream) means it is not. Used by the engine to tell "the
    /// folder broke" from "the account is unreachable".
    pub fn server_status(&self, server: &str) -> Result<(), ApiError> {
        let url = format!("{}/", server.trim_end_matches('/'));
        let response = self.http.request("GET", &url, &[], None)?;
        if (500..600).contains(&response.status) {
            return Err(ApiError::Http {
                status: response.status,
            });
        }
        Ok(())
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
        let base = dav_base(server, username);
        self.mkcol_segments(&base, username, password, remote_path)
    }

    /// Create the remote folder inside an OpenCloud space over WebDAV MKCOL
    /// (issue #55).
    ///
    /// OpenCloud folders map a whole space; a non-empty `remote_path` is the
    /// optional `--remote-folder` subpath. `MKCOL
    /// /remote.php/dav/spaces/<id>/<path>/` answers 201 on a real deployment
    /// (verified), so the same segment-by-segment creation the Nextcloud
    /// path uses applies. The space root itself (empty `remote_path`) is
    /// managed by the server and stays a no-op.
    pub fn ensure_opencloud_folder(
        &self,
        server: &str,
        username: &str,
        token: &str,
        space_id: &str,
        remote_path: &str,
    ) -> Result<(), ApiError> {
        let space = space_id.trim_matches('/');
        if space.is_empty() || remote_path.trim_matches('/').is_empty() {
            return Ok(());
        }
        let base = format!(
            "{}/remote.php/dav/spaces/{space}",
            server.trim_end_matches('/')
        );
        self.mkcol_segments(&base, username, token, remote_path)
    }

    /// Shared MKCOL walk: create each missing path segment under `base`.
    /// Idempotent per segment (201 created and 405 exists both succeed);
    /// 401/403 map to [`ApiError::AuthRejected`].
    fn mkcol_segments(
        &self,
        base: &str,
        username: &str,
        password: &str,
        remote_path: &str,
    ) -> Result<(), ApiError> {
        let path = remote_path.trim_matches('/');
        if path.is_empty() {
            return Ok(());
        }
        let authorization = basic_authorization(username, password);
        let mut accumulated = String::new();
        for segment in path.split('/') {
            if segment.is_empty() {
                continue;
            }
            if !accumulated.is_empty() {
                accumulated.push('/');
            }
            accumulated.push_str(&percent_encode_path(segment));
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
        let folder = format!(
            "{base}{}/",
            percent_encode_path(remote_path.trim_end_matches('/'))
        );
        let folder_path = href_path_of(&folder).to_owned();
        let entries = self.propfind(&folder, username, password)?;
        let children = entries
            .iter()
            .filter(|entry| entry.href_path != folder_path)
            .count();
        Ok(children > 0)
    }

    /// Return the WebDAV ETag of a folder root, used to gate a periodic
    /// reconciliation (issue #189).
    ///
    /// A single `PROPFIND` with `Depth: 0` asking for `<getetag/>` returns the
    /// folder's own ETag cheaply (mirrors the official client's `RequestEtagJob`).
    /// The app compares this against the last value it recorded: when it is
    /// unchanged, the folder's remote tree has not changed, so the full
    /// `nextcloudcmd` reconciliation can be skipped. Returned `trimmed`; an
    /// absent/empty `<getetag>` yields `Ok(None)`. Transport/auth errors keep
    /// their [`ApiError`] so the caller can decide (e.g. treat an unreachable
    /// server as "changed" to avoid skipping a real change).
    pub fn root_etag(
        &self,
        server: &str,
        username: &str,
        password: &str,
        remote_path: &str,
    ) -> Result<Option<String>, ApiError> {
        let base = dav_base(server, username);
        let folder = format!(
            "{base}{}/",
            percent_encode_path(remote_path.trim_end_matches('/'))
        );
        let authorization = basic_authorization(username, password);
        let headers = [
            ("Depth", "0"),
            ("Content-Type", "application/xml; charset=utf-8"),
            ("Authorization", authorization.as_str()),
        ];
        let response =
            self.http
                .request("PROPFIND", &folder, &headers, Some(PROPFIND_ETAG_BODY))?;
        map_status(response.status)?;
        Ok(parse_root_etag(&response.body))
    }

    /// Estimate the total size in bytes of a remote folder (issue #36).
    ///
    /// A single `PROPFIND` with `Depth: infinity` asks the server to walk
    /// the whole tree, and every `<d:getcontentlength>` of a file response
    /// is summed. Servers that refuse infinite depth (a common hardening on
    /// large instances) answer 400/403/507; that maps to `Ok(None)` — the
    /// size is simply unknown and the confirmation is skipped. 401 stays
    /// [`ApiError::AuthRejected`].
    pub fn remote_size(
        &self,
        server: &str,
        username: &str,
        password: &str,
        remote_path: &str,
    ) -> Result<Option<u64>, ApiError> {
        let base = dav_base(server, username);
        let folder = format!(
            "{base}{}/",
            percent_encode_path(remote_path.trim_end_matches('/'))
        );
        let authorization = basic_authorization(username, password);
        let headers = [
            ("Depth", "infinity"),
            ("Content-Type", "application/xml; charset=utf-8"),
            ("Authorization", authorization.as_str()),
        ];
        let response = self
            .http
            .request("PROPFIND", &folder, &headers, Some(PROPFIND_BODY))?;
        match response.status {
            207 => {}
            401 => return Err(ApiError::AuthRejected),
            // The server refuses the full-tree walk: size unknown.
            400 | 403 | 507 => return Ok(None),
            status => return Err(ApiError::Http { status }),
        }
        let entries = parse_multistatus(&response.body)?;
        let total: u64 = entries
            .iter()
            .filter(|entry| !entry.is_collection)
            .filter_map(|entry| entry.content_length)
            .sum();
        Ok(Some(total))
    }

    /// Estimate the size in bytes of an OpenCloud space (issue #36).
    ///
    /// OpenCloud folders mirror a whole space, so the drive's `quota.used`
    /// from the LibreGraph API is the closest estimate. Spaces without a
    /// quota report `Ok(None)`.
    pub fn opencloud_space_size(
        &self,
        server: &str,
        username: &str,
        token: &str,
        space_id: &str,
    ) -> Result<Option<u64>, ApiError> {
        let space = space_id.trim().trim_matches('/');
        if space.is_empty() {
            return Ok(None);
        }
        let url = format!("{}/graph/v1.0/drives/{space}", server.trim_end_matches('/'));
        let authorization = basic_authorization(username, token);
        let headers = [("Authorization", authorization.as_str())];
        let response = self.http.request("GET", &url, &headers, None)?;
        map_status(response.status)?;
        let payload: serde_json::Value =
            serde_json::from_slice(&response.body).map_err(|_| ApiError::InvalidResponse)?;
        let used = payload
            .get("quota")
            .and_then(|quota| quota.get("used"))
            .and_then(serde_json::Value::as_f64)
            .filter(|used| *used >= 0.0)
            .map(|used| used as u64);
        Ok(used)
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
            // Filter only the folder name (the last path segment): earlier
            // segments include the username and the fixed dav root, whose
            // substrings must not hide every folder (issue #128).
            let name = entry.href_path.rsplit('/').next().unwrap_or_default();
            if name.starts_with('.')
                || name.contains("trashbin")
                || name.contains("trash")
                || name.contains("versions")
            {
                continue;
            }
            // WebDAV hrefs arrive percent-encoded; decode so pickers show
            // and store the real name (issue #88). The engine re-encodes
            // when building URLs.
            let name = percent_decode_path(name);
            folders.push(format!("/{name}"));
        }
        folders.sort();
        Ok(folders)
    }

    /// Validate OpenCloud credentials (username + app token) against the
    /// LibreGraph API every OpenCloud server exposes.
    ///
    /// `GET /graph/v1.0/me` with `Basic user:app-token` returns the
    /// authenticated user (verified against a real OpenCloud deployment;
    /// the WebDAV spaces root answers 405 to PROPFIND there, so it is not a
    /// usable validation probe). Returns the account's display name, or
    /// `Ok(None)` when the payload does not carry one. 401/403 map to
    /// [`ApiError::AuthRejected`].
    pub fn validate_opencloud_credentials(
        &self,
        server: &str,
        username: &str,
        token: &str,
    ) -> Result<Option<String>, ApiError> {
        let url = format!("{}/graph/v1.0/me", server.trim_end_matches('/'));
        let authorization = basic_authorization(username, token);
        let headers = [
            ("Accept", "application/json"),
            ("Authorization", authorization.as_str()),
        ];
        let response = self.http.request("GET", &url, &headers, None)?;
        map_status(response.status)?;
        let payload: serde_json::Value =
            serde_json::from_slice(&response.body).map_err(|_| ApiError::InvalidResponse)?;
        let display_name = payload
            .get("displayName")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Ok(display_name)
    }

    /// List the spaces the account can synchronize (LibreGraph drives).
    ///
    /// `GET /graph/v1.0/drives` returns every drive visible to the account.
    /// Only the user's own personal space and project spaces are sync
    /// targets: other users' personal spaces (visible to admins) and the
    /// virtual `shares` aggregate are excluded, so the wizard also fetches
    /// `/graph/v1.0/me` to compare owners. Verified against a real OpenCloud
    /// deployment.
    pub fn list_opencloud_spaces(
        &self,
        server: &str,
        username: &str,
        token: &str,
    ) -> Result<Vec<OpenCloudSpace>, ApiError> {
        let base = server.trim_end_matches('/');
        let authorization = basic_authorization(username, token);
        let headers = [("Authorization", authorization.as_str())];

        let me_url = format!("{base}/graph/v1.0/me");
        let me = self
            .http
            .request("GET", &me_url, &headers, None)
            .and_then(|response| {
                map_status(response.status)?;
                serde_json::from_slice::<serde_json::Value>(&response.body)
                    .map_err(|_| ApiError::InvalidResponse)
            })?;
        let user_id = me.get("id").and_then(serde_json::Value::as_str);

        let drives_url = format!("{base}/graph/v1.0/drives");
        let drives = self
            .http
            .request("GET", &drives_url, &headers, None)
            .and_then(|response| {
                map_status(response.status)?;
                serde_json::from_slice::<serde_json::Value>(&response.body)
                    .map_err(|_| ApiError::InvalidResponse)
            })?;
        let items = drives
            .get("value")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();

        let mut spaces = Vec::new();
        for item in items {
            let drive_type = item.get("driveType").and_then(serde_json::Value::as_str);
            let owner = item
                .get("owner")
                .and_then(|owner| owner.get("user"))
                .and_then(|user| user.get("id"))
                .and_then(serde_json::Value::as_str);
            let own_personal =
                drive_type == Some("personal") && Some(owner.unwrap_or("")) == user_id;
            let project = drive_type == Some("project");
            if !own_personal && !project {
                continue;
            }
            let id = item
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if id.is_empty() {
                continue;
            }
            let display_name = item
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .filter(|name| !name.is_empty());
            spaces.push(OpenCloudSpace { id, display_name });
        }
        spaces.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        Ok(spaces)
    }

    /// Probe whether an OpenCloud space holds at least one entry.
    ///
    /// A Depth-1 PROPFIND on `/remote.php/dav/spaces/<id>/` (verified
    /// against a real OpenCloud deployment; only the root of the spaces
    /// tree rejects PROPFIND). Mirrors [`NextcloudApi::probe_remote`] for
    /// OpenCloud folders, which map a whole space (`remote_path` is the
    /// optional `--remote-folder` subpath handled by the engine).
    pub fn probe_opencloud_space(
        &self,
        server: &str,
        username: &str,
        token: &str,
        space_id: &str,
    ) -> Result<bool, ApiError> {
        let space = space_id.trim_matches('/');
        if space.is_empty() {
            return Ok(false);
        }
        let url = format!(
            "{}/remote.php/dav/spaces/{space}/",
            server.trim_end_matches('/')
        );
        let space_path = href_path_of(&url).to_owned();
        let entries = self.spaces_propfind(&url, username, token)?;
        let children = entries
            .iter()
            .filter(|entry| entry.href_path != space_path)
            .count();
        Ok(children > 0)
    }

    /// Shared Depth-1 PROPFIND used by the OpenCloud space probe.
    fn spaces_propfind(
        &self,
        url: &str,
        username: &str,
        token: &str,
    ) -> Result<Vec<PropfindEntry>, ApiError> {
        let authorization = basic_authorization(username, token);
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

    /// List the user's server-side trashbin (issue #38).
    ///
    /// `PROPFIND /remote.php/dav/trashbin/{user}/trash` with the Nextcloud
    /// trash properties (verified against the WebDAV trashbin developer
    /// documentation). OpenCloud has no documented trashbin endpoint, so
    /// this is only offered for Nextcloud accounts.
    pub fn list_trash(
        &self,
        server: &str,
        username: &str,
        password: &str,
    ) -> Result<Vec<TrashItem>, ApiError> {
        let url = format!(
            "{}/remote.php/dav/trashbin/{}/trash",
            server.trim_end_matches('/'),
            percent_encode_path(username)
        );
        let authorization = basic_authorization(username, password);
        let headers = [
            ("Depth", "1"),
            ("Content-Type", "application/xml; charset=utf-8"),
            ("Authorization", authorization.as_str()),
        ];
        let response = self
            .http
            .request("PROPFIND", &url, &headers, Some(TRASH_PROPFIND_BODY))?;
        map_status(response.status)?;
        let trash_path = href_path_of(&url).to_owned();
        let text = std::str::from_utf8(&response.body).map_err(|_| ApiError::InvalidResponse)?;
        let doc = Document::parse(text).map_err(|_| ApiError::InvalidResponse)?;
        let mut items = Vec::new();
        for response in doc
            .descendants()
            .filter(|node| node.has_tag_name((DAV_NS, "response")))
        {
            let href = response
                .descendants()
                .find(|node| node.has_tag_name((DAV_NS, "href")))
                .and_then(|node| node.text())
                .unwrap_or("");
            let href_path = href_path_of(href);
            if href_path == trash_path {
                continue;
            }
            let prop = find_prop(response);
            let is_collection = prop
                .and_then(|prop| {
                    prop.descendants()
                        .find(|node| node.has_tag_name((DAV_NS, "resourcetype")))
                })
                .map(|resource_type| {
                    resource_type
                        .descendants()
                        .any(|node| node.has_tag_name((DAV_NS, "collection")))
                })
                .unwrap_or(false);
            let property = |name: &str| {
                prop.and_then(|prop| {
                    prop.descendants()
                        .find(|node| node.has_tag_name((NC_NS, name)))
                })
                .and_then(|node| node.text())
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
            };
            let filename = property("trashbin-filename")
                .unwrap_or_else(|| href_path.rsplit('/').next().unwrap_or_default().to_owned());
            let deletion_time =
                property("trashbin-deletion-time").and_then(|text| text.parse::<i64>().ok());
            items.push(TrashItem {
                filename,
                original_location: property("trashbin-original-location"),
                deletion_time,
                is_collection,
            });
        }
        items.sort_by_key(|item| std::cmp::Reverse(item.deletion_time));
        Ok(items)
    }

    /// Restore one trashbin item to its original location (issue #38).
    ///
    /// A `MOVE` of `trashbin/{user}/trash/{filename}` into the special
    /// `trashbin/{user}/restore` folder restores it where it came from
    /// (verified against the WebDAV trashbin developer documentation).
    pub fn restore_trash_item(
        &self,
        server: &str,
        username: &str,
        password: &str,
        filename: &str,
    ) -> Result<(), ApiError> {
        let base = server.trim_end_matches('/');
        let encoded = percent_encode_path(filename);
        let url = format!("{base}/remote.php/dav/trashbin/{username}/trash/{encoded}");
        let destination = format!("{base}/remote.php/dav/trashbin/{username}/restore");
        let authorization = basic_authorization(username, password);
        let headers = [
            ("Destination", destination.as_str()),
            ("Authorization", authorization.as_str()),
        ];
        let response = self.http.request("MOVE", &url, &headers, None)?;
        match response.status {
            201 | 204 => Ok(()),
            401 | 403 => Err(ApiError::AuthRejected),
            status => Err(ApiError::Http { status }),
        }
    }

    /// Fetch the account's avatar image bytes (issue #50).
    ///
    /// Nextcloud serves `GET /avatar/{login}/64` (200 = image bytes; 404 or
    /// a redirect mean the account has no avatar) and OpenCloud exposes the
    /// LibreGraph user photo at `GET /graph/v1.0/me/photo/$value` (200 =
    /// image bytes, 404 = none). 401/403 map to
    /// [`ApiError::AuthRejected`]; any other status is an error. Note that
    /// the production agent follows redirects by default, so a redirecting
    /// avatar URL is judged by its final response.
    pub fn fetch_avatar(
        &self,
        provider: crate::nextcloud::driver::Provider,
        server: &str,
        username: &str,
        password: &str,
    ) -> Result<Option<Vec<u8>>, ApiError> {
        let base = server.trim_end_matches('/');
        let url = match provider {
            crate::nextcloud::driver::Provider::Nextcloud => {
                format!("{base}/avatar/{username}/64")
            }
            crate::nextcloud::driver::Provider::OpenCloud => {
                format!("{base}/graph/v1.0/me/photo/$value")
            }
        };
        let authorization = basic_authorization(username, password);
        let headers = [("Authorization", authorization.as_str())];
        let response = self.http.request("GET", &url, &headers, None)?;
        // Nextcloud answers 404 with the generated placeholder avatar in the
        // body when the user never uploaded one, so trust image bytes over
        // the status code (verified against a real server).
        if (response.status == 200 || response.status == 404)
            && Self::has_image_magic(&response.body)
        {
            return Ok(Some(response.body));
        }
        match response.status {
            200 | 404 | 301..=308 => Ok(None),
            401 | 403 => Err(ApiError::AuthRejected),
            status => Err(ApiError::Http { status }),
        }
    }

    /// Whether the payload starts with a known image signature (PNG, JPEG,
    /// GIF or WebP). Avatar endpoints answer errors with JSON/text bodies,
    /// which must not be painted as images.
    fn has_image_magic(body: &[u8]) -> bool {
        body.starts_with(&[0x89, b'P', b'N', b'G'])
            || body.starts_with(&[0xFF, 0xD8, 0xFF])
            || body.starts_with(b"GIF8")
            || (body.len() >= 12 && body.starts_with(b"RIFF") && &body[8..12] == b"WEBP")
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

/// Percent-encode a path segment (unreserved characters stay literal).
fn percent_encode_path(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Percent-decode a path segment: `%XX` escapes (and bare `+`, never used
/// for spaces in paths) back to their bytes. WebDAV hrefs arrive encoded,
/// so folder names must be decoded before display or storage (issue #88);
/// the engine re-encodes when it builds URLs, keeping a single encoding
/// end-to-end.
fn percent_decode_path(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            // Two hex digits must follow the percent sign; anything else is
            // kept literally.
            if let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                decoded.push(byte);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Base URL of the per-user WebDAV root.
fn dav_base(server: &str, username: &str) -> String {
    format!(
        "{}/remote.php/dav/files/{}",
        server.trim_end_matches('/'),
        percent_encode_path(username)
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

/// Parse the `<getetag>` of a root PROPFIND (issue #189).
///
/// Takes the first `<d:getetag>` in the response and returns its trimmed text.
/// An absent or empty value yields `None` (the folder reports no ETag).
fn parse_root_etag(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let doc = Document::parse(text).ok()?;
    doc.descendants()
        .find(|node| node.has_tag_name((DAV_NS, "getetag")))
        .and_then(|node| node.text())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
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
        let prop = find_prop(response);
        let is_collection = prop
            .and_then(|prop| {
                prop.descendants()
                    .find(|node| node.has_tag_name((DAV_NS, "resourcetype")))
            })
            .map(|resource_type| {
                resource_type
                    .descendants()
                    .any(|node| node.has_tag_name((DAV_NS, "collection")))
            })
            .unwrap_or(false);
        let content_length = prop
            .and_then(|prop| {
                prop.descendants()
                    .find(|node| node.has_tag_name((DAV_NS, "getcontentlength")))
            })
            .and_then(|node| node.text())
            .and_then(|text| text.trim().parse::<u64>().ok());
        entries.push(PropfindEntry {
            href_path,
            is_collection,
            content_length,
        });
    }
    Ok(entries)
}

/// Find the `<d:propstat>/<d:prop>` node of a response.
fn find_prop<'a, 'input>(response: Node<'a, 'input>) -> Option<Node<'a, 'input>> {
    response
        .descendants()
        .find(|node| node.has_tag_name((DAV_NS, "propstat")))
        .and_then(|propstat| {
            propstat
                .descendants()
                .find(|node| node.has_tag_name((DAV_NS, "prop")))
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

    const ROOT_ETAG_PROPFIND: &[u8] = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
  <d:response>
    <d:href>/remote.php/dav/files/alice/</d:href>
    <d:propstat>
      <d:prop><d:getetag>&quot;cafebabe-0123456789&quot;</d:getetag></d:prop>
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
    <d:href>/remote.php/dav/files/alice/M%C3%BAsica%20Albums/</d:href>
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

    // LibreGraph payloads mirroring a real OpenCloud deployment (generic
    // values): /me plus a /drives list with the user's own personal space,
    // another user's personal space, the virtual shares aggregate and a
    // project space.
    const OPENCLOUD_ME_JSON: &[u8] = br#"{"id":"9544a8dc-b70b-41a1-a8bc-676d972d898d","displayName":"nacho","mail":"nacho@example.com","userType":"Member"}"#;

    const OPENCLOUD_DRIVES_JSON: &[u8] = br#"{"value":[
 {"driveType":"personal","id":"7d443b01-21d3-484d-bf73-a2681d670fa1$9bc084a7-9bb4-47d5-84da-dd7857ab189c","name":"nacho","owner":{"user":{"id":"9544a8dc-b70b-41a1-a8bc-676d972d898d"}}},
 {"driveType":"personal","id":"7d443b01-21d3-484d-bf73-a2681d670fa1$72f47f61-c9f1-4114-b059-8b5f6a71757b","name":"Admin","owner":{"user":{"id":"3b9b8ce1-f1e3-4013-864c-eeb821bdd6f1"}}},
 {"driveType":"virtual","id":"a0ca6a90-a365-4782-871e-d44447bbc668$a0ca6a90-a365-4782-871e-d44447bbc668","name":"Shares"},
 {"driveType":"project","id":"bd2c9d0b-4e8f-4c3a-9f6e-2b1a5d8c7e3f$bd2c9d0b-4e8f-4c3a-9f6e-2b1a5d8c7e3f","name":"Team files"}
]}"#;

    const SPACE_ROOT_PROPFIND: &[u8] = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
  <d:response>
    <d:href>/remote.php/dav/spaces/1284d238-aa92-42ce-bdc4-426446b3c735/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/spaces/1284d238-aa92-42ce-bdc4-426446b3c735/Documents/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    const SPACE_EMPTY_PROPFIND: &[u8] = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
  <d:response>
    <d:href>/remote.php/dav/spaces/1284d238-aa92-42ce-bdc4-426446b3c735/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

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
    fn ensure_opencloud_folder_creates_segments_under_the_space() {
        let http = ScriptedHttp::new(&[201, 201]);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        api.ensure_opencloud_folder(
            "https://cloud.example.com",
            "alice",
            "token",
            "7d443b01$9bc084a7",
            "/cloud/sub",
        )
        .unwrap();
        let urls: Vec<String> = requests
            .borrow()
            .iter()
            .map(|request| request.url.clone())
            .collect();
        let base = "https://cloud.example.com/remote.php/dav/spaces/7d443b01$9bc084a7";
        assert_eq!(
            urls,
            vec![format!("{base}/cloud"), format!("{base}/cloud/sub")]
        );
        assert!(requests.borrow().iter().all(|r| r.method == "MKCOL"));
    }

    #[test]
    fn ensure_opencloud_folder_noops_on_space_root_or_missing_space() {
        let http = ScriptedHttp::new(&[]);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        api.ensure_opencloud_folder(
            "https://cloud.example.com",
            "alice",
            "token",
            "7d443b01$9bc084a7",
            "/",
        )
        .unwrap();
        api.ensure_opencloud_folder("https://cloud.example.com", "alice", "token", "", "/cloud")
            .unwrap();
        assert!(requests.borrow().is_empty());
    }

    #[test]
    fn ensure_opencloud_folder_maps_401_to_auth_rejected() {
        let http = ScriptedHttp::new(&[401]);
        let api = NextcloudApi::with_http(Box::new(http));
        assert!(matches!(
            api.ensure_opencloud_folder(
                "https://cloud.example.com",
                "alice",
                "token",
                "7d443b01$9bc084a7",
                "/cloud"
            ),
            Err(ApiError::AuthRejected)
        ));
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
    fn root_etag_reads_the_etag_with_depth_zero() {
        let http = FakeHttp::new(207, ROOT_ETAG_PROPFIND);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        let etag = api
            .root_etag("https://cloud.example.com", "alice", "secret", "/")
            .unwrap();
        assert_eq!(etag.as_deref(), Some("\"cafebabe-0123456789\""));
        let request = &requests.borrow()[0];
        assert_eq!(request.method, "PROPFIND");
        assert_eq!(header_value(request, "Depth"), Some("0"));
        assert!(request.body.is_some());
    }

    #[test]
    fn root_etag_absent_body_yields_none() {
        let http = FakeHttp::new(207, EMPTY_PROPFIND);
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.root_etag("https://cloud.example.com", "alice", "secret", "/")
                .unwrap(),
            None
        );
    }

    #[test]
    fn parse_root_etag_returns_trimmed_text() {
        let body = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response><d:propstat><d:prop><d:getetag>  &quot;abc&quot;  </d:getetag></d:prop></d:propstat></d:response>
</d:multistatus>"#;
        assert_eq!(parse_root_etag(body).as_deref(), Some("\"abc\""));
        assert_eq!(parse_root_etag(br#"<xx/>"#), None);
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

    // ---- remote_size / opencloud_space_size (issue #36) ----------------------

    /// A full-tree multistatus: files carry `getcontentlength`, folders and
    /// entries without a length are skipped by the sum.
    const SIZES_PROPFIND: &[u8] = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:nc="http://nextcloud.org/ns">
  <d:response>
    <d:href>/remote.php/dav/files/alice/Docs/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Docs/a.bin</d:href>
    <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>1000</d:getcontentlength></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Docs/sub/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Docs/sub/b.bin</d:href>
    <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>2000</d:getcontentlength></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Docs/sizeless.bin</d:href>
    <d:propstat><d:prop><d:resourcetype/></d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn remote_size_sums_file_lengths_with_depth_infinity() {
        let http = FakeHttp::new(207, SIZES_PROPFIND);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.remote_size("https://cloud.example.com", "alice", "secret", "/Docs")
                .unwrap(),
            Some(3000)
        );
        let request = &requests.borrow()[0];
        assert_eq!(request.method, "PROPFIND");
        assert_eq!(header_value(request, "Depth"), Some("infinity"));
        let body = request.body.as_deref().unwrap_or_default();
        let body = String::from_utf8_lossy(body);
        assert!(body.contains("getcontentlength"));
    }

    #[test]
    fn remote_size_depth_refusal_means_unknown() {
        for status in [400u16, 403, 507] {
            let http = FakeHttp::new(status, b"");
            let api = NextcloudApi::with_http(Box::new(http));
            assert_eq!(
                api.remote_size("https://cloud.example.com", "alice", "secret", "/Docs"),
                Ok(None),
                "status {status} should mean unknown size"
            );
        }
    }

    #[test]
    fn remote_size_keeps_auth_rejection() {
        let http = FakeHttp::new(401, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.remote_size("https://cloud.example.com", "alice", "secret", "/Docs"),
            Err(ApiError::AuthRejected)
        );
    }

    #[test]
    fn opencloud_space_size_reads_the_drive_quota() {
        let body = br#"{"id":"space$root","driveType":"personal","quota":{"used":123456789,"total":1000000000}}"#;
        let http = FakeHttp::new(200, body);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.opencloud_space_size("https://cloud.example.com", "alice", "token", "space$root")
                .unwrap(),
            Some(123456789)
        );
        assert!(requests.borrow()[0]
            .url
            .ends_with("/graph/v1.0/drives/space$root"));
    }

    #[test]
    fn opencloud_space_size_without_quota_is_unknown() {
        let http = FakeHttp::new(200, br#"{"id":"space$root"}"#);
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.opencloud_space_size("https://cloud.example.com", "alice", "token", "space$root"),
            Ok(None)
        );
    }

    // ---- server trash (issue #38) --------------------------------------------

    const TRASH_PROPFIND: &[u8] = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:nc="http://nextcloud.org/ns">
  <d:response>
    <d:href>/remote.php/dav/trashbin/alice/trash/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/trashbin/alice/trash/report.pdf.d1699999999</d:href>
    <d:propstat><d:prop>
      <d:resourcetype/>
      <nc:trashbin-filename>report.pdf.d1699999999</nc:trashbin-filename>
      <nc:trashbin-original-location>Documents/report.pdf</nc:trashbin-original-location>
      <nc:trashbin-deletion-time>1712345678</nc:trashbin-deletion-time>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/trashbin/alice/trash/photos.d1699990000</d:href>
    <d:propstat><d:prop>
      <d:resourcetype><d:collection/></d:resourcetype>
      <nc:trashbin-filename>photos.d1699990000</nc:trashbin-filename>
      <nc:trashbin-original-location>Photos</nc:trashbin-original-location>
      <nc:trashbin-deletion-time>1712000000</nc:trashbin-deletion-time>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/trashbin/alice/trash/legacy.d1</d:href>
    <d:propstat><d:prop><d:resourcetype/></d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn list_trash_parses_names_locations_and_dates() {
        let http = FakeHttp::new(207, TRASH_PROPFIND);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        let items = api
            .list_trash("https://cloud.example.com", "alice", "secret")
            .unwrap();
        // Newest first; the trash root itself is skipped.
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].filename, "report.pdf.d1699999999");
        assert_eq!(
            items[0].original_location.as_deref(),
            Some("Documents/report.pdf")
        );
        assert_eq!(items[0].deletion_time, Some(1712345678));
        assert!(!items[0].is_collection);
        assert!(items[1].is_collection);
        // An entry without the Nextcloud properties falls back to the href
        // name and unknown metadata.
        assert_eq!(items[2].filename, "legacy.d1");
        assert_eq!(items[2].original_location, None);
        assert_eq!(items[2].deletion_time, None);
        let request = &requests.borrow()[0];
        assert!(request
            .url
            .ends_with("/remote.php/dav/trashbin/alice/trash"));
        assert!(
            String::from_utf8_lossy(&request.body.clone().unwrap_or_default())
                .contains("trashbin-filename")
        );
    }

    #[test]
    fn list_trash_surfaces_auth_and_http_errors() {
        let http = FakeHttp::new(401, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.list_trash("https://cloud.example.com", "alice", "secret"),
            Err(ApiError::AuthRejected)
        );
        let http = FakeHttp::new(500, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.list_trash("https://cloud.example.com", "alice", "secret"),
            Err(ApiError::Http { status: 500 })
        );
    }

    #[test]
    fn restore_trash_item_moves_into_the_restore_folder() {
        let http = FakeHttp::new(201, b"");
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        api.restore_trash_item("https://cloud.example.com", "alice", "secret", "my file.d1")
            .unwrap();
        let request = &requests.borrow()[0];
        assert_eq!(request.method, "MOVE");
        assert!(request.url.ends_with("/trashbin/alice/trash/my%20file.d1"));
        assert_eq!(
            header_value(request, "Destination").unwrap(),
            "https://cloud.example.com/remote.php/dav/trashbin/alice/restore"
        );
    }

    #[test]
    fn restore_trash_item_maps_failures() {
        let http = FakeHttp::new(404, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.restore_trash_item("https://cloud.example.com", "alice", "secret", "gone.d1"),
            Err(ApiError::Http { status: 404 })
        );
        let http = FakeHttp::new(403, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.restore_trash_item("https://cloud.example.com", "alice", "secret", "x.d1"),
            Err(ApiError::AuthRejected)
        );
    }

    #[test]
    fn opencloud_space_size_without_space_makes_no_request() {
        let http = FakeHttp::new(200, br#"{}"#);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.opencloud_space_size("https://cloud.example.com", "alice", "token", "  "),
            Ok(None)
        );
        assert!(requests.borrow().is_empty());
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
        assert_eq!(folders, ["/Documents", "/Música Albums", "/Photos"]);
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

    // ---- OpenCloud credentials / spaces -----------------------------------

    #[test]
    fn validate_opencloud_hits_the_graph_me_endpoint() {
        let http = FakeHttp::new(200, OPENCLOUD_ME_JSON);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.validate_opencloud_credentials("https://cloud.example.com", "alice", "token"),
            Ok(Some("nacho".to_owned()))
        );
        let request = &requests.borrow()[0];
        assert_eq!(request.method, "GET");
        assert!(request.url.ends_with("/graph/v1.0/me"));
        assert!(header_value(request, "Authorization")
            .unwrap()
            .starts_with("Basic "));
    }

    #[test]
    fn validate_opencloud_rejects_a_bad_token() {
        let http = FakeHttp::new(401, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.validate_opencloud_credentials("https://cloud.example.com", "alice", "token"),
            Err(ApiError::AuthRejected)
        );
    }

    #[test]
    fn validate_opencloud_http_error_surfaces() {
        let http = FakeHttp::new(500, b"");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.validate_opencloud_credentials("https://cloud.example.com", "alice", "token"),
            Err(ApiError::Http { status: 500 })
        );
    }

    #[test]
    fn validate_opencloud_malformed_json_surfaces() {
        let http = FakeHttp::new(200, b"not json");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.validate_opencloud_credentials("https://cloud.example.com", "alice", "token"),
            Err(ApiError::InvalidResponse)
        );
    }

    /// Fake answering the two LibreGraph endpoints with distinct payloads.
    struct GraphHttp {
        me_body: Vec<u8>,
        drives_body: Vec<u8>,
        requests: Rc<RefCell<Vec<RecordedRequest>>>,
    }

    impl GraphHttp {
        fn new(me: &[u8], drives: &[u8]) -> Self {
            Self {
                me_body: me.to_vec(),
                drives_body: drives.to_vec(),
                requests: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }

    impl HttpClient for GraphHttp {
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
            let body = if url.contains("/graph/v1.0/drives") {
                self.drives_body.clone()
            } else {
                self.me_body.clone()
            };
            Ok(HttpResponse { status: 200, body })
        }
    }

    #[test]
    fn list_opencloud_spaces_keeps_own_personal_and_project_only() {
        let http = GraphHttp::new(OPENCLOUD_ME_JSON, OPENCLOUD_DRIVES_JSON);
        let api = NextcloudApi::with_http(Box::new(http));
        let spaces = api
            .list_opencloud_spaces("https://cloud.example.com", "alice", "token")
            .unwrap();
        assert_eq!(
            spaces,
            [
                OpenCloudSpace {
                    id: "bd2c9d0b-4e8f-4c3a-9f6e-2b1a5d8c7e3f$bd2c9d0b-4e8f-4c3a-9f6e-2b1a5d8c7e3f"
                        .to_owned(),
                    display_name: Some("Team files".to_owned()),
                },
                OpenCloudSpace {
                    id: "7d443b01-21d3-484d-bf73-a2681d670fa1$9bc084a7-9bb4-47d5-84da-dd7857ab189c"
                        .to_owned(),
                    display_name: Some("nacho".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn list_opencloud_spaces_malformed_json_surfaces() {
        let http = GraphHttp::new(OPENCLOUD_ME_JSON, b"not json");
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.list_opencloud_spaces("https://cloud.example.com", "alice", "token"),
            Err(ApiError::InvalidResponse)
        );
    }

    #[test]
    fn probe_opencloud_space_counts_children() {
        let http = FakeHttp::new(207, SPACE_ROOT_PROPFIND);
        let api = NextcloudApi::with_http(Box::new(http));
        assert!(api
            .probe_opencloud_space(
                "https://cloud.example.com",
                "alice",
                "token",
                "1284d238-aa92-42ce-bdc4-426446b3c735"
            )
            .unwrap());
    }

    #[test]
    fn probe_opencloud_empty_space_returns_false() {
        let http = FakeHttp::new(207, SPACE_EMPTY_PROPFIND);
        let api = NextcloudApi::with_http(Box::new(http));
        assert!(!api
            .probe_opencloud_space(
                "https://cloud.example.com",
                "alice",
                "token",
                "1284d238-aa92-42ce-bdc4-426446b3c735"
            )
            .unwrap());
    }

    #[test]
    fn probe_opencloud_space_without_id_is_false() {
        let http = FakeHttp::new(207, SPACE_EMPTY_PROPFIND);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        assert_eq!(
            api.probe_opencloud_space("https://cloud.example.com", "alice", "token", ""),
            Ok(false)
        );
        assert!(requests.borrow().is_empty());
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
        // Percent-encoded hrefs decode to the real names (issue #88).
        assert_eq!(folders, ["/Documents", "/Música Albums", "/Photos"]);
        handle.join().unwrap();
    }

    #[test]
    fn percent_decode_round_trips_spaces_and_accents() {
        // The inverse of the encoder used when building URLs: decoding what
        // the encoder produced must return the original, so the engine's
        // single re-encoding yields exactly one %XX per special byte. The
        // picker works with single-segment names (the folder name).
        for name in ["Música Albums", "a b&c", "plain", "100% seguro", "üñïçø∂é"] {
            let encoded = percent_encode_path(name);
            assert_eq!(percent_decode_path(&encoded), name, "name: {name}");
        }
        // Malformed escapes and bare percents stay literal.
        assert_eq!(percent_decode_path("100%"), "100%");
        assert_eq!(percent_decode_path("%zz"), "%zz");
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

    // ---- fetch_avatar (issue #50) -------------------------------------------

    const AVATAR_PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake-avatar-bytes";

    #[test]
    fn nextcloud_avatar_is_returned_on_200() {
        let http = FakeHttp::new(200, AVATAR_PNG);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        let avatar = api
            .fetch_avatar(
                crate::nextcloud::driver::Provider::Nextcloud,
                "https://cloud.example.com",
                "alice",
                "secret",
            )
            .unwrap();
        assert_eq!(avatar.as_deref(), Some(AVATAR_PNG));
        let requests = requests.borrow();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].url, "https://cloud.example.com/avatar/alice/64");
        assert!(header_value(&requests[0], "Authorization").is_some());
    }

    #[test]
    fn opencloud_avatar_uses_the_graph_photo_endpoint() {
        let http = FakeHttp::new(200, AVATAR_PNG);
        let requests = http.requests.clone();
        let api = NextcloudApi::with_http(Box::new(http));
        let avatar = api
            .fetch_avatar(
                crate::nextcloud::driver::Provider::OpenCloud,
                "https://cloud.example.com",
                "alice",
                "token",
            )
            .unwrap();
        assert_eq!(avatar.as_deref(), Some(AVATAR_PNG));
        let requests = requests.borrow();
        assert_eq!(
            requests[0].url,
            "https://cloud.example.com/graph/v1.0/me/photo/$value"
        );
    }

    #[test]
    fn avatar_is_none_on_redirect_or_empty_body() {
        for status in [301u16, 302, 200] {
            let body = if status == 200 { b"" } else { AVATAR_PNG };
            let api = NextcloudApi::with_http(Box::new(FakeHttp::new(status, body)));
            let avatar = api
                .fetch_avatar(
                    crate::nextcloud::driver::Provider::Nextcloud,
                    "https://cloud.example.com",
                    "alice",
                    "secret",
                )
                .unwrap();
            assert_eq!(avatar, None, "status {status}");
        }
    }

    #[test]
    fn avatar_uses_image_body_even_on_404() {
        // Nextcloud ships the generated placeholder avatar with a 404 status
        // when the user has no custom one; a JSON error body stays None.
        let api = NextcloudApi::with_http(Box::new(FakeHttp::new(404, AVATAR_PNG)));
        let avatar = api
            .fetch_avatar(
                crate::nextcloud::driver::Provider::Nextcloud,
                "https://cloud.example.com",
                "alice",
                "secret",
            )
            .unwrap();
        assert_eq!(avatar.as_deref(), Some(AVATAR_PNG));

        let api = NextcloudApi::with_http(Box::new(FakeHttp::new(404, b"[]")));
        let avatar = api
            .fetch_avatar(
                crate::nextcloud::driver::Provider::Nextcloud,
                "https://cloud.example.com",
                "alice",
                "secret",
            )
            .unwrap();
        assert_eq!(avatar, None);
    }

    #[test]
    fn avatar_maps_auth_and_http_failures() {
        let api = NextcloudApi::with_http(Box::new(FakeHttp::new(401, b"")));
        assert_eq!(
            api.fetch_avatar(
                crate::nextcloud::driver::Provider::Nextcloud,
                "https://cloud.example.com",
                "alice",
                "wrong",
            )
            .unwrap_err(),
            ApiError::AuthRejected
        );
        let api = NextcloudApi::with_http(Box::new(FakeHttp::new(500, b"")));
        assert_eq!(
            api.fetch_avatar(
                crate::nextcloud::driver::Provider::OpenCloud,
                "https://cloud.example.com",
                "alice",
                "token",
            )
            .unwrap_err(),
            ApiError::Http { status: 500 }
        );
    }
}
