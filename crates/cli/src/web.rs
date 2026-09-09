//! `ambidiff web`: loopback-only browser frontend server.
//!
//! One TcpListener; each admitted connection gets a thread that reads the
//! request head and either serves an embedded asset (GET, exact lookup, no
//! filesystem) or upgrades the exact `/ws` route through tungstenite's
//! complete handshake, whose callback enforces the loopback Host and a
//! matching (or absent) Origin. Auth follows hunk's pattern: a random
//! capability token rides in the URL FRAGMENT (never sent in requests, so
//! it cannot leak into logs); the page passes it as the first websocket
//! message within a total five-second handshake deadline and the server
//! compares constant-time.
//!
//! Resource bounds: at most `MAX_CONNECTIONS` connections, request heads of
//! 16 KiB, inbound messages of 1 MiB (over: an error and close 1009),
//! outbound messages of 16 MiB (over: a `tooLarge` error envelope in their
//! place), a five-second write deadline so a stalled subscriber is dropped
//! instead of stalling the server. Broadcasts are a latest-payload cell
//! per kind, filled by one thread from the shared [`Application`] and
//! serviced per subscriber between requests and on idle slices, so a
//! burst of changes never queues up.
//!
//! The native side owns fs/git/watch and pushes RAW diff text plus review
//! JSON; the wasm core in the page computes rows/spans/anchors/search with
//! the same code the TUI runs (the drift firewall extends to the browser).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ambidiff_core::git_source::RawDiff;
use ambidiff_core::protocol::{
    decode_comment_add, decode_comment_delete, decode_comment_edit, decode_lifecycle,
};
use ambidiff_core::review::{Action, Actor, Side};
use ambidiff_core::sanitize::sanitize_line;
use anyhow::{Context, Result};
use include_dir::{Dir, include_dir};
use serde_json::{Value, json};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tungstenite::{Message, WebSocket};

use crate::application::{AppError, Application, OutcomeValue, ReviewCommand};
use crate::args::WebArgs;
use crate::context::resolve_store;

static ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../web/dist");

/// Every message type the server dispatches after authentication (plus
/// `auth`, which is only valid as the first message).
pub const MESSAGE_TYPES: &[&str] = &[
    "auth",
    "refresh",
    "getFile",
    "getSrc",
    "comment.add",
    "comment.edit",
    "comment.delete",
    "comment.address",
    "comment.resolve",
    "comment.reopen",
    "rev.bump",
];

/// Concurrent connections served; the rest get 503 until permits return.
pub const MAX_CONNECTIONS: usize = 32;
/// Total time from accept to a successful authentication.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(5);
/// Longest request head read before deciding the route.
const HEAD_LIMIT: usize = 16 * 1024;
/// Largest inbound websocket message.
const MAX_INBOUND: usize = 1024 * 1024;
/// Largest outbound websocket message; larger payloads become an error.
const MAX_OUTBOUND: usize = 16 * 1024 * 1024;
/// Socket write deadline (a stalled subscriber is dropped after this).
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Idle read slice between broadcast deliveries.
const IDLE_SLICE: Duration = Duration::from_millis(100);
/// How often the broadcast thread drains the watch.
const BROADCAST_POLL: Duration = Duration::from_millis(100);
/// Back-off after an accept error so a broken listener cannot spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

pub fn run(args: WebArgs) -> Result<i32> {
    let store = resolve_store()?;
    let listener = TcpListener::bind(("127.0.0.1", args.port)).context("bind loopback listener")?;
    let port = listener.local_addr()?.port();
    let token = generate_token().context("generate session token")?;
    let url = format!("http://127.0.0.1:{port}/#t={token}");

    let mut app = Application::open(store);
    // Arm BEFORE the held load (the absorbed-write race).
    if let Err(err) = app.start_watch() {
        warn(&format!("watch not started: {err}"));
    }
    if let Err(err) = app.load() {
        warn(&format!("initial load failed: {err}"));
    }
    let app = Arc::new(Mutex::new(app));
    let broadcast = Arc::new(Broadcast::default());
    start_broadcast(&app, &broadcast);

    println!("ambidiff web at {url}");
    println!("loopback only; the token in the fragment is this session's key");
    if args.open {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }

    let state = Arc::new(ServerState {
        app,
        token,
        port,
        broadcast,
        connections: AtomicUsize::new(0),
    });
    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(err) => {
                warn(&format!("accept failed: {err}"));
                std::thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };
        let Some(permit) = Permit::acquire(&state) else {
            let mut stream = stream;
            let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
            let _ = respond(&mut stream, 503, "text/plain", b"too many connections");
            continue;
        };
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            let _permit = permit;
            if let Err(err) = handle_connection(stream, &state) {
                // Connection-level failures are ordinary (client went away,
                // handshake refused); log only for diagnostics.
                if std::env::var_os("AMBIDIFF_WEB_DEBUG").is_some() {
                    warn(&format!("connection: {err:#}"));
                }
            }
        });
    }
}

fn warn(message: &str) {
    for line in message.lines() {
        eprintln!("ambidiff: warning: {}", sanitize_line(line));
    }
}

struct ServerState {
    app: Arc<Mutex<Application>>,
    token: String,
    port: u16,
    broadcast: Arc<Broadcast>,
    connections: AtomicUsize,
}

/// One of `MAX_CONNECTIONS` admission permits; released on drop.
struct Permit(Arc<ServerState>);

impl Permit {
    fn acquire(state: &Arc<ServerState>) -> Option<Permit> {
        let mut current = state.connections.load(Ordering::Acquire);
        loop {
            if current >= MAX_CONNECTIONS {
                return None;
            }
            match state.connections.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Permit(Arc::clone(state))),
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 16 random bytes from the OS, hex-encoded. No fallback: a session without
/// real entropy is not started.
fn generate_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Constant-time equality (length leak is fine; tokens are fixed-length).
fn token_matches(expected: &str, got: &str) -> bool {
    if expected.len() != got.len() {
        return false;
    }
    expected
        .bytes()
        .zip(got.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

// -------------------------------------------------------------- broadcast

/// Latest payload per notification kind with an epoch; subscribers keep
/// the epochs they have delivered, so a burst collapses to one message.
#[derive(Default)]
struct Broadcast {
    inner: Mutex<BroadcastState>,
}

#[derive(Default)]
struct BroadcastState {
    review_epoch: u64,
    review_msg: Option<String>,
    diff_epoch: u64,
    diff_msg: Option<String>,
}

/// A subscriber's delivery cursor.
#[derive(Default, Clone, Copy)]
struct Seen {
    review: u64,
    diff: u64,
}

impl Broadcast {
    fn publish_review(&self, msg: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.review_epoch += 1;
        inner.review_msg = Some(msg);
    }

    fn publish_diff(&self, msg: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.diff_epoch += 1;
        inner.diff_msg = Some(msg);
    }

    /// Payloads newer than `seen`, and the cursor after delivering them.
    fn since(&self, seen: Seen) -> (Vec<String>, Seen) {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        if inner.review_epoch > seen.review
            && let Some(msg) = &inner.review_msg
        {
            out.push(msg.clone());
        }
        if inner.diff_epoch > seen.diff
            && let Some(msg) = &inner.diff_msg
        {
            out.push(msg.clone());
        }
        (
            out,
            Seen {
                review: inner.review_epoch,
                diff: inner.diff_epoch,
            },
        )
    }

    fn cursor(&self) -> Seen {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Seen {
            review: inner.review_epoch,
            diff: inner.diff_epoch,
        }
    }
}

/// One thread drains the watch, refreshes the application, and fills the
/// broadcast cell from the current snapshot (so a reconfigured source is
/// honoured; nothing is frozen in a closure).
fn start_broadcast(app: &Arc<Mutex<Application>>, broadcast: &Arc<Broadcast>) {
    let app = Arc::clone(app);
    let broadcast = Arc::clone(broadcast);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(BROADCAST_POLL);
            let mut app = app.lock().unwrap_or_else(|e| e.into_inner());
            let pending = app.poll();
            if !pending.review && !pending.diff {
                continue;
            }
            let before = app.generation();
            let loaded = if pending.diff {
                app.refresh().map(|_| ())
            } else {
                app.load().map(|_| ())
            };
            if let Err(err) = loaded {
                warn(&format!("reload after change failed: {err}"));
            }
            if pending.review {
                broadcast.publish_review(review_changed(&app).to_string());
            }
            // A `source` change reconfigures the comparison and bumps the
            // generation: publish it as a diff change too.
            if pending.diff || app.generation() != before {
                broadcast.publish_diff(diff_changed(&app).to_string());
            }
        }
    });
}

// ------------------------------------------------------- message builders

/// The review file's raw text: the page parses the same bytes the native
/// side does, so a retained (read-only) document is never normalised away.
fn review_text(app: &Application) -> Option<String> {
    std::fs::read_to_string(app.store().review_path()).ok()
}

/// `hello` and `snapshot` share this builder.
fn snapshot_message(app: &Application, kind: &str, id: Option<&Value>) -> Value {
    let mut msg = json!({
        "type": kind,
        "appVersion": env!("CARGO_PKG_VERSION"),
        "root": app.store().root().display().to_string(),
    });
    if let Some(id) = id {
        msg["id"] = id.clone();
    }
    match app.snapshot() {
        Some(snapshot) => {
            msg["review"] = json!(review_text(app).unwrap_or_default());
            msg["files"] = json!(snapshot.files);
            msg["warnings"] = json!(snapshot.warnings);
            msg["readOnly"] = json!(snapshot.read_only);
            msg["readOnlyReason"] = json!(snapshot.read_only_reason);
            msg["sourceError"] = json!(snapshot.source_error);
            msg["skipped"] = json!(snapshot.skipped);
            msg["comparison"] = json!(snapshot.comparison);
            msg["generation"] = json!(snapshot.generation);
        }
        None => {
            msg["review"] = json!("");
            msg["files"] = json!([]);
            msg["warnings"] = json!([]);
            msg["readOnly"] = json!(true);
            msg["readOnlyReason"] = json!("review not loaded");
            msg["sourceError"] = json!(null);
            msg["skipped"] = json!([]);
            msg["comparison"] = json!(null);
            msg["generation"] = json!(app.generation());
            msg["loadError"] = json!("review not loaded");
        }
    }
    msg
}

fn review_changed(app: &Application) -> Value {
    let (warnings, read_only, reason, generation) = match app.snapshot() {
        Some(s) => (
            s.warnings.clone(),
            s.read_only,
            s.read_only_reason.clone(),
            s.generation,
        ),
        None => (
            Vec::new(),
            true,
            Some("review not loaded".to_string()),
            app.generation(),
        ),
    };
    json!({
        "type": "reviewChanged",
        "review": review_text(app).unwrap_or_default(),
        "warnings": warnings,
        "readOnly": read_only,
        "readOnlyReason": reason,
        "generation": generation,
    })
}

fn diff_changed(app: &Application) -> Value {
    match app.snapshot() {
        Some(s) => json!({
            "type": "diffChanged",
            "files": s.files,
            "skipped": s.skipped,
            "sourceError": s.source_error,
            "comparison": s.comparison,
            "generation": s.generation,
        }),
        None => json!({
            "type": "diffChanged",
            "files": [],
            "skipped": [],
            "sourceError": "review not loaded",
            "comparison": null,
            "generation": app.generation(),
        }),
    }
}

fn error_message(id: &Value, code: &str, message: impl Into<String>) -> Value {
    json!({"type": "error", "id": id, "code": code, "message": message.into()})
}

// ------------------------------------------------------------ connections

/// The stream tungstenite and the asset server read through: replays the
/// already-read request head, and while a deadline is set derives every
/// read timeout from it so slow input cannot extend the handshake.
struct Conn {
    stream: TcpStream,
    replay: Vec<u8>,
    pos: usize,
    deadline: Option<Instant>,
}

impl Conn {
    fn new(stream: TcpStream, replay: Vec<u8>, deadline: Option<Instant>) -> Conn {
        Conn {
            stream,
            replay,
            pos: 0,
            deadline,
        }
    }

    /// Leave the handshake phase: fixed idle slices from here on.
    fn clear_deadline(&mut self) -> std::io::Result<()> {
        self.deadline = None;
        self.stream.set_read_timeout(Some(IDLE_SLICE))
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.replay.len() {
            let n = (self.replay.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.replay[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        if let Some(deadline) = self.deadline {
            let now = Instant::now();
            if now >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "handshake deadline passed",
                ));
            }
            self.stream.set_read_timeout(Some(deadline - now))?;
        }
        self.stream.read(buf)
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

/// What the request head tells us before routing.
#[derive(Debug, PartialEq, Eq)]
struct RequestLine {
    method: String,
    target: String,
    upgrade: bool,
}

/// Parse the request line and the `Upgrade` header; everything else is
/// tungstenite's job (websocket) or irrelevant (assets).
fn parse_request_head(head: &[u8]) -> Option<RequestLine> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let upgrade = lines
        .filter_map(|l| l.split_once(':'))
        .any(|(name, value)| {
            name.trim().eq_ignore_ascii_case("upgrade")
                && value.trim().eq_ignore_ascii_case("websocket")
        });
    Some(RequestLine {
        method,
        target,
        upgrade,
    })
}

/// Read the request head (through the CRLFCRLF) under the deadline and the
/// size cap.
fn read_head(stream: &mut TcpStream, deadline: Instant) -> Result<Vec<u8>> {
    let mut head = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        anyhow::ensure!(
            head.len() < HEAD_LIMIT,
            "request head exceeds {HEAD_LIMIT} bytes"
        );
        let now = Instant::now();
        anyhow::ensure!(now < deadline, "handshake deadline passed");
        stream.set_read_timeout(Some(deadline - now))?;
        if stream.read(&mut byte)? == 0 {
            anyhow::bail!("client closed before the head completed");
        }
        head.push(byte[0]);
    }
    Ok(head)
}

/// The handshake policy: the loopback host we bound, and either no Origin
/// (native clients) or exactly this server's origin (the page).
fn handshake_policy(port: u16, host: Option<&str>, origin: Option<&str>) -> bool {
    let expected_host = format!("127.0.0.1:{port}");
    let expected_origin = format!("http://127.0.0.1:{port}");
    host == Some(expected_host.as_str()) && origin.is_none_or(|o| o == expected_origin)
}

struct HandshakeCheck {
    port: u16,
}

impl tungstenite::handshake::server::Callback for HandshakeCheck {
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
        if handshake_policy(self.port, header("host"), header("origin")) {
            Ok(response)
        } else {
            let mut refused = ErrorResponse::new(Some("forbidden".to_string()));
            *refused.status_mut() = tungstenite::http::StatusCode::FORBIDDEN;
            Err(refused)
        }
    }
}

fn handle_connection(mut stream: TcpStream, state: &Arc<ServerState>) -> Result<()> {
    let deadline = Instant::now() + HANDSHAKE_DEADLINE;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let head = read_head(&mut stream, deadline)?;
    let Some(request) = parse_request_head(&head) else {
        return respond(&mut stream, 400, "text/plain", b"bad request");
    };
    if request.method != "GET" {
        return respond(&mut stream, 405, "text/plain", b"method not allowed");
    }
    if request.target == "/ws" {
        if !request.upgrade {
            return respond(&mut stream, 404, "text/plain", b"not found");
        }
        let conn = Conn::new(stream, head, Some(deadline));
        let config = WebSocketConfig {
            max_message_size: Some(MAX_INBOUND),
            max_frame_size: Some(MAX_INBOUND),
            ..Default::default()
        };
        let ws = tungstenite::accept_hdr_with_config(
            conn,
            HandshakeCheck { port: state.port },
            Some(config),
        )
        .map_err(|e| anyhow::anyhow!("websocket handshake: {e}"))?;
        return serve_websocket(ws, state);
    }
    serve_asset(&mut stream, &request.target)
}

fn respond(stream: &mut TcpStream, code: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    Ok(())
}

fn serve_asset(stream: &mut TcpStream, target: &str) -> Result<()> {
    let clean = target.split(['?', '#']).next().unwrap_or("/");
    let name = match clean {
        "/" | "" => "index.html",
        other => other.trim_start_matches('/'),
    };
    // Exact embedded lookup only: no filesystem, no traversal surface.
    match ASSETS.get_file(name) {
        Some(file) => {
            let content_type = match name.rsplit('.').next().unwrap_or("") {
                "html" => "text/html; charset=utf-8",
                "js" => "text/javascript",
                "css" => "text/css",
                "wasm" => "application/wasm",
                "json" => "application/json",
                _ => "application/octet-stream",
            };
            respond(stream, 200, content_type, file.contents())
        }
        None => respond(stream, 404, "text/plain", b"not found"),
    }
}

/// The text actually sent for a payload: itself, or an error envelope in
/// its place when it exceeds the outbound budget, so the review itself is
/// never the reason a page hangs.
fn bound_payload(payload: Value) -> String {
    let text = payload.to_string();
    if text.len() <= MAX_OUTBOUND {
        return text;
    }
    let id = payload.get("id").cloned().unwrap_or(Value::Null);
    error_message(
        &id,
        "tooLarge",
        format!(
            "response of {} bytes exceeds the {MAX_OUTBOUND} byte limit",
            text.len()
        ),
    )
    .to_string()
}

fn send_bounded(ws: &mut WebSocket<Conn>, payload: Value) -> Result<()> {
    Ok(ws.send(Message::Text(bound_payload(payload)))?)
}

fn deliver_broadcasts(
    ws: &mut WebSocket<Conn>,
    state: &ServerState,
    seen: &mut Seen,
) -> Result<()> {
    let (payloads, next) = state.broadcast.since(*seen);
    for payload in payloads {
        if payload.len() > MAX_OUTBOUND {
            let replaced = error_message(
                &Value::Null,
                "tooLarge",
                format!("broadcast exceeds the {MAX_OUTBOUND} byte limit; use refresh"),
            );
            ws.send(Message::Text(replaced.to_string()))?;
        } else {
            ws.send(Message::Text(payload))?;
        }
    }
    *seen = next;
    Ok(())
}

/// Drain and discard whatever the peer still sends, for a bounded time and
/// volume, so a close frame we sent reaches it before the socket goes away.
fn linger(conn: &mut Conn) {
    const LINGER: Duration = Duration::from_secs(2);
    const LINGER_BYTES: usize = 4 * 1024 * 1024;
    let deadline = Instant::now() + LINGER;
    let mut discarded = 0usize;
    let mut buf = [0u8; 16 * 1024];
    while discarded < LINGER_BYTES {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if conn.stream.set_read_timeout(Some(deadline - now)).is_err() {
            break;
        }
        match conn.stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => discarded += n,
        }
    }
}

fn is_timeout(err: &tungstenite::Error) -> bool {
    matches!(
        err,
        tungstenite::Error::Io(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
    )
}

fn serve_websocket(mut ws: WebSocket<Conn>, state: &Arc<ServerState>) -> Result<()> {
    // First message must authenticate within the (absolute) deadline;
    // pings are answered but do not extend it.
    let auth = loop {
        match ws.read() {
            Ok(Message::Text(text)) => break text,
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            Ok(_) => return Ok(()),
            Err(e) if is_timeout(&e) => {
                let _ = ws.close(Some(CloseFrame {
                    code: CloseCode::Policy,
                    reason: "authentication deadline".into(),
                }));
                let _ = ws.flush();
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
    };
    let auth: Value = serde_json::from_str(&auth).unwrap_or_default();
    let token = auth["token"].as_str().unwrap_or_default();
    if auth["type"] != "auth" || !token_matches(&state.token, token) {
        let _ = ws.send(Message::Text(
            error_message(&Value::Null, "unauthorized", "unauthorized").to_string(),
        ));
        let _ = ws.close(None);
        let _ = ws.flush();
        return Ok(());
    }
    ws.get_mut().clear_deadline()?;

    // Everything published before this subscriber existed is already in
    // its hello; deliver only what comes after.
    let mut seen = state.broadcast.cursor();
    let hello = {
        let mut app = state.app.lock().unwrap_or_else(|e| e.into_inner());
        let _ = app.load();
        snapshot_message(&app, "hello", None)
    };
    send_bounded(&mut ws, hello)?;

    loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                let response = handle_client_message(state, &text);
                send_bounded(&mut ws, response)?;
                deliver_broadcasts(&mut ws, state, &mut seen)?;
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(e) if is_timeout(&e) => {
                deliver_broadcasts(&mut ws, state, &mut seen)?;
            }
            Err(tungstenite::Error::Capacity(
                tungstenite::error::CapacityError::MessageTooLong { size, max_size },
            )) => {
                let _ = ws.send(Message::Text(
                    error_message(
                        &Value::Null,
                        "tooLarge",
                        format!("message of {size} bytes exceeds the {max_size} byte limit"),
                    )
                    .to_string(),
                ));
                let _ = ws.close(Some(CloseFrame {
                    code: CloseCode::Size,
                    reason: "message too large".into(),
                }));
                let _ = ws.flush();
                // The client is probably still sending the rest of that
                // message; linger briefly, discarding it, so it can read the
                // reason instead of a broken pipe.
                linger(ws.get_mut());
                return Ok(());
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
    }
}

// --------------------------------------------------------------- dispatch

fn handle_client_message(state: &ServerState, text: &str) -> Value {
    let request: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return error_message(&Value::Null, "decode", format!("invalid JSON: {e}")),
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let kind = request["type"].as_str().unwrap_or_default().to_string();
    if !MESSAGE_TYPES.contains(&kind.as_str()) {
        return error_message(&id, "unknownType", format!("unknown message type {kind:?}"));
    }
    let result = match kind.as_str() {
        "refresh" => refresh(state, &id),
        "getFile" => get_file(state, &request),
        "getSrc" => get_src(state, &request),
        "comment.add" => decode_comment_add(&request)
            .map_err(AppError::from)
            .and_then(|req| execute(state, ReviewCommand::Add(req))),
        "comment.edit" => decode_comment_edit(&request, "commentId")
            .map_err(AppError::from)
            .and_then(|req| execute(state, ReviewCommand::Edit(req))),
        "comment.delete" => decode_comment_delete(&request, "commentId")
            .map_err(AppError::from)
            .and_then(|req| execute(state, ReviewCommand::Delete(req))),
        "comment.address" => lifecycle(state, &request, Action::Address, Actor::Agent),
        "comment.resolve" => lifecycle(state, &request, Action::Resolve, Actor::Human),
        "comment.reopen" => lifecycle(state, &request, Action::Reopen, Actor::Human),
        "rev.bump" => execute(state, ReviewCommand::RevBump),
        // `auth` is only valid as the first message.
        other => {
            return error_message(
                &id,
                "unknownType",
                format!("unexpected message type {other:?}"),
            );
        }
    };
    match result {
        Ok(mut value) => {
            value["id"] = id;
            value
        }
        Err(err) => error_message(&id, err.kind(), err.to_string()),
    }
}

fn refresh(state: &ServerState, id: &Value) -> Result<Value, AppError> {
    let mut app = state.app.lock().unwrap_or_else(|e| e.into_inner());
    app.refresh()?;
    Ok(snapshot_message(&app, "snapshot", Some(id)))
}

fn path_of(request: &Value) -> Result<String, AppError> {
    request["path"].as_str().map(str::to_string).ok_or_else(|| {
        AppError::Decode(ambidiff_core::protocol::DecodeError::Missing {
            field: "path".to_string(),
        })
    })
}

fn get_file(state: &ServerState, request: &Value) -> Result<Value, AppError> {
    let path = path_of(request)?;
    let app = state.app.lock().unwrap_or_else(|e| e.into_inner());
    let (entry, raw) = app.raw_file(&path)?;
    Ok(match raw {
        RawDiff::Text {
            raw,
            old_total_lines,
        } => json!({
            "type": "file", "path": path, "entry": entry,
            "raw": raw, "oldTotalLines": old_total_lines,
        }),
        RawDiff::TooLarge { adds, dels } => json!({
            "type": "file", "path": path, "entry": entry,
            "tooLarge": {"adds": adds, "dels": dels},
        }),
    })
}

/// New-side content for a LISTED path only (gap expansion); the review
/// root reader denies anything outside the tree.
fn get_src(state: &ServerState, request: &Value) -> Result<Value, AppError> {
    let path = path_of(request)?;
    let app = state.app.lock().unwrap_or_else(|e| e.into_inner());
    let content = app
        .content(Side::New, &path)?
        .ok_or_else(|| AppError::NoSource {
            reason: format!("cannot read {path:?}"),
        })?;
    Ok(json!({"type": "src", "path": path, "content": content}))
}

fn lifecycle(
    state: &ServerState,
    request: &Value,
    action: Action,
    actor: Actor,
) -> Result<Value, AppError> {
    let req = decode_lifecycle(request, "commentId")?;
    execute(state, ReviewCommand::Lifecycle { action, actor, req })
}

fn execute(state: &ServerState, cmd: ReviewCommand) -> Result<Value, AppError> {
    let mut app = state.app.lock().unwrap_or_else(|e| e.into_inner());
    let outcome = app.execute(cmd)?;
    Ok(match outcome.value {
        OutcomeValue::Comment(comment) => {
            json!({"type": "comment", "comment": comment, "warnings": outcome.warnings})
        }
        OutcomeValue::Deleted(id) => {
            json!({"type": "deleted", "commentId": id, "warnings": outcome.warnings})
        }
        OutcomeValue::Revision(revision) => {
            json!({"type": "revision", "revision": revision, "warnings": outcome.warnings})
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_32_hex_chars_and_unique() {
        let a = generate_token().expect("entropy");
        let b = generate_token().expect("entropy");
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert!(token_matches(&a, &a));
        assert!(!token_matches(&a, &b));
        assert!(!token_matches(&a, &a[..31]));
    }

    #[test]
    fn handshake_policy_requires_loopback_host_and_matching_or_absent_origin() {
        assert!(handshake_policy(7000, Some("127.0.0.1:7000"), None));
        assert!(handshake_policy(
            7000,
            Some("127.0.0.1:7000"),
            Some("http://127.0.0.1:7000")
        ));
        assert!(!handshake_policy(7000, Some("localhost:7000"), None));
        assert!(!handshake_policy(7000, Some("127.0.0.1:7001"), None));
        assert!(!handshake_policy(7000, None, None));
        assert!(!handshake_policy(
            7000,
            Some("127.0.0.1:7000"),
            Some("http://evil.example")
        ));
        assert!(!handshake_policy(
            7000,
            Some("127.0.0.1:7000"),
            Some("http://127.0.0.1:7001")
        ));
        assert!(!handshake_policy(
            7000,
            Some("127.0.0.1:7000"),
            Some("null")
        ));
    }

    #[test]
    fn request_head_parsing_routes_on_target_and_upgrade() {
        let head =
            b"GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: WebSocket\r\nConnection: Upgrade\r\n\r\n";
        assert_eq!(
            parse_request_head(head),
            Some(RequestLine {
                method: "GET".into(),
                target: "/ws".into(),
                upgrade: true
            })
        );
        let plain = b"GET /ws HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(!parse_request_head(plain).expect("parsed").upgrade);
        let prefixed = b"GET /wsx HTTP/1.1\r\nUpgrade: websocket\r\n\r\n";
        assert_eq!(parse_request_head(prefixed).expect("parsed").target, "/wsx");
        assert_eq!(parse_request_head(b"\r\n\r\n"), None);
    }

    #[test]
    fn conn_replays_the_head_before_reading_the_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        let mut conn = Conn::new(server, b"HEAD".to_vec(), None);
        let mut buf = [0u8; 2];
        assert_eq!(conn.read(&mut buf).expect("read"), 2);
        assert_eq!(&buf, b"HE");
        assert_eq!(conn.read(&mut buf).expect("read"), 2);
        assert_eq!(&buf, b"AD");
        // Replay exhausted: the next read hits the socket.
        (&client).write_all(b"xy").expect("send");
        assert_eq!(conn.read(&mut buf).expect("read"), 2);
        assert_eq!(&buf, b"xy");
    }

    #[test]
    fn conn_read_fails_once_the_deadline_has_passed() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let _client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        let mut conn = Conn::new(server, Vec::new(), Some(Instant::now()));
        let mut buf = [0u8; 1];
        let err = conn.read(&mut buf).expect_err("deadline passed");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn broadcast_cell_coalesces_and_delivers_once() {
        let cell = Broadcast::default();
        let mut seen = cell.cursor();
        cell.publish_review("r1".into());
        cell.publish_review("r2".into());
        cell.publish_diff("d1".into());
        let (payloads, next) = cell.since(seen);
        assert_eq!(payloads, vec!["r2".to_string(), "d1".to_string()]);
        seen = next;
        let (payloads, _) = cell.since(seen);
        assert!(payloads.is_empty(), "delivered once");
    }

    #[test]
    fn oversized_payloads_are_replaced_by_an_error_envelope() {
        let big = json!({"type": "src", "id": 4, "content": "z".repeat(MAX_OUTBOUND + 1)});
        let sent: Value = serde_json::from_str(&bound_payload(big)).expect("json");
        assert_eq!(sent["type"], "error");
        assert_eq!(sent["id"], 4);
        assert_eq!(sent["code"], "tooLarge");
        let small = json!({"type": "src", "id": 5, "content": "ok"});
        assert_eq!(bound_payload(small.clone()), small.to_string());
    }
}
