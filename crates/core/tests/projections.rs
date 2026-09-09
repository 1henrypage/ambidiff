//! Projection conformance harness (S6): `fixtures/projections/` cases,
//! hand-worked from semantics, run natively (reading from disk) and under
//! wasm32 (embedded via `include_dir`, mirroring `tests/conformance.rs`).
//! See `fixtures/projections/README.md` for the case list and layout.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use ambidiff_core::model::{FileDiff, FileDiffKind, FileEntry, FileStatus};
use ambidiff_core::parser::parse_file_diff;
use ambidiff_core::projection::{FileFilter, ReviewProjection};
use ambidiff_core::review::{Comment, ReviewFile, Side, Source, Status};
use ambidiff_core::rows::{Row, ViewMode};
use ambidiff_core::view::ViewOptions;
use ambidiff_core::view_state::ViewState;

// Native runs read fixtures from disk so edits are always fresh; the wasm
// build embeds them at compile time (no filesystem there). The wasm test
// runner touches this file first so the embed can never go stale.
#[cfg(target_arch = "wasm32")]
static CASES: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/../../fixtures/projections");

#[cfg(not(target_arch = "wasm32"))]
fn read_json(case: &str, name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/projections")
        .join(case)
        .join(name);
    let content =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&content).unwrap_or_else(|e| panic!("{case}/{name} is not JSON: {e}"))
}

#[cfg(target_arch = "wasm32")]
fn read_json(case: &str, name: &str) -> Value {
    let file = CASES
        .get_file(format!("{case}/{name}"))
        .unwrap_or_else(|| panic!("{case}/{name} missing from the embed"));
    let content = String::from_utf8_lossy(file.contents());
    serde_json::from_str(&content).unwrap_or_else(|e| panic!("{case}/{name} is not JSON: {e}"))
}

fn comment_from(v: &Value) -> Comment {
    let status: Status = serde_json::from_value(v["status"].clone()).expect("status");
    Comment {
        id: v["id"].as_str().expect("id").to_string(),
        rev: 1,
        status,
        path: v["path"].as_str().map(str::to_string),
        side: None,
        line: None,
        end_line: None,
        snippet: None,
        body: "b".to_string(),
        response: None,
        author: "a".to_string(),
        created_at: "t".to_string(),
        updated_at: "t".to_string(),
        extra: Map::new(),
    }
}

fn review_from(comments: &[Value]) -> ReviewFile {
    let mut review = ReviewFile::new("fixture".to_string(), Source::git(None), "t");
    review.comments = comments.iter().map(comment_from).collect();
    review
}

fn files_from(v: &Value) -> Vec<FileEntry> {
    serde_json::from_value(v.clone()).expect("files array")
}

fn entry(path: &str) -> FileEntry {
    FileEntry {
        path: path.to_string(),
        old_path: None,
        status: FileStatus::Modified,
        adds: None,
        dels: None,
    }
}

// ---------------------------------------------------------------- cases

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn filtered_tree() {
    let input = read_json("filtered-tree", "input.json");
    let expected = read_json("filtered-tree", "expected.json");
    let files = files_from(&input["files"]);
    let comments = input["comments"].as_array().expect("comments").clone();
    let review = review_from(&comments);
    let filter: FileFilter = serde_json::from_value(input["filter"].clone()).expect("filter");
    let projection = ReviewProjection::new(&review, &files, filter);

    let visible = projection.visible_files();
    assert_eq!(
        serde_json::to_value(&visible).expect("json"),
        expected["visibleFiles"]
    );

    let tree = projection.tree_rows(&BTreeSet::new());
    assert_eq!(serde_json::to_value(&tree).expect("json"), expected["tree"]);

    assert_eq!(
        serde_json::to_value(projection.counts_for("src/a.rs")).expect("json"),
        expected["countsSrcA"]
    );
    assert_eq!(
        serde_json::to_value(projection.counts_for("docs/readme.md")).expect("json"),
        expected["countsReadme"]
    );
    assert_eq!(
        serde_json::to_value(projection.counts_for("src/b.rs")).expect("json"),
        expected["countsSrcB"]
    );
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn rename_unattached() {
    let input = read_json("rename-unattached", "input.json");
    let expected = read_json("rename-unattached", "expected.json");
    let files = files_from(&input["files"]);
    let comments = input["comments"].as_array().expect("comments").clone();
    let review = review_from(&comments);
    let projection = ReviewProjection::new(&review, &files, FileFilter::All);

    let placed = projection.file_comments("new/name.rs");
    let placed_json: Vec<Value> = placed
        .iter()
        .map(|p| json!({"index": p.index, "commentId": p.comment.id, "wasPath": p.was_path}))
        .collect();
    assert_eq!(Value::Array(placed_json), expected["fileComments"]);

    let overview = projection.overview();
    let overview_json: Vec<Value> = overview
        .iter()
        .map(|o| json!({"commentId": o.comment.id, "unattached": o.unattached}))
        .collect();
    assert_eq!(Value::Array(overview_json), expected["overview"]);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn placeholder_kinds() {
    let input = read_json("placeholder-kinds", "input.json");
    let expected = read_json("placeholder-kinds", "expected.json");
    let cases = input["cases"].as_array().expect("cases");
    let expected_views = expected["views"].as_array().expect("views");
    assert_eq!(cases.len(), expected_views.len());
    for (case, expected_view) in cases.iter().zip(expected_views) {
        let path = case["path"].as_str().expect("path");
        let kind: FileDiffKind = serde_json::from_value(case["kind"].clone()).expect("kind");
        let diff = FileDiff::placeholder(kind);
        let view =
            ambidiff_core::view::build_file_view(&entry(path), &diff, ViewOptions::default());
        assert_eq!(serde_json::to_value(&view).expect("json"), *expected_view);
        // B16 regression, restated here: each wire count appears once.
        let text = serde_json::to_string(&view).expect("serialize");
        assert_eq!(text.matches("\"adds\"").count(), 1);
        assert_eq!(text.matches("\"dels\"").count(), 1);
    }
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn unicode_search() {
    let input = read_json("unicode-search", "input.json");
    let expected = read_json("unicode-search", "expected.json");
    let path = input["path"].as_str().expect("path");
    let raw = input["diff"].as_str().expect("diff");
    let query = input["query"].as_str().expect("query");
    let diff = parse_file_diff(raw).expect("parse");
    let state = ViewState::new(entry(path), diff, ViewOptions::default(), 1);
    let matches = state.search(query);
    assert_eq!(
        serde_json::to_value(&matches).expect("json"),
        expected["matches"]
    );
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn expansion_option_change() {
    let input = read_json("expansion-option-change", "input.json");
    let expected = read_json("expansion-option-change", "expected.json");
    let path = input["path"].as_str().expect("path");
    let raw = input["diff"].as_str().expect("diff");
    let old_total = input["oldTotalLines"].as_u64().expect("oldTotalLines") as u32;
    let gap_id = input["gapId"].as_str().expect("gapId");
    let content = input["content"].as_str().expect("content");
    let generation = input["generation"].as_u64().expect("generation");

    let mut diff = parse_file_diff(raw).expect("parse");
    diff.old_total_lines = Some(old_total);
    let mut state = ViewState::new(entry(path), diff, ViewOptions::default(), generation);

    state.mark_wanted(gap_id);
    state
        .expand(gap_id, content, generation)
        .expect("expand ok");
    let after_expand = &expected["afterExpand"];
    assert_eq!(state.view().mode, ViewMode::Unified);
    assert_eq!(
        state.view().rows.len(),
        after_expand["rowCount"].as_u64().expect("rowCount") as usize
    );
    assert_eq!(
        state
            .view()
            .rows
            .iter()
            .any(|r| matches!(r, Row::Gap { .. })),
        after_expand["hasGap"].as_bool().expect("hasGap")
    );

    state.set_options(ViewOptions {
        mode: ViewMode::Split,
        ..ViewOptions::default()
    });
    let after_split = &expected["afterSetOptionsSplit"];
    assert_eq!(state.view().mode, ViewMode::Split);
    assert_eq!(
        state.view().rows.len(),
        after_split["rowCount"].as_u64().expect("rowCount") as usize
    );
    assert_eq!(
        state
            .view()
            .rows
            .iter()
            .any(|r| matches!(r, Row::Gap { .. })),
        after_split["hasGap"].as_bool().expect("hasGap")
    );
    let Row::Split { left, right, .. } = state.view().rows.last().expect("a last row") else {
        panic!("expected the last row to be a split row");
    };
    assert_eq!(
        left.text,
        after_split["lastRowLeftText"].as_str().expect("text")
    );
    assert_eq!(
        left.line,
        Some(after_split["lastRowLeftLine"].as_u64().expect("line") as u32)
    );
    assert_eq!(
        right.text,
        after_split["lastRowRightText"].as_str().expect("text")
    );
    assert_eq!(
        right.line,
        Some(after_split["lastRowRightLine"].as_u64().expect("line") as u32)
    );
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn line_extrema() {
    let input = read_json("line-extrema", "input.json");
    let raw = input["insertionAtU32Max"].as_str().expect("raw");
    let diff = parse_file_diff(raw).expect("parse must accept this header (B24)");
    let rows = ambidiff_core::rows::build_rows(&diff, ambidiff_core::rows::BuildOptions::default());
    // The point of this case: parsing and row-building this boundary header
    // never panics.
    assert!(!rows.is_empty());

    let trailing_raw = input["trailingGapDiff"].as_str().expect("diff");
    let total = input["trailingGapOldTotalLines"].as_u64().expect("total") as u32;
    let mut trailing_diff = parse_file_diff(trailing_raw).expect("parse");
    trailing_diff.old_total_lines = Some(total);
    let rows = ambidiff_core::rows::build_rows(
        &trailing_diff,
        ambidiff_core::rows::BuildOptions::default(),
    );
    let Some(Row::Gap { gap, .. }) = rows.last() else {
        panic!("expected a trailing gap");
    };
    assert_eq!(gap.old_range, (2, u32::MAX));

    // Smoke check that this module reaches the same `Side` the wire
    // protocol uses (exercised properly by the other cases' anchors).
    let _ = Side::New;
}

// ------------------------------------------------------------- properties

/// The full pipeline (a single hunk at arbitrary u32 extents, fed through
/// the row builder in both modes) is total: it never panics, no matter how
/// close old/new starts and lengths sit to u32::MAX, as long as each side's
/// own extent still fits u32 (the parser's own boundary; reproduced here
/// directly since the `Hunk` is built by hand rather than through the
/// parser). Native only: proptest is not a wasm32 dev-dependency.
#[cfg(not(target_arch = "wasm32"))]
mod props {
    use ambidiff_core::model::{DiffLine, FileDiff, FileDiffKind, Hunk, LineKind};
    use ambidiff_core::rows::{BuildOptions, ViewMode, build_rows};
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn pipeline_is_total_at_full_u32_range(
            old_start in 0u32..=u32::MAX,
            old_len in 0u32..=4,
            new_start in 1u32..=u32::MAX,
            new_len in 1u32..=4,
        ) {
            // Keep each side's own extent within the u32 range the parser
            // itself would accept (start + len <= 2^32).
            prop_assume!((old_start as u64) + (old_len as u64) <= 1u64 << 32);
            prop_assume!((new_start as u64) + (new_len as u64) <= 1u64 << 32);

            let mut lines = Vec::new();
            let mut old_num = old_start;
            let mut new_num = new_start;
            for _ in 0..old_len.max(new_len).max(1) {
                lines.push(DiffLine {
                    old_num,
                    new_num,
                    kind: LineKind::Context,
                    content: "x".to_string(),
                });
                old_num = old_num.saturating_add(1);
                new_num = new_num.saturating_add(1);
            }
            let diff = FileDiff {
                kind: FileDiffKind::Text,
                hunks: vec![Hunk {
                    old_start,
                    old_len,
                    new_start,
                    new_len,
                    context: String::new(),
                    lines,
                }],
                old_total_lines: Some(old_start.saturating_add(old_len).max(1)),
                old_missing_newline: false,
                new_missing_newline: false,
            };
            for mode in [ViewMode::Unified, ViewMode::Split] {
                let _ = build_rows(&diff, BuildOptions { mode, word_diff: true });
            }
        }
    }
}
