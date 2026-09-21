//! wasm-bindgen exports for the browser painter.
//!
//! The browser runs the SAME derivations as the native frontends: the
//! server pushes raw diff text and review JSON, and this module computes
//! rows, word-diff, highlight spans, anchors, tree, and search client-side.
//! All values cross the boundary as JSON strings (no serde-wasm-bindgen
//! dependency); errors come back as {"error": "..."} objects.
//!
//! State (review, files, loaded views) lives in a thread_local so search
//! and anchoring never re-serialize whole views across the boundary. Each
//! loaded file gets one [`ViewState`], which owns its own expansion and
//! generation bookkeeping (S1): this module no longer re-derives gap
//! slicing, comment tallies, or placements itself.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use wasm_bindgen::prelude::*;

use crate::anchor::{ActiveCell, TargetScope};
use crate::model::{FileDiff, FileDiffKind, FileEntry, FileStatus};
use crate::parser::parse_file_diff;
use crate::projection::{FileFilter, ReviewProjection};
use crate::review::{ReviewFile, Source, Status, parse_review};
use crate::stack::TargetId;
use crate::view::ViewOptions;
use crate::view_state::ViewState;

struct WasmState {
    review: Option<ReviewFile>,
    files: Vec<FileEntry>,
    /// Bumped by every [`ad_set_files`] call; the signature every loaded
    /// [`ViewState`] and every `ad_expand` content payload is checked
    /// against.
    generation: u64,
    views: BTreeMap<String, ViewState>,
    /// The stack's live targets and the selected one ([`ad_set_targets`]);
    /// empty and `None` outside stack reviews, which is the unscoped
    /// projection.
    targets: Vec<TargetId>,
    selected: Option<TargetId>,
}

impl WasmState {
    fn scope(&self) -> TargetScope {
        TargetScope {
            selected: self.selected.clone(),
            live: self.targets.clone(),
        }
    }
}

thread_local! {
    static STATE: RefCell<WasmState> = const {
        RefCell::new(WasmState {
            review: None,
            files: Vec::new(),
            generation: 0,
            views: BTreeMap::new(),
            targets: Vec::new(),
            selected: None,
        })
    };
}

/// Build the projection every export derives from (the stored review or an
/// empty one, the stored listing, the stored target scope) and hand it to
/// `f`. One builder, so the tree, a file's comments, the overview, and an
/// expansion can never disagree about scope.
fn with_projection<T>(
    s: &WasmState,
    filter: FileFilter,
    f: impl FnOnce(&ReviewProjection<'_>) -> T,
) -> T {
    let placeholder;
    let review: &ReviewFile = match &s.review {
        Some(r) => r,
        None => {
            placeholder = empty_review();
            &placeholder
        }
    };
    let projection = ReviewProjection::scoped(review, &s.files, filter, s.scope());
    f(&projection)
}

fn err_json(msg: impl std::fmt::Display) -> String {
    serde_json::json!({ "error": msg.to_string() }).to_string()
}

fn ok_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| err_json(e))
}

/// A review with no comments, used whenever no review file has been set yet
/// so every projection call has something to derive placements from.
fn empty_review() -> ReviewFile {
    ReviewFile::new(String::new(), Source::git(None), "1970-01-01T00:00:00Z")
}

fn default_entry(path: &str) -> FileEntry {
    FileEntry {
        path: path.to_string(),
        old_path: None,
        status: FileStatus::Modified,
        adds: None,
        dels: None,
    }
}

/// Decode `{mode?, wordDiff?, theme?}` into [`ViewOptions`]; unknown keys
/// (such as `oldTotalLines`, read separately by the caller) are ignored.
fn decode_opts(opts_json: &str) -> Result<(ViewOptions, Value), String> {
    let raw: Value = serde_json::from_str(opts_json).unwrap_or_default();
    let wire = crate::protocol::decode_view_options(&raw).map_err(|e| e.to_string())?;
    Ok((wire.into(), raw))
}

/// Protocol/app version handshake for the painter.
#[wasm_bindgen]
pub fn ad_version() -> String {
    serde_json::json!({
        "appVersion": env!("CARGO_PKG_VERSION"),
        "schemaMajor": crate::review::SCHEMA_MAJOR,
    })
    .to_string()
}

/// Store the review file content; returns summary + salvage warnings.
#[wasm_bindgen]
pub fn ad_set_review(content: &str) -> String {
    match parse_review(content) {
        Ok(outcome) => {
            let counts = outcome.review.counts();
            let summary = serde_json::json!({
                "name": outcome.review.review,
                "revision": outcome.review.revision,
                "counts": {
                    "open": counts.open,
                    "addressed": counts.addressed,
                    "resolved": counts.resolved,
                    "reopened": counts.reopened,
                },
                "readOnly": outcome.read_only,
                "warnings": outcome.warnings,
            });
            STATE.with(|s| s.borrow_mut().review = Some(outcome.review));
            summary.to_string()
        }
        Err(e) => err_json(e),
    }
}

/// Store the changed-file listing (JSON array of FileEntry) and bump the
/// generation every cached view and expand payload is checked against.
#[wasm_bindgen]
pub fn ad_set_files(files_json: &str) -> String {
    match serde_json::from_str::<Vec<FileEntry>>(files_json) {
        Ok(files) => STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.files = files;
            s.generation += 1;
            serde_json::json!({ "generation": s.generation }).to_string()
        }),
        Err(e) => err_json(e),
    }
}

/// Store the stack's live targets and the selected one:
/// `{"targets": [TargetId...], "selected": TargetId | null}`. Every later
/// projection (tree, file comments, overview, expand) is scoped to it.
/// Outside a stack review pass `{"targets": [], "selected": null}`.
#[wasm_bindgen]
pub fn ad_set_targets(targets_json: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Wire {
        #[serde(default)]
        targets: Vec<TargetId>,
        #[serde(default)]
        selected: Option<TargetId>,
    }
    match serde_json::from_str::<Wire>(targets_json) {
        Ok(wire) => {
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                s.targets = wire.targets;
                s.selected = wire.selected;
            });
            "{}".to_string()
        }
        Err(e) => err_json(e),
    }
}

/// The file tree (flattened rows) for the stored listing, with per-file
/// comment tallies. `collapsed_json` is a JSON array of collapsed dir paths.
/// Unfiltered (compat): always projects with [`FileFilter::All`].
#[wasm_bindgen]
pub fn ad_tree(collapsed_json: &str) -> String {
    let collapsed: BTreeSet<String> = serde_json::from_str(collapsed_json).unwrap_or_default();
    STATE.with(|s| {
        let s = s.borrow();
        with_projection(&s, FileFilter::All, |projection| {
            let snapshot = projection.snapshot(&collapsed);
            let rows_json: Vec<Value> = snapshot
                .tree
                .iter()
                .map(|row| {
                    let mut v = serde_json::to_value(row).unwrap_or_default();
                    if let Some(fi) = row.file_index {
                        v["entry"] = serde_json::to_value(&s.files[fi]).unwrap_or_default();
                    }
                    v
                })
                .collect();
            serde_json::json!({
                "rows": rows_json,
                "reviewLevelComments": snapshot.review_level_comments,
                "unattachedComments": snapshot.unattached_comments,
            })
            .to_string()
        })
    })
}

/// Parse a raw single-file diff, build (or reload) its cached view, return
/// it. `opts_json`: {mode, wordDiff, theme, oldTotalLines?}. Reloading a
/// path already cached at the current generation replays its held
/// expansions instead of discarding them (B10).
#[wasm_bindgen]
pub fn ad_load_file(path: &str, raw_diff: &str, opts_json: &str) -> String {
    let (opts, raw_opts) = match decode_opts(opts_json) {
        Ok(v) => v,
        Err(e) => return err_json(e),
    };
    let mut diff = match parse_file_diff(raw_diff) {
        Ok(d) => d,
        Err(e) => return err_json(e),
    };
    if let Some(total) = raw_opts.get("oldTotalLines").and_then(Value::as_u64) {
        diff.old_total_lines = u32::try_from(total).ok();
    }

    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let generation = s.generation;
        let entry = s
            .files
            .iter()
            .find(|e| e.path == path)
            .cloned()
            .unwrap_or_else(|| default_entry(path));
        match s.views.get_mut(path) {
            Some(state) => {
                state.set_options(opts);
                state.reload(entry, diff, generation);
            }
            None => {
                s.views.insert(
                    path.to_string(),
                    ViewState::new(entry, diff, opts, generation),
                );
            }
        }
        ok_json(s.views.get(path).expect("just inserted").view())
    })
}

/// Load (or reload) a placeholder view for a binary or too-large diff:
/// `kind_json` is `{"kind":"binary","desc":...}` or
/// `{"kind":"toolarge","adds":...,"dels":...}` -- the one placeholder-
/// capable view path every diff kind goes through (B16).
#[wasm_bindgen]
pub fn ad_load_placeholder(path: &str, kind_json: &str, opts_json: &str) -> String {
    let kind: FileDiffKind = match serde_json::from_str(kind_json) {
        Ok(k) => k,
        Err(e) => return err_json(e),
    };
    let (opts, _) = match decode_opts(opts_json) {
        Ok(v) => v,
        Err(e) => return err_json(e),
    };
    let diff = FileDiff::placeholder(kind);

    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let generation = s.generation;
        let entry = s
            .files
            .iter()
            .find(|e| e.path == path)
            .cloned()
            .unwrap_or_else(|| default_entry(path));
        match s.views.get_mut(path) {
            Some(state) => {
                state.set_options(opts);
                state.reload(entry, diff, generation);
            }
            None => {
                s.views.insert(
                    path.to_string(),
                    ViewState::new(entry, diff, opts, generation),
                );
            }
        }
        ok_json(s.views.get(path).expect("just inserted").view())
    })
}

/// Comments placed on a cached file, with row anchors and rename badges.
#[wasm_bindgen]
pub fn ad_file_comments(path: &str) -> String {
    STATE.with(|s| {
        let s = s.borrow();
        let Some(state) = s.views.get(path) else {
            return err_json(format!("{path:?} not loaded"));
        };
        with_projection(&s, FileFilter::All, |projection| {
            ok_json(&state.file_projection(projection).comments)
        })
    })
}

/// Review-level and unattached comments (for the overview pane); `wasOn`
/// names the target a comment was made on when that target left the stack.
#[wasm_bindgen]
pub fn ad_overview_comments() -> String {
    STATE.with(|s| {
        let s = s.borrow();
        with_projection(&s, FileFilter::All, |projection| {
            let records: Vec<Value> = projection
                .overview()
                .iter()
                .map(|o| {
                    serde_json::json!({
                        "comment": o.comment,
                        "unattached": o.unattached,
                        "wasOn": o.was_on,
                    })
                })
                .collect();
            ok_json(&records)
        })
    })
}

/// Search a cached file's rows.
#[wasm_bindgen]
pub fn ad_search(path: &str, query: &str) -> String {
    STATE.with(|s| {
        let s = s.borrow();
        match s.views.get(path) {
            Some(state) => ok_json(&state.search(query)),
            None => err_json(format!("{path:?} not loaded")),
        }
    })
}

/// Resolve a line number against a cached file's rows (the `:<num>` motion).
#[wasm_bindgen]
pub fn ad_goto_line(path: &str, line: u32) -> String {
    STATE.with(|s| {
        let s = s.borrow();
        match s.views.get(path) {
            Some(state) => ok_json(&crate::goto::find_line(&state.view().rows, line)),
            None => err_json(format!("{path:?} not loaded")),
        }
    })
}

/// Expand a gap in a cached view. Splices the resulting rows into the
/// cached view (so anchors and search see the expanded state) and returns
/// them alongside the now-current view and comments; no reload can silently
/// undo this (B10). Uses the state's own (current) generation.
#[wasm_bindgen]
pub fn ad_expand(path: &str, gap_id: &str, content: &str) -> String {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let generation = s.generation;
        let Some(state) = s.views.get_mut(path) else {
            return err_json(format!("{path:?} not loaded"));
        };
        state.mark_wanted(gap_id);
        let expansion = match state.expand(gap_id, content, generation) {
            Ok(e) => e,
            Err(e) => return err_json(e),
        };
        let s = &*s;
        with_projection(s, FileFilter::All, |projection| {
            let state = s.views.get(path).expect("just expanded");
            let fp = state.file_projection(projection);
            serde_json::json!({
                "expansion": expansion,
                "view": fp.view,
                "comments": fp.comments,
            })
            .to_string()
        })
    })
}

/// The full projection (filtered tree, counts, overview) for the stored
/// review and file listing. `opts_json`: `{filter?, collapsed?}`.
#[wasm_bindgen]
pub fn ad_projection(opts_json: &str) -> String {
    let raw: Value = serde_json::from_str(opts_json).unwrap_or_default();
    let filter: FileFilter = raw
        .get("filter")
        .and_then(|f| serde_json::from_value(f.clone()).ok())
        .unwrap_or(FileFilter::All);
    let collapsed: BTreeSet<String> = raw
        .get("collapsed")
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default();
    STATE.with(|s| {
        let s = s.borrow();
        with_projection(&s, filter, |projection| {
            ok_json(&projection.snapshot(&collapsed))
        })
    })
}

/// The projection (view + anchored comments) for one cached file.
#[wasm_bindgen]
pub fn ad_file_projection(path: &str) -> String {
    STATE.with(|s| {
        let s = s.borrow();
        let Some(state) = s.views.get(path) else {
            return err_json(format!("{path:?} not loaded"));
        };
        with_projection(&s, FileFilter::All, |projection| {
            ok_json(&state.file_projection(projection))
        })
    })
}

/// Apply new view options to every cached view.
#[wasm_bindgen]
pub fn ad_set_view_options(opts_json: &str) -> String {
    let (opts, _) = match decode_opts(opts_json) {
        Ok(v) => v,
        Err(e) => return err_json(e),
    };
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        for state in s.views.values_mut() {
            state.set_options(opts);
        }
    });
    "{}".to_string()
}

/// Gaps a cached file still wants expanded (requested but currently
/// collapsed, e.g. after a generation bump dropped their content).
#[wasm_bindgen]
pub fn ad_pending_expansions(path: &str) -> String {
    STATE.with(|s| {
        let s = s.borrow();
        match s.views.get(path) {
            Some(state) => ok_json(&state.pending_expansions()),
            None => err_json(format!("{path:?} not loaded")),
        }
    })
}

/// The semantic (side, line) a new comment on `row` of a cached file should
/// anchor to, given which split-mode cell has focus (`"auto"|"left"|"right"`,
/// ignored in unified mode). Returns `null` when the row carries no line on
/// either side.
#[wasm_bindgen]
pub fn ad_anchor_target(path: &str, row: usize, cell: &str) -> String {
    let Some(cell) = ActiveCell::parse(cell) else {
        return err_json(format!("unknown cell {cell:?}"));
    };
    STATE.with(|s| {
        let s = s.borrow();
        let Some(state) = s.views.get(path) else {
            return err_json(format!("{path:?} not loaded"));
        };
        match state.anchor_target(row, cell) {
            Some((side, line)) => serde_json::json!({ "side": side, "line": line }).to_string(),
            None => "null".to_string(),
        }
    })
}

/// The shared command table (web chords drive the keyboard map).
#[wasm_bindgen]
pub fn ad_commands() -> String {
    ok_json(&crate::commands::COMMANDS)
}

/// Whether deleting a comment in `status_json` should ask the human first
/// (`crate::lifecycle::Status::delete_needs_confirm`), so the browser never
/// re-spells the rule in TypeScript.
#[wasm_bindgen]
pub fn ad_delete_needs_confirm(status_json: &str) -> String {
    match serde_json::from_str::<Status>(status_json) {
        Ok(status) => ok_json(&status.delete_needs_confirm()),
        Err(e) => err_json(e),
    }
}

/// Wasm-side checks of the exports end to end: these only run under the
/// wasm32 target (`scripts/test-wasm.sh`), which is the point, since the
/// browser painter calls exactly these entry points.
#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    const REVIEW: &str = r#"{"ambidiff":1,"review":"w","revision":1,"source":{"kind":"git"},
        "createdAt":"t","updatedAt":"t","comments":[
          {"id":"c-1","rev":1,"status":"open","path":"src/a.rs","side":"new","line":2,
           "snippet":"B","body":"why??","author":"h","createdAt":"t","updatedAt":"t"},
          {"id":"c-2","rev":1,"status":"open","path":"big.js","side":"new","line":9,
           "body":"huge","author":"h","createdAt":"t","updatedAt":"t"}]}"#;
    const FILES: &str = r#"[{"path":"big.js","status":"modified","adds":12000,"dels":20},
        {"path":"src/a.rs","status":"modified","adds":1,"dels":0},
        {"path":"src/quiet.rs","status":"modified","adds":1,"dels":1}]"#;
    const RAW: &str = "@@ -1,2 +1,3 @@\n a\n+B\n c\n";
    const OPTS: &str = r#"{"mode":"unified","wordDiff":true,"theme":"none"}"#;

    fn v(s: String) -> serde_json::Value {
        let value: serde_json::Value = serde_json::from_str(&s).expect("json");
        assert!(value.get("error").is_none(), "{s}");
        value
    }

    fn seed() -> u64 {
        v(ad_set_review(REVIEW));
        v(ad_set_targets(r#"{"targets": [], "selected": null}"#));
        v(ad_set_files(FILES))["generation"].as_u64().expect("gen")
    }

    const STACK_REVIEW: &str = r#"{"ambidiff":1,"review":"w","revision":1,"source":{"kind":"git","stack":{}},
        "createdAt":"t","updatedAt":"t","comments":[
          {"id":"c-1","rev":1,"status":"open","path":"src/a.rs","side":"new","line":2,
           "target":{"kind":"branch","name":"auth-2"},"body":"mine","author":"h","createdAt":"t","updatedAt":"t"},
          {"id":"c-2","rev":1,"status":"open","path":"src/a.rs","side":"new","line":2,
           "target":{"kind":"branch","name":"auth-1"},"body":"other pr","author":"h","createdAt":"t","updatedAt":"t"},
          {"id":"c-3","rev":1,"status":"open","path":"src/a.rs","side":"new","line":2,
           "target":{"kind":"branch","name":"auth-0"},"body":"merged away","author":"h","createdAt":"t","updatedAt":"t"},
          {"id":"c-4","rev":1,"status":"open","path":"src/a.rs","side":"new","line":2,
           "body":"legacy","author":"h","createdAt":"t","updatedAt":"t"}]}"#;

    #[wasm_bindgen_test]
    fn set_targets_scopes_file_comments_overview_and_counts() {
        v(ad_set_review(STACK_REVIEW));
        v(ad_set_files(FILES));
        v(ad_set_targets(
            r#"{"targets": [{"kind":"branch","name":"auth-1"},{"kind":"branch","name":"auth-2"},{"kind":"stack"}],
                "selected": {"kind":"branch","name":"auth-2"}}"#,
        ));
        v(ad_load_file("src/a.rs", RAW, OPTS));
        let ids: Vec<String> = v(ad_file_comments("src/a.rs"))
            .as_array()
            .expect("comments")
            .iter()
            .map(|c| c["comment"]["id"].as_str().expect("id").to_string())
            .collect();
        assert_eq!(ids, vec!["c-1", "c-4"], "own target and legacy only");
        let overview = v(ad_overview_comments());
        assert_eq!(overview.as_array().expect("overview").len(), 1);
        assert_eq!(overview[0]["comment"]["id"], "c-3");
        assert_eq!(overview[0]["unattached"], true);
        assert_eq!(overview[0]["wasOn"], "auth-0");
        let projection = v(ad_projection(r#"{"filter":"all","collapsed":[]}"#));
        assert_eq!(projection["counts"]["src/a.rs"]["total"], 2);
        assert_eq!(projection["selected"]["name"], "auth-2");
        assert_eq!(projection["targetCounts"][0]["counts"]["total"], 1);
        assert_eq!(projection["targetCounts"][1]["counts"]["total"], 1);
        assert_eq!(projection["wasOnComments"], 1);
        assert_eq!(projection["untargetedComments"], 1);

        // Selecting the other PR flips which comment is visible.
        v(ad_set_targets(
            r#"{"targets": [{"kind":"branch","name":"auth-1"},{"kind":"branch","name":"auth-2"},{"kind":"stack"}],
                "selected": {"kind":"branch","name":"auth-1"}}"#,
        ));
        let ids: Vec<String> = v(ad_file_comments("src/a.rs"))
            .as_array()
            .expect("comments")
            .iter()
            .map(|c| c["comment"]["id"].as_str().expect("id").to_string())
            .collect();
        assert_eq!(ids, vec!["c-2", "c-4"]);
        let bad = ad_set_targets(r#"{"targets": ["auth-1"]}"#);
        assert!(bad.contains("error"), "{bad}");
        seed();
    }

    #[wasm_bindgen_test]
    fn set_files_bumps_the_generation() {
        let g1 = seed();
        let g2 = v(ad_set_files(FILES))["generation"].as_u64().expect("gen");
        assert!(g2 > g1);
    }

    #[wasm_bindgen_test]
    fn expand_paints_from_the_cached_view_and_survives_a_same_generation_reload() {
        seed();
        let opts = r#"{"mode":"unified","wordDiff":true,"theme":"none","oldTotalLines":5}"#;
        let view = v(ad_load_file("src/a.rs", RAW, opts));
        let rows_before = view["rows"].as_array().expect("rows").len();
        assert_eq!(view["rows"][rows_before - 1]["type"], "gap");

        let result = v(ad_expand("src/a.rs", "trailing", "a\nB\nc\nd\ne\nf\n"));
        let rows = result["expansion"]["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 3);
        assert_eq!(result["expansion"]["at"], rows_before - 1);
        let expanded_len = result["view"]["rows"].as_array().expect("rows").len();
        assert_eq!(expanded_len, rows_before - 1 + 3);
        assert_eq!(result["comments"][0]["comment"]["id"], "c-1");

        // The projection shows the expanded view, and reloading the same
        // raw diff at the same generation keeps the expansion (B10).
        assert_eq!(
            v(ad_file_projection("src/a.rs"))["view"]["rows"]
                .as_array()
                .expect("rows")
                .len(),
            expanded_len
        );
        let reloaded = v(ad_load_file("src/a.rs", RAW, opts));
        assert_eq!(
            reloaded["rows"].as_array().expect("rows").len(),
            expanded_len
        );
        assert!(
            v(ad_pending_expansions("src/a.rs"))
                .as_array()
                .expect("gaps")
                .is_empty()
        );

        // A new generation drops the content but remembers the wish.
        v(ad_set_files(FILES));
        let fresh = v(ad_load_file("src/a.rs", RAW, opts));
        assert_eq!(fresh["rows"].as_array().expect("rows").len(), rows_before);
        let pending = v(ad_pending_expansions("src/a.rs"));
        assert_eq!(pending[0]["id"], "trailing");
    }

    #[wasm_bindgen_test]
    fn placeholder_views_keep_their_comments_and_serialise_counts_once() {
        seed();
        let view = v(ad_load_placeholder(
            "big.js",
            r#"{"kind":"toolarge","adds":12000,"dels":20}"#,
            OPTS,
        ));
        assert_eq!(view["kind"], "toolarge");
        assert_eq!(view["adds"], 12000);
        assert_eq!(view["dels"], 20);
        assert!(view["rows"].as_array().expect("rows").is_empty());
        let text = ad_load_placeholder(
            "big.js",
            r#"{"kind":"toolarge","adds":12000,"dels":20}"#,
            OPTS,
        );
        assert_eq!(text.matches("\"adds\"").count(), 1, "{text}");
        let projection = v(ad_file_projection("big.js"));
        assert_eq!(projection["comments"][0]["comment"]["id"], "c-2");
        assert_eq!(
            projection["comments"][0]["anchor"]["row"],
            serde_json::Value::Null
        );
        assert_eq!(projection["comments"][0]["anchor"]["clamped"], true);
    }

    #[wasm_bindgen_test]
    fn projection_filters_the_tree_and_counts_per_path() {
        seed();
        let all = v(ad_projection(r#"{"filter":"all","collapsed":[]}"#));
        assert_eq!(all["files"].as_array().expect("files").len(), 3);
        assert_eq!(all["counts"]["src/a.rs"]["todo"], 1);
        let annotated = v(ad_projection(r#"{"filter":"annotated","collapsed":[]}"#));
        let names: Vec<&str> = annotated["tree"]
            .as_array()
            .expect("tree")
            .iter()
            .filter(|r| r["isDir"] == false)
            .filter_map(|r| r["name"].as_str())
            .collect();
        // Directories sort before files: `src/a.rs` precedes `big.js`.
        assert_eq!(names, vec!["a.rs", "big.js"]);
        let unreviewed = v(ad_projection(r#"{"filter":"unreviewed","collapsed":[]}"#));
        let names: Vec<&str> = unreviewed["tree"]
            .as_array()
            .expect("tree")
            .iter()
            .filter(|r| r["isDir"] == false)
            .filter_map(|r| r["name"].as_str())
            .collect();
        assert_eq!(names, vec!["quiet.rs"]);
    }

    #[wasm_bindgen_test]
    fn anchor_target_search_and_view_options_follow_the_cached_view() {
        seed();
        v(ad_load_file("src/a.rs", RAW, OPTS));
        // Row 2 is the added line "B" on the new side.
        assert_eq!(
            v(ad_anchor_target("src/a.rs", 2, "auto")),
            serde_json::json!({"side": "new", "line": 2})
        );
        assert_eq!(
            ad_anchor_target("src/a.rs", 0, "auto"),
            "null",
            "the hunk header carries no line"
        );
        let hits = v(ad_search("src/a.rs", "b"));
        assert_eq!(hits.as_array().expect("hits").len(), 1);
        v(ad_set_view_options(
            r#"{"mode":"split","wordDiff":false,"theme":"none"}"#,
        ));
        let split = v(ad_file_projection("src/a.rs"));
        assert_eq!(split["view"]["mode"], "split");
        assert_eq!(split["view"]["rows"][1]["type"], "split");
    }

    #[wasm_bindgen_test]
    fn goto_line_resolves_against_the_cached_view() {
        seed();
        v(ad_load_file("src/a.rs", RAW, OPTS));
        // RAW is "@@ -1,2 +1,3 @@\n a\n+B\n c\n": row 2 is the added "B" line.
        assert_eq!(
            v(ad_goto_line("src/a.rs", 2)),
            serde_json::json!({"kind": "exact", "row": 2, "side": "new"})
        );
        assert_eq!(
            ad_goto_line("nope.rs", 1),
            err_json("\"nope.rs\" not loaded")
        );
    }

    #[wasm_bindgen_test]
    fn delete_needs_confirm_matches_lifecycle_for_every_status() {
        assert_eq!(v(ad_delete_needs_confirm("\"open\"")), true);
        assert_eq!(v(ad_delete_needs_confirm("\"addressed\"")), true);
        assert_eq!(v(ad_delete_needs_confirm("\"reopened\"")), true);
        assert_eq!(v(ad_delete_needs_confirm("\"resolved\"")), false);
        let err = ad_delete_needs_confirm("\"bogus\"");
        assert!(err.contains("error"), "{err}");
    }
}
