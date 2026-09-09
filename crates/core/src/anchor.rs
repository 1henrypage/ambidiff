//! Anchor resolution: where each comment renders today, no matter what
//! happened to the code since it was written.
//!
//! Lifecycle is id-keyed, so paths never gate it; this module only decides
//! display placement. Review-wide resolution order per comment path:
//! (1) matches a current file path, (2) matches a file's rename origin
//! (anchor on the renamed file with a "was <old>" badge), (3) no match:
//! the comment renders in an "unattached" group with its snippet, never
//! silently dropped. Within a file, the recorded line either matches its
//! captured snippet (normal), mismatches (outdated badge), sits inside a
//! collapsed gap (anchored to the gap), or is gone entirely (clamped).

use serde::Serialize;

use crate::model::FileEntry;
use crate::projection::PlacedComment;
use crate::review::{Comment, Side, snippet_matches};
use crate::rows::{CellKind, Row};

/// Review-wide placement of one comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Placement {
    /// Review-level comment (`path: null`).
    Review,
    /// Anchored to a current file; `was_path` carries the rename badge.
    #[serde(rename_all = "camelCase")]
    File {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        was_path: Option<String>,
    },
    /// No current file matches (deleted, or renamed beyond git's detection).
    Unattached,
}

/// Place every comment against the current changed-file list. The result is
/// parallel to `comments`.
pub fn place_comments(comments: &[Comment], files: &[FileEntry]) -> Vec<Placement> {
    comments
        .iter()
        .map(|comment| {
            let Some(path) = &comment.path else {
                return Placement::Review;
            };
            if files.iter().any(|f| &f.path == path) {
                return Placement::File {
                    path: path.clone(),
                    was_path: None,
                };
            }
            if let Some(renamed) = files
                .iter()
                .find(|f| f.old_path.as_deref() == Some(path.as_str()))
            {
                return Placement::File {
                    path: renamed.path.clone(),
                    was_path: Some(path.clone()),
                };
            }
            Placement::Unattached
        })
        .collect()
}

/// Where a comment lands within one file's row stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RowAnchor {
    pub comment_id: String,
    /// Index into the row stream the comment renders under; None for
    /// file-level comments (rendered in the file header area).
    pub row: Option<usize>,
    /// The recorded line no longer matches its captured snippet.
    pub outdated: bool,
    /// The recorded line is not visible; the anchor was clamped to the
    /// nearest earlier row.
    pub clamped: bool,
}

/// Find the row showing `line` on `side`, returning (row index, cell text).
/// Side coherence needs no cell-kind check: the row builder guarantees (and
/// the property suite asserts) that unified add rows carry no old number,
/// remove rows no new number, and split cells sit on their own side.
fn find_line_row(rows: &[Row], side: Side, line: u32) -> Option<(usize, &str)> {
    for (idx, row) in rows.iter().enumerate() {
        match row {
            Row::Unified {
                old_num,
                new_num,
                cell,
                ..
            } => {
                let num = match side {
                    Side::Old => *old_num,
                    Side::New => *new_num,
                };
                if num == Some(line) {
                    return Some((idx, &cell.text));
                }
            }
            Row::Split { left, right, .. } => {
                let cell = match side {
                    Side::Old => left,
                    Side::New => right,
                };
                if cell.kind != CellKind::Empty && cell.line == Some(line) {
                    return Some((idx, &cell.text));
                }
            }
            _ => {}
        }
    }
    None
}

/// Find the gap row whose range covers `line` on `side`.
fn find_gap_row(rows: &[Row], side: Side, line: u32) -> Option<usize> {
    rows.iter().position(|row| match row {
        Row::Gap { gap, .. } => {
            let (start, end) = match side {
                Side::Old => gap.old_range,
                Side::New => gap.new_range,
            };
            (start..=end).contains(&line)
        }
        _ => false,
    })
}

/// Last row whose `side` line number is below `line` (clamp target). None
/// when no row anywhere carries a number on that side at all -- there is
/// nothing sensible to clamp to (an empty or placeholder view), so the
/// caller renders the comment in the file group instead of pinning it to an
/// arbitrary row 0.
fn clamp_row(rows: &[Row], side: Side, line: u32) -> Option<usize> {
    let mut best = None;
    for (idx, row) in rows.iter().enumerate() {
        let num = match row {
            Row::Unified {
                old_num, new_num, ..
            } => match side {
                Side::Old => *old_num,
                Side::New => *new_num,
            },
            Row::Split { left, right, .. } => match side {
                Side::Old => left.line,
                Side::New => right.line,
            },
            _ => None,
        };
        if let Some(n) = num
            && n < line
        {
            best = Some(idx);
        }
    }
    best
}

/// Anchor one file's comments into its row stream.
pub fn anchor_comments_to_rows(rows: &[Row], comments: &[&Comment]) -> Vec<RowAnchor> {
    comments
        .iter()
        .map(|comment| {
            let Some(line) = comment.line else {
                return RowAnchor {
                    comment_id: comment.id.clone(),
                    row: None,
                    outdated: false,
                    clamped: false,
                };
            };
            // Salvage guarantees side on line comments; stay defensive.
            let side = comment.side.unwrap_or(Side::New);

            if let Some((idx, text)) = find_line_row(rows, side, line) {
                let outdated = comment
                    .snippet
                    .as_deref()
                    .is_some_and(|snippet| !snippet_matches(snippet, text));
                return RowAnchor {
                    comment_id: comment.id.clone(),
                    row: Some(idx),
                    outdated,
                    clamped: false,
                };
            }

            if let Some(idx) = find_gap_row(rows, side, line) {
                // The line exists but is collapsed; anchoring to the gap is
                // exact, so no outdated or clamped badge.
                return RowAnchor {
                    comment_id: comment.id.clone(),
                    row: Some(idx),
                    outdated: false,
                    clamped: false,
                };
            }

            // The line is not visible anywhere: clamp to the last lower-
            // numbered row on that side, if any exist at all.
            match clamp_row(rows, side, line) {
                Some(idx) => RowAnchor {
                    comment_id: comment.id.clone(),
                    row: Some(idx),
                    outdated: true,
                    clamped: true,
                },
                None => RowAnchor {
                    comment_id: comment.id.clone(),
                    row: None,
                    outdated: false,
                    clamped: true,
                },
            }
        })
        .collect()
}

/// A comment placed against a current file, with its row anchor and rename
/// badge, ready to serialise as `{"comment", "anchor", "wasPath"}`.
#[derive(Debug, Clone, Serialize)]
pub struct AnchoredComment<'a> {
    #[serde(skip)]
    pub index: usize,
    pub comment: &'a Comment,
    pub anchor: RowAnchor,
    #[serde(rename = "wasPath")]
    pub was_path: Option<&'a str>,
}

/// Anchor a file's already-placed comments into its row stream. The single
/// owner of "placements + tallies + anchors together" the projection needs
/// (S1/B15): callers get placement, anchor, and rename badge from one call.
pub fn anchor_file_comments<'a>(
    rows: &[Row],
    comments: &[PlacedComment<'a>],
) -> Vec<AnchoredComment<'a>> {
    let refs: Vec<&Comment> = comments.iter().map(|p| p.comment).collect();
    let anchors = anchor_comments_to_rows(rows, &refs);
    comments
        .iter()
        .zip(anchors)
        .map(|(placed, anchor)| AnchoredComment {
            index: placed.index,
            comment: placed.comment,
            anchor,
            was_path: placed.was_path,
        })
        .collect()
}

/// Which split-mode cell last had focus, for anchoring a new comment (B18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveCell {
    Auto,
    Left,
    Right,
}

impl ActiveCell {
    pub fn parse(s: &str) -> Option<ActiveCell> {
        match s {
            "auto" => Some(ActiveCell::Auto),
            "left" => Some(ActiveCell::Left),
            "right" => Some(ActiveCell::Right),
            _ => None,
        }
    }
}

/// The semantic (side, line) a new comment on `row` should anchor to, given
/// which cell has focus. Unified rows have one cell: new-side wins when
/// present (added/context lines), old-side otherwise (removed lines). Split
/// rows honour the active cell: a removed left cell anchors old, a context
/// left cell anchors new (the row's own recorded rule -- context lines
/// always anchor new-side), a non-empty right cell anchors new; `Auto`
/// prefers the right cell and falls back to the left (B18: clicking a
/// removed line in the left half of a paired row must not silently create a
/// new-side comment).
pub fn anchor_target(row: &Row, cell: ActiveCell) -> Option<(Side, u32)> {
    match row {
        Row::Unified {
            old_num, new_num, ..
        } => new_num
            .map(|n| (Side::New, n))
            .or_else(|| old_num.map(|n| (Side::Old, n))),
        Row::Split { left, right, .. } => {
            let try_left = || match left.kind {
                CellKind::Remove => left.line.map(|l| (Side::Old, l)),
                CellKind::Context => right.line.map(|l| (Side::New, l)),
                _ => None,
            };
            let try_right = || {
                if right.kind == CellKind::Empty {
                    None
                } else {
                    right.line.map(|l| (Side::New, l))
                }
            };
            match cell {
                ActiveCell::Left => try_left(),
                ActiveCell::Right => try_right(),
                ActiveCell::Auto => try_right().or_else(try_left),
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileStatus;
    use crate::parser::parse_file_diff;
    use crate::review::Status;
    use crate::rows::{BuildOptions, ViewMode, build_rows};
    use serde_json::Map;

    fn entry(path: &str, old_path: Option<&str>) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            old_path: old_path.map(str::to_string),
            status: if old_path.is_some() {
                FileStatus::Renamed
            } else {
                FileStatus::Modified
            },
            adds: None,
            dels: None,
        }
    }

    fn comment(id: &str, path: Option<&str>, line: Option<u32>, snippet: Option<&str>) -> Comment {
        Comment {
            id: id.to_string(),
            rev: 1,
            status: Status::Open,
            path: path.map(str::to_string),
            side: line.map(|_| Side::New),
            line,
            end_line: None,
            snippet: snippet.map(str::to_string),
            body: "b".to_string(),
            response: None,
            author: "a".to_string(),
            created_at: "t".to_string(),
            updated_at: "t".to_string(),
            extra: Map::new(),
        }
    }

    #[test]
    fn placement_partitions() {
        let files = vec![
            entry("src/a.rs", None),
            entry("src/new_name.rs", Some("src/old_name.rs")),
        ];
        let comments = vec![
            comment("c-review", None, None, None),
            comment("c-direct", Some("src/a.rs"), Some(1), None),
            comment("c-renamed", Some("src/old_name.rs"), Some(1), None),
            comment("c-gone", Some("src/deleted.rs"), Some(1), None),
        ];
        let placements = place_comments(&comments, &files);
        assert_eq!(placements[0], Placement::Review);
        assert_eq!(
            placements[1],
            Placement::File {
                path: "src/a.rs".into(),
                was_path: None
            }
        );
        assert_eq!(
            placements[2],
            Placement::File {
                path: "src/new_name.rs".into(),
                was_path: Some("src/old_name.rs".into())
            }
        );
        assert_eq!(placements[3], Placement::Unattached);
    }

    #[test]
    fn placement_prefers_direct_path_over_rename_origin() {
        // A file whose old name equals another comment's path AND still
        // exists as a current path: direct match wins.
        let files = vec![entry("keep.rs", None), entry("moved.rs", Some("keep.rs"))];
        let comments = vec![comment("c", Some("keep.rs"), None, None)];
        let placements = place_comments(&comments, &files);
        assert_eq!(
            placements[0],
            Placement::File {
                path: "keep.rs".into(),
                was_path: None
            }
        );
    }

    fn sample_rows(mode: ViewMode) -> Vec<Row> {
        let raw = "@@ -1,4 +1,4 @@\n one\n-two old\n+two new\n three\n four\n";
        let diff = parse_file_diff(raw).expect("parse");
        build_rows(
            &diff,
            BuildOptions {
                mode,
                word_diff: false,
            },
        )
    }

    #[test]
    fn line_anchor_grid_matching_snippet() {
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let rows = sample_rows(mode);
            // New-side line 2 is the added line "two new".
            let c = comment("c", Some("f"), Some(2), Some("two new"));
            let anchors = anchor_comments_to_rows(&rows, &[&c]);
            let a = &anchors[0];
            assert!(!a.outdated && !a.clamped, "{mode:?}");
            let idx = a.row.expect("row");
            match &rows[idx] {
                Row::Unified { cell, .. } => assert_eq!(cell.text, "two new"),
                Row::Split { right, .. } => assert_eq!(right.text, "two new"),
                other => panic!("unexpected row {other:?}"),
            }
        }
    }

    #[test]
    fn line_anchor_grid_snippet_mismatch_is_outdated() {
        let rows = sample_rows(ViewMode::Unified);
        let c = comment("c", Some("f"), Some(2), Some("completely different"));
        let anchors = anchor_comments_to_rows(&rows, &[&c]);
        assert!(anchors[0].outdated);
        assert!(
            !anchors[0].clamped,
            "line still visible, only content drifted"
        );
    }

    #[test]
    fn whitespace_only_drift_is_not_outdated() {
        let rows = sample_rows(ViewMode::Unified);
        let c = comment("c", Some("f"), Some(2), Some("  two   new  "));
        let anchors = anchor_comments_to_rows(&rows, &[&c]);
        assert!(!anchors[0].outdated);
    }

    #[test]
    fn old_side_comment_anchors_to_removed_line() {
        let rows = sample_rows(ViewMode::Unified);
        let mut c = comment("c", Some("f"), Some(2), Some("two old"));
        c.side = Some(Side::Old);
        let anchors = anchor_comments_to_rows(&rows, &[&c]);
        let idx = anchors[0].row.expect("row");
        let Row::Unified { cell, .. } = &rows[idx] else {
            panic!("expected unified row");
        };
        assert_eq!(cell.kind, CellKind::Remove);
        assert_eq!(cell.text, "two old");
        assert!(!anchors[0].outdated);
    }

    #[test]
    fn line_inside_gap_anchors_to_gap_row() {
        let raw = "@@ -10,2 +10,2 @@\n ten\n-eleven\n+ELEVEN\n";
        let diff = parse_file_diff(raw).expect("parse");
        let rows = build_rows(&diff, BuildOptions::default());
        // Line 5 (new side) sits in the leading gap (1..9).
        let c = comment("c", Some("f"), Some(5), Some("whatever"));
        let anchors = anchor_comments_to_rows(&rows, &[&c]);
        let idx = anchors[0].row.expect("row");
        assert!(matches!(rows[idx], Row::Gap { .. }));
        assert!(!anchors[0].outdated && !anchors[0].clamped);
    }

    #[test]
    fn vanished_line_is_clamped_and_outdated() {
        let rows = sample_rows(ViewMode::Unified);
        // New side has 4 lines; line 999 is gone.
        let c = comment("c", Some("f"), Some(999), Some("anything"));
        let anchors = anchor_comments_to_rows(&rows, &[&c]);
        assert!(anchors[0].clamped);
        assert!(anchors[0].outdated);
        // Clamped exactly to the LAST row carrying a new-side number (line
        // 4, "four"), not to the top of the file.
        let idx = anchors[0].row.expect("row");
        let Row::Unified { cell, new_num, .. } = &rows[idx] else {
            panic!("expected unified row");
        };
        assert_eq!(*new_num, Some(4));
        assert_eq!(cell.text, "four");
    }

    #[test]
    fn old_side_clamp_lands_on_last_old_numbered_row() {
        // Two hunks, no trailing gap (old total unknown): a vanished
        // old-side line past the last hunk must clamp to the LAST row
        // carrying an old number, not to row 0.
        let raw = "@@ -1,2 +1,2 @@\n a1\n-a2\n+A2\n@@ -30,2 +30,2 @@\n b1\n-b2\n+B2\n";
        let diff = parse_file_diff(raw).expect("parse");
        let rows = build_rows(
            &diff,
            BuildOptions {
                mode: ViewMode::Unified,
                word_diff: false,
            },
        );
        let mut c = comment("c", Some("f"), Some(999), Some("gone"));
        c.side = Some(Side::Old);
        let anchors = anchor_comments_to_rows(&rows, &[&c]);
        assert!(anchors[0].clamped);
        let idx = anchors[0].row.expect("row");
        let Row::Unified { old_num, .. } = &rows[idx] else {
            panic!("expected unified row");
        };
        assert_eq!(*old_num, Some(31), "last old-side row is b2 (line 31)");
    }

    #[test]
    fn split_mode_clamp_uses_cell_numbers() {
        let raw = "@@ -1,2 +1,2 @@\n a1\n-a2\n+A2\n";
        let diff = parse_file_diff(raw).expect("parse");
        let rows = build_rows(
            &diff,
            BuildOptions {
                mode: ViewMode::Split,
                word_diff: false,
            },
        );
        let c = comment("c", Some("f"), Some(999), Some("gone"));
        let anchors = anchor_comments_to_rows(&rows, &[&c]);
        assert!(anchors[0].clamped);
        let idx = anchors[0].row.expect("row");
        let Row::Split { right, .. } = &rows[idx] else {
            panic!("expected split row");
        };
        assert_eq!(right.line, Some(2), "last new-side split cell is A2");
    }

    #[test]
    fn line_in_between_hunks_gap_anchors_to_that_gap() {
        // A line hidden between hunks belongs to the between gap, not the
        // leading one and not a clamp.
        let raw = "@@ -1,2 +1,2 @@\n a1\n-a2\n+A2\n@@ -30,2 +30,2 @@\n b1\n-b2\n+B2\n";
        let diff = parse_file_diff(raw).expect("parse");
        let rows = build_rows(
            &diff,
            BuildOptions {
                mode: ViewMode::Unified,
                word_diff: false,
            },
        );
        let mut c = comment("c", Some("f"), Some(10), Some("hidden"));
        c.side = Some(Side::Old);
        let anchors = anchor_comments_to_rows(&rows, &[&c]);
        assert!(!anchors[0].clamped && !anchors[0].outdated);
        let idx = anchors[0].row.expect("row");
        let Row::Gap { gap, .. } = &rows[idx] else {
            panic!("expected gap row");
        };
        assert_eq!(gap.id, "before:1");
    }

    #[test]
    fn file_level_comment_has_no_row() {
        let rows = sample_rows(ViewMode::Unified);
        let c = comment("c", Some("f"), None, None);
        let anchors = anchor_comments_to_rows(&rows, &[&c]);
        assert_eq!(anchors[0].row, None);
        assert!(!anchors[0].outdated && !anchors[0].clamped);
    }

    #[test]
    fn line_comment_with_no_rows_on_its_side_renders_in_the_file_group() {
        // An empty row stream (a binary/too-large placeholder) has no row on
        // either side at all: clamp finds nothing, so the anchor must not
        // pin to a fabricated row 0.
        let rows: Vec<Row> = Vec::new();
        let c = comment("c", Some("f"), Some(1), Some("anything"));
        let anchors = anchor_comments_to_rows(&rows, &[&c]);
        assert_eq!(anchors[0].row, None);
        assert!(!anchors[0].outdated);
        assert!(anchors[0].clamped);
    }

    #[test]
    fn anchor_file_comments_pairs_placement_with_row_anchor() {
        use crate::projection::PlacedComment;
        let rows = sample_rows(ViewMode::Unified);
        let c = comment("c", Some("f"), Some(2), Some("two new"));
        let placed = vec![PlacedComment {
            index: 0,
            comment: &c,
            was_path: Some("old.rs"),
        }];
        let anchored = anchor_file_comments(&rows, &placed);
        assert_eq!(anchored.len(), 1);
        assert_eq!(anchored[0].was_path, Some("old.rs"));
        assert_eq!(anchored[0].comment.id, "c");
        assert!(!anchored[0].anchor.outdated);
        let json = serde_json::to_value(&anchored[0]).expect("json");
        assert_eq!(json["wasPath"], "old.rs");
        assert!(json.get("index").is_none());
    }

    #[test]
    fn active_cell_parses_known_strings_only() {
        assert_eq!(ActiveCell::parse("auto"), Some(ActiveCell::Auto));
        assert_eq!(ActiveCell::parse("left"), Some(ActiveCell::Left));
        assert_eq!(ActiveCell::parse("right"), Some(ActiveCell::Right));
        assert_eq!(ActiveCell::parse("center"), None);
    }

    #[test]
    fn anchor_target_unified_prefers_new_then_old() {
        let rows = sample_rows(ViewMode::Unified);
        // Row 2 is the removed line: no new_num, so old side wins.
        let Row::Unified { .. } = &rows[2] else {
            panic!("expected unified");
        };
        assert_eq!(
            anchor_target(&rows[2], ActiveCell::Auto),
            Some((Side::Old, 2))
        );
        // Row 3 is the added line: new side wins.
        assert_eq!(
            anchor_target(&rows[3], ActiveCell::Auto),
            Some((Side::New, 2))
        );
    }

    #[test]
    fn anchor_target_split_left_on_removed_text_is_old_side() {
        // B18: clicking removed text in the left half of a paired row must
        // anchor old-side, not silently fall through to the right cell.
        let rows = sample_rows(ViewMode::Split);
        let removed_row = rows
            .iter()
            .find(|r| matches!(r, Row::Split{left, ..} if left.kind == CellKind::Remove))
            .expect("a split row with a removed left cell");
        assert_eq!(
            anchor_target(removed_row, ActiveCell::Left),
            Some((Side::Old, 2))
        );
        // The same row's Auto target still prefers the (present) right cell.
        assert_eq!(
            anchor_target(removed_row, ActiveCell::Auto),
            Some((Side::New, 2))
        );
    }

    #[test]
    fn anchor_target_split_left_on_context_anchors_new_side() {
        let rows = sample_rows(ViewMode::Split);
        let context_row = rows
            .iter()
            .find(|r| matches!(r, Row::Split{left, ..} if left.kind == CellKind::Context))
            .expect("a context split row");
        assert_eq!(
            anchor_target(context_row, ActiveCell::Left),
            Some((Side::New, 1))
        );
    }

    #[test]
    fn anchor_target_split_right_empty_cell_yields_none() {
        let raw = "@@ -1,3 +1,2 @@\n a\n-b\n-gone\n+B\n";
        let diff = parse_file_diff(raw).expect("parse");
        let rows = build_rows(
            &diff,
            BuildOptions {
                mode: ViewMode::Split,
                word_diff: false,
            },
        );
        let empty_right_row = rows
            .iter()
            .find(|r| matches!(r, Row::Split{right, ..} if right.kind == CellKind::Empty))
            .expect("a row with an empty right cell");
        assert_eq!(anchor_target(empty_right_row, ActiveCell::Right), None);
    }

    #[test]
    fn no_snippet_is_never_outdated() {
        let rows = sample_rows(ViewMode::Unified);
        let c = comment("c", Some("f"), Some(2), None);
        let anchors = anchor_comments_to_rows(&rows, &[&c]);
        assert!(!anchors[0].outdated);
    }
}
