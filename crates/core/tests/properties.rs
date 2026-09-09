//! Property-based tests (proptest): totality of the parser and word-diff
//! over arbitrary input, row-builder invariants over whatever the parser
//! yields, and review-file round-trip identity for valid files.

// Native-only suite: uses filesystem, subprocesses, or proptest's std rng.
#![cfg(feature = "native")]

use proptest::prelude::*;

use ambidiff_core::model::LineKind;
use ambidiff_core::parser::parse_file_diff;
use ambidiff_core::review::{Comment, ReviewFile, Side, Source, Status, parse_review, to_json};
use ambidiff_core::rows::{BuildOptions, CellKind, Row, ViewMode, build_rows};
use ambidiff_core::sanitize::sanitize_line;
use ambidiff_core::worddiff::{LinePair, compute_intra_ranges, pair_lines};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn parser_never_panics_on_arbitrary_input(input in ".{0,2000}") {
        let _ = parse_file_diff(&input);
    }

    #[test]
    fn parser_never_panics_on_diff_shaped_input(
        header in "@@ -[0-9]{1,3},[0-9]{1,2} \\+[0-9]{1,3},[0-9]{1,2} @@",
        lines in prop::collection::vec("[ +\\-\\\\]?.{0,40}", 0..30),
    ) {
        let raw = format!("{header}\n{}", lines.join("\n"));
        if let Ok(diff) = parse_file_diff(&raw) {
            // Line numbers on typed lines are consistent with their kind.
            for hunk in &diff.hunks {
                for line in &hunk.lines {
                    match line.kind {
                        LineKind::Add => prop_assert_eq!(line.old_num, 0),
                        LineKind::Remove => prop_assert_eq!(line.new_num, 0),
                        LineKind::Context => {}
                    }
                }
            }
        }
    }

    #[test]
    fn worddiff_never_panics_and_ranges_are_sound(
        minus in ".{0,600}",
        plus in ".{0,600}",
    ) {
        if let Some((minus_ranges, plus_ranges)) = compute_intra_ranges(&minus, &plus) {
            for (ranges, line) in [(&minus_ranges, &minus), (&plus_ranges, &plus)] {
                let mut prev_end = 0u32;
                for r in ranges.iter() {
                    prop_assert!(r.start < r.end, "non-empty range");
                    prop_assert!(r.start >= prev_end, "ordered, non-overlapping");
                    prop_assert!((r.end as usize) <= line.len(), "in bounds");
                    prop_assert!(line.is_char_boundary(r.start as usize));
                    prop_assert!(line.is_char_boundary(r.end as usize));
                    prev_end = r.end;
                }
            }
        }
    }

    #[test]
    fn worddiff_identical_lines_never_produce_ranges(line in ".{1,400}") {
        prop_assert_eq!(compute_intra_ranges(&line, &line), None);
    }

    #[test]
    fn pairing_never_reuses_a_line(
        flags in prop::collection::vec(any::<bool>(), 1..40),
    ) {
        let contents: Vec<String> = (0..flags.len()).map(|i| format!("line {i}")).collect();
        let lines: Vec<LinePair> = flags
            .iter()
            .zip(&contents)
            .map(|(is_remove, content)| LinePair { content, is_remove: *is_remove })
            .collect();
        let pairs = pair_lines(&lines);
        let mut removes_seen = std::collections::HashSet::new();
        let mut adds_seen = std::collections::HashSet::new();
        for p in &pairs {
            prop_assert!(lines[p.remove_idx].is_remove);
            prop_assert!(!lines[p.add_idx].is_remove);
            prop_assert!(removes_seen.insert(p.remove_idx), "remove used once");
            prop_assert!(adds_seen.insert(p.add_idx), "add used once");
        }
    }

    #[test]
    fn rows_conserve_lines_for_any_parsed_input(
        raw in "(@@ -[0-9]{1,2},[0-9]{1,2} \\+[0-9]{1,2},[0-9]{1,2} @@\n([ +\\-].{0,20}\n){0,20}){0,3}",
        mode_split in any::<bool>(),
    ) {
        let Ok(diff) = parse_file_diff(&raw) else { return Ok(()); };
        let mode = if mode_split { ViewMode::Split } else { ViewMode::Unified };
        let rows = build_rows(&diff, BuildOptions { mode, word_diff: true });

        let mut expected_old = Vec::new();
        let mut expected_new = Vec::new();
        for h in &diff.hunks {
            for l in &h.lines {
                match l.kind {
                    LineKind::Context => { expected_old.push(l.old_num); expected_new.push(l.new_num); }
                    LineKind::Add => expected_new.push(l.new_num),
                    LineKind::Remove => expected_old.push(l.old_num),
                }
            }
        }
        let mut got_old = Vec::new();
        let mut got_new = Vec::new();
        for row in &rows {
            match row {
                Row::Unified { old_num, new_num, .. } => {
                    if let Some(o) = old_num { got_old.push(*o); }
                    if let Some(n) = new_num { got_new.push(*n); }
                }
                Row::Split { left, right, .. } => {
                    if left.kind != CellKind::Empty { got_old.push(left.line.unwrap_or(0)); }
                    if right.kind != CellKind::Empty { got_new.push(right.line.unwrap_or(0)); }
                }
                _ => {}
            }
        }
        prop_assert_eq!(got_old, expected_old);
        prop_assert_eq!(got_new, expected_new);
    }

    #[test]
    fn sanitize_output_is_always_terminal_safe(input in "\\PC{0,200}|.{0,200}") {
        let out = sanitize_line(&input);
        for c in out.chars() {
            let code = c as u32;
            prop_assert!(c == '\t' || (code >= 0x20 && code != 0x7f && !(0x80..=0x9f).contains(&code)));
        }
    }

    #[test]
    fn review_round_trip_is_identity(review in arb_review()) {
        let json = to_json(&review).expect("valid review always serializes");
        let outcome = parse_review(&json).expect("valid file parses");
        prop_assert_eq!(outcome.review, review);
        prop_assert!(outcome.warnings.is_empty(), "no salvage on valid files: {:?}", outcome.warnings);
        prop_assert!(!outcome.read_only);
    }
}

/// Strategy for structurally valid comments (invalid records exercise the
/// quarantine path, which is intentionally lossy, so identity only holds for
/// valid files).
fn arb_comment() -> impl Strategy<Value = Comment> {
    let status = prop_oneof![
        Just(Status::Open),
        Just(Status::Addressed),
        Just(Status::Resolved),
        Just(Status::Reopened),
    ];
    let anchor = prop_oneof![
        // Review-level: no path, no line, no side.
        Just((None, None, None, None)),
        // File-level: path only.
        ("[a-z]{1,10}/[a-z]{1,10}\\.[a-z]{1,3}").prop_map(|p| (Some(p), None, None, None)),
        // Line or range.
        (
            "[a-z]{1,10}\\.[a-z]{1,3}",
            1u32..5000,
            0u32..10,
            prop::bool::ANY,
        )
            .prop_map(|(p, line, extent, old_side)| {
                let side = if old_side { Side::Old } else { Side::New };
                let end = (extent > 0).then_some(line + extent);
                (Some(p), Some(side), Some(line), end)
            }),
    ];
    (
        "c-[a-f0-9]{4,8}",
        1u32..100,
        status,
        anchor,
        prop::option::of(".{0,60}"),
        ".{1,80}",
        prop::option::of(".{0,40}"),
        "[a-z]{1,12}",
    )
        .prop_map(
            |(id, rev, status, (path, side, line, end_line), snippet, body, response, author)| {
                Comment {
                    id,
                    rev,
                    status,
                    path,
                    side,
                    line,
                    end_line,
                    snippet,
                    body,
                    response,
                    author,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    updated_at: "2026-01-01T00:00:00Z".to_string(),
                    extra: serde_json::Map::new(),
                }
            },
        )
}

fn arb_review() -> impl Strategy<Value = ReviewFile> {
    (
        ".{0,30}",
        1u32..50,
        prop::option::of("[a-zA-Z0-9_./~^-]{1,20}"),
        prop::collection::vec(arb_comment(), 0..8),
    )
        .prop_map(|(name, revision, base, mut comments)| {
            // Deduplicate ids; duplicates are a quarantine case by design.
            let mut seen = std::collections::HashSet::new();
            comments.retain(|c| seen.insert(c.id.clone()));
            let mut review = ReviewFile::new(name, Source::git(base), "2026-01-01T00:00:00Z");
            review.revision = revision;
            review.comments = comments;
            review
        })
}
