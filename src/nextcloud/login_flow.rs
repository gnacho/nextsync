//! Nextcloud Login Flow v2 (the "Sign in with browser" path).
//!
//! Port of `src/nextsync/nextcloud/login_flow.py` (v0.4.0). The flow has two
//! steps, both plain HTTP against the user's server:
//!
//! 1. **Initiate** ([`LoginFlowV2::initiate`]): `POST <server>/index.php/login/v2`
//!    with the app's User-Agent. The server answers a JSON payload with the
//!    `poll` endpoint + token pair and the `login` URL the user must open in
//!    a browser to authorize the app.
//! 2. **Poll** ([`LoginFlowV2::poll`]): `POST <poll.endpoint>` with a
//!    `token=<...>` form body every [`POLL_INTERVAL_SECONDS`] seconds. A 404
//!    means "not authorized yet" ([`PollOutcome::Pending`]); once the user
//!    authorizes, the server answers 200 with the final `server`, `loginName`
//!    and `appPassword` triple ([`PollOutcome::Authorized`]).
//!
//! The UI layer drives the timer, opens the browser and enforces the
//! [`MAX_POLLS`] budget (600 polls × 2 s = 20 minutes, like the Python
//! `TimeoutError`).
//!
//! # Deviations from `login_flow.py` (motivated)
//!
//! - The Python class owns a GLib timer and a callback; this port is a pure
//!   synchronous client (the crate's module contract) — the wizard drives the
//!   polling from the main loop with `gio::spawn_blocking` for the HTTP work.
//! - Python ignores transport errors during polling (a flaky network just
//!   skips one round); the Rust driver replicates that policy at the call
//!   site instead of silently here, so [`LoginFlowV2::poll`] surfaces every
//!   error and the caller decides.
//! - `trust_invalid_certificates` is not honored here: the production
//!   [`UreqHttpClient`] has no such option either (the wizard persists the
//!   choice for the sync engine), matching the existing `validate_credentials`
//!   path.

use std::error::Error;
use std::fmt;

use crate::nextcloud::api::{ApiError, HttpClient, UreqHttpClient};
use crate::util::i18n::t;

/// Path of the Login Flow v2 initiation endpoint, relative to the server URL.
const LOGIN_V2_PATH: &str = "index.php/login/v2";

/// Seconds between poll requests (the Python `GLib.timeout_add_seconds(2, …)`).
pub const POLL_INTERVAL_SECONDS: u32 = 2;

/// Poll attempts before the flow expires (600 × 2 s = 20 minutes).
pub const MAX_POLLS: usize = 600;

/// Content type of the poll request body (Python `urlencode` default).
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";

/// A successfully initiated Login Flow v2 session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginFlowStart {
    /// Absolute URL to poll until the user authorizes (`poll.endpoint`).
    pub poll_endpoint: String,
    /// Secret token posted to the poll endpoint (`poll.token`).
    pub poll_token: String,
    /// URL the user opens in a browser to authorize the app (`login`).
    pub login_url: String,
}

/// The credentials returned once the user authorizes the flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginFlowResult {
    /// Canonical server URL (may differ from the typed one after a redirect).
    pub server: String,
    /// The account login name (usable as the `loginName` of the account).
    pub login_name: String,
    /// The server-generated app password (the account secret).
    pub app_password: String,
}

/// Outcome of one poll request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// HTTP 404: the user has not authorized the app yet, keep polling.
    Pending,
    /// HTTP 2xx with the final credentials.
    Authorized(LoginFlowResult),
}

/// Error of the Login Flow v2 client.
///
/// The variants keep the init/poll distinction of the Python messages so the
/// UI can show them verbatim; [`LoginFlowError::message`] returns the
/// translated (catalog-aware) sentence with the status interpolated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginFlowError {
    /// Transport-level failure (connection, DNS, TLS, timeout).
    Transport,
    /// The initiation endpoint answered a non-2xx status.
    InitHttp { status: u16 },
    /// The initiation payload was not the expected JSON.
    InitInvalidResponse,
    /// The poll endpoint answered a non-2xx, non-404 status.
    PollHttp { status: u16 },
    /// The final payload was not the expected JSON.
    PollInvalidResponse,
}

impl LoginFlowError {
    /// The user-visible sentence, translated through the i18n catalog.
    pub fn message(&self) -> String {
        match self {
            Self::Transport => t("Network error.").to_string(),
            Self::InitHttp { status } => {
                t("Login Flow returned HTTP {status}.").replacen("{status}", &status.to_string(), 1)
            }
            Self::InitInvalidResponse => t("Invalid Login Flow response.").to_string(),
            Self::PollHttp { status } => t("Login authorization returned HTTP {status}.").replacen(
                "{status}",
                &status.to_string(),
                1,
            ),
            Self::PollInvalidResponse => t("Invalid authorization response.").to_string(),
        }
    }
}

impl fmt::Display for LoginFlowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl Error for LoginFlowError {}

/// The Login Flow v2 client (initiate + poll), transport-injectable.
pub struct LoginFlowV2 {
    http: Box<dyn HttpClient>,
}

impl LoginFlowV2 {
    /// Create a client with the production HTTP backend (ureq + rustls).
    pub fn new() -> Self {
        Self::with_http(Box::new(UreqHttpClient::new()))
    }

    /// Create a client with a custom transport (tests inject a fake here).
    pub fn with_http(http: Box<dyn HttpClient>) -> Self {
        Self { http }
    }

    /// Start a Login Flow v2 session against `server`.
    ///
    /// `server` is the wizard-normalized server URL; trailing slashes are
    /// trimmed like in the Python (`server.rstrip('/')`).
    pub fn initiate(&self, server: &str) -> Result<LoginFlowStart, LoginFlowError> {
        let url = format!("{}/{LOGIN_V2_PATH}", server.trim_end_matches('/'));
        let agent = user_agent();
        let headers = [("User-Agent", agent.as_str())];
        let response = self
            .http
            .request("POST", &url, &headers, None)
            .map_err(map_init_error)?;
        if !(200..300).contains(&response.status) {
            return Err(LoginFlowError::InitHttp {
                status: response.status,
            });
        }
        parse_flow_start(&response.body)
    }

    /// Poll the endpoint once.
    ///
    /// HTTP 404 maps to [`PollOutcome::Pending`]; 2xx carries the final
    /// credentials. Any other status or a malformed payload is an error.
    pub fn poll(&self, start: &LoginFlowStart) -> Result<PollOutcome, LoginFlowError> {
        let body = format!("token={}", form_urlencode(&start.poll_token));
        let agent = user_agent();
        let headers = [
            ("Content-Type", FORM_CONTENT_TYPE),
            ("User-Agent", agent.as_str()),
        ];
        let response = self
            .http
            .request(
                "POST",
                &start.poll_endpoint,
                &headers,
                Some(body.as_bytes()),
            )
            .map_err(|_| LoginFlowError::Transport)?;
        if response.status == 404 {
            return Ok(PollOutcome::Pending);
        }
        if !(200..300).contains(&response.status) {
            return Err(LoginFlowError::PollHttp {
                status: response.status,
            });
        }
        parse_flow_result(&response.body).map(PollOutcome::Authorized)
    }
}

impl Default for LoginFlowV2 {
    fn default() -> Self {
        Self::new()
    }
}

/// `NextSync/<version>`, the User-Agent the Python session sends.
fn user_agent() -> String {
    format!("NextSync/{}", env!("CARGO_PKG_VERSION"))
}

/// Map a transport failure of the initiation step.
fn map_init_error(error: ApiError) -> LoginFlowError {
    match error {
        ApiError::InvalidResponse => LoginFlowError::InitInvalidResponse,
        _ => LoginFlowError::Transport,
    }
}

/// Parse the initiation payload (`poll.endpoint`, `poll.token`, `login`).
fn parse_flow_start(body: &[u8]) -> Result<LoginFlowStart, LoginFlowError> {
    let payload: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| LoginFlowError::InitInvalidResponse)?;
    let nested = |outer: &str, inner: &str| {
        payload
            .get(outer)
            .and_then(|node| node.get(inner))
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let field = |name: &str| {
        payload
            .get(name)
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Ok(LoginFlowStart {
        poll_endpoint: nested("poll", "endpoint").ok_or(LoginFlowError::InitInvalidResponse)?,
        poll_token: nested("poll", "token").ok_or(LoginFlowError::InitInvalidResponse)?,
        login_url: field("login").ok_or(LoginFlowError::InitInvalidResponse)?,
    })
}

/// Parse the final payload (`server`, `loginName`, `appPassword`).
fn parse_flow_result(body: &[u8]) -> Result<LoginFlowResult, LoginFlowError> {
    let payload: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| LoginFlowError::PollInvalidResponse)?;
    let field = |name: &str| {
        payload
            .get(name)
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Ok(LoginFlowResult {
        server: field("server").ok_or(LoginFlowError::PollInvalidResponse)?,
        login_name: field("loginName").ok_or(LoginFlowError::PollInvalidResponse)?,
        app_password: field("appPassword").ok_or(LoginFlowError::PollInvalidResponse)?,
    })
}

/// Percent-encode a value for an `application/x-www-form-urlencoded` body.
///
/// Python's `urlencode` escapes everything but letters, digits and `_.-`;
/// the poll token never carries spaces, so the `+` mapping is irrelevant.
fn form_urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nextcloud::api::HttpResponse;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A realistic initiation payload (shape from the Login Flow v2 docs).
    const START_JSON: &[u8] = br#"{
        "poll": {
            "token": "v5E2lQAZKdzqW5OSR7OQFnpZ8oERmLXdc7n6NTuplbqXFsTpEVXzIpmZMx35O51e",
            "endpoint": "https://cloud.example.com/index.php/login/v2/poll"
        },
        "login": "https://cloud.example.com/index.php/login/v2/flow/AbCdEf123"
    }"#;

    /// A realistic final payload.
    const RESULT_JSON: &[u8] = br#"{
        "server": "https://cloud.example.com/",
        "loginName": "alice",
        "appPassword": "pencil-lion-42-cloudy-river"
    }"#;

    /// Deterministic fake transport (same pattern as `api.rs`).
    #[derive(Default)]
    struct FakeHttp {
        responses: Vec<Result<HttpResponse, ApiError>>,
        requests: Rc<RefCell<Vec<RecordedRequest>>>,
    }

    impl FakeHttp {
        /// Serve the given responses in order (repeating the last one).
        fn request_at(&self, index: usize) -> Result<HttpResponse, ApiError> {
            match self.responses.get(index) {
                Some(result) => result.clone(),
                None => self.responses.last().cloned().unwrap_or(Ok(HttpResponse {
                    status: 200,
                    body: Vec::new(),
                })),
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
            let index = self.requests.borrow().len();
            self.requests.borrow_mut().push(RecordedRequest {
                method: method.to_owned(),
                url: url.to_owned(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body: body.map(<[u8]>::to_vec),
            });
            self.request_at(index)
        }
    }

    fn header_value<'a>(request: &'a RecordedRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn flow_with(
        responses: Vec<Result<HttpResponse, ApiError>>,
    ) -> (LoginFlowV2, Rc<RefCell<Vec<RecordedRequest>>>) {
        let http = FakeHttp {
            responses,
            requests: Rc::new(RefCell::new(Vec::new())),
        };
        let requests = http.requests.clone();
        (LoginFlowV2::with_http(Box::new(http)), requests)
    }

    // ---- initiate -----------------------------------------------------------

    #[test]
    fn initiate_posts_the_login_v2_endpoint() {
        let (flow, requests) = flow_with(vec![Ok(HttpResponse {
            status: 200,
            body: START_JSON.to_vec(),
        })]);
        let start = flow.initiate("https://cloud.example.com").unwrap();
        let request = &requests.borrow()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "https://cloud.example.com/index.php/login/v2");
        assert_eq!(
            header_value(request, "User-Agent"),
            Some(format!("NextSync/{}", env!("CARGO_PKG_VERSION")).as_str())
        );
        assert!(start.poll_endpoint.ends_with("/login/v2/poll"));
        assert!(start.login_url.ends_with("/flow/AbCdEf123"));
        assert_eq!(
            start.poll_token,
            "v5E2lQAZKdzqW5OSR7OQFnpZ8oERmLXdc7n6NTuplbqXFsTpEVXzIpmZMx35O51e"
        );
    }

    #[test]
    fn initiate_trims_trailing_slashes_from_the_server() {
        let (flow, requests) = flow_with(vec![Ok(HttpResponse {
            status: 200,
            body: START_JSON.to_vec(),
        })]);
        flow.initiate("https://cloud.example.com///").unwrap();
        assert_eq!(
            requests.borrow()[0].url,
            "https://cloud.example.com/index.php/login/v2"
        );
    }

    #[test]
    fn initiate_http_error_maps_to_init_http() {
        let (flow, _) = flow_with(vec![Ok(HttpResponse {
            status: 429,
            body: Vec::new(),
        })]);
        assert_eq!(
            flow.initiate("https://cloud.example.com"),
            Err(LoginFlowError::InitHttp { status: 429 })
        );
    }

    #[test]
    fn initiate_malformed_payload_maps_to_init_invalid_response() {
        for body in [
            &b"not json"[..],
            br#"{"poll":{"endpoint":"https://x/poll"}}"#,
            br#"{"poll":{"token":"t"},"login":"https://x/flow"}"#,
            br#"{"poll":{"endpoint":"https://x/poll","token":""},"login":"https://x/flow"}"#,
            br#"{"poll":{"endpoint":"https://x/poll","token":"t"}}"#,
            br#"{"poll":{"endpoint":"","token":"t"},"login":"https://x/flow"}"#,
        ] {
            let (flow, _) = flow_with(vec![Ok(HttpResponse {
                status: 200,
                body: body.to_vec(),
            })]);
            assert_eq!(
                flow.initiate("https://cloud.example.com"),
                Err(LoginFlowError::InitInvalidResponse),
                "body: {}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn initiate_transport_failure_maps_to_transport() {
        let (flow, _) = flow_with(vec![Err(ApiError::Transport)]);
        assert_eq!(
            flow.initiate("https://cloud.example.com"),
            Err(LoginFlowError::Transport)
        );
    }

    // ---- poll ---------------------------------------------------------------

    #[test]
    fn poll_posts_the_token_as_a_form_body() {
        let (flow, requests) = flow_with(vec![Ok(HttpResponse {
            status: 200,
            body: RESULT_JSON.to_vec(),
        })]);
        let start = LoginFlowStart {
            poll_endpoint: "https://cloud.example.com/index.php/login/v2/poll".to_string(),
            poll_token: "a+b/c=d".to_string(),
            login_url: "https://cloud.example.com/index.php/login/v2/flow/X".to_string(),
        };
        flow.poll(&start).unwrap();
        let request = &requests.borrow()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.url,
            "https://cloud.example.com/index.php/login/v2/poll"
        );
        assert_eq!(
            header_value(request, "Content-Type"),
            Some(FORM_CONTENT_TYPE)
        );
        assert_eq!(request.body.as_deref(), Some(&b"token=a%2Bb%2Fc%3Dd"[..]));
    }

    #[test]
    fn poll_404_means_pending() {
        let (flow, _) = flow_with(vec![Ok(HttpResponse {
            status: 404,
            body: Vec::new(),
        })]);
        let start = LoginFlowStart {
            poll_endpoint: "https://cloud.example.com/poll".to_string(),
            poll_token: "t".to_string(),
            login_url: "https://cloud.example.com/flow".to_string(),
        };
        assert_eq!(flow.poll(&start), Ok(PollOutcome::Pending));
    }

    #[test]
    fn poll_2xx_returns_the_credentials() {
        let (flow, _) = flow_with(vec![Ok(HttpResponse {
            status: 200,
            body: RESULT_JSON.to_vec(),
        })]);
        let start = LoginFlowStart {
            poll_endpoint: "https://cloud.example.com/poll".to_string(),
            poll_token: "t".to_string(),
            login_url: "https://cloud.example.com/flow".to_string(),
        };
        assert_eq!(
            flow.poll(&start),
            Ok(PollOutcome::Authorized(LoginFlowResult {
                server: "https://cloud.example.com/".to_string(),
                login_name: "alice".to_string(),
                app_password: "pencil-lion-42-cloudy-river".to_string(),
            }))
        );
    }

    #[test]
    fn poll_http_error_maps_to_poll_http() {
        let (flow, _) = flow_with(vec![Ok(HttpResponse {
            status: 500,
            body: Vec::new(),
        })]);
        let start = LoginFlowStart {
            poll_endpoint: "https://cloud.example.com/poll".to_string(),
            poll_token: "t".to_string(),
            login_url: "https://cloud.example.com/flow".to_string(),
        };
        assert_eq!(
            flow.poll(&start),
            Err(LoginFlowError::PollHttp { status: 500 })
        );
    }

    #[test]
    fn poll_malformed_payload_maps_to_poll_invalid_response() {
        for body in [
            &b"not json"[..],
            br#"{"server":"https://x","loginName":"alice"}"#,
            br#"{"server":"https://x","appPassword":"p"}"#,
            br#"{"loginName":"","server":"https://x","appPassword":"p"}"#,
        ] {
            let (flow, _) = flow_with(vec![Ok(HttpResponse {
                status: 200,
                body: body.to_vec(),
            })]);
            let start = LoginFlowStart {
                poll_endpoint: "https://cloud.example.com/poll".to_string(),
                poll_token: "t".to_string(),
                login_url: "https://cloud.example.com/flow".to_string(),
            };
            assert_eq!(
                flow.poll(&start),
                Err(LoginFlowError::PollInvalidResponse),
                "body: {}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn poll_transport_failure_maps_to_transport() {
        let (flow, _) = flow_with(vec![Err(ApiError::Transport)]);
        let start = LoginFlowStart {
            poll_endpoint: "https://cloud.example.com/poll".to_string(),
            poll_token: "t".to_string(),
            login_url: "https://cloud.example.com/flow".to_string(),
        };
        assert_eq!(flow.poll(&start), Err(LoginFlowError::Transport));
    }

    // ---- error messages ------------------------------------------------------

    #[test]
    fn error_messages_are_translated() {
        use crate::util::i18n::{reset_locale, set_locale, Locale};
        set_locale(Locale::English);
        assert_eq!(
            LoginFlowError::InitHttp { status: 429 }.message(),
            "Login Flow returned HTTP 429."
        );
        assert_eq!(
            LoginFlowError::PollInvalidResponse.message(),
            "Invalid authorization response."
        );
        set_locale(Locale::Spanish);
        assert_eq!(
            LoginFlowError::InitInvalidResponse.message(),
            "Respuesta de Login Flow no válida."
        );
        assert_eq!(
            LoginFlowError::PollHttp { status: 503 }.message(),
            "La autorización devolvió HTTP 503."
        );
        set_locale(Locale::English);
        reset_locale();
    }

    // ---- form encoding -------------------------------------------------------

    #[test]
    fn form_urlencode_escapes_reserved_characters() {
        assert_eq!(form_urlencode("plain"), "plain");
        assert_eq!(form_urlencode("a b"), "a%20b");
        assert_eq!(form_urlencode("a+b/c?d=e&f"), "a%2Bb%2Fc%3Fd%3De%26f");
    }

    /// The poll budget matches the Python 20-minute window.
    #[test]
    fn poll_budget_is_twenty_minutes() {
        assert_eq!(POLL_INTERVAL_SECONDS, 2);
        assert_eq!(MAX_POLLS, 600);
        assert_eq!(
            POLL_INTERVAL_SECONDS as u64 * MAX_POLLS as u64,
            20 * 60,
            "600 polls every 2 seconds must span 20 minutes"
        );
    }

    // ---- integration with a real local server -------------------------------

    /// Initiate + poll against a live tiny_http server, exercising the real
    /// ureq transport end to end (localhost only, no net).
    #[test]
    fn integration_initiate_and_poll_against_local_server() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let base = format!("http://{addr}");
        let start_json =
            String::from_utf8_lossy(START_JSON).replace("https://cloud.example.com", &base);
        let handle = std::thread::spawn(move || {
            let initiate = server.recv().unwrap();
            assert_eq!(initiate.method().as_str(), "POST");
            assert!(initiate.url().ends_with("/index.php/login/v2"));
            let has_user_agent = initiate.headers().iter().any(|header| {
                header.field.equiv("User-Agent") && !header.value.as_str().is_empty()
            });
            assert!(has_user_agent, "the initiation must carry a User-Agent");
            initiate
                .respond(tiny_http::Response::from_string(start_json).with_status_code(200))
                .unwrap();

            let mut poll = server.recv().unwrap();
            assert_eq!(poll.method().as_str(), "POST");
            let mut body = String::new();
            let _ = std::io::Read::read_to_string(poll.as_reader(), &mut body);
            assert!(body.starts_with("token="), "body was {body}");
            poll.respond(
                tiny_http::Response::from_string(String::from_utf8_lossy(RESULT_JSON).into_owned())
                    .with_status_code(200),
            )
            .unwrap();
        });

        let flow = LoginFlowV2::new();
        let start = flow.initiate(&base).unwrap();
        assert!(start.poll_endpoint.starts_with(&base));
        match flow.poll(&start).unwrap() {
            PollOutcome::Authorized(result) => {
                assert_eq!(result.login_name, "alice");
                assert_eq!(result.app_password, "pencil-lion-42-cloudy-river");
            }
            PollOutcome::Pending => panic!("the local server already authorized the flow"),
        }
        handle.join().unwrap();
    }

    /// A real 404 from the poll endpoint maps to `Pending`.
    #[test]
    fn integration_poll_404_against_local_server() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let base = format!("http://{addr}");
        let handle = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            request
                .respond(tiny_http::Response::from_string("not yet").with_status_code(404))
                .unwrap();
        });

        let flow = LoginFlowV2::new();
        let start = LoginFlowStart {
            poll_endpoint: format!("{base}/index.php/login/v2/poll"),
            poll_token: "t".to_string(),
            login_url: format!("{base}/index.php/login/v2/flow/X"),
        };
        assert_eq!(flow.poll(&start), Ok(PollOutcome::Pending));
        handle.join().unwrap();
    }
}
