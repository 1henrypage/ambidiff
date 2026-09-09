//! Row builder: the shared render model every frontend paints.
//!
//! Turns a [`FileDiff`] into a flat stream of rows for either unified or
//! split (side-by-side) layout, following hunk's `buildSplitRows` shape:
//! change blocks pair removes and adds positionally with empty-cell padding,
//! collapsed gaps are addressed by stable ids (`before:<hunk>` / `trailing`),
//! and every row carries a stable content-addressed key so cursors survive
//! reloads. Word-diff ranges attach to lines via revdiff's greedy pairing,
//! which also classifies pure additions vs modified lines.

use serde::{Deserialize, Serialize};

use crate::highlight::HlSpan;
use crate::model::{DiffLine, FileDiff, Hunk, LineKind};
use crate::util::fnv1a32;
use crate::worddiff::{self, LinePair, Range};

/// Layout mode for the row stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    Unified,
    Split,
}

/// The role of one rendered cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CellKind {
    Context,
    Add,
    Remove,
    /// Split-mode padding opposite an unpaired add or remove.
    Empty,
}

/// One rendered cell: a side of a split row, or the body of a unified row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cell {
    pub kind: CellKind,
    /// Line number on this cell's side; None for empty padding.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub line: Option<u32>,
    pub text: String,
    /// Changed byte ranges from word-diff (only on add/remove cells).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub word_ranges: Vec<Range>,
    /// Syntax highlight spans (byte offsets into `text`).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub hl: Vec<HlSpan>,
}

impl Cell {
    fn empty() -> Self {
        Cell {
            kind: CellKind::Empty,
            line: None,
            text: String::new(),
            word_ranges: Vec::new(),
            hl: Vec::new(),
        }
    }
}

/// A collapsed unchanged region between (or around) hunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GapInfo {
    /// Stable gap id: `before:<hunkIndex>` or `trailing`.
    pub id: String,
    /// Number of collapsed lines.
    pub count: u32,
    /// 1-based inclusive old-side line range this gap covers.
    pub old_range: (u32, u32),
    /// 1-based inclusive new-side line range this gap covers.
    pub new_range: (u32, u32),
}

impl GapInfo {
    /// A gap is internally consistent when its recorded `count` equals the
    /// span of both its old and new ranges and it is non-empty. Defends
    /// `ViewState::expand` against a corrupted or tampered `GapInfo` crossing
    /// a wire boundary.
    pub fn is_consistent(&self) -> bool {
        if self.count == 0 {
            return false;
        }
        let old_span = self
            .old_range
            .1
            .checked_sub(self.old_range.0)
            .map(|d| d + 1);
        let new_span = self
            .new_range
            .1
            .checked_sub(self.new_range.0)
            .map(|d| d + 1);
        old_span == Some(self.count) && new_span == Some(self.count)
    }
}

/// One row of the render model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Row {
    #[serde(rename_all = "camelCase")]
    HunkHeader {
        key: String,
        hunk: u32,
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Gap { key: String, gap: GapInfo },
    /// Unified-mode line: one cell, both line numbers live on the cell side
    /// semantics (old for removes, new for adds, both for context).
    #[serde(rename_all = "camelCase")]
    Unified {
        key: String,
        hunk: u32,
        old_num: Option<u32>,
        new_num: Option<u32>,
        cell: Cell,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        is_expansion: bool,
    },
    /// Split-mode line: old side left, new side right.
    #[serde(rename_all = "camelCase")]
    Split {
        key: String,
        hunk: u32,
        left: Cell,
        right: Cell,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        is_expansion: bool,
    },
}

impl Row {
    pub fn key(&self) -> &str {
        match self {
            Row::HunkHeader { key, .. }
            | Row::Gap { key, .. }
            | Row::Unified { key, .. }
            | Row::Split { key, .. } => key,
        }
    }
}

/// Options for [`build_rows`].
#[derive(Debug, Clone, Copy)]
pub struct BuildOptions {
    pub mode: ViewMode,
    /// Compute intra-line word-diff ranges for paired remove/add lines.
    pub word_diff: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            mode: ViewMode::Unified,
            word_diff: true,
        }
    }
}

fn short_hash(text: &str) -> String {
    format!("{:08x}", fnv1a32(text))
}

fn line_key(old_num: u32, new_num: u32, content: &str) -> String {
    format!("l:{old_num}:{new_num}:{}", short_hash(content))
}

fn split_key(left: &Cell, right: &Cell) -> String {
    let old = left.line.unwrap_or(0);
    let new = right.line.unwrap_or(0);
    let h = fnv1a32(&left.text).wrapping_add(fnv1a32(&right.text).rotate_left(16));
    format!("s:{old}:{new}:{h:08x}")
}

/// Format the hunk header text shown to users.
fn hunk_header_text(hunk: &Hunk) -> String {
    let base = format!(
        "@@ -{},{} +{},{} @@",
        hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
    );
    if hunk.context.is_empty() {
        base
    } else {
        format!("{base} {}", hunk.context)
    }
}

/// Next untouched old-side line after a hunk. Insertion-only hunks
/// (`old_len == 0` at position K) insert between old lines K and K+1, so the
/// untouched region resumes at K+1. Widened to u64: a hunk header at the top
/// of the u32 range (`old_start = 4294967295`) must not overflow-panic here
/// (B24); the row builder tolerates it by simply producing no gap when the
/// arithmetic no longer fits in u32.
fn next_untouched_old(hunk: &Hunk) -> u64 {
    hunk.old_start as u64 + (hunk.old_len as u64).max(1)
}

fn next_untouched_new(hunk: &Hunk) -> u64 {
    hunk.new_start as u64 + (hunk.new_len as u64).max(1)
}

/// First old-side line actually covered by a hunk; for insertion-only hunks
/// the covered region is empty and the boundary sits after `old_start`.
fn covered_start_old(hunk: &Hunk) -> u64 {
    if hunk.old_len > 0 {
        hunk.old_start as u64
    } else {
        hunk.old_start as u64 + 1
    }
}

/// Narrow a u64 gap computation back to u32 fields, checked. Returns None
/// (no gap at all, rather than a wrapped or truncated one) the moment any
/// field would not fit u32 -- the safe degradation for B24's boundary input.
fn gap_from_u64(
    id: String,
    count: u64,
    old_range: (u64, u64),
    new_range: (u64, u64),
) -> Option<GapInfo> {
    let c = |x: u64| u32::try_from(x).ok();
    Some(GapInfo {
        id,
        count: c(count)?,
        old_range: (c(old_range.0)?, c(old_range.1)?),
        new_range: (c(new_range.0)?, c(new_range.1)?),
    })
}

/// Compute the collapsed gap before hunk `idx`, if any. Gap line counts are
/// equal on both sides (unchanged region), so the new-side range derives
/// from the previous hunk's boundary plus the old-side count.
fn leading_gap(diff: &FileDiff, idx: usize) -> Option<GapInfo> {
    let hunk = &diff.hunks[idx];
    let (prev_old, prev_new): (u64, u64) = if idx == 0 {
        (1, 1)
    } else {
        let prev = &diff.hunks[idx - 1];
        (next_untouched_old(prev), next_untouched_new(prev))
    };
    let old_start = covered_start_old(hunk);
    if old_start <= prev_old {
        return None;
    }
    let count = old_start - prev_old;
    gap_from_u64(
        format!("before:{idx}"),
        count,
        (prev_old, old_start - 1),
        (prev_new, prev_new + count - 1),
    )
}

/// Compute the trailing gap after the last hunk, when the old file's total
/// line count is known.
fn trailing_gap(diff: &FileDiff) -> Option<GapInfo> {
    let total = diff.old_total_lines? as u64;
    let last = diff.hunks.last()?;
    let next_old = next_untouched_old(last);
    let next_new = next_untouched_new(last);
    if total < next_old {
        return None;
    }
    let count = total - next_old + 1;
    gap_from_u64(
        "trailing".to_string(),
        count,
        (next_old, total),
        (next_new, next_new + count - 1),
    )
}

fn gap_row(gap: GapInfo) -> Row {
    Row::Gap {
        key: format!("g:{}", gap.id),
        gap,
    }
}

/// Word-diff ranges computed for the lines of one hunk, indexed by position
/// in `hunk.lines`.
fn hunk_word_ranges(hunk: &Hunk) -> Vec<Vec<Range>> {
    let mut ranges: Vec<Vec<Range>> = vec![Vec::new(); hunk.lines.len()];
    let mut block_start = None;
    for i in 0..=hunk.lines.len() {
        let in_change = i < hunk.lines.len() && hunk.lines[i].kind != LineKind::Context;
        match (block_start, in_change) {
            (None, true) => block_start = Some(i),
            (Some(start), false) => {
                compute_block_ranges(&hunk.lines[start..i], start, &mut ranges);
                block_start = None;
            }
            _ => {}
        }
    }
    ranges
}

/// Run pairing + intra-line diff over one contiguous change block, writing
/// results into `ranges` at absolute hunk-line indices.
fn compute_block_ranges(block: &[DiffLine], offset: usize, ranges: &mut [Vec<Range>]) {
    let inputs: Vec<LinePair> = block
        .iter()
        .map(|l| LinePair {
            content: &l.content,
            is_remove: l.kind == LineKind::Remove,
        })
        .collect();
    for pair in worddiff::pair_lines(&inputs) {
        let minus = &block[pair.remove_idx].content;
        let plus = &block[pair.add_idx].content;
        if let Some((minus_ranges, plus_ranges)) = worddiff::compute_intra_ranges(minus, plus) {
            ranges[offset + pair.remove_idx] = minus_ranges;
            ranges[offset + pair.add_idx] = plus_ranges;
        }
    }
}

fn make_cell(line: &DiffLine, side_num: u32, word_ranges: Vec<Range>) -> Cell {
    Cell {
        kind: match line.kind {
            LineKind::Context => CellKind::Context,
            LineKind::Add => CellKind::Add,
            LineKind::Remove => CellKind::Remove,
        },
        line: Some(side_num),
        text: line.content.clone(),
        word_ranges,
        hl: Vec::new(),
    }
}

/// Build the row stream for a text diff. Binary and too-large diffs have no
/// rows; the caller renders their placeholders from [`FileDiff::kind`].
pub fn build_rows(diff: &FileDiff, opts: BuildOptions) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();

    for (idx, hunk) in diff.hunks.iter().enumerate() {
        if let Some(gap) = leading_gap(diff, idx) {
            rows.push(gap_row(gap));
        }

        rows.push(Row::HunkHeader {
            key: format!("h:{idx}"),
            hunk: idx as u32,
            text: hunk_header_text(hunk),
        });

        let word_ranges = if opts.word_diff {
            hunk_word_ranges(hunk)
        } else {
            vec![Vec::new(); hunk.lines.len()]
        };

        match opts.mode {
            ViewMode::Unified => build_unified_hunk(idx as u32, hunk, &word_ranges, &mut rows),
            ViewMode::Split => build_split_hunk(idx as u32, hunk, &word_ranges, &mut rows),
        }
    }

    if let Some(gap) = trailing_gap(diff) {
        rows.push(gap_row(gap));
    }

    rows
}

fn build_unified_hunk(hunk_idx: u32, hunk: &Hunk, word_ranges: &[Vec<Range>], rows: &mut Vec<Row>) {
    for (i, line) in hunk.lines.iter().enumerate() {
        let (old_num, new_num, side_num) = match line.kind {
            LineKind::Context => (Some(line.old_num), Some(line.new_num), line.new_num),
            LineKind::Add => (None, Some(line.new_num), line.new_num),
            LineKind::Remove => (Some(line.old_num), None, line.old_num),
        };
        rows.push(Row::Unified {
            key: line_key(line.old_num, line.new_num, &line.content),
            hunk: hunk_idx,
            old_num,
            new_num,
            cell: make_cell(line, side_num, word_ranges[i].clone()),
            is_expansion: false,
        });
    }
}

fn build_split_hunk(hunk_idx: u32, hunk: &Hunk, word_ranges: &[Vec<Range>], rows: &mut Vec<Row>) {
    let mut i = 0;
    let lines = &hunk.lines;
    while i < lines.len() {
        if lines[i].kind == LineKind::Context {
            let line = &lines[i];
            let left = make_cell(line, line.old_num, Vec::new());
            let mut right = make_cell(line, line.new_num, Vec::new());
            right.line = Some(line.new_num);
            let key = split_key(&left, &right);
            rows.push(Row::Split {
                key,
                hunk: hunk_idx,
                left,
                right,
                is_expansion: false,
            });
            i += 1;
            continue;
        }

        // Change block: collect removes and adds, pair positionally with
        // empty-cell padding (hunk's split layout).
        let start = i;
        while i < lines.len() && lines[i].kind != LineKind::Context {
            i += 1;
        }
        let block = &lines[start..i];
        let removes: Vec<usize> = (0..block.len())
            .filter(|&j| block[j].kind == LineKind::Remove)
            .collect();
        let adds: Vec<usize> = (0..block.len())
            .filter(|&j| block[j].kind == LineKind::Add)
            .collect();
        let rows_in_block = removes.len().max(adds.len());
        for offset in 0..rows_in_block {
            let left = removes.get(offset).map(|&j| {
                let line = &block[j];
                make_cell(line, line.old_num, word_ranges[start + j].clone())
            });
            let right = adds.get(offset).map(|&j| {
                let line = &block[j];
                make_cell(line, line.new_num, word_ranges[start + j].clone())
            });
            let left = left.unwrap_or_else(Cell::empty);
            let right = right.unwrap_or_else(Cell::empty);
            let key = split_key(&left, &right);
            rows.push(Row::Split {
                key,
                hunk: hunk_idx,
                left,
                right,
                is_expansion: false,
            });
        }
    }
}

/// Build the rows that replace an expanded gap. `lines` is the new-side file
/// content for the gap's `new_range` (context lines are identical on both
/// sides). Line numbers count up from the gap's recorded ranges.
pub fn build_expansion_rows(gap: &GapInfo, lines: &[&str], mode: ViewMode, hunk: u32) -> Vec<Row> {
    let mut rows = Vec::with_capacity(lines.len());
    for (i, text) in lines.iter().enumerate() {
        // u64 intermediates: a gap sitting at the very top of the u32 range
        // must not overflow-panic when content runs longer than the gap.
        let old_num_u64 = gap.old_range.0 as u64 + i as u64;
        let new_num_u64 = gap.new_range.0 as u64 + i as u64;
        if old_num_u64 > gap.old_range.1 as u64 {
            break; // more content than the gap covers; never overflow the gap
        }
        let Some(old_num) = u32::try_from(old_num_u64).ok() else {
            break;
        };
        let Some(new_num) = u32::try_from(new_num_u64).ok() else {
            break;
        };
        let cell = Cell {
            kind: CellKind::Context,
            line: Some(new_num),
            text: (*text).to_string(),
            word_ranges: Vec::new(),
            hl: Vec::new(),
        };
        match mode {
            ViewMode::Unified => rows.push(Row::Unified {
                key: line_key(old_num, new_num, text),
                hunk,
                old_num: Some(old_num),
                new_num: Some(new_num),
                cell,
                is_expansion: true,
            }),
            ViewMode::Split => {
                let mut left = cell.clone();
                left.line = Some(old_num);
                let right = cell;
                let key = split_key(&left, &right);
                rows.push(Row::Split {
                    key,
                    hunk,
                    left,
                    right,
                    is_expansion: true,
                });
            }
        }
    }
    rows
}

/// Slice the lines of one gap out of a full new-side file text. Strips one
/// trailing `\r` per line (CRLF tolerance) and drops the trailing empty
/// artifact `split('\n')` yields when `content` ends with a newline, the
/// same rule the parser applies to raw diff text. The single owner of gap
/// content slicing (S1): every frontend called its own copy of this before.
pub fn slice_gap_lines<'a>(gap: &GapInfo, content: &'a str) -> Vec<&'a str> {
    let mut lines: Vec<&str> = content
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    let start = gap.new_range.0.saturating_sub(1) as usize;
    let end = (gap.new_range.1 as usize).min(lines.len());
    lines.get(start..end).unwrap_or_default().to_vec()
}

/// Slice `content` for `gap` and build its expansion rows in one call: the
/// shared path frontends use instead of re-deriving gap content slicing.
pub fn expansion_rows_from_content(
    gap: &GapInfo,
    content: &str,
    mode: ViewMode,
    hunk: u32,
) -> Vec<Row> {
    let lines = slice_gap_lines(gap, content);
    build_expansion_rows(gap, &lines, mode, hunk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileDiffKind;
    use crate::parser::parse_file_diff;

    fn diff(raw: &str) -> FileDiff {
        parse_file_diff(raw).expect("parse")
    }

    fn build(raw: &str, mode: ViewMode) -> Vec<Row> {
        build_rows(
            &diff(raw),
            BuildOptions {
                mode,
                word_diff: true,
            },
        )
    }

    /// Invariant: every source diff line appears exactly once in the rows.
    fn assert_line_conservation(diff: &FileDiff, rows: &[Row]) {
        let mut expected_old: Vec<u32> = Vec::new();
        let mut expected_new: Vec<u32> = Vec::new();
        for h in &diff.hunks {
            for l in &h.lines {
                match l.kind {
                    LineKind::Context => {
                        expected_old.push(l.old_num);
                        expected_new.push(l.new_num);
                    }
                    LineKind::Add => expected_new.push(l.new_num),
                    LineKind::Remove => expected_old.push(l.old_num),
                }
            }
        }
        let mut got_old: Vec<u32> = Vec::new();
        let mut got_new: Vec<u32> = Vec::new();
        for row in rows {
            match row {
                Row::Unified {
                    old_num, new_num, ..
                } => {
                    if let Some(o) = old_num {
                        got_old.push(*o);
                    }
                    if let Some(n) = new_num {
                        got_new.push(*n);
                    }
                }
                Row::Split { left, right, .. } => {
                    if left.kind != CellKind::Empty {
                        got_old.push(left.line.expect("left line"));
                    }
                    if right.kind != CellKind::Empty {
                        got_new.push(right.line.expect("right line"));
                    }
                }
                _ => {}
            }
        }
        assert_eq!(got_old, expected_old, "old-side lines conserved in order");
        assert_eq!(got_new, expected_new, "new-side lines conserved in order");
    }

    const BASIC: &str = "@@ -1,4 +1,4 @@\n a\n-b\n-c\n+B\n+C\n d\n";

    #[test]
    fn unified_rows_preserve_parser_order() {
        let d = diff(BASIC);
        let rows = build(BASIC, ViewMode::Unified);
        assert!(matches!(rows[0], Row::HunkHeader { .. }));
        let kinds: Vec<CellKind> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Unified { cell, .. } => Some(cell.kind),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                CellKind::Context,
                CellKind::Remove,
                CellKind::Remove,
                CellKind::Add,
                CellKind::Add,
                CellKind::Context
            ]
        );
        assert_line_conservation(&d, &rows);
    }

    #[test]
    fn split_rows_pair_positionally() {
        let d = diff(BASIC);
        let rows = build(BASIC, ViewMode::Split);
        // header + context + 2 paired change rows + context
        assert_eq!(rows.len(), 5);
        let Row::Split { left, right, .. } = &rows[2] else {
            panic!("expected split row");
        };
        assert_eq!(left.kind, CellKind::Remove);
        assert_eq!(left.text, "b");
        assert_eq!(right.kind, CellKind::Add);
        assert_eq!(right.text, "B");
        assert_line_conservation(&d, &rows);
    }

    #[test]
    fn split_pads_unpaired_add_with_empty_cell() {
        let raw = "@@ -1,2 +1,3 @@\n a\n-b\n+B\n+extra\n";
        let d = diff(raw);
        let rows = build(raw, ViewMode::Split);
        let Row::Split { left, right, .. } = &rows[3] else {
            panic!("expected split row");
        };
        assert_eq!(left.kind, CellKind::Empty);
        assert_eq!(left.line, None);
        assert_eq!(right.kind, CellKind::Add);
        assert_eq!(right.text, "extra");
        assert_line_conservation(&d, &rows);
    }

    #[test]
    fn split_pads_unpaired_remove_with_empty_cell() {
        let raw = "@@ -1,3 +1,2 @@\n a\n-b\n-gone\n+B\n";
        let rows = build(raw, ViewMode::Split);
        let Row::Split { left, right, .. } = &rows[3] else {
            panic!("expected split row");
        };
        assert_eq!(left.kind, CellKind::Remove);
        assert_eq!(left.text, "gone");
        assert_eq!(right.kind, CellKind::Empty);
    }

    #[test]
    fn column_counts_conserved_in_split_mode() {
        let raw = "@@ -1,5 +1,4 @@\n a\n-b\n-c\n-d\n+X\n e\n";
        let d = diff(raw);
        let rows = build(raw, ViewMode::Split);
        assert_line_conservation(&d, &rows);
        // 3 removes vs 1 add: block renders 3 rows, 2 with empty right cells.
        let empties = rows
            .iter()
            .filter(|r| matches!(r, Row::Split { right, .. } if right.kind == CellKind::Empty))
            .count();
        assert_eq!(empties, 2);
    }

    #[test]
    fn leading_gap_before_first_hunk() {
        let raw = "@@ -5,2 +5,2 @@\n x\n-y\n+Y\n";
        let rows = build(raw, ViewMode::Unified);
        let Row::Gap { gap, .. } = &rows[0] else {
            panic!("expected leading gap");
        };
        assert_eq!(gap.id, "before:0");
        assert_eq!(gap.count, 4);
        assert_eq!(gap.old_range, (1, 4));
        assert_eq!(gap.new_range, (1, 4));
    }

    #[test]
    fn between_hunks_gap_uses_hunk_metadata() {
        let raw = "@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -10,2 +10,2 @@\n j\n-k\n+K\n";
        let rows = build(raw, ViewMode::Unified);
        let gaps: Vec<&GapInfo> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Gap { gap, .. } => Some(gap),
                _ => None,
            })
            .collect();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].id, "before:1");
        assert_eq!(gaps[0].count, 7); // old lines 3..9
        assert_eq!(gaps[0].old_range, (3, 9));
        assert_eq!(gaps[0].new_range, (3, 9));
    }

    #[test]
    fn trailing_gap_requires_total_lines() {
        let raw = "@@ -1,2 +1,2 @@\n a\n-b\n+B\n";
        let mut d = diff(raw);
        let rows = build_rows(&d, BuildOptions::default());
        assert!(!rows.iter().any(|r| matches!(r, Row::Gap { .. })));

        d.old_total_lines = Some(10);
        let rows = build_rows(&d, BuildOptions::default());
        let Some(Row::Gap { gap, .. }) = rows.last() else {
            panic!("expected trailing gap");
        };
        assert_eq!(gap.id, "trailing");
        assert_eq!(gap.count, 8);
        assert_eq!(gap.old_range, (3, 10));
        assert_eq!(gap.new_range, (3, 10));
    }

    #[test]
    fn insertion_only_hunk_gap_accounting_covers_every_line() {
        // Insert after old line 5 in a 10-line file: leading gap must cover
        // lines 1..5 (5 lines) and trailing 6..10 (5 lines); nothing lost.
        let raw = "@@ -5,0 +6 @@\n+inserted\n";
        let mut d = diff(raw);
        d.old_total_lines = Some(10);
        let rows = build_rows(&d, BuildOptions::default());
        let gaps: Vec<&GapInfo> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Gap { gap, .. } => Some(gap),
                _ => None,
            })
            .collect();
        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0].old_range, (1, 5));
        assert_eq!(gaps[0].count, 5);
        assert_eq!(gaps[1].old_range, (6, 10));
        assert_eq!(gaps[1].count, 5);
        assert_eq!(gaps[0].count + gaps[1].count, 10);
    }

    #[test]
    fn insertion_at_top_has_no_leading_gap() {
        let raw = "@@ -0,0 +1,2 @@\n+a\n+b\n";
        let rows = build(raw, ViewMode::Unified);
        assert!(matches!(rows[0], Row::HunkHeader { .. }));
    }

    #[test]
    fn full_context_diff_has_no_gaps() {
        let raw = "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        let mut d = diff(raw);
        d.old_total_lines = Some(3);
        let rows = build_rows(&d, BuildOptions::default());
        assert!(!rows.iter().any(|r| matches!(r, Row::Gap { .. })));
    }

    #[test]
    fn word_diff_ranges_attach_to_paired_lines() {
        let raw = "@@ -1,2 +1,2 @@\n keep\n-let x = foo(1)\n+let x = bar(1)\n";
        let rows = build(raw, ViewMode::Unified);
        let cells: Vec<&Cell> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Unified { cell, .. } => Some(cell),
                _ => None,
            })
            .collect();
        assert!(cells[0].word_ranges.is_empty());
        assert_eq!(cells[1].word_ranges, vec![Range { start: 8, end: 11 }]);
        assert_eq!(cells[2].word_ranges, vec![Range { start: 8, end: 11 }]);
    }

    #[test]
    fn word_diff_off_yields_no_ranges() {
        let raw = "@@ -1 +1 @@\n-let x = foo(1)\n+let x = bar(1)\n";
        let rows = build_rows(
            &diff(raw),
            BuildOptions {
                mode: ViewMode::Unified,
                word_diff: false,
            },
        );
        for row in &rows {
            if let Row::Unified { cell, .. } = row {
                assert!(cell.word_ranges.is_empty());
            }
        }
    }

    #[test]
    fn row_keys_are_unique_and_stable() {
        let raw = "@@ -1,4 +1,4 @@\n a\n-b\n-c\n+B\n+C\n d\n@@ -8,2 +8,2 @@\n x\n-y\n+Y\n";
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let rows1 = build(raw, mode);
            let rows2 = build(raw, mode);
            let keys1: Vec<&str> = rows1.iter().map(Row::key).collect();
            let keys2: Vec<&str> = rows2.iter().map(Row::key).collect();
            assert_eq!(keys1, keys2, "keys stable across rebuilds");
            let mut deduped = keys1.clone();
            deduped.sort_unstable();
            deduped.dedup();
            assert_eq!(deduped.len(), keys1.len(), "keys unique within a view");
        }
    }

    #[test]
    fn expansion_rows_take_gap_numbering() {
        let gap = GapInfo {
            id: "before:1".into(),
            count: 3,
            old_range: (3, 5),
            new_range: (4, 6),
        };
        let rows = build_expansion_rows(&gap, &["ctx3", "ctx4", "ctx5"], ViewMode::Unified, 1);
        assert_eq!(rows.len(), 3);
        let Row::Unified {
            old_num,
            new_num,
            is_expansion,
            ..
        } = &rows[0]
        else {
            panic!("expected unified row");
        };
        assert_eq!((*old_num, *new_num), (Some(3), Some(4)));
        assert!(is_expansion);
    }

    #[test]
    fn expansion_rows_never_overflow_the_gap() {
        let gap = GapInfo {
            id: "before:0".into(),
            count: 2,
            old_range: (1, 2),
            new_range: (1, 2),
        };
        let rows = build_expansion_rows(&gap, &["a", "b", "c", "d"], ViewMode::Split, 0);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn hunk_header_at_u32_max_does_not_panic_building_rows() {
        // B24 minimal reproduction: an accepted header whose old_start sits
        // at u32::MAX must not overflow-panic while computing gaps. The
        // arithmetic here happens to still fit u32 (old_start+1 == 2^32,
        // and every derived field stays at or below u32::MAX), so a huge
        // but valid leading gap is produced; the point of this test is
        // solely that building rows over it never panics.
        let raw = "@@ -4294967295,0 +1,1 @@\n+a\n";
        let d = diff(raw);
        let rows = build_rows(&d, BuildOptions::default());
        assert!(matches!(rows[0], Row::Gap { .. }));
        assert!(matches!(rows[1], Row::HunkHeader { .. }));
    }

    #[test]
    fn gap_computation_degrades_to_no_gap_when_the_u32_range_is_exceeded() {
        // Two hunks each individually accepted by the parser's extent check
        // can still combine to an intermediate that overflows u32: hunk 0
        // pushes the new side to the top of the range, hunk 1's old side
        // jumps to the top of ITS range too. `gap_from_u64` must recognise
        // this and produce no gap rather than a wrapped/truncated one.
        let mut d = diff("@@ -1,1 +4294967294,2 @@\n a\n");
        d.hunks.push(Hunk {
            old_start: 4294967295,
            old_len: 0,
            new_start: 1,
            new_len: 1,
            context: String::new(),
            lines: vec![DiffLine {
                old_num: 0,
                new_num: 1,
                kind: LineKind::Add,
                content: "a".into(),
            }],
        });
        let rows = build_rows(&d, BuildOptions::default());
        // No panic, and the gap between the two hunks cannot be represented
        // in u32 (its new-side end would be ~2^33): it is simply absent.
        assert!(!rows.iter().any(|r| matches!(r, Row::Gap { .. })));
    }

    #[test]
    fn trailing_gap_near_u32_max_does_not_panic() {
        let raw = "@@ -1,1 +1,1 @@\n a\n";
        let mut d = diff(raw);
        d.old_total_lines = Some(u32::MAX);
        let rows = build_rows(&d, BuildOptions::default());
        let Some(Row::Gap { gap, .. }) = rows.last() else {
            panic!("expected trailing gap");
        };
        assert_eq!(gap.old_range, (2, u32::MAX));
    }

    #[test]
    fn slice_gap_lines_strips_cr_and_trailing_newline_artifact() {
        let gap = GapInfo {
            id: "trailing".into(),
            count: 2,
            old_range: (1, 2),
            new_range: (1, 2),
        };
        let content = "one\r\ntwo\r\n";
        let lines = slice_gap_lines(&gap, content);
        assert_eq!(lines, vec!["one", "two"]);
    }

    #[test]
    fn expansion_rows_from_content_matches_manual_slice_and_build() {
        let gap = GapInfo {
            id: "before:0".into(),
            count: 2,
            old_range: (3, 4),
            new_range: (3, 4),
        };
        let content = "x\ny\nctx3\nctx4\nz\n";
        let rows = expansion_rows_from_content(&gap, content, ViewMode::Unified, 0);
        let manual = build_expansion_rows(&gap, &["ctx3", "ctx4"], ViewMode::Unified, 0);
        assert_eq!(rows, manual);
    }

    #[test]
    fn gap_is_consistent_checks_count_matches_both_ranges() {
        let ok = GapInfo {
            id: "before:0".into(),
            count: 3,
            old_range: (1, 3),
            new_range: (1, 3),
        };
        assert!(ok.is_consistent());
        let bad_count = GapInfo {
            count: 5,
            ..ok.clone()
        };
        assert!(!bad_count.is_consistent());
        let bad_range = GapInfo {
            new_range: (1, 2),
            ..ok.clone()
        };
        assert!(!bad_range.is_consistent());
        let zero = GapInfo {
            count: 0,
            old_range: (1, 0),
            new_range: (1, 0),
            ..ok
        };
        assert!(!zero.is_consistent());
    }

    #[test]
    fn binary_diff_builds_no_rows() {
        let raw = "diff --git a/x.png b/x.png\nBinary files a/x.png and b/x.png differ\n";
        let d = diff(raw);
        assert!(matches!(d.kind, FileDiffKind::Binary { .. }));
        assert!(build_rows(&d, BuildOptions::default()).is_empty());
    }
}
