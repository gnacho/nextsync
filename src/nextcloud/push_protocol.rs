//! notify_push protocol helpers: capability parsing and the tolerant
//! WebSocket handshake.
//!
//! Fase 4 (Task 4.1). Mirrors `nextcloud/push_protocol.py` for the pure
//! protocol parts (capabilities discovery and transport validation) and adds
//! the *tolerant* client handshake that the Python client gets from libsoup.
//!
//! The strict handshake validation bundled with tungstenite rejects servers
//! that advertise extra values in the `Upgrade` header (e.g. openresty sending
//! `Upgrade: h2,h2c, websocket`): it compares the whole header value with
//! `eq_ignore_ascii_case("websocket")`. The helper here only requires that one
//! comma/whitespace-separated token equals `websocket` and that the
//! `Sec-WebSocket-Accept` header matches the RFC 6455 SHA-1 answer, ignoring
//! any other (possibly malformed) header line. This replicates the behaviour
//! libsoup3 gives the Python client without its bugs.

use serde_json::Value;

/// Push endpoints discovered from the OCS capabilities response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushEndpoints {
    /// Absolute `ws`/`wss` URL of the notify_push WebSocket.
    pub websocket: String,
    /// Optional pre-authentication endpoint (`POST`).
    pub pre_auth: Option<String>,
}

/// Parse the `notify_push` capability out of an OCS capabilities payload.
///
/// Returns `Ok(None)` when the server does not advertise notify_push (or the
/// advertised websocket URL is missing/empty), `Err` when the payload is
/// structurally malformed (the Python original propagates the exception and
/// the caller schedules a reconnect), and `Ok(Some(...))` for a usable pair.
pub fn parse_push_capability(payload: &Value) -> Result<Option<PushEndpoints>, String> {
    let capabilities = payload
        .get("ocs")
        .and_then(|o| o.get("data"))
        .and_then(|d| d.get("capabilities"));
    let push = match capabilities {
        Some(capabilities) => capabilities.get("notify_push"),
        None => None,
    };
    let push = match push {
        Some(Value::Object(push)) => push,
        Some(_) | None => return Ok(None),
    };
    // `endpoints = push.get("endpoints", push)`: fall back to the whole
    // notify_push object when the `endpoints` key is absent.
    let endpoints = match push.get("endpoints") {
        Some(Value::Object(endpoints)) => endpoints,
        Some(_) => return Err("notify_push endpoints is not an object.".to_string()),
        None => push,
    };
    let websocket = match endpoints.get("websocket") {
        Some(Value::String(url)) if !url.is_empty() => url.clone(),
        _ => return Ok(None),
    };
    let pre_auth = match endpoints.get("pre_auth") {
        Some(Value::String(url)) => Some(url.clone()),
        _ => None,
    };
    Ok(Some(PushEndpoints {
        websocket,
        pre_auth,
    }))
}

/// Validate the transport of a push WebSocket URL against the server URL.
///
/// Mirrors `push_protocol.validate_push_transport`: the WebSocket scheme must
/// be `ws`/`wss` and a secure server must never downgrade to `ws`.
pub fn validate_push_transport(server_url: &str, websocket_url: &str) -> Result<(), String> {
    let server_scheme = parse_url(server_url)?.scheme;
    // A server-supplied URL with an unsupported scheme is "invalid push URL"
    // (the Python original checks the scheme before anything else).
    let websocket_scheme = match parse_url(websocket_url) {
        Ok(parts) => parts.scheme,
        Err(_) => {
            return Err("The server returned an invalid push WebSocket URL.".to_string());
        }
    };
    if !matches!(websocket_scheme.as_str(), "ws" | "wss") {
        return Err("The server returned an invalid push WebSocket URL.".to_string());
    }
    if server_scheme == "https" && websocket_scheme != "wss" {
        return Err(
            "Refusing to downgrade secure Nextcloud push to an insecure WebSocket.".to_string(),
        );
    }
    Ok(())
}

/// Build the HTTP Basic `Authorization` header value.
pub fn basic_authorization(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        data_encoding::BASE64.encode(format!("{username}:{password}").as_bytes())
    )
}

/// The RFC 6455 answer for a `Sec-WebSocket-Key`.
///
/// `base64(SHA-1(key || "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))`. Delegates to
/// tungstenite's handshake helper so both validations share one implementation.
pub fn websocket_accept_key(key: &str) -> String {
    tungstenite::handshake::derive_accept_key(key.as_bytes())
}

/// A parsed HTTP/1.x response head (status line + headers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpHead {
    /// Status code (e.g. `101`, `200`).
    pub status: u16,
    /// Header lines as `(lowercased name, value)` pairs, in order.
    pub headers: Vec<(String, String)>,
}

/// Parse an HTTP response head, tolerating malformed header lines.
///
/// Lines without a `:` separator are ignored instead of failing the parse
/// (the strict parser in tungstenite's handshake would reject them). Used both
/// for WebSocket handshake responses and for plain HTTP responses.
pub fn parse_http_head(head: &[u8]) -> Result<HttpHead, String> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().ok_or("empty HTTP response.")?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or("malformed HTTP status line.")?
        .parse::<u16>()
        .map_err(|_| "malformed HTTP status code.".to_string())?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some(index) = line.find(':') {
            let name = line[..index].trim().to_ascii_lowercase();
            let value = line[index + 1..].trim().to_string();
            headers.push((name, value));
        }
    }
    Ok(HttpHead { status, headers })
}

/// All values of the header named `name` (lowercased).
pub fn header_values<'a>(head: &'a HttpHead, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    head.headers
        .iter()
        .filter(move |(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// Validate a WebSocket handshake response against the request key.
///
/// Only three things are checked (RFC 6455 + the openresty tolerance): the
/// status must be `101`, one token of the `Upgrade` header must equal
/// `websocket` (case-insensitive, ignoring `h2,h2c` and friends), and the
/// `Sec-WebSocket-Accept` header must match the SHA-1 answer. Everything else
/// is ignored.
pub fn verify_websocket_handshake(head: &HttpHead, key: &str) -> Result<(), String> {
    if head.status != 101 {
        return Err(format!(
            "WebSocket handshake returned HTTP {}.",
            head.status
        ));
    }
    let upgrade = header_values(head, "upgrade")
        .flat_map(|value| value.split([',', ' ', '\t']))
        .any(|token| token.eq_ignore_ascii_case("websocket"));
    if !upgrade {
        return Err("The WebSocket response lacks the Upgrade: websocket header.".to_string());
    }
    let expected = websocket_accept_key(key);
    let accept_matches = header_values(head, "sec-websocket-accept").any(|value| value == expected);
    if !accept_matches {
        return Err(
            "The WebSocket response has an invalid Sec-WebSocket-Accept header.".to_string(),
        );
    }
    Ok(())
}

/// Split an absolute `ws`/`wss`/`http`/`https` URL into its parts.
pub fn parse_url(url: &str) -> Result<UrlParts, String> {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => return Err(format!("Invalid URL (missing scheme): {url}")),
    };
    if !matches!(scheme.as_str(), "ws" | "wss" | "http" | "https") {
        return Err(format!("Unsupported URL scheme: {scheme}"));
    }
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], rest[index..].to_string()),
        None => (rest, "/".to_string()),
    };
    let authority = match authority.rfind('@') {
        Some(index) => &authority[index + 1..],
        None => authority,
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port))
            if !host.is_empty() && !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) =>
        {
            let port = port
                .parse::<u16>()
                .map_err(|_| format!("Invalid port in URL: {url}"))?;
            (strip_ipv6_brackets(host), Some(port))
        }
        _ => (strip_ipv6_brackets(authority), None),
    };
    if host.is_empty() {
        return Err(format!("Invalid URL (empty host): {url}"));
    }
    let default_port = match scheme.as_str() {
        "ws" | "http" => 80,
        "wss" | "https" => 443,
        _ => unreachable!("scheme checked above"),
    };
    Ok(UrlParts {
        scheme,
        host,
        port: port.unwrap_or(default_port),
        path,
    })
}

/// Parts of a parsed absolute URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts {
    /// Lowercased scheme (`ws`, `wss`, `http`, `https`).
    pub scheme: String,
    /// Host without userinfo or IPv6 brackets.
    pub host: String,
    /// Effective port (explicit or the scheme default).
    pub port: u16,
    /// Request path (always at least `/`).
    pub path: String,
}

/// Strip surrounding brackets from an IPv6 literal (`[::1]` → `::1`).
fn strip_ipv6_brackets(host: &str) -> String {
    if host.starts_with('[') && host.ends_with(']') {
        host[1..host.len() - 1].to_string()
    } else {
        host.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- parse_push_capability ----------------------------------------------

    #[test]
    fn parses_websocket_and_pre_auth_endpoints() {
        let payload = json!({
            "ocs": { "data": { "capabilities": {
                "notify_push": {
                    "endpoints": {
                        "websocket": "wss://cloud.example.com/apps/notify_push/",
                        "pre_auth": "https://cloud.example.com/apps/notify_push/pre_auth"
                    }
                }
            } } }
        });
        let endpoints = parse_push_capability(&payload).unwrap().expect("endpoints");
        assert_eq!(
            endpoints.websocket,
            "wss://cloud.example.com/apps/notify_push/"
        );
        assert_eq!(
            endpoints.pre_auth.as_deref(),
            Some("https://cloud.example.com/apps/notify_push/pre_auth")
        );
    }

    #[test]
    fn endpoints_may_live_at_the_notify_push_root() {
        let payload = json!({
            "ocs": { "data": { "capabilities": {
                "notify_push": {
                    "websocket": "wss://cloud.example.com/push/",
                    "pre_auth": "https://cloud.example.com/push/pre"
                }
            } } }
        });
        let endpoints = parse_push_capability(&payload).unwrap().expect("endpoints");
        assert_eq!(endpoints.websocket, "wss://cloud.example.com/push/");
        assert_eq!(
            endpoints.pre_auth.as_deref(),
            Some("https://cloud.example.com/push/pre")
        );
    }

    #[test]
    fn missing_notify_push_capability_is_none() {
        assert_eq!(parse_push_capability(&json!({})).unwrap(), None);
        assert_eq!(parse_push_capability(&json!({ "ocs": {} })).unwrap(), None);
        assert_eq!(
            parse_push_capability(&json!({ "ocs": { "data": { "capabilities": {} } } })).unwrap(),
            None
        );
        assert_eq!(
            parse_push_capability(&json!({ "ocs": { "data": { "capabilities": {
                "files": { "version": "1.0.0" }
            } } } }))
            .unwrap(),
            None
        );
    }

    #[test]
    fn empty_or_missing_websocket_is_none() {
        for payload in [
            json!({ "ocs": { "data": { "capabilities": { "notify_push": {} } } } }),
            json!({ "ocs": { "data": { "capabilities": { "notify_push": { "websocket": "" } } } } }),
            json!({ "ocs": { "data": { "capabilities": { "notify_push": { "endpoints": {} } } } } }),
        ] {
            assert_eq!(
                parse_push_capability(&payload).unwrap(),
                None,
                "payload: {payload}"
            );
        }
    }

    #[test]
    fn malformed_endpoints_is_an_error() {
        let payload = json!({
            "ocs": { "data": { "capabilities": { "notify_push": { "endpoints": "nope" } } } }
        });
        assert!(parse_push_capability(&payload).is_err());
    }

    #[test]
    fn pre_auth_only_accepted_when_a_string() {
        let payload = json!({
            "ocs": { "data": { "capabilities": {
                "notify_push": { "websocket": "wss://x/", "pre_auth": 42 }
            } } }
        });
        let endpoints = parse_push_capability(&payload).unwrap().expect("endpoints");
        assert_eq!(endpoints.pre_auth, None);
    }

    // ---- validate_push_transport --------------------------------------------

    #[test]
    fn rejects_downgrade_from_https_to_ws() {
        let err =
            validate_push_transport("https://cloud.example.com", "ws://cloud.example.com/push")
                .unwrap_err();
        assert!(err.contains("Refusing to downgrade"));
    }

    #[test]
    fn accepts_wss_over_https_and_ws_over_http() {
        validate_push_transport("https://cloud.example.com", "wss://cloud.example.com/push")
            .unwrap();
        validate_push_transport("http://cloud.example.com", "ws://cloud.example.com/push").unwrap();
        validate_push_transport("http://cloud.example.com", "wss://cloud.example.com/push")
            .unwrap();
    }

    #[test]
    fn rejects_non_websocket_schemes() {
        for bad in [
            "ftp://cloud.example.com/push",
            "tcp://cloud.example.com/push",
        ] {
            let err = validate_push_transport("https://cloud.example.com", bad).unwrap_err();
            assert!(err.contains("invalid push WebSocket URL"), "url: {bad}");
        }
    }

    // ---- basic_authorization ------------------------------------------------

    #[test]
    fn basic_authorization_encodes_user_password() {
        assert_eq!(basic_authorization("user", "pass"), "Basic dXNlcjpwYXNz");
    }

    // ---- RFC 6455 accept key ------------------------------------------------

    #[test]
    fn accept_key_matches_the_rfc_6455_example() {
        // Section 1.3: the request key "dGhlIHNhbXBsZSBub25jZQ==" (the sample
        // nonce "The sample nonce") must produce this exact accept value.
        assert_eq!(
            websocket_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    // ---- tolerant handshake parsing -----------------------------------------

    #[test]
    fn handshake_head_parses_status_and_headers() {
        let head = parse_http_head(
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
        )
        .unwrap();
        assert_eq!(head.status, 101);
        assert_eq!(
            header_values(&head, "upgrade").collect::<Vec<_>>(),
            vec!["websocket"]
        );
    }

    #[test]
    fn handshake_head_tolerates_malformed_lines() {
        let head = parse_http_head(
            b"HTTP/1.1 101 Switching Protocols\r\nBroken header without colon\r\nUpgrade: websocket\r\n\r\n",
        )
        .unwrap();
        assert_eq!(head.status, 101);
        assert_eq!(
            header_values(&head, "upgrade").collect::<Vec<_>>(),
            vec!["websocket"]
        );
    }

    #[test]
    fn verify_accepts_comma_separated_upgrade_header() {
        // openresty prefixes extra upgrade tokens; the strict tungstenite
        // validation rejects the whole value, ours must not.
        let expected = websocket_accept_key("dGhlIHNhbXBsZSBub25jZQ==");
        let head = HttpHead {
            status: 101,
            headers: vec![
                ("upgrade".to_string(), "h2,h2c, websocket".to_string()),
                ("connection".to_string(), "Upgrade".to_string()),
                ("sec-websocket-accept".to_string(), expected.clone()),
            ],
        };
        verify_websocket_handshake(&head, "dGhlIHNhbXBsZSBub25jZQ==").unwrap();
    }

    #[test]
    fn verify_rejects_wrong_status_or_accept() {
        let expected = websocket_accept_key("dGhlIHNhbXBsZSBub25jZQ==");
        let wrong_status = HttpHead {
            status: 200,
            headers: vec![
                ("upgrade".to_string(), "websocket".to_string()),
                ("sec-websocket-accept".to_string(), expected.clone()),
            ],
        };
        assert!(verify_websocket_handshake(&wrong_status, "dGhlIHNhbXBsZSBub25jZQ==").is_err());

        let wrong_accept = HttpHead {
            status: 101,
            headers: vec![
                ("upgrade".to_string(), "websocket".to_string()),
                (
                    "sec-websocket-accept".to_string(),
                    "dG9wIHNlY3JldA==".to_string(),
                ),
            ],
        };
        assert!(verify_websocket_handshake(&wrong_accept, "dGhlIHNhbXBsZSBub25jZQ==").is_err());
    }

    #[test]
    fn verify_rejects_missing_upgrade_token() {
        let head = HttpHead {
            status: 101,
            headers: vec![
                ("upgrade".to_string(), "h2,h2c".to_string()),
                (
                    "sec-websocket-accept".to_string(),
                    websocket_accept_key("x"),
                ),
            ],
        };
        assert!(verify_websocket_handshake(&head, "x").is_err());
    }

    // ---- URL parsing ---------------------------------------------------------

    #[test]
    fn url_parts_default_ports_and_paths() {
        assert_eq!(
            parse_url("wss://cloud.example.com/apps/notify_push/").unwrap(),
            UrlParts {
                scheme: "wss".into(),
                host: "cloud.example.com".into(),
                port: 443,
                path: "/apps/notify_push/".into()
            }
        );
        assert_eq!(
            parse_url("ws://localhost:8080/push").unwrap(),
            UrlParts {
                scheme: "ws".into(),
                host: "localhost".into(),
                port: 8080,
                path: "/push".into()
            }
        );
        assert_eq!(
            parse_url("https://cloud.example.com").unwrap(),
            UrlParts {
                scheme: "https".into(),
                host: "cloud.example.com".into(),
                port: 443,
                path: "/".into()
            }
        );
    }

    #[test]
    fn url_parts_handle_ipv6_and_userinfo() {
        assert_eq!(parse_url("wss://user@[::1]:9443/push").unwrap().host, "::1");
        assert_eq!(parse_url("wss://user@[::1]:9443/push").unwrap().port, 9443);
    }

    #[test]
    fn url_parts_reject_bad_inputs() {
        assert!(parse_url("cloud.example.com/push").is_err());
        assert!(parse_url("ftp://cloud.example.com/push").is_err());
        assert!(parse_url("wss:///missing-host").is_err());
    }
}
