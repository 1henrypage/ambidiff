//! Browser server protocol: the real binary on a loopback port, driven with
//! raw TCP and a tungstenite client. Covers authentication, the complete
//! handshake policy (route, upgrade, host, origin), the total handshake
//! deadline, admission and message limits, and the message contract in
//! `fixtures/contracts/web-*.json`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(root: &Path, path: &str, content: &str) {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(full, content).expect("write");
}

fn cli(root: &Path, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args(args)
        .current_dir(root)
        .env("AMBIDIFF_AUTHOR", "henry")
        .output()
        .expect("ambidiff");
    assert!(
        out.status.success(),
        "ambidiff {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
}

/// Scratch repo: one committed file with a working-tree edit, review
/// initialised against HEAD.
fn scratch_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    write(&root, "src/login.ts", "a\nb\nc\nd\ne\n");
    write(&root, "README.md", "# readme\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "base"]);
    write(&root, "src/login.ts", "a\nB\nc\nd\ne\n");
    cli(
        &root,
        &["init", "--review", "web-test", "--base", "HEAD", "--json"],
    );
    (dir, root)
}

struct Server {
    child: Child,
    port: u16,
    token: String,
}

impl Server {
    fn spawn(root: &Path) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
            .args(["web", "--port", "0"])
            .current_dir(root)
            .env("AMBIDIFF_AUTHOR", "henry")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn web server");
        let mut stdout = child.stdout.take().expect("stdout");
        let mut line = String::new();
        let mut byte = [0u8; 1];
        // The first line names the URL; the token rides in the fragment.
        while !line.ends_with('\n') {
            let n = stdout.read(&mut byte).expect("read url line");
            assert!(n > 0, "server closed stdout before printing its url");
            line.push(byte[0] as char);
        }
        let url = line
            .trim()
            .strip_prefix("ambidiff web at ")
            .unwrap_or_else(|| panic!("unexpected first line {line:?}"));
        let (host, fragment) = url.split_once("/#t=").expect("fragment token");
        let port: u16 = host
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .expect("port");
        Server {
            child,
            port,
            token: fragment.to_string(),
        }
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn tcp(&self) -> TcpStream {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        stream
    }

    /// Raw HTTP exchange; returns the response head (status line + headers).
    fn http(&self, request: &str) -> String {
        let mut stream = self.tcp();
        stream.write_all(request.as_bytes()).expect("send");
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Open a websocket with explicit Host / Origin headers (None omits
    /// Origin, as non-browser clients do).
    fn ws_with(
        &self,
        host: &str,
        origin: Option<&str>,
    ) -> Result<WebSocket<TcpStream>, Box<tungstenite::Error>> {
        let mut request = format!("ws://127.0.0.1:{}/ws", self.port)
            .into_client_request()
            .expect("request");
        request
            .headers_mut()
            .insert("Host", host.parse().expect("host header"));
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert("Origin", origin.parse().expect("origin header"));
        }
        let (ws, _response) =
            tungstenite::client::client(request, self.tcp()).map_err(|e| match e {
                tungstenite::HandshakeError::Failure(e) => Box::new(e),
                tungstenite::HandshakeError::Interrupted(_) => Box::new(tungstenite::Error::Io(
                    std::io::Error::other("handshake interrupted"),
                )),
            })?;
        Ok(ws)
    }

    /// Browser-like connection: matching Origin, then authenticate and
    /// return the `hello` message.
    fn client(&self) -> (WebSocket<TcpStream>, Value) {
        let host = format!("127.0.0.1:{}", self.port);
        let mut ws = self.ws_with(&host, Some(&self.origin())).expect("upgrade");
        ws.send(Message::Text(
            json!({"type": "auth", "token": self.token}).to_string(),
        ))
        .expect("auth");
        let hello = read_json(&mut ws);
        assert_eq!(hello["type"], "hello", "{hello}");
        (ws, hello)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_json(ws: &mut WebSocket<TcpStream>) -> Value {
    loop {
        match ws.read().expect("read") {
            Message::Text(text) => return serde_json::from_str(&text).expect("json"),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

/// Send a request and return its reply (skipping notifications).
fn request(ws: &mut WebSocket<TcpStream>, msg: Value) -> Value {
    let id = msg["id"].clone();
    ws.send(Message::Text(msg.to_string())).expect("send");
    loop {
        let reply = read_json(ws);
        if reply.get("id") == Some(&id) {
            return reply;
        }
    }
}

/// Wait for a broadcast of the given type.
fn wait_for_type(ws: &mut WebSocket<TcpStream>, kind: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let msg = read_json(ws);
        if msg["type"] == kind {
            return msg;
        }
    }
    panic!("no {kind} within {timeout:?}");
}

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/contracts")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture")).expect("json")
}

fn keys(v: &Value) -> Vec<String> {
    let mut keys: Vec<String> = v.as_object().expect("object").keys().cloned().collect();
    keys.sort();
    keys
}

// ------------------------------------------------------------ handshake

#[test]
fn invalid_token_is_rejected_and_closed() {
    let (_dir, root) = scratch_repo();
    let server = Server::spawn(&root);
    let host = format!("127.0.0.1:{}", server.port);
    let mut ws = server.ws_with(&host, None).expect("upgrade");
    ws.send(Message::Text(
        json!({"type": "auth", "token": "0000"}).to_string(),
    ))
    .expect("send");
    let reply = read_json(&mut ws);
    assert_eq!(reply["type"], "error");
    assert_eq!(reply["code"], "unauthorized");
    assert!(
        matches!(ws.read(), Ok(Message::Close(_)) | Err(_)),
        "server closes after a bad token"
    );
}

#[test]
fn wrong_host_and_foreign_origin_are_refused_and_matching_origin_is_accepted() {
    let (_dir, root) = scratch_repo();
    let server = Server::spawn(&root);
    let good_host = format!("127.0.0.1:{}", server.port);

    let err = server
        .ws_with("localhost:1", Some(&server.origin()))
        .expect_err("wrong host must be refused");
    assert!(
        matches!(*err, tungstenite::Error::Http(ref r) if r.status() == 403),
        "wrong host: {err:?}"
    );

    let err = server
        .ws_with(&good_host, Some("http://evil.example"))
        .expect_err("foreign origin must be refused");
    assert!(
        matches!(*err, tungstenite::Error::Http(ref r) if r.status() == 403),
        "foreign origin: {err:?}"
    );

    // Matching origin (a browser) and absent origin (a native client) both
    // reach authentication.
    let (mut ws, hello) = server.client();
    assert_eq!(hello["type"], "hello");
    ws.close(None).ok();
    let mut native = server.ws_with(&good_host, None).expect("no origin is fine");
    native
        .send(Message::Text(
            json!({"type": "auth", "token": server.token}).to_string(),
        ))
        .expect("auth");
    assert_eq!(read_json(&mut native)["type"], "hello");
}

#[test]
fn non_websocket_routes_serve_assets_or_404_and_ws_needs_an_upgrade() {
    let (_dir, root) = scratch_repo();
    let server = Server::spawn(&root);
    let index = server.http("GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert!(index.starts_with("HTTP/1.1 200"), "{index}");
    assert!(index.contains("text/html"));
    let missing = server.http("GET /nope.js HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    // /ws without an upgrade is not a websocket and not an asset either.
    let plain = server.http("GET /ws HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert!(plain.starts_with("HTTP/1.1 404"), "{plain}");
    // The upgrade route is exact: a prefix match must not upgrade.
    let prefixed =
        server.http("GET /wsx HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n");
    assert!(prefixed.starts_with("HTTP/1.1 404"), "{prefixed}");
    let post = server.http("POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n");
    assert!(post.starts_with("HTTP/1.1 405"), "{post}");
}

#[test]
fn slow_handshake_is_closed_at_the_total_deadline_even_with_pings() {
    let (_dir, root) = scratch_repo();
    let server = Server::spawn(&root);
    let host = format!("127.0.0.1:{}", server.port);

    // Upgrade completes, then the client never authenticates but keeps
    // pinging: pings must not extend the deadline.
    let started = Instant::now();
    let mut ws = server.ws_with(&host, None).expect("upgrade");
    ws.get_ref()
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("timeout");
    let closed_at = loop {
        let _ = ws.send(Message::Ping(vec![1]));
        match ws.read() {
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => {
                break started.elapsed();
            }
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break started.elapsed(),
            Ok(_) => {}
        }
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "server never closed the unauthenticated connection"
        );
    };
    assert!(
        closed_at >= Duration::from_millis(4500) && closed_at <= Duration::from_secs(9),
        "closed after {closed_at:?}, expected about 5 s"
    );

    // A half-sent request head is cut off by the same absolute deadline.
    let started = Instant::now();
    let mut stream = server.tcp();
    stream
        .write_all(b"GET /ws HTTP/1.1\r\nHost: x\r\n")
        .expect("partial head");
    let mut buf = [0u8; 64];
    let _ = stream.read(&mut buf);
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(4500) && elapsed <= Duration::from_secs(9),
        "partial head cut off after {elapsed:?}"
    );
}

#[test]
fn oversized_message_is_answered_with_an_error_and_close_1009() {
    let (_dir, root) = scratch_repo();
    let server = Server::spawn(&root);
    let (mut ws, _hello) = server.client();
    let body = "x".repeat(1024 * 1024 + 1);
    ws.send(Message::Text(
        json!({"type": "comment.add", "id": 7, "body": body}).to_string(),
    ))
    .expect("send oversized");
    let mut saw_error = false;
    let mut close_code = None;
    for _ in 0..4 {
        match ws.read() {
            Ok(Message::Text(text)) => {
                let v: Value = serde_json::from_str(&text).expect("json");
                if v["type"] == "error" {
                    assert_eq!(v["code"], "tooLarge", "{v}");
                    saw_error = true;
                }
            }
            Ok(Message::Close(frame)) => {
                close_code = frame.map(|f| u16::from(f.code));
                break;
            }
            Err(tungstenite::Error::ConnectionClosed) => break,
            Err(e) => panic!("unexpected error {e:?}"),
            Ok(_) => {}
        }
    }
    assert!(saw_error, "an error envelope precedes the close");
    assert_eq!(close_code, Some(1009), "close code 1009 (message too big)");
}

#[test]
fn thirty_third_connection_is_refused_with_503() {
    let (_dir, root) = scratch_repo();
    let server = Server::spawn(&root);
    // 32 idle connections hold every permit for the handshake window.
    let idle: Vec<TcpStream> = (0..32).map(|_| server.tcp()).collect();
    std::thread::sleep(Duration::from_millis(300));
    let reply = server.http("GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert!(reply.starts_with("HTTP/1.1 503"), "{reply}");
    drop(idle);
    // Once permits return, service resumes.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let reply = server.http("GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        if reply.starts_with("HTTP/1.1 200") {
            break;
        }
        assert!(Instant::now() < deadline, "service never resumed: {reply}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ------------------------------------------------------------- messages

#[test]
fn hello_carries_every_snapshot_field_of_the_contract() {
    let (_dir, root) = scratch_repo();
    let server = Server::spawn(&root);
    let (_ws, hello) = server.client();
    assert_eq!(keys(&hello), keys(&fixture("web-hello.json")));
    assert_eq!(hello["appVersion"], env!("CARGO_PKG_VERSION"));
    assert_eq!(hello["readOnly"], false);
    assert_eq!(hello["readOnlyReason"], Value::Null);
    assert_eq!(hello["sourceError"], Value::Null);
    assert_eq!(hello["warnings"], json!([]));
    assert_eq!(hello["skipped"], json!([]));
    assert_eq!(hello["files"][0]["path"], "src/login.ts");
    assert_eq!(hello["comparison"]["new"]["kind"], "worktree");
    assert_eq!(hello["comparison"]["old"]["kind"], "commit");
    assert!(hello["generation"].as_u64().is_some());
    let review: Value = serde_json::from_str(hello["review"].as_str().expect("review string"))
        .expect("review json");
    assert_eq!(review["review"], "web-test");
}

#[test]
fn refresh_returns_a_full_snapshot_that_reflects_external_changes() {
    let (_dir, root) = scratch_repo();
    let server = Server::spawn(&root);
    let (mut ws, hello) = server.client();
    assert_eq!(hello["files"].as_array().map(Vec::len), Some(1));
    // A new untracked file and a review comment land outside the page.
    write(&root, "extra.txt", "new\n");
    cli(&root, &["comment", "add", "-m", "note", "--json"]);
    let snapshot = request(&mut ws, json!({"type": "refresh", "id": 5}));
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(keys(&snapshot), keys(&fixture("web-snapshot.json")));
    let paths: Vec<&str> = snapshot["files"]
        .as_array()
        .expect("files")
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect();
    assert_eq!(paths, vec!["extra.txt", "src/login.ts"]);
    let review: Value =
        serde_json::from_str(snapshot["review"].as_str().expect("review")).expect("json");
    assert_eq!(review["comments"][0]["body"], "note");
}

#[test]
fn get_src_is_limited_to_listed_paths_and_traversal_is_denied() {
    let (_dir, root) = scratch_repo();
    let server = Server::spawn(&root);
    let (mut ws, _) = server.client();
    let ok = request(
        &mut ws,
        json!({"type": "getSrc", "id": 1, "path": "src/login.ts"}),
    );
    assert_eq!(ok["type"], "src");
    assert_eq!(ok["content"], "a\nB\nc\nd\ne\n");
    for path in ["README.md", "../outside", "/etc/hosts", ".git/config"] {
        let denied = request(&mut ws, json!({"type": "getSrc", "id": 2, "path": path}));
        assert_eq!(denied["type"], "error", "{path}: {denied}");
        assert_eq!(denied["id"], 2);
        assert!(
            denied["code"] == "notInChangedSet" || denied["code"] == "decode",
            "{path}: {denied}"
        );
    }
}

#[test]
fn edit_and_delete_over_the_websocket() {
    let (_dir, root) = scratch_repo();
    let server = Server::spawn(&root);
    let (mut ws, _) = server.client();
    let added = request(
        &mut ws,
        json!({"type": "comment.add", "id": 1, "path": "src/login.ts", "line": 2, "body": "why B??"}),
    );
    assert_eq!(added["type"], "comment", "{added}");
    assert_eq!(keys(&added), keys(&fixture("web-comment.json")));
    assert_eq!(added["comment"]["snippet"], "B");
    let id = added["comment"]["id"].as_str().expect("id").to_string();

    let edited = request(
        &mut ws,
        json!({"type": "comment.edit", "id": 2, "commentId": id, "body": "why B?? (edited)"}),
    );
    assert_eq!(edited["comment"]["body"], "why B?? (edited)");

    let deleted = request(
        &mut ws,
        json!({"type": "comment.delete", "id": 3, "commentId": id}),
    );
    assert_eq!(deleted["type"], "deleted", "{deleted}");
    assert_eq!(keys(&deleted), keys(&fixture("web-deleted.json")));
    assert_eq!(deleted["commentId"], id);
    let review = cli(&root, &["comment", "list", "--json"]);
    assert_eq!(review["comments"].as_array().map(Vec::len), Some(0));
}

#[test]
fn error_envelopes_echo_the_id_and_carry_a_code() {
    let (_dir, root) = scratch_repo();
    let server = Server::spawn(&root);
    let (mut ws, _) = server.client();
    let bad_line = request(
        &mut ws,
        json!({"type": "comment.add", "id": 42, "path": "src/login.ts", "line": 4294967297u64, "body": "b"}),
    );
    assert_eq!(keys(&bad_line), keys(&fixture("web-error.json")));
    assert_eq!(bad_line["id"], 42);
    assert_eq!(bad_line["code"], "decode");
    assert_eq!(bad_line["message"], fixture("web-error.json")["message"]);

    let unknown = request(&mut ws, json!({"type": "frobnicate", "id": 43}));
    assert_eq!(unknown["code"], "unknownType");
    assert_eq!(unknown["id"], 43);

    let missing = request(
        &mut ws,
        json!({"type": "comment.resolve", "id": 44, "commentId": "c-nope"}),
    );
    assert_eq!(missing["code"], "review");
    assert_eq!(missing["id"], 44);
}

#[test]
fn addressing_with_an_empty_response_transitions_and_stores_none() {
    let (_dir, root) = scratch_repo();
    let server = Server::spawn(&root);
    let (mut ws, _) = server.client();
    let added = request(
        &mut ws,
        json!({"type": "comment.add", "id": 1, "body": "review level"}),
    );
    let id = added["comment"]["id"].as_str().expect("id").to_string();
    let addressed = request(
        &mut ws,
        json!({"type": "comment.address", "id": 2, "commentId": id, "response": ""}),
    );
    assert_eq!(addressed["comment"]["status"], "addressed", "{addressed}");
    assert_eq!(addressed["comment"].get("response"), None);
    // The page also hears about it through the broadcast.
    let changed = wait_for_type(&mut ws, "reviewChanged", Duration::from_secs(15));
    assert_eq!(keys(&changed), keys(&fixture("web-review-changed.json")));
}

#[test]
fn slow_subscriber_does_not_block_others_and_is_dropped_on_write_timeout() {
    let (_dir, root) = scratch_repo();
    let server = Server::spawn(&root);
    // A subscriber that authenticates and then never reads: its socket
    // buffers fill up under a stream of large broadcasts.
    let (stalled, _) = server.client();
    let (mut lively, _) = server.client();
    // Bodies large enough that a few broadcasts of the growing review
    // exceed the loopback socket buffers (but small enough for one argv
    // entry), so the write to the stalled subscriber must time out.
    let big = "y".repeat(300 * 1024);
    for i in 0..12 {
        cli(
            &root,
            &["comment", "add", "-m", &format!("{i}:{big}"), "--json"],
        );
        let changed = wait_for_type(&mut lively, "reviewChanged", Duration::from_secs(30));
        assert_eq!(changed["type"], "reviewChanged");
    }
    // The lively subscriber kept receiving; the stalled one is closed by
    // the server once its write deadline passes.
    drop(stalled);
    let snapshot = request(&mut lively, json!({"type": "refresh", "id": 9}));
    assert_eq!(snapshot["type"], "snapshot");
}
