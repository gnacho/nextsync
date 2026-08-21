//! notify_push WebSocket client.
//!
//! Fase 4 (Task 4.1). A Rust port of the Python `NotifyPushClient`
//! (`nextcloud/push.py`): it discovers the notify_push endpoints over OCS,
//! authenticates (pre-auth token or app password), keeps the WebSocket alive
//! and reports file-change hints, `notify_notification` hints (issue #31) and
//! a [`PushState`] machine.
//!
//! Threading: the GLib main thread is never blocked. Every connect attempt
//! spawns one blocking worker thread (`std::thread` + an `async_channel`; the
//! worker performs the HTTP/TLS/WebSocket I/O with a 500 ms read timeout so a
//! disconnect is observed within a second). Events travel over the channel and
//! are consumed by a `glib::spawn_future_local` loop on the main context,
//! which applies the state machine, the exponential backoff timer
//! (`glib::timeout_add_local`) and the callbacks. Generations replicate the
//! Python `_generation` guard: every connect/disconnect bumps the counter and
//! events from a stale attempt are discarded.
//!
//! TLS follows the plan decision: `rustls` with the system root certificates
//! (`rustls-native-certs`). The WebSocket handshake is done manually and
//! *tolerantly* (see [`push_protocol`]) so servers behind openresty that send
//! `Upgrade: h2,h2c, websocket` are accepted.
//!
//! Provider duality: only `Provider::Nextcloud` accounts get a push channel.
//! `configure` silently disables push for `Provider::OpenCloud` (which has no
//! notify_push), so an OpenCloud account never attempts to connect.

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serde_json::Value;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, Message, Role, WebSocket, WebSocketConfig};
use tungstenite::stream::MaybeTlsStream;

use crate::nextcloud::driver::Provider;
use crate::nextcloud::push_protocol::{
    basic_authorization, header_values, parse_http_head, parse_push_capability, parse_url,
    validate_push_transport, verify_websocket_handshake, HttpHead, PushEndpoints,
};
use crate::state::PushState;

/// The stream a push connection runs over: plain TCP or rustls TLS.
type PushStream = MaybeTlsStream<TcpStream>;

/// Reconnect delays in seconds, mirroring `NotifyPushClient.BACKOFF_SECONDS`.
pub const BACKOFF_SECONDS: [u64; 6] = [2, 5, 10, 30, 60, 300];

/// The backoff delay for a given failed-attempt index (capped at the last
/// value, exactly like `BACKOFF_SECONDS[min(index, len - 1)]` in Python).
pub fn backoff_seconds(backoff_index: usize) -> u64 {
    BACKOFF_SECONDS[backoff_index.min(BACKOFF_SECONDS.len() - 1)]
}

/// Whether the provider exposes a push channel at all.
///
/// Only Nextcloud has notify_push; OpenCloud accounts rely on remote-interval
/// polling and must never start a push worker.
pub fn remote_push_supported(provider: Provider) -> bool {
    matches!(provider, Provider::Nextcloud)
}

/// How the WebSocket authenticated (used to decide the password fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    PreAuth,
    Password,
}

/// An event sent by a worker thread to the main context.
#[derive(Debug)]
struct PushEvent {
    /// The connect attempt this event belongs to (stale events are dropped).
    generation: u64,
    kind: PushEventKind,
}

#[derive(Debug)]
enum PushEventKind {
    /// Discovery/pre-auth/handshake failed; the main thread reconnects.
    DiscoverFailed(String),
    /// The server rejected the credentials (401/403 or a text message).
    AuthRequired,
    /// The server does not advertise notify_push.
    Unsupported,
    /// The WebSocket sent `authenticated`.
    Authenticated,
    /// The WebSocket sent a `notify_file*` hint.
    FileNotification,
    /// The WebSocket sent a `notify_notification` hint (new server
    /// notification, issue #31).
    Notification,
    /// The connection closed without an intentional close.
    Closed {
        reason: String,
        auth_mode: AuthMode,
        authenticated: bool,
    },
}

/// Handle for one running worker thread.
struct PushConnection {
    /// Flip to `true` to make the worker stop within one read timeout.
    stop: Arc<AtomicBool>,
}

/// Shared state of a [`NotifyPushClient`] (all main-thread only).
struct PushInner {
    provider: Provider,
    server: String,
    username: String,
    password: String,
    enabled: bool,
    online: bool,
    generation: u64,
    backoff_index: usize,
    force_password_auth: bool,
    connection: Option<PushConnection>,
    reconnect: Option<glib::JoinHandle<()>>,
    tls: Option<Arc<ClientConfig>>,
    backoff_scale: f64,
    on_file_notification: Rc<dyn Fn()>,
    on_notification: Rc<dyn Fn()>,
    on_state: Rc<dyn Fn(PushState, String)>,
}

/// Client for the Nextcloud notify_push WebSocket protocol.
///
/// Cloneable (the clone shares the same state and callbacks). All methods are
/// meant to be called from the main thread.
#[derive(Clone)]
pub struct NotifyPushClient {
    inner: Rc<RefCell<PushInner>>,
}

impl NotifyPushClient {
    /// Create a client for an account bound to `provider`.
    ///
    /// `on_file_notification` fires on every remote file hint, `on_notification`
    /// on every `notify_notification` server hint, `on_state` on every
    /// [`PushState`] transition (message included). All three run on the main
    /// thread.
    pub fn new(
        provider: Provider,
        on_file_notification: impl Fn() + 'static,
        on_notification: impl Fn() + 'static,
        on_state: impl Fn(PushState, String) + 'static,
    ) -> Self {
        Self {
            inner: Rc::new(RefCell::new(PushInner {
                provider,
                server: String::new(),
                username: String::new(),
                password: String::new(),
                enabled: false,
                online: true,
                generation: 0,
                backoff_index: 0,
                force_password_auth: false,
                connection: None,
                reconnect: None,
                tls: None,
                backoff_scale: 1.0,
                on_file_notification: Rc::new(on_file_notification),
                on_notification: Rc::new(on_notification),
                on_state: Rc::new(on_state),
            })),
        }
    }

    /// Configure the account and (re)start the push channel as needed.
    ///
    /// Mirrors `configure`: a change in server/user/enabled disconnects first;
    /// push only starts when enabled *and* online. OpenCloud accounts are
    /// always treated as disabled.
    pub fn configure(&self, server: &str, username: &str, password: &str, enabled: bool) {
        let effective_enabled = enabled && remote_push_supported(self.inner.borrow().provider);
        let changed = {
            let mut inner = self.inner.borrow_mut();
            // The password participates in the change detection (issue #133):
            // after a re-authentication the worker must pick up the new
            // secret immediately, not keep the one captured at startup.
            let changed = (server, username, password, effective_enabled)
                != (
                    inner.server.as_str(),
                    inner.username.as_str(),
                    inner.password.as_str(),
                    inner.enabled,
                );
            inner.server = server.to_string();
            inner.username = username.to_string();
            inner.password = password.to_string();
            inner.enabled = effective_enabled;
            changed
        };
        if changed {
            self.disconnect(false);
        }
        if !effective_enabled {
            self.on_state(PushState::Disabled, "Push notifications are disabled.");
        } else if self.inner.borrow().online {
            self.connect();
        }
    }

    /// Reflect the network status; disconnects (keeping the config) when
    /// going offline and reconnects when back online.
    pub fn set_online(&self, online: bool) {
        self.inner.borrow_mut().online = online;
        if !online {
            self.disconnect(true);
        } else if self.inner.borrow().enabled {
            self.connect();
        }
    }

    /// Start a connect attempt (discovery + auth + WebSocket) unless one is
    /// already running or push is not enabled/online.
    pub fn connect(&self) {
        let (generation, server, username, password, force_password_auth, tls) = {
            let mut inner = self.inner.borrow_mut();
            if !inner.enabled || !inner.online || inner.connection.is_some() {
                return;
            }
            inner.generation += 1;
            let generation = inner.generation;
            inner.cancel_reconnect();
            if inner.tls.is_none() {
                inner.tls = Some(build_tls_config());
            }
            (
                generation,
                inner.server.clone(),
                inner.username.clone(),
                inner.password.clone(),
                inner.force_password_auth,
                inner.tls.clone().expect("tls built above"),
            )
        };
        self.on_state(PushState::Connecting, "Discovering server push support…");
        self.spawn_worker(
            generation,
            server,
            username,
            password,
            force_password_auth,
            tls,
        );
    }

    /// Close the push channel. `keep_enabled` keeps the configuration (and the
    /// DISABLED state emission) while still stopping the worker.
    pub fn disconnect(&self, keep_enabled: bool) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.generation += 1;
            inner.cancel_reconnect();
            if let Some(connection) = inner.connection.take() {
                connection.stop.store(true, Ordering::SeqCst);
            }
            inner.force_password_auth = false;
        }
        if !keep_enabled {
            self.on_state(PushState::Disabled, "Disconnected");
        }
    }

    fn spawn_worker(
        &self,
        generation: u64,
        server: String,
        username: String,
        password: String,
        force_password_auth: bool,
        tls: Arc<ClientConfig>,
    ) {
        let (tx, rx) = async_channel::unbounded::<PushEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            push_worker_main(WorkerInputs {
                generation,
                server,
                username,
                password,
                force_password_auth,
                tls,
                stop: worker_stop,
                tx,
            });
        });
        self.inner.borrow_mut().connection = Some(PushConnection { stop });
        self.spawn_consumer(rx);
    }

    /// Drain worker events on the main context, discarding stale generations.
    fn spawn_consumer(&self, rx: async_channel::Receiver<PushEvent>) {
        let client = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                if event.generation != client.inner.borrow().generation {
                    continue;
                }
                client.handle_event(event);
            }
        });
    }

    fn handle_event(&self, event: PushEvent) {
        match event.kind {
            PushEventKind::DiscoverFailed(reason) => {
                self.clear_connection();
                self.schedule_reconnect(reason);
            }
            PushEventKind::AuthRequired => {
                self.clear_connection();
                self.on_state(PushState::AuthRequired, "Push authentication failed.");
            }
            PushEventKind::Unsupported => {
                self.clear_connection();
                self.on_state(
                    PushState::Unsupported,
                    "This server does not offer notify_push.",
                );
            }
            PushEventKind::Authenticated => {
                let mut inner = self.inner.borrow_mut();
                inner.force_password_auth = false;
                inner.backoff_index = 0;
                drop(inner);
                self.on_state(PushState::Connected, "Connected");
            }
            PushEventKind::FileNotification => {
                let callback = self.inner.borrow().on_file_notification.clone();
                callback();
            }
            PushEventKind::Notification => {
                let callback = self.inner.borrow().on_notification.clone();
                callback();
            }
            PushEventKind::Closed {
                reason,
                auth_mode,
                authenticated,
            } => {
                self.clear_connection();
                let details = {
                    let mut inner = self.inner.borrow_mut();
                    if !inner.enabled || !inner.online {
                        None
                    } else {
                        let mut details = reason;
                        if !authenticated && auth_mode == AuthMode::PreAuth {
                            // The server closed before authenticating with a
                            // pre-auth token: retry with the app password.
                            inner.force_password_auth = true;
                            details.push_str(" Retrying with direct app-password authentication.");
                        } else if !authenticated && auth_mode == AuthMode::Password {
                            inner.force_password_auth = false;
                        }
                        Some(details)
                    }
                };
                if let Some(details) = details {
                    self.schedule_reconnect(details);
                }
            }
        }
    }

    fn schedule_reconnect(&self, _reason: String) {
        let (delay, scale) = {
            let mut inner = self.inner.borrow_mut();
            if !inner.enabled || !inner.online {
                return;
            }
            let delay = backoff_seconds(inner.backoff_index);
            inner.backoff_index += 1;
            (delay, inner.backoff_scale)
        };
        self.on_state(
            PushState::Reconnecting,
            format!("Reconnecting in {delay} seconds"),
        );
        self.cancel_reconnect();
        let client = self.clone();
        let context = glib::MainContext::ref_thread_default();
        let handle = context.spawn_local(async move {
            glib::timeout_future(Duration::from_millis(
                (delay as f64 * 1000.0 * scale) as u64,
            ))
            .await;
            client.reconnect();
        });
        self.inner.borrow_mut().reconnect = Some(handle);
    }

    fn reconnect(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.reconnect = None;
        inner.connection = None;
        drop(inner);
        self.connect();
    }

    fn cancel_reconnect(&self) {
        self.inner.borrow_mut().cancel_reconnect();
    }

    fn clear_connection(&self) {
        self.inner.borrow_mut().connection = None;
    }

    fn on_state(&self, state: PushState, message: impl Into<String>) {
        let callback = self.inner.borrow().on_state.clone();
        callback(state, message.into());
    }

    #[cfg(test)]
    fn set_backoff_scale(&self, scale: f64) {
        self.inner.borrow_mut().backoff_scale = scale;
    }
}

impl PushInner {
    fn cancel_reconnect(&mut self) {
        if let Some(handle) = self.reconnect.take() {
            handle.abort();
        }
    }
}

/// Build a `rustls` client configuration trusting the system roots.
fn build_tls_config() -> Arc<ClientConfig> {
    let mut root_store = RootCertStore::empty();
    let certificates = rustls_native_certs::load_native_certs();
    root_store.add_parsable_certificates(certificates.certs);
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
    )
}

/// Everything a worker thread needs to run one connect attempt.
struct WorkerInputs {
    generation: u64,
    server: String,
    username: String,
    password: String,
    force_password_auth: bool,
    tls: Arc<ClientConfig>,
    stop: Arc<AtomicBool>,
    tx: async_channel::Sender<PushEvent>,
}

/// The full connect pipeline of one worker: discovery, pre-auth, handshake,
/// authentication and the message loop. Runs on its own thread.
fn push_worker_main(inputs: WorkerInputs) {
    let WorkerInputs {
        generation,
        server,
        username,
        password,
        force_password_auth,
        tls,
        stop,
        tx,
    } = inputs;
    let capabilities_url = format!(
        "{}/ocs/v2.php/cloud/capabilities?format=json",
        server.trim_end_matches('/')
    );
    let authorization = basic_authorization(&username, &password);
    let headers = [
        ("Accept", "application/json"),
        ("OCS-APIREQUEST", "true"),
        ("Authorization", authorization.as_str()),
    ];

    let response = match http_request(&capabilities_url, "GET", &headers, &tls, &stop) {
        Ok(response) => response,
        Err(reason) => {
            let _ = tx.send_blocking(PushEvent {
                generation,
                kind: PushEventKind::DiscoverFailed(reason),
            });
            return;
        }
    };
    if response.status == 401 || response.status == 403 {
        let _ = tx.send_blocking(PushEvent {
            generation,
            kind: PushEventKind::AuthRequired,
        });
        return;
    }
    if !(200..300).contains(&response.status) {
        let _ = tx.send_blocking(PushEvent {
            generation,
            kind: PushEventKind::DiscoverFailed(format!(
                "Capabilities returned HTTP {}.",
                response.status
            )),
        });
        return;
    }
    let endpoints = match parse_capabilities(&response.body) {
        Ok(Some(endpoints)) => endpoints,
        Ok(None) => {
            let _ = tx.send_blocking(PushEvent {
                generation,
                kind: PushEventKind::Unsupported,
            });
            return;
        }
        Err(reason) => {
            let _ = tx.send_blocking(PushEvent {
                generation,
                kind: PushEventKind::DiscoverFailed(reason),
            });
            return;
        }
    };
    if let Err(reason) = validate_push_transport(&server, &endpoints.websocket) {
        let _ = tx.send_blocking(PushEvent {
            generation,
            kind: PushEventKind::DiscoverFailed(reason),
        });
        return;
    }

    let mut pre_auth_token: Option<String> = None;
    if let Some(pre_auth_url) = &endpoints.pre_auth {
        if !force_password_auth {
            match http_request(pre_auth_url, "POST", &headers, &tls, &stop) {
                Ok(response) if (200..300).contains(&response.status) => {
                    pre_auth_token = parse_pre_auth_token(&response.body);
                }
                Ok(_) | Err(_) => {}
            }
        }
    }
    let auth_mode = if pre_auth_token.is_some() {
        AuthMode::PreAuth
    } else {
        AuthMode::Password
    };

    let mut websocket = match open_websocket(&endpoints.websocket, &tls, &stop) {
        Ok(websocket) => websocket,
        Err(reason) => {
            let _ = tx.send_blocking(PushEvent {
                generation,
                kind: PushEventKind::DiscoverFailed(reason),
            });
            return;
        }
    };

    let send_auth = match &pre_auth_token {
        Some(token) => websocket.send(Message::text(token.clone())),
        None => {
            let first = websocket.send(Message::text(username.clone()));
            let second = websocket.send(Message::text(password.clone()));
            first.and(second)
        }
    };
    if let Err(error) = send_auth {
        let _ = tx.send_blocking(PushEvent {
            generation,
            kind: PushEventKind::DiscoverFailed(format!(
                "Failed to send push credentials: {error}"
            )),
        });
        return;
    }

    let mut authenticated = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            graceful_close(&mut websocket);
            return;
        }
        match websocket.read() {
            Ok(Message::Text(text)) => {
                let text = text.as_str().trim();
                if text == "authenticated" {
                    authenticated = true;
                    let _ = tx.send_blocking(PushEvent {
                        generation,
                        kind: PushEventKind::Authenticated,
                    });
                } else if text == "notify_file"
                    || text == "notify_file_id"
                    || text.starts_with("notify_file_id ")
                {
                    let _ = tx.send_blocking(PushEvent {
                        generation,
                        kind: PushEventKind::FileNotification,
                    });
                } else if text == "notify_notification" {
                    let _ = tx.send_blocking(PushEvent {
                        generation,
                        kind: PushEventKind::Notification,
                    });
                } else if text == "invalid credentials" || text == "authentication failed" {
                    let _ = tx.send_blocking(PushEvent {
                        generation,
                        kind: PushEventKind::AuthRequired,
                    });
                }
            }
            Ok(Message::Ping(_)) => {
                let _ = websocket.flush();
            }
            Ok(Message::Pong(_)) | Ok(Message::Frame(_)) | Ok(Message::Binary(_)) => {}
            Ok(Message::Close(_)) => break,
            Err(tungstenite::Error::ConnectionClosed) => break,
            Err(tungstenite::Error::Io(error)) if is_would_block(&error) => continue,
            Err(error) => {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let _ = tx.send_blocking(PushEvent {
                    generation,
                    kind: PushEventKind::Closed {
                        reason: format!("Push connection closed with error: {error}."),
                        auth_mode,
                        authenticated,
                    },
                });
                return;
            }
        }
    }
    if stop.load(Ordering::SeqCst) {
        return;
    }
    let _ = tx.send_blocking(PushEvent {
        generation,
        kind: PushEventKind::Closed {
            reason: "Push connection closed.".to_string(),
            auth_mode,
            authenticated,
        },
    });
}

/// Perform the tolerant WebSocket handshake on a fresh connection.
fn open_websocket(
    url: &str,
    tls: &Arc<ClientConfig>,
    stop: &AtomicBool,
) -> Result<WebSocket<PushStream>, String> {
    let parts = parse_url(url)?;
    let mut stream = connect_stream(&parts, tls, stop)?;
    let key = generate_websocket_key();
    let request = build_handshake_request(&parts.path, &parts.host, parts.port, &key);
    write_stream(&mut stream, request.as_bytes())?;
    let (head, tail) = read_until_headers_done(&mut stream, stop)?;
    let head = parse_http_head(&head)?;
    verify_websocket_handshake(&head, &key)?;
    let websocket = WebSocket::from_partially_read(
        stream,
        tail,
        Role::Client,
        Some(WebSocketConfig::default()),
    );
    Ok(websocket)
}

/// Send a close frame (best effort) before shutting the connection down.
fn graceful_close(websocket: &mut WebSocket<PushStream>) {
    let _ = websocket.close(Some(CloseFrame {
        code: CloseCode::Normal,
        reason: "Application state changed".into(),
    }));
    let _ = websocket.flush();
}

/// Generate a random `Sec-WebSocket-Key` (base64 of 16 random bytes, RFC 6455).
fn generate_websocket_key() -> String {
    let nonce: [u8; 16] = rand::random();
    data_encoding::BASE64.encode(&nonce)
}

fn parse_capabilities(body: &[u8]) -> Result<Option<PushEndpoints>, String> {
    let payload: Value =
        serde_json::from_slice(body).map_err(|e| format!("Invalid capabilities response: {e}"))?;
    parse_push_capability(&payload)
}

/// Extract the pre-auth token from the response body (plain text or JSON).
///
/// Mirrors `_request_pre_auth`: a JSON body yields `token` or
/// `ocs.data.token`; any other text is used verbatim.
fn parse_pre_auth_token(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body).trim().to_string();
    if text.starts_with('{') {
        let Ok(data) = serde_json::from_str::<Value>(&text) else {
            return None;
        };
        return json_token(&data);
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Read `token` (or `ocs.data.token`) out of a parsed JSON payload.
fn json_token(data: &Value) -> Option<String> {
    let top = data
        .get("token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if top.is_some() {
        return top;
    }
    data.get("ocs")
        .and_then(|ocs| ocs.get("data"))
        .and_then(|data| data.get("token"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Open a TCP (and TLS when `wss`) connection to the URL authority.
fn connect_stream(
    parts: &crate::nextcloud::push_protocol::UrlParts,
    tls: &Arc<ClientConfig>,
    stop: &AtomicBool,
) -> Result<PushStream, String> {
    let addrs = (parts.host.as_str(), parts.port)
        .to_socket_addrs()
        .map_err(|error| format!("Could not resolve {}: {error}", parts.host))?;
    let mut last_error = None;
    for addr in addrs {
        if stop.load(Ordering::SeqCst) {
            return Err("Push connection stopped.".to_string());
        }
        let tcp = match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
            Ok(tcp) => tcp,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        let _ = tcp.set_nodelay(true);
        let _ = tcp.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = tcp.set_write_timeout(Some(Duration::from_secs(30)));
        if parts.scheme == "wss" || parts.scheme == "https" {
            let server_name = ServerName::try_from(parts.host.clone())
                .map_err(|_| format!("Invalid TLS server name: {}", parts.host))?;
            let connection = ClientConnection::new(Arc::clone(tls), server_name)
                .map_err(|error| format!("TLS setup failed: {error}"))?;
            let mut stream = StreamOwned::new(connection, tcp);
            let _ = stream.sock.set_read_timeout(Some(Duration::from_secs(30)));
            stream
                .conn
                .complete_io(&mut stream.sock)
                .map_err(|error| format!("TLS handshake failed: {error}"))?;
            let _ = stream
                .sock
                .set_read_timeout(Some(Duration::from_millis(500)));
            return Ok(PushStream::Rustls(stream));
        }
        return Ok(PushStream::Plain(tcp));
    }
    Err(format!(
        "Could not connect to {}:{}: {}",
        parts.host,
        parts.port,
        last_error.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// A parsed HTTP response (status + decoded body).
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

/// Perform one HTTP/1.1 request over a fresh TLS/plain connection.
fn http_request(
    url: &str,
    method: &str,
    headers: &[(&str, &str)],
    tls: &Arc<ClientConfig>,
    stop: &AtomicBool,
) -> Result<HttpResponse, String> {
    let parts = parse_url(url)?;
    let mut stream = connect_stream(&parts, tls, stop)?;
    let request = build_http_request(method, &parts.path, &parts.host, parts.port, headers);
    write_stream(&mut stream, request.as_bytes())?;
    let (head, mut body) = read_until_headers_done(&mut stream, stop)?;
    let head = parse_http_head(&head)?;
    decode_body(&head, &mut body, &mut stream, stop)?;
    Ok(HttpResponse {
        status: head.status,
        body,
    })
}

/// Read the rest of an HTTP body according to the framing headers.
fn decode_body(
    head: &HttpHead,
    body: &mut Vec<u8>,
    stream: &mut PushStream,
    stop: &AtomicBool,
) -> Result<(), String> {
    if let Some(length) = header_values(head, "content-length")
        .next()
        .and_then(|value| value.trim().parse::<usize>().ok())
    {
        if body.len() < length {
            let mut pending = read_exact_with_stop(stream, length - body.len(), stop)?;
            body.append(&mut pending);
        }
        body.truncate(length);
        return Ok(());
    }
    let chunked = header_values(head, "transfer-encoding").any(|value| {
        value
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
    });
    if chunked {
        *body = decode_chunked(body, stream, stop)?;
    } else {
        let mut tail = read_until_close_with_stop(stream, stop)?;
        body.append(&mut tail);
    }
    Ok(())
}

/// Read until the header terminator, returning `(head, tail)` where `tail` is
/// any bytes already read past the blank line (they belong to the body or to
/// the WebSocket stream).
fn read_until_headers_done(
    stream: &mut impl Read,
    stop: &AtomicBool,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match read_retry(stream, &mut chunk, stop)? {
            0 => return Err("Connection closed before the response headers.".to_string()),
            n => {
                buffer.extend_from_slice(&chunk[..n]);
                if let Some(position) = find_subsequence(&buffer, b"\r\n\r\n") {
                    let tail = buffer.split_off(position + 4);
                    return Ok((buffer, tail));
                }
                if buffer.len() > 128 * 1024 {
                    return Err("Response headers too large.".to_string());
                }
            }
        }
    }
}

/// Read exactly `length` bytes, tolerating read timeouts while `stop` is off.
fn read_exact_with_stop(
    stream: &mut impl Read,
    length: usize,
    stop: &AtomicBool,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(length);
    let mut chunk = [0u8; 4096];
    while out.len() < length {
        match read_retry(stream, &mut chunk, stop)? {
            0 => return Err("Connection closed before the response body.".to_string()),
            n => out.extend_from_slice(&chunk[..n]),
        }
    }
    out.truncate(length);
    Ok(out)
}

/// Read until the peer closes the connection.
fn read_until_close_with_stop(
    stream: &mut impl Read,
    stop: &AtomicBool,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match read_retry(stream, &mut chunk, stop)? {
            0 => return Ok(out),
            n => out.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Read one chunk from the stream, retrying on timeouts until `stop`.
fn read_retry(
    stream: &mut impl Read,
    chunk: &mut [u8],
    stop: &AtomicBool,
) -> Result<usize, String> {
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err("Push connection stopped.".to_string());
        }
        match stream.read(chunk) {
            Ok(n) => return Ok(n),
            Err(error) if is_would_block(&error) => continue,
            Err(error) => return Err(format!("Read failed: {error}")),
        }
    }
}

/// Decode a chunked transfer-encoding body (`body` already holds the prefix).
fn decode_chunked(
    body: &mut Vec<u8>,
    stream: &mut impl Read,
    stop: &AtomicBool,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let size_line = take_line(body, stream, stop)?;
        let size = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| format!("Malformed chunk size: {size_line}"))?;
        if size == 0 {
            loop {
                let trailer = take_line(body, stream, stop)?;
                if trailer.is_empty() {
                    break;
                }
            }
            return Ok(out);
        }
        ensure_buffered(body, stream, size + 2, stop)?;
        out.extend_from_slice(&body[..size]);
        body.drain(..size + 2);
    }
}

/// Ensure `pending` holds at least `n` bytes, reading from `stream` as needed.
fn ensure_buffered(
    pending: &mut Vec<u8>,
    stream: &mut impl Read,
    n: usize,
    stop: &AtomicBool,
) -> Result<(), String> {
    while pending.len() < n {
        let mut chunk = [0u8; 4096];
        let got = read_retry(stream, &mut chunk, stop)?;
        if got == 0 {
            return Err("Connection closed inside a chunk.".to_string());
        }
        pending.extend_from_slice(&chunk[..got]);
    }
    Ok(())
}

/// Extract one `\r\n`-terminated line from `pending` (reading more if needed).
fn take_line(
    pending: &mut Vec<u8>,
    stream: &mut impl Read,
    stop: &AtomicBool,
) -> Result<String, String> {
    loop {
        if let Some(position) = find_subsequence(pending, b"\r\n") {
            let line: Vec<u8> = pending.drain(..position).collect();
            pending.drain(..2);
            return Ok(String::from_utf8_lossy(&line).into_owned());
        }
        ensure_buffered(pending, stream, 1, stop)?;
    }
}

fn write_stream(stream: &mut impl Write, data: &[u8]) -> Result<(), String> {
    stream
        .write_all(data)
        .map_err(|error| format!("Write failed: {error}"))
}

fn is_would_block(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock || error.kind() == io::ErrorKind::TimedOut
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Build the WebSocket upgrade request (RFC 6455 client opening handshake).
fn build_handshake_request(path: &str, host: &str, port: u16, key: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         \r\n",
        authority = host_authority(host, port),
    )
}

/// Build a plain HTTP/1.1 request (used for capabilities and pre-auth).
fn build_http_request(
    method: &str,
    path: &str,
    host: &str,
    port: u16,
    headers: &[(&str, &str)],
) -> String {
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n",
        authority = host_authority(host, port),
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request
}

/// The `Host` header value: the port is included only when non-default.
fn host_authority(host: &str, port: u16) -> String {
    let default = matches!(port, 80 | 443);
    if default {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::net::{Shutdown, SocketAddr, TcpListener};
    use std::sync::atomic::AtomicUsize;
    use std::thread::JoinHandle;
    use std::time::Instant;

    /// Serialize the GLib-pumping tests: `glib::timeout_add_local` briefly
    /// acquires the process-global default context, which is not re-entrant
    /// across threads.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    type TestStates = Rc<RefCell<Vec<(PushState, String)>>>;
    type TestNotifications = Rc<Cell<u32>>;

    fn test_tls_config() -> Arc<ClientConfig> {
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(RootCertStore::empty())
                .with_no_client_auth(),
        )
    }

    fn test_client(provider: Provider) -> (NotifyPushClient, TestStates, TestNotifications) {
        let states = Rc::new(RefCell::new(Vec::new()));
        let notifications = Rc::new(Cell::new(0));
        let states_clone = Rc::clone(&states);
        let notifications_clone = Rc::clone(&notifications);
        let client = NotifyPushClient::new(
            provider,
            move || {
                notifications_clone.set(notifications_clone.get() + 1);
            },
            || {},
            move |state, message| {
                states_clone.borrow_mut().push((state, message));
            },
        );
        (client, states, notifications)
    }

    // ---- backoff and provider support ----------------------------------------

    #[test]
    fn backoff_seconds_caps_at_the_maximum() {
        assert_eq!(backoff_seconds(0), 2);
        assert_eq!(backoff_seconds(1), 5);
        assert_eq!(backoff_seconds(2), 10);
        assert_eq!(backoff_seconds(3), 30);
        assert_eq!(backoff_seconds(4), 60);
        assert_eq!(backoff_seconds(5), 300);
        assert_eq!(backoff_seconds(6), 300);
        assert_eq!(backoff_seconds(100), 300);
    }

    #[test]
    fn remote_push_supported_only_for_nextcloud() {
        assert!(remote_push_supported(Provider::Nextcloud));
        assert!(!remote_push_supported(Provider::OpenCloud));
    }

    #[test]
    fn pre_auth_token_parses_text_and_json() {
        assert_eq!(
            parse_pre_auth_token(b"plain-token"),
            Some("plain-token".to_string())
        );
        assert_eq!(
            parse_pre_auth_token(br#"{"token":"json-token"}"#),
            Some("json-token".to_string())
        );
        assert_eq!(
            parse_pre_auth_token(br#"{"ocs":{"data":{"token":"nested-token"}}}"#),
            Some("nested-token".to_string())
        );
        assert_eq!(parse_pre_auth_token(b""), None);
        assert_eq!(parse_pre_auth_token(br#"{"broken"#), None);
        assert_eq!(
            parse_pre_auth_token(br#"{"ocs":{"data":{}}}"#),
            None,
            "a JSON body without a token yields None"
        );
    }

    // ---- facade state transitions (no network) --------------------------------

    #[test]
    fn configure_disabled_reports_disabled_state() {
        let (client, states, _notifications) = test_client(Provider::Nextcloud);
        client.configure("https://cloud.example.com", "alice", "secret", false);
        let states = states.borrow();
        assert!(states
            .iter()
            .all(|(state, _)| *state == PushState::Disabled));
        assert_eq!(states.last().unwrap().1, "Push notifications are disabled.");
    }

    #[test]
    fn configure_with_a_new_password_reconnects() {
        // Issue #133: a re-authentication changes the password; the push
        // worker must pick it up immediately. Each configure with a changed
        // password disconnects and starts a fresh connect attempt.
        let (client, states, _notifications) = test_client(Provider::Nextcloud);
        client.configure("https://cloud.example.com", "alice", "secret", true);
        client.configure("https://cloud.example.com", "alice", "new-secret", true);
        let states = states.borrow();
        let connects = states
            .iter()
            .filter(|(state, _)| matches!(state, PushState::Connecting))
            .count();
        assert!(
            connects >= 2,
            "a password change must reconnect (states: {states:?})"
        );
    }

    #[test]
    fn configure_with_the_same_password_does_not_reconnect() {
        // Same server/user/password/enabled → nothing changes, so no new
        // connect attempt beyond the first.
        let (client, states, _notifications) = test_client(Provider::Nextcloud);
        client.configure("https://cloud.example.com", "alice", "secret", true);
        client.configure("https://cloud.example.com", "alice", "secret", true);
        let states = states.borrow();
        let connects = states
            .iter()
            .filter(|(state, _)| matches!(state, PushState::Connecting))
            .count();
        assert_eq!(connects, 1, "an identical configure must not reconnect");
    }

    #[test]
    fn opencloud_account_never_attempts_to_connect() {
        let (client, states, notifications) = test_client(Provider::OpenCloud);
        client.configure("https://cloud.example.com", "alice", "secret", true);
        let states = states.borrow();
        assert!(
            !states.iter().any(|(state, _)| matches!(
                state,
                PushState::Connecting | PushState::Reconnecting | PushState::Connected
            )),
            "an OpenCloud account must not try to connect: {states:?}"
        );
        assert_eq!(notifications.get(), 0);
        assert_eq!(states.last().unwrap().0, PushState::Disabled);
    }

    // ---- tolerant handshake + message loop (direct worker, no GLib) ----------

    /// A tiny configurable Nextcloud push server over local TCP.
    struct FakePushServer {
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
        pre_auth_requests: Arc<AtomicUsize>,
        ws_connections: Arc<AtomicUsize>,
    }

    impl FakePushServer {
        fn start(upgrade_header: &str, close_before_auth: bool, send_notification: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake server");
            let addr = listener.local_addr().expect("local address");
            let stop = Arc::new(AtomicBool::new(false));
            let pre_auth_requests = Arc::new(AtomicUsize::new(0));
            let ws_connections = Arc::new(AtomicUsize::new(0));
            let handle = {
                let stop = Arc::clone(&stop);
                let pre_auth_requests = Arc::clone(&pre_auth_requests);
                let ws_connections = Arc::clone(&ws_connections);
                let upgrade_header = upgrade_header.to_string();
                let _ = listener.set_nonblocking(true);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((stream, _)) => handle_connection(
                                stream,
                                addr,
                                &upgrade_header,
                                close_before_auth,
                                send_notification,
                                &pre_auth_requests,
                                &ws_connections,
                            ),
                            Err(error) if is_would_block(&error) => {
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            Err(_) => break,
                        }
                    }
                })
            };
            Self {
                addr,
                stop,
                handle: Some(handle),
                pre_auth_requests,
                ws_connections,
            }
        }

        fn server_url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn pre_auth_requests(&self) -> usize {
            self.pre_auth_requests.load(Ordering::SeqCst)
        }

        fn ws_connections(&self) -> usize {
            self.ws_connections.load(Ordering::SeqCst)
        }
    }

    impl Drop for FakePushServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn handle_connection(
        mut stream: TcpStream,
        addr: SocketAddr,
        upgrade_header: &str,
        close_before_auth: bool,
        send_notification: bool,
        pre_auth_requests: &AtomicUsize,
        ws_connections: &AtomicUsize,
    ) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            match stream.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);
                    if let Some(position) = find_subsequence(&buffer, b"\r\n\r\n") {
                        break position + 4;
                    }
                }
                Err(error) if is_would_block(&error) => continue,
                Err(_) => return,
            }
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
        let tail = buffer[head_end..].to_vec();
        let path = head
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();

        if path.contains("capabilities") {
            let body = format!(
                r#"{{"ocs":{{"data":{{"capabilities":{{"notify_push":{{"endpoints":{{"websocket":"ws://{addr}/apps/notify_push/","pre_auth":"http://{addr}/apps/notify_push/pre_auth"}}}}}}}}}}}}"#
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            return;
        }
        if path.contains("pre_auth") {
            pre_auth_requests.fetch_add(1, Ordering::SeqCst);
            let body = r#"{"token":"sekrit-token"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            return;
        }
        if path.contains("notify_push") {
            ws_connections.fetch_add(1, Ordering::SeqCst);
            let key = head
                .lines()
                .find_map(|line| {
                    let colon = line.find(':')?;
                    let name = line[..colon].trim().to_ascii_lowercase();
                    if name == "sec-websocket-key" {
                        Some(line[colon + 1..].trim().to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let accept = crate::nextcloud::push_protocol::websocket_accept_key(&key);
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: {upgrade_header}\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            );
            if stream.write_all(response.as_bytes()).is_err() {
                return;
            }
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let mut websocket = WebSocket::from_partially_read(stream, tail, Role::Server, None);
            // Consume the authentication frames (one token or user + password).
            let _ = websocket.read();
            let _ = websocket.read();
            if close_before_auth {
                let _ = websocket.close(None);
                let _ = websocket.flush();
                return;
            }
            let _ = websocket.send(Message::text("authenticated"));
            let _ = websocket.send(Message::text("notify_file_id 42"));
            if send_notification {
                let _ = websocket.send(Message::text("notify_notification"));
            }
            let _ = websocket.close(None);
            let _ = websocket.flush();
            return;
        }
        let _ = stream.shutdown(Shutdown::Both);
    }

    #[test]
    fn tolerant_handshake_accepts_comma_separated_upgrade_header() {
        let server = FakePushServer::start("h2,h2c, websocket", false, false);
        let (tx, rx) = async_channel::unbounded::<PushEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let tx = tx.clone();
            std::thread::spawn(move || {
                push_worker_main(WorkerInputs {
                    generation: 1,
                    server: server.server_url(),
                    username: "alice".to_string(),
                    password: "secret".to_string(),
                    force_password_auth: false,
                    tls: test_tls_config(),
                    stop,
                    tx,
                });
            })
        };
        let events = collect_events(&rx, 3, Duration::from_secs(15));
        let _ = worker.join();
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| match &event.kind {
                PushEventKind::Authenticated => "authenticated",
                PushEventKind::FileNotification => "file_notification",
                PushEventKind::Closed { .. } => "closed",
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(kinds, vec!["authenticated", "file_notification", "closed"]);
    }

    /// A `notify_notification` text message must surface as a
    /// `Notification` event (issue #31).
    #[test]
    fn notify_notification_text_emits_a_notification_event() {
        let server = FakePushServer::start("websocket", false, true);
        let (tx, rx) = async_channel::unbounded::<PushEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let tx = tx.clone();
            std::thread::spawn(move || {
                push_worker_main(WorkerInputs {
                    generation: 1,
                    server: server.server_url(),
                    username: "alice".to_string(),
                    password: "secret".to_string(),
                    force_password_auth: false,
                    tls: test_tls_config(),
                    stop,
                    tx,
                });
            })
        };
        let events = collect_events(&rx, 4, Duration::from_secs(15));
        let _ = worker.join();
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| match &event.kind {
                PushEventKind::Authenticated => "authenticated",
                PushEventKind::FileNotification => "file_notification",
                PushEventKind::Notification => "notification",
                PushEventKind::Closed { .. } => "closed",
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "authenticated",
                "file_notification",
                "notification",
                "closed"
            ]
        );
    }

    /// Drain `count` events off the channel (used by worker-level tests).
    fn collect_events(
        rx: &async_channel::Receiver<PushEvent>,
        count: usize,
        timeout: Duration,
    ) -> Vec<PushEvent> {
        let mut events = Vec::new();
        let deadline = Instant::now() + timeout;
        while events.len() < count {
            if Instant::now() >= deadline {
                panic!("timed out; received {}/{} events", events.len(), count);
            }
            match rx.try_recv() {
                Ok(event) => events.push(event),
                Err(_) => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        events
    }

    // ---- facade reconnect with pre-auth fallback (GLib pump) -----------------

    #[test]
    fn pre_auth_fallback_to_password_after_unauthenticated_close() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let server = FakePushServer::start("websocket", true, false);
        let context = glib::MainContext::new();
        let (client, states, _notifications) = test_client(Provider::Nextcloud);
        context
            .with_thread_default(|| {
                client.set_backoff_scale(0.01);
                client.configure(&server.server_url(), "alice", "secret", true);
                let deadline = Instant::now() + Duration::from_secs(30);
                while server.ws_connections() < 2 && Instant::now() < deadline {
                    let _ = context.iteration(false);
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(
                    server.ws_connections() >= 2,
                    "the second (password) attempt should happen"
                );
                assert_eq!(
                    server.pre_auth_requests(),
                    1,
                    "pre_auth must be skipped once the password fallback kicks in"
                );
                let reconnecting = states
                    .borrow()
                    .iter()
                    .any(|(state, _)| *state == PushState::Reconnecting);
                assert!(
                    reconnecting,
                    "a reconnect should have been scheduled: {:?}",
                    *states.borrow()
                );
                client.disconnect(false);
            })
            .expect("the test main context is available");
    }

    #[test]
    fn connect_failure_schedules_reconnect_with_backoff() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // An unbound port: every attempt fails fast at connect time.
        let context = glib::MainContext::new();
        let (client, states, _notifications) = test_client(Provider::Nextcloud);
        context
            .with_thread_default(|| {
                client.set_backoff_scale(0.001);
                client.configure("http://127.0.0.1:1/", "alice", "secret", true);
                let deadline = Instant::now() + Duration::from_secs(30);
                while Instant::now() < deadline {
                    let connecting = states
                        .borrow()
                        .iter()
                        .filter(|(state, _)| *state == PushState::Connecting)
                        .count();
                    if connecting >= 2 {
                        break;
                    }
                    let _ = context.iteration(false);
                    std::thread::sleep(Duration::from_millis(5));
                }
                let (connecting, reconnecting) = {
                    let states = states.borrow();
                    (
                        states
                            .iter()
                            .filter(|(state, _)| *state == PushState::Connecting)
                            .count(),
                        states
                            .iter()
                            .any(|(state, _)| *state == PushState::Reconnecting),
                    )
                };
                assert!(
                    connecting >= 2,
                    "failed connects should retry with backoff: {:?}",
                    *states.borrow()
                );
                assert!(reconnecting);
                client.disconnect(false);
            })
            .expect("the test main context is available");
    }

    #[test]
    fn set_online_false_stops_reconnecting() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let context = glib::MainContext::new();
        let (client, states, _notifications) = test_client(Provider::Nextcloud);
        context
            .with_thread_default(|| {
                client.set_backoff_scale(0.001);
                client.configure("http://127.0.0.1:1/", "alice", "secret", true);
                // Wait until the first failure reached Reconnecting.
                let deadline = Instant::now() + Duration::from_secs(15);
                while Instant::now() < deadline {
                    if states
                        .borrow()
                        .iter()
                        .any(|(state, _)| *state == PushState::Reconnecting)
                    {
                        break;
                    }
                    let _ = context.iteration(false);
                    std::thread::sleep(Duration::from_millis(5));
                }
                let states_before = states.borrow().len();
                client.set_online(false);
                let connecting_before = states
                    .borrow()
                    .iter()
                    .filter(|(state, _)| *state == PushState::Connecting)
                    .count();
                std::thread::sleep(Duration::from_millis(150));
                let (connecting_after, new_states) = {
                    let states = states.borrow();
                    (
                        states
                            .iter()
                            .filter(|(state, _)| *state == PushState::Connecting)
                            .count(),
                        states[states_before..].to_vec(),
                    )
                };
                assert_eq!(
                    connecting_before, connecting_after,
                    "going offline must cancel the reconnect timer"
                );
                // keep_enabled disconnects emit no new states at all.
                assert!(
                    new_states.is_empty(),
                    "set_online(false) must not emit states: {new_states:?}"
                );
                client.disconnect(true);
            })
            .expect("the test main context is available");
    }

    /// Real-server integration test (disabled by default).
    ///
    /// Requires a Nextcloud server with notify_push enabled. Run it with:
    ///
    /// ```sh
    /// NEXTSYNC_PUSH_SERVER=https://cloud.example.com \
    /// NEXTSYNC_PUSH_USER=alice \
    /// NEXTSYNC_PUSH_PASSWORD=secret \
    /// cargo test --lib nextcloud::push::tests::real_server_connect -- --ignored --nocapture
    /// ```
    ///
    /// The test asserts the client reaches `Connected` (through discovery,
    /// pre-auth and the WebSocket handshake) and then shuts down cleanly. It
    /// is `#[ignore]` because no real server is available in this environment;
    /// without the variables it reports success and does nothing.
    #[test]
    #[ignore = "requires a real Nextcloud server with notify_push"]
    fn real_server_connect() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Ok(server) = std::env::var("NEXTSYNC_PUSH_SERVER") else {
            return;
        };
        let username = std::env::var("NEXTSYNC_PUSH_USER").unwrap_or_default();
        let password = std::env::var("NEXTSYNC_PUSH_PASSWORD").unwrap_or_default();
        let context = glib::MainContext::new();
        let (client, states, _notifications) = test_client(Provider::Nextcloud);
        context
            .with_thread_default(|| {
                client.configure(&server, &username, &password, true);
                let deadline = Instant::now() + Duration::from_secs(30);
                while Instant::now() < deadline {
                    let connected = states
                        .borrow()
                        .iter()
                        .any(|(state, _)| *state == PushState::Connected);
                    let terminal = states.borrow().iter().any(|(state, _)| {
                        matches!(
                            state,
                            PushState::AuthRequired | PushState::Unsupported | PushState::Disabled
                        )
                    });
                    if connected || terminal {
                        break;
                    }
                    let _ = context.iteration(false);
                    std::thread::sleep(Duration::from_millis(20));
                }
                let states = states.borrow().clone();
                assert!(
                    states
                        .iter()
                        .any(|(state, _)| *state == PushState::Connected),
                    "push never connected: {states:?}"
                );
                client.disconnect(false);
            })
            .expect("the test main context is available");
    }
}
