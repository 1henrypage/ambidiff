//! Wire contract fixtures against the core: request payloads decode exactly
//! as `fixtures/contracts/request-*.json` specify, and the core-level value
//! shapes (comment, review file, file entry, file view, anchored comment,
//! expansion rows) serialise byte-for-byte to the response fixtures.
//!
//! Transport envelopes (`stdio-*` results, `web-*` messages) are asserted by
//! the CLI crate's engine and web tests against these same files, and the
//! browser decoders by `web/src/protocol.test.ts`.

#![cfg(not(target_arch = "wasm32"))]

use std::path::PathBuf;

use serde_json::{Value, json};

use ambidiff_core::anchor::{Placement, anchor_comments_to_rows, place_comments};
use ambidiff_core::model::{FileEntry, FileStatus};
use ambidiff_core::parser::parse_file_diff;
use ambidiff_core::protocol::{
    DecodeError, decode_comment_add, decode_comment_delete, decode_comment_edit, decode_expand,
    decode_lifecycle, decode_view, decode_view_options,
};
use ambidiff_core::review::{Comment, parse_review, to_json};
use ambidiff_core::rows::{GapInfo, Row, ViewMode, build_expansion_rows};
use ambidiff_core::view::{ViewOptions, build_file_view};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/contracts")
}

fn fixture(name: &str) -> Value {
    let path = fixture_dir().join(name);
    let content =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&content).unwrap_or_else(|e| panic!("{name} is not JSON: {e}"))
}

fn error_summary(err: &DecodeError) -> Value {
    json!({ "kind": err.kind(), "field": err.field() })
}

/// Run one request fixture through `decode`, comparing each case's decoded
/// value (serialised) or error summary against the fixture.
fn check_request_cases(
    file: &str,
    decode: impl Fn(&Value, Option<&str>) -> Result<Value, DecodeError>,
) {
    let doc = fixture(file);
    let cases = doc["cases"].as_array().expect("cases array");
    assert!(!cases.is_empty(), "{file}: no cases");
    for case in cases {
        let name = case["name"].as_str().expect("case name");
        let id_field = case.get("idField").and_then(Value::as_str);
        let got = decode(&case["payload"], id_field);
        match (case.get("ok"), case.get("error")) {
            (Some(expected), None) => {
                let value = got.unwrap_or_else(|e| panic!("{file}: {name}: unexpected error {e}"));
                assert_eq!(&value, expected, "{file}: {name}");
            }
            (None, Some(expected)) => {
                let err = match got {
                    Ok(value) => panic!("{file}: {name}: expected an error, decoded {value}"),
                    Err(e) => e,
                };
                assert_eq!(&error_summary(&err), expected, "{file}: {name}");
            }
            _ => panic!("{file}: {name}: a case needs exactly one of ok / error"),
        }
    }
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("serialise")
}

#[test]
fn request_fixtures_decode_as_specified() {
    check_request_cases("request-comment-add.json", |v, _| {
        decode_comment_add(v).map(|r| to_value(&r))
    });
    check_request_cases("request-comment-edit.json", |v, id| {
        decode_comment_edit(v, id.expect("idField")).map(|r| to_value(&r))
    });
    check_request_cases("request-comment-delete.json", |v, id| {
        decode_comment_delete(v, id.expect("idField")).map(|r| to_value(&r))
    });
    check_request_cases("request-lifecycle.json", |v, id| {
        decode_lifecycle(v, id.expect("idField")).map(|r| to_value(&r))
    });
    check_request_cases("request-view-options.json", |v, _| {
        decode_view_options(v).map(|r| to_value(&r))
    });
    check_request_cases("request-view.json", |v, _| {
        decode_view(v).map(|r| to_value(&r))
    });
    check_request_cases("request-expand.json", |v, _| {
        decode_expand(v).map(|r| to_value(&r))
    });
}

#[test]
fn every_envelope_payload_decodes_with_its_transport_spelling() {
    let doc = fixture("request-envelopes.json");
    for req in doc["stdio"].as_array().expect("stdio envelopes") {
        let method = req["method"].as_str().expect("method");
        let params = req.get("params").cloned().unwrap_or(json!({}));
        let result = match method {
            "view" => decode_view(&params).map(|_| ()),
            "expand" => decode_expand(&params).map(|_| ()),
            "comment.add" => decode_comment_add(&params).map(|_| ()),
            "comment.edit" => decode_comment_edit(&params, "id").map(|_| ()),
            "comment.delete" => decode_comment_delete(&params, "id").map(|_| ()),
            "comment.address" | "comment.resolve" | "comment.reopen" => {
                decode_lifecycle(&params, "id").map(|_| ())
            }
            _ => Ok(()),
        };
        result.unwrap_or_else(|e| panic!("stdio {method}: {e}"));
    }
    for msg in doc["web"].as_array().expect("web envelopes") {
        let kind = msg["type"].as_str().expect("type");
        let result = match kind {
            "comment.add" => decode_comment_add(msg).map(|_| ()),
            "comment.edit" => decode_comment_edit(msg, "commentId").map(|_| ()),
            "comment.delete" => decode_comment_delete(msg, "commentId").map(|_| ()),
            "comment.address" | "comment.resolve" | "comment.reopen" => {
                decode_lifecycle(msg, "commentId").map(|_| ())
            }
            _ => Ok(()),
        };
        result.unwrap_or_else(|e| panic!("web {kind}: {e}"));
        assert!(
            kind == "auth" || msg.get("id").is_some(),
            "web {kind}: every request except auth carries an id"
        );
    }
}

#[test]
fn comment_fixtures_are_the_core_serialisation() {
    for file in [
        "stdio-comment-add.json",
        "stdio-comment-edit.json",
        "stdio-comment-address.json",
    ] {
        let doc = fixture(file);
        let comment: Comment =
            serde_json::from_value(doc.clone()).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(to_value(&comment), doc, "{file}: comment round trip");
    }
    let web = fixture("web-comment.json");
    assert_eq!(web["comment"], fixture("stdio-comment-add.json"));
}

#[test]
fn review_fixture_parses_cleanly_and_reserialises_identically() {
    let doc = fixture("stdio-review.json");
    let outcome = parse_review(&doc["review"].to_string()).expect("parse");
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    assert!(!outcome.read_only);
    assert_eq!(to_value(&outcome.review), doc["review"]);

    // The browser receives the review as a string: the same document,
    // pretty-printed the way the store writes it.
    let hello = fixture("web-hello.json");
    let text = hello["review"].as_str().expect("review string");
    let parsed = parse_review(text).expect("parse hello review");
    assert_eq!(parsed.review, outcome.review);
    assert_eq!(
        to_json(&parsed.review).expect("serialise"),
        text,
        "canonical on-disk form"
    );
}

fn fixture_entry() -> FileEntry {
    FileEntry {
        path: "src/login.ts".into(),
        old_path: None,
        status: FileStatus::Modified,
        adds: Some(1),
        dels: Some(0),
    }
}

#[test]
fn file_entry_fixtures_are_the_core_serialisation() {
    let entry = fixture_entry();
    assert_eq!(to_value(&entry), fixture("web-file-text.json")["entry"]);
    assert_eq!(
        to_value(&vec![entry.clone()]),
        fixture("web-diff-changed.json")["files"]
    );
    let listed: Vec<FileEntry> =
        serde_json::from_value(fixture("web-hello.json")["files"].clone()).expect("entries");
    assert_eq!(listed, vec![entry]);
}

fn fixture_view_options() -> ViewOptions {
    ViewOptions {
        mode: ViewMode::Unified,
        word_diff: true,
        highlight: None,
    }
}

#[test]
fn text_view_fixture_is_the_core_view_with_its_anchored_comment() {
    let raw = fixture("web-file-text.json")["raw"]
        .as_str()
        .expect("raw")
        .to_string();
    let mut diff = parse_file_diff(&raw).expect("parse");
    diff.old_total_lines = Some(5);
    let view = build_file_view(&fixture_entry(), &diff, fixture_view_options());
    let expected = fixture("stdio-view-text.json");
    assert_eq!(to_value(&view), expected["view"]);

    let review = parse_review(&fixture("stdio-review.json")["review"].to_string())
        .expect("parse")
        .review;
    let files = vec![fixture_entry()];
    let placements = place_comments(&review.comments, &files);
    assert_eq!(
        placements[0],
        Placement::File {
            path: "src/login.ts".into(),
            was_path: None
        }
    );
    let refs: Vec<&Comment> = review.comments.iter().collect();
    let anchors = anchor_comments_to_rows(&view.rows, &refs);
    let records: Vec<Value> = refs
        .iter()
        .zip(&anchors)
        .map(|(comment, anchor)| json!({"comment": comment, "anchor": anchor, "wasPath": null}))
        .collect();
    assert_eq!(Value::Array(records), expected["comments"]);
}

#[test]
fn too_large_view_fixture_carries_each_count_once() {
    // B16 regression: a too-large placeholder used to flatten its own
    // adds/dels alongside the view's own fields, emitting each wire count
    // twice. FileDiff::placeholder + FileView's manual Serialize impl are
    // the one placeholder-capable path that must not repeat itself.
    use ambidiff_core::model::FileDiff;

    let diff = FileDiff::placeholder(ambidiff_core::model::FileDiffKind::TooLarge {
        adds: 12_000,
        dels: 20,
    });
    let entry = FileEntry {
        path: "vendor/bundle.js".into(),
        old_path: None,
        status: FileStatus::Modified,
        adds: None,
        dels: None,
    };
    let view = build_file_view(&entry, &diff, fixture_view_options());
    let expected = fixture("stdio-view-toolarge.json");
    assert_eq!(to_value(&view), expected["view"]);
}

#[test]
fn binary_view_fixture_is_the_core_view() {
    let raw = "diff --git a/logo.png b/logo.png\nBinary files a/logo.png and b/logo.png differ\n";
    let diff = parse_file_diff(raw).expect("parse");
    let entry = FileEntry {
        path: "logo.png".into(),
        old_path: None,
        status: FileStatus::Modified,
        adds: None,
        dels: None,
    };
    let view = build_file_view(&entry, &diff, fixture_view_options());
    assert_eq!(to_value(&view), fixture("stdio-view-binary.json")["view"]);
}

#[test]
fn expand_fixture_rows_are_the_core_expansion_rows() {
    let expected = fixture("stdio-expand.json");
    let gap: GapInfo = serde_json::from_value(expected["gap"].clone()).expect("gap");
    let content = fixture("web-src.json")["content"]
        .as_str()
        .expect("content")
        .to_string();
    let lines: Vec<&str> = content.split('\n').collect();
    let start = (gap.new_range.0 - 1) as usize;
    let end = gap.new_range.1 as usize;
    let rows = build_expansion_rows(&gap, &lines[start..end], ViewMode::Unified, 0);
    assert_eq!(to_value(&rows), expected["rows"]);

    // Splicing the rows over the gap row yields the expanded view.
    let mut view_rows: Vec<Row> =
        serde_json::from_value(fixture("stdio-view-text.json")["view"]["rows"].clone())
            .expect("rows");
    let at = expected["at"].as_u64().expect("at") as usize;
    assert!(matches!(view_rows[at], Row::Gap { .. }));
    view_rows.splice(at..=at, rows);
    assert_eq!(to_value(&view_rows), expected["view"]["rows"]);
}

#[test]
fn web_error_fixture_carries_the_decoder_message() {
    let doc = fixture("web-error.json");
    let err = DecodeError::OutOfRange {
        field: "line".into(),
    };
    assert_eq!(doc["code"], "decode");
    assert_eq!(doc["message"], err.to_string());
}
