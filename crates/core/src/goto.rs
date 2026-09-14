//! Resolving a line number to a row index (the `:<num>` motion).
//!
//! Modeled on `search.rs`: a pure function over `&[Row]`, its own result
//! vocabulary, no frontend state. New-side numbering is tried first (the
//! side rule prefers "line N of the file as it will be"), old-side is the
//! fallback for pure-deletion regions the new side has no number for.

use serde::Serialize;

use crate::rows::Row;

/// Which side's numbering matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LineSide {
    New,
    Old,
}

/// Where a requested line number lands in the row stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum LineTarget {
    /// The line is visible at this row index.
    Exact { row: usize, side: LineSide },
    /// The line is hidden inside a collapsed gap; `row` is the gap's own
    /// row index and `gap_id` addresses it for expansion.
    InGap { row: usize, gap_id: String },
    /// The line is not in the diff; `row` is the nearest line row at or
    /// before it, or None when nothing precedes it.
    Nearest { row: Option<usize> },
}

/// One side's line number for a row, or None for rows that carry no line
/// of their own on that side (hunk headers, gaps, empty split padding).
fn line_num(row: &Row, side: LineSide) -> Option<u32> {
    match row {
        Row::Unified {
            old_num, new_num, ..
        } => match side {
            LineSide::New => *new_num,
            LineSide::Old => *old_num,
        },
        Row::Split { left, right, .. } => match side {
            LineSide::New => right.line,
            LineSide::Old => left.line,
        },
        _ => None,
    }
}

/// Outcome of scanning one side for `line`.
enum Scan {
    Exact(usize),
    InGap(usize, String),
    /// The last row whose number on this side is `< line`, if any.
    Nearest(Option<usize>),
}

fn scan_side(rows: &[Row], line: u32, side: LineSide) -> Scan {
    let mut nearest = None;
    for (idx, row) in rows.iter().enumerate() {
        if let Some(n) = line_num(row, side) {
            if n == line {
                return Scan::Exact(idx);
            }
            if n < line {
                nearest = Some(idx);
            }
            continue;
        }
        if let Row::Gap { gap, .. } = row {
            let (lo, hi) = match side {
                LineSide::New => gap.new_range,
                LineSide::Old => gap.old_range,
            };
            if lo <= line && line <= hi {
                return Scan::InGap(idx, gap.id.clone());
            }
        }
    }
    Scan::Nearest(nearest)
}

/// Resolve a 1-based line number against the row stream. Line 0 is never
/// valid and always resolves to `Nearest { row: None }`.
pub fn find_line(rows: &[Row], line: u32) -> LineTarget {
    if line == 0 {
        return LineTarget::Nearest { row: None };
    }

    let new_scan = scan_side(rows, line, LineSide::New);
    match &new_scan {
        Scan::Exact(row) => {
            return LineTarget::Exact {
                row: *row,
                side: LineSide::New,
            };
        }
        Scan::InGap(row, gap_id) => {
            return LineTarget::InGap {
                row: *row,
                gap_id: gap_id.clone(),
            };
        }
        Scan::Nearest(_) => {}
    }

    match scan_side(rows, line, LineSide::Old) {
        Scan::Exact(row) => LineTarget::Exact {
            row,
            side: LineSide::Old,
        },
        Scan::InGap(row, gap_id) => LineTarget::InGap { row, gap_id },
        Scan::Nearest(old_nearest) => {
            let new_nearest = match new_scan {
                Scan::Nearest(n) => n,
                _ => None,
            };
            LineTarget::Nearest {
                row: new_nearest.or(old_nearest),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_file_diff;
    use crate::rows::{BuildOptions, GapInfo, ViewMode, build_expansion_rows, build_rows};

    fn rows(raw: &str, mode: ViewMode) -> Vec<Row> {
        build_rows(
            &parse_file_diff(raw).expect("parse"),
            BuildOptions {
                mode,
                word_diff: false,
            },
        )
    }

    #[test]
    fn exact_new_side_match_unified() {
        let rs = rows("@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n", ViewMode::Unified);
        // Rows: header, "a" (ctx), "b" (remove), "B" (add, new_num = 2), "c".
        assert_eq!(
            find_line(&rs, 2),
            LineTarget::Exact {
                row: 3,
                side: LineSide::New
            }
        );
    }

    #[test]
    fn exact_new_side_match_split_resolves_through_right_cell() {
        let rs = rows("@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n", ViewMode::Split);
        let target = find_line(&rs, 2);
        let LineTarget::Exact { row, side } = target else {
            panic!("expected exact match, got {target:?}");
        };
        assert_eq!(side, LineSide::New);
        let Row::Split { right, .. } = &rs[row] else {
            panic!("expected split row");
        };
        assert_eq!(right.line, Some(2));
    }

    #[test]
    fn old_side_fallback_across_a_pure_deletion_region() {
        // Old file "a,b,c" (3 lines); new file "a" (1 line): lines 2 and 3
        // are pure deletions with no new-side number at all.
        let rs = rows("@@ -1,3 +1,1 @@\n a\n-b\n-c\n", ViewMode::Unified);
        let target = find_line(&rs, 3);
        let LineTarget::Exact { row, side } = target else {
            panic!("expected exact match, got {target:?}");
        };
        assert_eq!(side, LineSide::Old);
        let Row::Unified { old_num, .. } = &rs[row] else {
            panic!("expected unified row");
        };
        assert_eq!(*old_num, Some(3));
    }

    #[test]
    fn line_inside_a_collapsed_gap_returns_in_gap_with_the_right_id() {
        let rs = rows("@@ -5,2 +5,2 @@\n x\n-y\n+Y\n", ViewMode::Unified);
        let target = find_line(&rs, 2);
        assert_eq!(
            target,
            LineTarget::InGap {
                row: 0,
                gap_id: "before:0".to_string()
            }
        );
    }

    #[test]
    fn line_past_eof_returns_nearest_at_the_last_line_row() {
        let rs = rows("@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n", ViewMode::Unified);
        let last_line_row = rs.len() - 1; // the trailing context row "c"
        assert_eq!(
            find_line(&rs, 1000),
            LineTarget::Nearest {
                row: Some(last_line_row)
            }
        );
    }

    #[test]
    fn line_before_the_first_hunk_lands_in_its_leading_gap() {
        let rs = rows("@@ -5,2 +5,2 @@\n x\n-y\n+Y\n", ViewMode::Unified);
        assert_eq!(
            find_line(&rs, 1),
            LineTarget::InGap {
                row: 0,
                gap_id: "before:0".to_string()
            }
        );
    }

    #[test]
    fn an_expansion_rows_line_number_is_matched() {
        let gap = GapInfo {
            id: "before:1".into(),
            count: 3,
            old_range: (3, 5),
            new_range: (4, 6),
        };
        let rs = build_expansion_rows(&gap, &["ctx3", "ctx4", "ctx5"], ViewMode::Unified, 1);
        assert_eq!(
            find_line(&rs, 5),
            LineTarget::Exact {
                row: 1,
                side: LineSide::New
            }
        );
    }

    #[test]
    fn line_zero_is_always_nearest_none() {
        let rs = rows("@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n", ViewMode::Unified);
        assert_eq!(find_line(&rs, 0), LineTarget::Nearest { row: None });
        assert_eq!(find_line(&[], 0), LineTarget::Nearest { row: None });
    }
}
