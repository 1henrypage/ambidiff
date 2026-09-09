//! JSON-RPC-shaped stdio engine for editor plugins (newline-delimited JSON,
//! LSP-style lifecycle, versioned handshake).
//!
//! Requests:  {"id": N, "method": "...", "params": {...}}
//! Responses: {"id": N, "result": ...} | {"id": N, "error": {code, message, data: {kind}}}
//! Notifications (engine -> client, no id): {"method": "reviewChanged"} and
//! {"method": "diffChanged"}, driven by the application's watch.
//!
//! Every method is served by the shared [`Application`]; this file only
//! decodes payloads (through `protocol.rs`), maps errors into the RPC
//! envelope, and keeps the plugin's fetch-and-splice expansion flow
//! equivalent: the engine remembers expanded gap ids per file and restores
//! them on every `view`, so the rows `expand` returns are exactly the rows
//! the next `view` carries in the gap's place.
//!
//! The protocol version is independent of the app version so the plugin
//! repo can release on its own cadence and warn on mismatch.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ambidiff_core::projection::FileFilter;
use ambidiff_core::protocol::{
    decode_comment_add, decode_comment_delete, decode_comment_edit, decode_expand,
    decode_lifecycle, decode_view,
};
use ambidiff_core::review::{Action, Actor};
use ambidiff_core::sanitize::sanitize_line;
use anyhow::Result;
use serde_json::{Value, json};

use crate::application::{AppError, Application, OutcomeValue, ReviewCommand, Snapshot};
use crate::args::EngineArgs;
use crate::context::resolve_store;

/// Bumped on breaking wire changes; the plugin warns on mismatch.
pub const PROTOCOL_VERSION: u32 = 1;

/// Every method the engine dispatches, served in `initialize.methods` so a
/// client can feature-detect additive methods without a version bump.
pub const METHODS: &[&str] = &[
    "initialize",
    "review",
    "files",
    "view",
    "expand",
    "commands",
    "comment.add",
    "comment.edit",
    "comment.delete",
    "comment.address",
    "comment.resolve",
    "comment.reopen",
    "rev.bump",
    "shutdown",
];

/// Longest request line accepted; longer lines are discarded and answered
/// with a parse error so a runaway client cannot exhaust memory.
const MAX_REQUEST_LINE: usize = 1024 * 1024;

/// How often the notifier drains the watch.
const NOTIFY_POLL: Duration = Duration::from_millis(100);

pub fn run(args: EngineArgs) -> Result<i32> {
    if !args.stdio {
        anyhow::bail!("the engine speaks stdio only; pass --stdio");
    }
    let store = resolve_store()?;
    let mut app = Application::open(store);
    // Arm BEFORE the held load: a write between the two is detected, never
    // absorbed into the watch baseline.
    if let Err(err) = app.start_watch() {
        warn(&format!("watch not started: {err}"));
    }
    if let Ok(ms) = std::env::var("AMBIDIFF_TEST_STARTUP_DELAY_MS")
        && let Ok(ms) = ms.parse::<u64>()
    {
        // Test hook: widens the arm-to-load window so the startup race is
        // observable end to end.
        std::thread::sleep(Duration::from_millis(ms));
    }
    if let Err(err) = app.load() {
        warn(&format!("initial load failed: {err}"));
    }
    Engine {
        app: Arc::new(Mutex::new(app)),
        expanded: BTreeMap::new(),
    }
    .serve()
}

fn warn(message: &str) {
    for line in message.lines() {
        eprintln!("ambidiff: warning: {}", sanitize_line(line));
    }
}

struct Engine {
    app: Arc<Mutex<Application>>,
    /// Expanded gap ids per path, restored on every view of that path.
    expanded: BTreeMap<String, BTreeSet<String>>,
}

/// Shared stdout so watch notifications interleave safely with responses;
/// a failed write marks the sink broken and the request loop exits.
struct Out {
    stdout: Mutex<std::io::Stdout>,
    broken: AtomicBool,
}

impl Out {
    fn send(&self, value: &Value) {
        let mut stdout = self.stdout.lock().unwrap_or_else(|e| e.into_inner());
        let mut write = || -> std::io::Result<()> {
            serde_json::to_writer(&mut *stdout, value)?;
            stdout.write_all(b"\n")?;
            stdout.flush()
        };
        if write().is_err() {
            self.broken.store(true, Ordering::Relaxed);
        }
    }

    fn is_broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }
}

struct RpcError {
    code: i64,
    message: String,
    kind: Option<&'static str>,
}

impl RpcError {
    fn method_not_found(method: &str) -> Self {
        RpcError {
            code: -32601,
            message: format!("unknown method {method:?}"),
            kind: None,
        }
    }

    fn parse(message: impl Into<String>) -> Self {
        RpcError {
            code: -32700,
            message: message.into(),
            kind: None,
        }
    }

    fn envelope(&self) -> Value {
        let mut error = json!({"code": self.code, "message": self.message});
        if let Some(kind) = self.kind {
            error["data"] = json!({"kind": kind});
        }
        error
    }
}

impl From<AppError> for RpcError {
    fn from(err: AppError) -> Self {
        RpcError {
            code: if err.is_invalid_input() {
                -32602
            } else {
                -32000
            },
            message: err.to_string(),
            kind: Some(err.kind()),
        }
    }
}

impl From<ambidiff_core::protocol::DecodeError> for RpcError {
    fn from(err: ambidiff_core::protocol::DecodeError) -> Self {
        RpcError::from(AppError::from(err))
    }
}

/// Read one request line, bounded. `Ok(None)` at EOF; `Err` when the line
/// exceeds the limit (the rest of it is discarded) so the caller answers
/// with a parse error and keeps serving.
fn read_request_line(stdin: &mut impl BufRead) -> std::io::Result<Option<Result<Vec<u8>, ()>>> {
    let mut line = Vec::new();
    let n = stdin
        .by_ref()
        .take(MAX_REQUEST_LINE as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_REQUEST_LINE {
        // Discard the rest of the line in bounded chunks; `read_until`
        // stops at the newline, so the next request is never swallowed.
        if line.last() != Some(&b'\n') {
            let mut chunk = Vec::new();
            loop {
                chunk.clear();
                let read = stdin.by_ref().take(8192).read_until(b'\n', &mut chunk)?;
                if read == 0 || chunk.last() == Some(&b'\n') {
                    break;
                }
            }
        }
        return Ok(Some(Err(())));
    }
    Ok(Some(Ok(line)))
}

impl Engine {
    fn serve(mut self) -> Result<i32> {
        let out = Arc::new(Out {
            stdout: Mutex::new(std::io::stdout()),
            broken: AtomicBool::new(false),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let notifier = self.start_notifier(&out, &stop);

        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        let exit = loop {
            if out.is_broken() {
                break 1;
            }
            let line = match read_request_line(&mut stdin)? {
                None => break 0,
                Some(Err(())) => {
                    out.send(&json!({
                        "id": null,
                        "error": RpcError::parse(format!(
                            "request line exceeds {MAX_REQUEST_LINE} bytes"
                        )).envelope()
                    }));
                    continue;
                }
                Some(Ok(line)) => line,
            };
            let Ok(text) = String::from_utf8(line) else {
                out.send(&json!({
                    "id": null,
                    "error": RpcError::parse("request line is not valid UTF-8").envelope()
                }));
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            let request: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    out.send(&json!({
                        "id": null,
                        "error": RpcError::parse(format!("parse error: {e}")).envelope()
                    }));
                    continue;
                }
            };
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let params = request.get("params").cloned().unwrap_or(Value::Null);

            if method == "shutdown" {
                out.send(&json!({"id": id, "result": null}));
                break 0;
            }
            let response = match self.dispatch(&method, &params) {
                Ok(result) => json!({"id": id, "result": result}),
                Err(err) => json!({"id": id, "error": err.envelope()}),
            };
            out.send(&response);
        };

        stop.store(true, Ordering::Relaxed);
        let _ = notifier.join();
        if let Ok(mutex) = Arc::try_unwrap(self.app)
            && let Ok(app) = mutex.into_inner()
        {
            app.stop();
        }
        Ok(exit)
    }

    /// Forward watch hints as notifications. The thread also keeps the
    /// application's snapshot current so a request that follows a
    /// notification sees the new state.
    fn start_notifier(
        &self,
        out: &Arc<Out>,
        stop: &Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        let app = Arc::clone(&self.app);
        let out = Arc::clone(out);
        let stop = Arc::clone(stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(NOTIFY_POLL);
                let (pending, diff_changed) = {
                    let mut app = app.lock().unwrap_or_else(|e| e.into_inner());
                    let pending = app.poll();
                    let before = app.generation();
                    if pending.diff {
                        let _ = app.refresh();
                    } else if pending.review {
                        let _ = app.load();
                    }
                    // A review edit that changes `source` reconfigures the
                    // comparison and bumps the generation: that is a diff
                    // change for the client even without a watch hint.
                    (pending, pending.diff || app.generation() != before)
                };
                if pending.review {
                    out.send(&json!({"method": "reviewChanged"}));
                }
                if diff_changed {
                    out.send(&json!({"method": "diffChanged"}));
                }
            }
        })
    }

    fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => self.initialize(),
            "review" => self.review(),
            "files" => self.files(),
            "view" => self.view(params),
            "expand" => self.expand(params),
            "commands" => {
                Ok(serde_json::to_value(ambidiff_core::commands::COMMANDS).unwrap_or(Value::Null))
            }
            "comment.add" => {
                let req = decode_comment_add(params)?;
                self.execute(ReviewCommand::Add(req))
            }
            "comment.edit" => {
                let req = decode_comment_edit(params, "id")?;
                self.execute(ReviewCommand::Edit(req))
            }
            "comment.delete" => {
                let req = decode_comment_delete(params, "id")?;
                self.execute(ReviewCommand::Delete(req))
            }
            "comment.address" => self.lifecycle(params, Action::Address, Actor::Agent),
            "comment.resolve" => self.lifecycle(params, Action::Resolve, Actor::Human),
            "comment.reopen" => self.lifecycle(params, Action::Reopen, Actor::Human),
            "rev.bump" => self.execute(ReviewCommand::RevBump),
            other => Err(RpcError::method_not_found(other)),
        }
    }

    fn lifecycle(
        &mut self,
        params: &Value,
        action: Action,
        actor: Actor,
    ) -> Result<Value, RpcError> {
        let req = decode_lifecycle(params, "id")?;
        self.execute(ReviewCommand::Lifecycle { action, actor, req })
    }

    fn execute(&mut self, cmd: ReviewCommand) -> Result<Value, RpcError> {
        let mut app = self.app.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = app.execute(cmd)?;
        Ok(match outcome.value {
            OutcomeValue::Comment(comment) => serde_json::to_value(&comment).unwrap_or(Value::Null),
            OutcomeValue::Deleted(id) => json!({"deleted": id}),
            OutcomeValue::Revision(revision) => json!({"revision": revision}),
        })
    }

    /// Every read path re-takes the held snapshot: cheap when the diff is
    /// clean, and it keeps direct file edits by agents visible immediately.
    fn with_loaded<T>(
        &self,
        f: impl FnOnce(&Application, &Snapshot) -> Result<T, RpcError>,
    ) -> Result<T, RpcError> {
        let mut app = self.app.lock().unwrap_or_else(|e| e.into_inner());
        app.poll();
        app.load()?;
        let snapshot = app.snapshot().ok_or(AppError::NotLoaded)?;
        f(&app, snapshot)
    }

    fn initialize(&self) -> Result<Value, RpcError> {
        self.with_loaded(|app, snapshot| {
            let counts = snapshot.review.counts();
            Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "appVersion": env!("CARGO_PKG_VERSION"),
                "root": app.store().root().display().to_string(),
                "review": {
                    "name": snapshot.review.review,
                    "revision": snapshot.review.revision,
                    "counts": {
                        "open": counts.open,
                        "addressed": counts.addressed,
                        "resolved": counts.resolved,
                        "reopened": counts.reopened,
                    },
                    "readOnly": snapshot.read_only,
                    "readOnlyReason": snapshot.read_only_reason,
                    "warnings": snapshot.warnings,
                },
                "generation": snapshot.generation,
                "sourceError": snapshot.source_error,
                "comparison": snapshot.comparison,
                "methods": METHODS,
            }))
        })
    }

    fn review(&self) -> Result<Value, RpcError> {
        self.with_loaded(|_, snapshot| {
            Ok(json!({
                "review": snapshot.review,
                "warnings": snapshot.warnings,
                "readOnly": snapshot.read_only,
                "readOnlyReason": snapshot.read_only_reason,
            }))
        })
    }

    fn files(&self) -> Result<Value, RpcError> {
        self.with_loaded(|_, snapshot| {
            let projection = snapshot.projection(FileFilter::All);
            let files: Vec<Value> = snapshot
                .files
                .iter()
                .map(|e| {
                    let counts = projection.counts_for(&e.path);
                    let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
                    v["commentsTodo"] = json!(counts.todo);
                    v["commentsTotal"] = json!(counts.total);
                    v
                })
                .collect();
            Ok(json!({
                "files": files,
                "reviewLevelComments": projection.review_only().total,
                "unattachedComments": projection.unattached().total,
                "skipped": snapshot.skipped,
                "sourceError": snapshot.source_error,
                "comparison": snapshot.comparison,
                "warnings": snapshot.warnings,
                "generation": snapshot.generation,
            }))
        })
    }

    fn view(&self, params: &Value) -> Result<Value, RpcError> {
        let req = decode_view(params)?;
        let wanted = self.expanded.get(&req.path).cloned().unwrap_or_default();
        self.with_loaded(|app, snapshot| {
            let mut view = app.file(&req.path, req.options.into())?;
            app.restore_expansions(&mut view, &wanted)?;
            let projection = snapshot.projection(FileFilter::All);
            let file = view.file_projection(&projection);
            Ok(json!({
                "view": file.view,
                "comments": file.comments,
                "generation": snapshot.generation,
            }))
        })
    }

    fn expand(&mut self, params: &Value) -> Result<Value, RpcError> {
        let req = decode_expand(params)?;
        let wanted = self.expanded.get(&req.path).cloned().unwrap_or_default();
        let result = self.with_loaded(|app, snapshot| {
            let mut view = app.file(&req.path, req.options.into())?;
            app.restore_expansions(&mut view, &wanted)?;
            let expansion = app.expand(&mut view, &req.gap_id)?;
            let projection = snapshot.projection(FileFilter::All);
            let file = view.file_projection(&projection);
            Ok(json!({
                "rows": expansion.rows,
                "at": expansion.at,
                "gap": expansion.gap,
                "view": file.view,
                "comments": file.comments,
                "generation": snapshot.generation,
            }))
        })?;
        self.expanded
            .entry(req.path)
            .or_default()
            .insert(req.gap_id);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_table_matches_dispatch_and_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for m in METHODS {
            assert!(seen.insert(*m), "duplicate method {m}");
        }
        assert!(METHODS.contains(&"comment.edit") && METHODS.contains(&"comment.delete"));
        assert!(METHODS.contains(&"shutdown"));
    }

    #[test]
    fn request_lines_are_bounded_and_the_rest_is_discarded() {
        let mut huge = vec![b'x'; MAX_REQUEST_LINE + 10];
        huge.push(b'\n');
        huge.extend_from_slice(b"{\"id\":1}\n");
        let mut reader = std::io::Cursor::new(huge);
        assert!(matches!(read_request_line(&mut reader), Ok(Some(Err(())))));
        let next = read_request_line(&mut reader).expect("read");
        assert_eq!(next, Some(Ok(b"{\"id\":1}\n".to_vec())));
        assert_eq!(read_request_line(&mut reader).expect("eof"), None);
    }

    #[test]
    fn a_line_exactly_at_the_limit_is_accepted() {
        let mut line = vec![b'y'; MAX_REQUEST_LINE - 1];
        line.push(b'\n');
        let mut reader = std::io::Cursor::new(line.clone());
        assert_eq!(
            read_request_line(&mut reader).expect("read"),
            Some(Ok(line))
        );
    }

    #[test]
    fn app_errors_map_to_invalid_params_or_internal_codes() {
        let invalid: RpcError = AppError::NotInChangedSet { path: "p".into() }.into();
        assert_eq!(invalid.code, -32602);
        assert_eq!(invalid.envelope()["data"]["kind"], "notInChangedSet");
        let internal: RpcError = AppError::NotLoaded.into();
        assert_eq!(internal.code, -32000);
        assert_eq!(internal.envelope()["data"]["kind"], "notLoaded");
        assert!(RpcError::parse("x").envelope().get("data").is_none());
    }
}
