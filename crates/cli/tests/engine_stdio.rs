//! Stdio engine integration: spawn the real binary, speak newline-delimited
//! JSON, assert the protocol contract the nvim plugin depends on.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

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
    assert!(out.status.success(), "git {args:?}");
}

fn write(root: &Path, path: &str, content: &str) {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(full, content).expect("write");
}

fn scratch_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    write(
        &root,
        "lib.py",
        "def f():\n    return 1\n\ndef g():\n    return 2\n",
    );
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "base"]);
    write(
        &root,
        "lib.py",
        "def f():\n    return 100\n\ndef g():\n    return 2\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args(["init", "--review", "engine-test", "--base", "HEAD"])
        .current_dir(&root)
        .output()
        .expect("init");
    assert!(out.status.success());
    (dir, root)
}

struct EngineClient {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
}

impl EngineClient {
    fn spawn(root: &Path) -> Self {
        Self::spawn_with_env(root, &[])
    }

    fn spawn_with_env(root: &Path, env: &[(&str, &str)]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
            .args(["engine", "--stdio"])
            .current_dir(root)
            .env("AMBIDIFF_AUTHOR", "engine-test")
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn engine");
        let stdin = child.stdin.take().expect("stdin");
        let reader = BufReader::new(child.stdout.take().expect("stdout"));
        EngineClient {
            child,
            stdin,
            reader,
            next_id: 1,
        }
    }

    /// Send a request; return its result, skipping interleaved notifications.
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let message = self.request_raw(method, params);
        if let Some(err) = message.get("error") {
            panic!("{method} failed: {err}");
        }
        message["result"].clone()
    }

    /// Send a request; return the whole response envelope (result or error).
    fn request_raw(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = serde_json::json!({"id": id, "method": method, "params": params});
        writeln!(self.stdin, "{request}").expect("send");
        self.stdin.flush().expect("flush");
        loop {
            let message = self.read_message();
            if message.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return message;
            }
        }
    }

    /// Send raw bytes (one line, newline included) and return the first
    /// message that is not a notification.
    fn send_raw(&mut self, bytes: &[u8]) -> serde_json::Value {
        self.stdin.write_all(bytes).expect("send raw");
        self.stdin.flush().expect("flush");
        loop {
            let message = self.read_message();
            if message.get("method").is_none() {
                return message;
            }
        }
    }

    /// Expect an error response for `method` and return its `error` object.
    fn expect_error(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let message = self.request_raw(method, params);
        message
            .get("error")
            .cloned()
            .unwrap_or_else(|| panic!("{method} unexpectedly succeeded: {message}"))
    }

    fn read_message(&mut self) -> serde_json::Value {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).expect("read line");
        assert!(n > 0, "engine closed stdout (killed by the watchdog?)");
        serde_json::from_str(&line).expect("valid ndjson from engine")
    }

    /// Wait for a notification with the given method. A cancellable
    /// watchdog kills the engine at the deadline so a missing notification
    /// fails the test instead of hanging the blocking read forever.
    fn expect_notification(&mut self, method: &str, timeout: Duration) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let deadline = Instant::now() + timeout;
        let pid = self.child.id();
        let done = Arc::new(AtomicBool::new(false));
        let done_flag = Arc::clone(&done);
        std::thread::spawn(move || {
            while Instant::now() < deadline {
                if done_flag.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        });
        while Instant::now() < deadline {
            let message = self.read_message();
            if message.get("id").is_none()
                && message.get("method").and_then(serde_json::Value::as_str) == Some(method)
            {
                done.store(true, Ordering::Relaxed);
                return;
            }
        }
        panic!("never received notification {method:?}");
    }

    fn shutdown(mut self) {
        let request = serde_json::json!({"id": 9999, "method": "shutdown"});
        let _ = writeln!(self.stdin, "{request}");
        let _ = self.stdin.flush();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    assert!(status.success(), "engine exit status");
                    return;
                }
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let _ = self.child.kill();
        panic!("engine did not exit after shutdown");
    }
}

#[test]
fn handshake_view_comment_lifecycle_round_trip() {
    let (_dir, root) = scratch_repo();
    let mut client = EngineClient::spawn(&root);

    // Handshake carries the protocol version and review summary.
    let init = client.request("initialize", serde_json::json!({}));
    assert_eq!(init["protocolVersion"], 1);
    assert_eq!(init["review"]["name"], "engine-test");
    assert_eq!(init["review"]["counts"]["open"], 0);

    // Files listing with numstat and comment counts.
    let files = client.request("files", serde_json::json!({}));
    let list = files["files"].as_array().expect("files array");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["path"], "lib.py");
    assert_eq!(list[0]["adds"], 1);
    assert_eq!(list[0]["commentsTotal"], 0);

    // A view: rows with numbers, kinds, word ranges, highlight spans.
    let view = client.request(
        "view",
        serde_json::json!({"path": "lib.py", "mode": "unified", "theme": "dark"}),
    );
    let rows = view["view"]["rows"].as_array().expect("rows");
    assert!(rows.iter().any(|r| r["type"] == "hunkHeader"));
    let del = rows
        .iter()
        .find(|r| r["cell"]["kind"] == "remove")
        .expect("remove row");
    assert_eq!(del["oldNum"], 2);
    assert_eq!(del["cell"]["text"], "    return 1");
    assert!(
        del["cell"]["wordRanges"]
            .as_array()
            .is_some_and(|w| !w.is_empty()),
        "word ranges present"
    );
    assert!(
        rows.iter()
            .any(|r| r["cell"]["hl"].as_array().is_some_and(|h| !h.is_empty())),
        "python highlights present"
    );

    // Comment through the engine; snippet captured under the lock.
    let comment = client.request(
        "comment.add",
        serde_json::json!({"path": "lib.py", "line": 2, "body": "why 100??"}),
    );
    let id = comment["id"].as_str().expect("id").to_string();
    assert_eq!(comment["snippet"], "    return 100");
    assert_eq!(comment["author"], "engine-test");

    // The view now carries the comment with its anchor.
    let view = client.request("view", serde_json::json!({"path": "lib.py"}));
    let comments = view["comments"].as_array().expect("comments");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["comment"]["id"], id.as_str());
    assert!(comments[0]["anchor"]["row"].is_u64());
    assert_eq!(comments[0]["anchor"]["outdated"], false);

    // Lifecycle through the engine.
    let addressed = client.request(
        "comment.address",
        serde_json::json!({"id": id, "response": "constant is intentional"}),
    );
    assert_eq!(addressed["status"], "addressed");
    let resolved = client.request("comment.resolve", serde_json::json!({"id": id}));
    assert_eq!(resolved["status"], "resolved");

    // Commands table is served for keymap defaults.
    let commands = client.request("commands", serde_json::json!({}));
    assert!(commands.as_array().is_some_and(|c| c.len() > 20));

    client.shutdown();
}

#[test]
fn external_review_write_produces_notification() {
    let (_dir, root) = scratch_repo();
    let mut client = EngineClient::spawn(&root);
    client.request("initialize", serde_json::json!({}));

    // An agent writes via the CLI while the engine runs.
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args(["comment", "add", "-m", "review-level note"])
        .current_dir(&root)
        .output()
        .expect("cli add");
    assert!(out.status.success());

    client.expect_notification("reviewChanged", Duration::from_secs(15));
    let review = client.request("review", serde_json::json!({}));
    assert_eq!(review["review"]["comments"][0]["body"], "review-level note");
    client.shutdown();
}

#[test]
fn expand_serves_gap_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    let body: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    write(&root, "big.txt", &body);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "base"]);
    write(&root, "big.txt", &body.replace("line 10", "line ten"));
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args(["init", "--base", "HEAD"])
        .current_dir(&root)
        .output()
        .expect("init");
    assert!(out.status.success());

    let mut client = EngineClient::spawn(&root);
    let expanded = client.request(
        "expand",
        serde_json::json!({"path": "big.txt", "gapId": "before:0"}),
    );
    let rows = expanded["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 6, "leading gap covers lines 1..6");
    assert_eq!(rows[0]["cell"]["text"], "line 1");
    assert_eq!(rows[0]["isExpansion"], true);
    client.shutdown();
}

fn fixture(name: &str) -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/contracts")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture")).expect("json")
}

fn keys(v: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = v.as_object().expect("object").keys().cloned().collect();
    keys.sort();
    keys
}

fn cli(root: &Path, args: &[&str]) -> serde_json::Value {
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args(args)
        .current_dir(root)
        .env("AMBIDIFF_AUTHOR", "henry")
        .output()
        .expect("cli");
    assert!(
        out.status.success(),
        "ambidiff {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
}

/// Rewrite the review file in place the way an agent editing it directly
/// would (no lock, whole file).
fn rewrite_review(root: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = root.join(".ambidiff.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    edit(&mut value);
    std::fs::write(&path, serde_json::to_string_pretty(&value).expect("json")).expect("write");
}

#[test]
fn initialize_and_files_carry_the_contract_fields_and_the_method_list() {
    let (_dir, root) = scratch_repo();
    let mut client = EngineClient::spawn(&root);
    let init = client.request("initialize", serde_json::json!({}));
    let expected = fixture("stdio-initialize.json");
    assert_eq!(keys(&init), keys(&expected));
    assert_eq!(keys(&init["review"]), keys(&expected["review"]));
    assert_eq!(init["methods"], expected["methods"]);
    assert_eq!(init["sourceError"], serde_json::Value::Null);
    assert_eq!(init["comparison"]["old"]["kind"], "commit");
    assert_eq!(init["comparison"]["new"]["kind"], "worktree");
    assert_eq!(init["review"]["readOnlyReason"], serde_json::Value::Null);

    let files = client.request("files", serde_json::json!({}));
    assert_eq!(keys(&files), keys(&fixture("stdio-files.json")));
    assert_eq!(files["skipped"], serde_json::json!([]));
    assert_eq!(files["generation"], init["generation"]);

    let review = client.request("review", serde_json::json!({}));
    assert_eq!(keys(&review), keys(&fixture("stdio-review.json")));

    let view = client.request("view", serde_json::json!({"path": "lib.py"}));
    assert_eq!(keys(&view), keys(&fixture("stdio-view-text.json")));
    assert_eq!(view["view"]["kind"], "text");
    client.shutdown();
}

#[test]
fn edit_and_delete_methods_round_trip() {
    let (_dir, root) = scratch_repo();
    let mut client = EngineClient::spawn(&root);
    let added = client.request(
        "comment.add",
        serde_json::json!({"path": "lib.py", "line": 2, "body": "first"}),
    );
    let id = added["id"].as_str().expect("id").to_string();
    let edited = client.request(
        "comment.edit",
        serde_json::json!({"id": id, "body": "second"}),
    );
    assert_eq!(edited["body"], "second");
    assert_eq!(edited["id"], id.as_str());
    assert_eq!(keys(&edited), keys(&fixture("stdio-comment-edit.json")));
    let err = client.expect_error("comment.edit", serde_json::json!({"id": id, "body": "  "}));
    assert_eq!(err["code"], -32602, "blank body is invalid input: {err}");

    let deleted = client.request("comment.delete", serde_json::json!({"id": id}));
    assert_eq!(deleted, serde_json::json!({"deleted": id}));
    let err = client.expect_error("comment.delete", serde_json::json!({"id": id}));
    assert_eq!(err["code"], -32602, "{err}");
    assert_eq!(err["data"]["kind"], "review");
    let review = client.request("review", serde_json::json!({}));
    assert_eq!(review["review"]["comments"], serde_json::json!([]));
    client.shutdown();
}

#[test]
fn line_values_are_validated_not_truncated() {
    let (_dir, root) = scratch_repo();
    let mut client = EngineClient::spawn(&root);
    for line in [
        serde_json::json!(4_294_967_297u64),
        serde_json::json!(-1),
        serde_json::json!("2"),
        serde_json::json!(0),
    ] {
        let err = client.expect_error(
            "comment.add",
            serde_json::json!({"path": "lib.py", "line": line, "body": "b"}),
        );
        assert_eq!(err["code"], -32602, "{line}: {err}");
        assert_eq!(err["data"]["kind"], "decode", "{line}: {err}");
        assert!(
            err["message"].as_str().is_some_and(|m| m.contains("line")),
            "{line}: {err}"
        );
    }
    let review = client.request("review", serde_json::json!({}));
    assert_eq!(
        review["review"]["comments"],
        serde_json::json!([]),
        "nothing was stored"
    );
    client.shutdown();
}

#[test]
fn unknown_side_is_rejected_and_an_omitted_side_defaults_to_new() {
    let (_dir, root) = scratch_repo();
    let mut client = EngineClient::spawn(&root);
    let err = client.expect_error(
        "comment.add",
        serde_json::json!({"path": "lib.py", "line": 2, "side": "left", "body": "b"}),
    );
    assert_eq!(err["code"], -32602, "{err}");
    assert_eq!(err["data"]["kind"], "decode");
    let added = client.request(
        "comment.add",
        serde_json::json!({"path": "lib.py", "line": 2, "body": "b"}),
    );
    assert_eq!(added["side"], "new");
    let old = client.request(
        "comment.add",
        serde_json::json!({"path": "lib.py", "line": 2, "side": "old", "body": "b"}),
    );
    assert_eq!(old["side"], "old");
    assert_eq!(
        old["snippet"], "    return 1",
        "old side reads the old endpoint"
    );
    client.shutdown();
}

#[test]
fn oversized_and_invalid_request_lines_are_parse_errors_and_the_engine_continues() {
    let (_dir, root) = scratch_repo();
    let mut client = EngineClient::spawn(&root);
    let mut huge = "{\"id\": 1, \"method\": \"initialize\", \"params\": {\"pad\": \"".to_string();
    huge.push_str(&"x".repeat(1024 * 1024 + 16));
    huge.push_str("\"}}\n");
    let reply = client.send_raw(huge.as_bytes());
    assert_eq!(reply["id"], serde_json::Value::Null, "{reply}");
    assert_eq!(reply["error"]["code"], -32700, "{reply}");

    let reply = client.send_raw(b"{\"id\": 2, \"method\": \"\xff\xfe\"}\n");
    assert_eq!(reply["error"]["code"], -32700, "{reply}");

    let init = client.request("initialize", serde_json::json!({}));
    assert_eq!(init["protocolVersion"], 1, "still serving after bad input");
    client.shutdown();
}

/// The nvim plugin expands by splicing the `expand` rows over the gap row
/// of a freshly requested `view`; the engine therefore returns views that
/// already contain every expansion, and the spliced rows must equal the
/// rows the next `view` shows, across repeated expansions and a comment
/// added in between.
#[test]
fn expand_rows_equal_the_nvim_splice_across_repeated_expansions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    let body: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    write(&root, "big.txt", &body);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "base"]);
    write(
        &root,
        "big.txt",
        &body
            .replace("line 10\n", "line ten\n")
            .replace("line 30\n", "line thirty\n"),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args(["init", "--base", "HEAD"])
        .current_dir(&root)
        .output()
        .expect("init");
    assert!(out.status.success());

    let mut client = EngineClient::spawn(&root);
    let splice = |view: &serde_json::Value, gap_id: &str, rows: &serde_json::Value| {
        let mut out = view["view"]["rows"].as_array().expect("rows").clone();
        let at = out
            .iter()
            .position(|r| r["type"] == "gap" && r["gap"]["id"] == gap_id)
            .unwrap_or_else(|| panic!("no gap {gap_id} in view"));
        out.splice(at..=at, rows.as_array().expect("rows").iter().cloned());
        serde_json::Value::Array(out)
    };

    for gap_id in ["before:0", "before:1", "trailing"] {
        let before = client.request("view", serde_json::json!({"path": "big.txt"}));
        let expanded = client.request(
            "expand",
            serde_json::json!({"path": "big.txt", "gapId": gap_id}),
        );
        assert_eq!(
            keys(&expanded),
            keys(&fixture("stdio-expand.json")),
            "{gap_id}"
        );
        assert_eq!(expanded["gap"]["id"], gap_id);
        let after = client.request("view", serde_json::json!({"path": "big.txt"}));
        assert_eq!(
            after["view"]["rows"],
            splice(&before, gap_id, &expanded["rows"]),
            "{gap_id}: the next view equals the plugin's splice"
        );
        assert_eq!(expanded["view"]["rows"], after["view"]["rows"]);
        let at = expanded["at"].as_u64().expect("at") as usize;
        assert_eq!(before["view"]["rows"][at]["type"], "gap");

        if gap_id == "before:0" {
            // A comment on a line that was collapsed a moment ago anchors to
            // the expansion row, in both the expand result and the view.
            let comment = client.request(
                "comment.add",
                serde_json::json!({"path": "big.txt", "line": 3, "body": "inside the gap"}),
            );
            let view = client.request("view", serde_json::json!({"path": "big.txt"}));
            let anchored = view["comments"]
                .as_array()
                .expect("comments")
                .iter()
                .find(|c| c["comment"]["id"] == comment["id"])
                .expect("comment anchored");
            let row = anchored["anchor"]["row"].as_u64().expect("row") as usize;
            assert_eq!(view["view"]["rows"][row]["newNum"], 3);
            assert_eq!(view["view"]["rows"][row]["isExpansion"], true);
        }
    }
    let err = client.expect_error(
        "expand",
        serde_json::json!({"path": "big.txt", "gapId": "before:9"}),
    );
    assert_eq!(err["code"], -32602);
    assert_eq!(err["data"]["kind"], "noSuchGap");
    client.shutdown();
}

#[test]
fn a_write_in_the_startup_window_is_detected() {
    let (_dir, root) = scratch_repo();
    // The hook holds the engine between arming its watch and its first
    // held load; a write in that window must both be loaded and notified.
    let mut client =
        EngineClient::spawn_with_env(&root, &[("AMBIDIFF_TEST_STARTUP_DELAY_MS", "1500")]);
    std::thread::sleep(Duration::from_millis(300));
    cli(&root, &["comment", "add", "-m", "during startup", "--json"]);
    client.expect_notification("reviewChanged", Duration::from_secs(15));
    let review = client.request("review", serde_json::json!({}));
    assert_eq!(review["review"]["comments"][0]["body"], "during startup");
    client.shutdown();
}

#[test]
fn a_source_change_reconfigures_listing_signature_and_comparison() {
    let (_dir, root) = scratch_repo();
    // Stage the edit: HEAD..worktree still lists lib.py, index..worktree
    // (the default source) lists nothing.
    git(&root, &["add", "lib.py"]);
    let mut client = EngineClient::spawn(&root);
    let init = client.request("initialize", serde_json::json!({}));
    assert_eq!(init["comparison"]["old"]["kind"], "commit");
    let files = client.request("files", serde_json::json!({}));
    assert_eq!(files["files"].as_array().map(Vec::len), Some(1));
    let generation = files["generation"].as_u64().expect("generation");

    rewrite_review(&root, |v| {
        v["source"] = serde_json::json!({"kind": "git"});
    });
    client.expect_notification("diffChanged", Duration::from_secs(15));
    let files = client.request("files", serde_json::json!({}));
    assert_eq!(files["comparison"]["old"]["kind"], "index");
    assert_eq!(files["comparison"]["new"]["kind"], "worktree");
    assert_eq!(files["files"], serde_json::json!([]));
    assert_eq!(files["sourceError"], serde_json::Value::Null);
    assert!(
        files["generation"].as_u64().expect("generation") > generation,
        "a reconfigured source bumps the generation"
    );
    client.shutdown();
}

#[test]
fn a_source_error_is_distinct_from_an_empty_changed_set() {
    let (_dir, root) = scratch_repo();
    rewrite_review(&root, |v| {
        v["source"] = serde_json::json!({"kind": "git", "base": "no-such-ref"});
    });
    let mut client = EngineClient::spawn(&root);
    let init = client.request("initialize", serde_json::json!({}));
    assert!(
        init["sourceError"]
            .as_str()
            .is_some_and(|e| e.contains("no-such-ref")),
        "{init}"
    );
    assert_eq!(init["comparison"], serde_json::Value::Null);
    let files = client.request("files", serde_json::json!({}));
    assert_eq!(files["files"], serde_json::json!([]));
    assert!(files["sourceError"].is_string(), "{files}");
    let err = client.expect_error("view", serde_json::json!({"path": "lib.py"}));
    assert_eq!(err["data"]["kind"], "notInChangedSet", "{err}");
    client.shutdown();
}

#[test]
fn addressing_with_an_empty_response_transitions_and_stores_none() {
    let (_dir, root) = scratch_repo();
    let mut client = EngineClient::spawn(&root);
    let added = client.request("comment.add", serde_json::json!({"body": "note"}));
    let id = added["id"].as_str().expect("id").to_string();
    let addressed = client.request(
        "comment.address",
        serde_json::json!({"id": id, "response": ""}),
    );
    assert_eq!(addressed["status"], "addressed");
    assert_eq!(addressed.get("response"), None, "{addressed}");
    let again = client.expect_error("comment.address", serde_json::json!({"id": id}));
    assert_eq!(
        again["code"], -32602,
        "illegal transition is invalid input: {again}"
    );
    assert_eq!(again["data"]["kind"], "review");
    client.shutdown();
}
