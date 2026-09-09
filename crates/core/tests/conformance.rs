//! Conformance harness: the drift firewall.
//!
//! Hand-written adversarial fixtures under `fixtures/cases/` carry golden
//! row-model and word-diff expectations written from semantics, so a buggy
//! primitive cannot self-validate. The exact same corpus and assertions run
//! natively and under wasm (`wasm-pack test`), and highlight output is
//! parity-hashed between the two targets.
//!
//! Each case directory holds:
//!   input.diff                raw git diff output
//!   meta.json                 { description, path, oldTotalLines?, kind? }
//!   expected/rows-unified.json  compact row projection (see below)
//!   expected/rows-split.json
//!   expected/highlight.hash     parity hash of highlight output
//!
//! Compact projection grammar (hand-writable, semantic, no key hashes):
//!   ["hunk", text]
//!   ["gap", id, count, [oldStart, oldEnd], [newStart, newEnd]]
//!   ["ctx", oldNum, newNum, text]
//!   ["add", newNum, text] or ["add", newNum, text, [[s,e],..]]
//!   ["del", oldNum, text] or ["del", oldNum, text, [[s,e],..]]
//!   ["row", cell, cell]  with cell = null | ["ctx",n,text] | ["add"|"del",n,text(,ranges)]
//!
//! Regenerate with UPDATE_FIXTURES=1 (native only), then re-review every
//! changed expectation against the intended semantics before committing.

use serde_json::{Value, json};

use ambidiff_core::highlight::{ThemeChoice, highlight_diff};
use ambidiff_core::model::{FileDiff, FileDiffKind};
use ambidiff_core::parser::parse_file_diff;
use ambidiff_core::rows::{BuildOptions, Cell, CellKind, Row, ViewMode, build_rows};
use ambidiff_core::util::fnv1a64;

// Native runs read fixtures from disk so edits are always fresh; the wasm
// build embeds them at compile time (no filesystem there). The wasm test
// runner touches this file first so the embed can never go stale.
#[cfg(target_arch = "wasm32")]
static CASES: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/../../fixtures/cases");

#[cfg(not(target_arch = "wasm32"))]
fn case_names() -> Vec<String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cases");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("fixtures/cases")
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    names.sort_unstable();
    names
}

#[cfg(target_arch = "wasm32")]
fn case_names() -> Vec<String> {
    let mut names: Vec<String> = CASES
        .dirs()
        .filter_map(|d| d.path().file_name().and_then(|n| n.to_str()))
        .map(String::from)
        .collect();
    names.sort_unstable();
    names
}

fn cell_proj(cell: &Cell) -> Value {
    match cell.kind {
        CellKind::Empty => Value::Null,
        CellKind::Context => json!(["ctx", cell.line, cell.text]),
        CellKind::Add | CellKind::Remove => {
            let tag = if cell.kind == CellKind::Add {
                "add"
            } else {
                "del"
            };
            if cell.word_ranges.is_empty() {
                json!([tag, cell.line, cell.text])
            } else {
                let ranges: Vec<[u32; 2]> =
                    cell.word_ranges.iter().map(|r| [r.start, r.end]).collect();
                json!([tag, cell.line, cell.text, ranges])
            }
        }
    }
}

fn project(rows: &[Row]) -> Value {
    let items: Vec<Value> = rows
        .iter()
        .map(|row| match row {
            Row::HunkHeader { text, .. } => json!(["hunk", text]),
            Row::Gap { gap, .. } => json!([
                "gap",
                gap.id,
                gap.count,
                [gap.old_range.0, gap.old_range.1],
                [gap.new_range.0, gap.new_range.1]
            ]),
            Row::Unified {
                old_num,
                new_num,
                cell,
                ..
            } => match cell.kind {
                CellKind::Context => json!(["ctx", old_num, new_num, cell.text]),
                _ => cell_proj(cell),
            },
            Row::Split { left, right, .. } => {
                json!(["row", cell_proj(left), cell_proj(right)])
            }
        })
        .collect();
    Value::Array(items)
}

#[cfg(not(target_arch = "wasm32"))]
fn read_case_file(case: &str, name: &str) -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/cases")
        .join(case)
        .join(name);
    std::fs::read_to_string(path).ok()
}

#[cfg(target_arch = "wasm32")]
fn read_case_file(case: &str, name: &str) -> Option<String> {
    CASES
        .get_file(format!("{case}/{name}"))
        .map(|f| String::from_utf8_lossy(f.contents()).into_owned())
}

fn highlight_parity_hash(path: &str, diff: &FileDiff) -> String {
    let dark =
        serde_json::to_string(&highlight_diff(path, diff, ThemeChoice::Dark)).unwrap_or_default();
    let light =
        serde_json::to_string(&highlight_diff(path, diff, ThemeChoice::Light)).unwrap_or_default();
    format!("{:016x}", fnv1a64(format!("{dark}|{light}").as_bytes()))
}

struct CaseResult {
    name: String,
    unified: Value,
    split: Value,
    kind: Value,
    highlight_hash: String,
}

fn run_case(name: &str) -> CaseResult {
    let input = read_case_file(name, "input.diff").expect("input.diff");
    let meta: Value = serde_json::from_str(&read_case_file(name, "meta.json").expect("meta.json"))
        .expect("meta parses");
    let path = meta["path"].as_str().expect("meta.path").to_string();

    let mut diff = parse_file_diff(&input).expect("fixture parses");
    if let Some(total) = meta["oldTotalLines"].as_u64() {
        diff.old_total_lines = Some(total as u32);
    }

    let kind = match &diff.kind {
        FileDiffKind::Text => json!("text"),
        FileDiffKind::Binary { desc } => json!({ "binary": desc }),
        FileDiffKind::TooLarge { adds, dels } => json!({ "tooLarge": [adds, dels] }),
    };

    let unified = project(&build_rows(
        &diff,
        BuildOptions {
            mode: ViewMode::Unified,
            word_diff: true,
        },
    ));
    let split = project(&build_rows(
        &diff,
        BuildOptions {
            mode: ViewMode::Split,
            word_diff: true,
        },
    ));
    let highlight_hash = highlight_parity_hash(&path, &diff);

    CaseResult {
        name: name.to_string(),
        unified,
        split,
        kind,
        highlight_hash,
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn maybe_update(result: &CaseResult) -> bool {
    if std::env::var("UPDATE_FIXTURES").is_err() {
        return false;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/cases")
        .join(&result.name)
        .join("expected");
    std::fs::create_dir_all(&dir).expect("mkdir expected");
    let pretty = |v: &Value| {
        let mut s = serde_json::to_string_pretty(v).expect("json");
        s.push('\n');
        s
    };
    std::fs::write(dir.join("rows-unified.json"), pretty(&result.unified)).expect("write");
    std::fs::write(dir.join("rows-split.json"), pretty(&result.split)).expect("write");
    std::fs::write(dir.join("kind.json"), pretty(&result.kind)).expect("write");
    std::fs::write(
        dir.join("highlight.hash"),
        format!("{}\n", result.highlight_hash),
    )
    .expect("write");
    true
}

#[cfg(target_arch = "wasm32")]
fn maybe_update(_result: &CaseResult) -> bool {
    false
}

fn assert_case(result: &CaseResult) {
    let expected = |file: &str| -> Value {
        let content =
            read_case_file(&result.name, &format!("expected/{file}")).unwrap_or_else(|| {
                panic!(
                    "{}: missing expected/{file}; run UPDATE_FIXTURES=1 and review",
                    result.name
                )
            });
        serde_json::from_str(&content).expect("expected file parses")
    };
    assert_eq!(
        result.unified,
        expected("rows-unified.json"),
        "{}: unified rows drifted",
        result.name
    );
    assert_eq!(
        result.split,
        expected("rows-split.json"),
        "{}: split rows drifted",
        result.name
    );
    assert_eq!(
        result.kind,
        expected("kind.json"),
        "{}: kind drifted",
        result.name
    );
    let expected_hash = read_case_file(&result.name, "expected/highlight.hash")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    assert_eq!(
        result.highlight_hash, expected_hash,
        "{}: highlight parity hash drifted (native and wasm must agree)",
        result.name
    );
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn conformance_corpus() {
    let names = case_names();
    assert!(!names.is_empty(), "fixture corpus must not be empty");

    let mut updated = false;
    for name in &names {
        let result = run_case(name);
        if maybe_update(&result) {
            updated = true;
            continue;
        }
        assert_case(&result);
    }
    assert!(
        !updated,
        "fixtures regenerated; review the diff and re-run without UPDATE_FIXTURES"
    );
}
