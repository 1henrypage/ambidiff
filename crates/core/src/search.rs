//! In-diff search over the row model (a gap even hunk has).
//!
//! Literal smart-case matching: a query with no ASCII uppercase matches
//! case-insensitively (ASCII folding only, so byte offsets into the original
//! text stay exact for span highlighting); any uppercase makes it sensitive.

use serde::Serialize;

use crate::rows::Row;

/// Which cell of a row a match landed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchCell {
    Unified,
    Left,
    Right,
}

/// One search hit; byte offsets into the cell's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    pub row: usize,
    pub cell: MatchCell,
    pub start: u32,
    pub end: u32,
}

fn fold(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

/// Find `needle` in `haystack` starting at `from`, optionally ASCII-folded.
fn find_from(haystack: &str, needle: &str, from: usize, fold_case: bool) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if h.len() < n.len() {
        return None;
    }
    for start in from..=(h.len() - n.len()) {
        let hit = if fold_case {
            h[start..start + n.len()]
                .iter()
                .zip(n)
                .all(|(a, b)| fold(*a) == fold(*b))
        } else {
            &h[start..start + n.len()] == n
        };
        if hit {
            return Some(start);
        }
    }
    None
}

fn push_matches(
    out: &mut Vec<SearchMatch>,
    row: usize,
    cell: MatchCell,
    text: &str,
    query: &str,
    fold_case: bool,
) {
    let mut from = 0;
    while let Some(start) = find_from(text, query, from, fold_case) {
        let end = start + query.len();
        out.push(SearchMatch {
            row,
            cell,
            start: start as u32,
            end: end as u32,
        });
        from = end;
    }
}

/// Search all line rows for a literal query. Empty queries match nothing.
pub fn search_rows(rows: &[Row], query: &str) -> Vec<SearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let fold_case = !query.bytes().any(|b| b.is_ascii_uppercase());
    let mut out = Vec::new();
    for (idx, row) in rows.iter().enumerate() {
        match row {
            Row::Unified { cell, .. } => {
                push_matches(
                    &mut out,
                    idx,
                    MatchCell::Unified,
                    &cell.text,
                    query,
                    fold_case,
                );
            }
            Row::Split { left, right, .. } => {
                push_matches(&mut out, idx, MatchCell::Left, &left.text, query, fold_case);
                push_matches(
                    &mut out,
                    idx,
                    MatchCell::Right,
                    &right.text,
                    query,
                    fold_case,
                );
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_file_diff;
    use crate::rows::{BuildOptions, ViewMode, build_rows};

    fn rows(mode: ViewMode) -> Vec<Row> {
        let raw = "@@ -1,3 +1,3 @@\n Alpha beta\n-gamma ALPHA\n+delta alpha\n";
        build_rows(
            &parse_file_diff(raw).expect("parse"),
            BuildOptions {
                mode,
                word_diff: false,
            },
        )
    }

    #[test]
    fn lowercase_query_is_case_insensitive() {
        let matches = search_rows(&rows(ViewMode::Unified), "alpha");
        // "Alpha" (context), "ALPHA" (remove), "alpha" (add).
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn uppercase_query_is_case_sensitive() {
        let matches = search_rows(&rows(ViewMode::Unified), "ALPHA");
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn offsets_index_the_original_text() {
        let matches = search_rows(&rows(ViewMode::Unified), "beta");
        assert_eq!(matches.len(), 1);
        assert_eq!((matches[0].start, matches[0].end), (6, 10));
    }

    #[test]
    fn split_mode_reports_cells() {
        let matches = search_rows(&rows(ViewMode::Split), "alpha");
        let cells: Vec<MatchCell> = matches.iter().map(|m| m.cell).collect();
        // Context row matches in both cells, plus one in each change cell.
        assert_eq!(
            cells,
            vec![
                MatchCell::Left,
                MatchCell::Right,
                MatchCell::Left,
                MatchCell::Right
            ]
        );
    }

    #[test]
    fn multiple_hits_in_one_line_are_non_overlapping() {
        let raw = "@@ -1 +1 @@\n-x\n+aaaa\n";
        let rows = build_rows(
            &parse_file_diff(raw).expect("parse"),
            BuildOptions::default(),
        );
        let matches = search_rows(&rows, "aa");
        assert_eq!(matches.len(), 2);
        assert_eq!((matches[0].start, matches[1].start), (0, 2));
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert!(search_rows(&rows(ViewMode::Unified), "").is_empty());
    }

    #[test]
    fn unicode_content_is_safe() {
        let raw = "@@ -1 +1 @@\n-\u{4f60}\u{597d}x\n+\u{4f60}\u{597d}y\n";
        let rows = build_rows(
            &parse_file_diff(raw).expect("parse"),
            BuildOptions::default(),
        );
        let matches = search_rows(&rows, "\u{597d}");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].start, 3);
    }
}
