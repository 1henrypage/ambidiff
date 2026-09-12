//! Pure, frontend-local text wrapping for the terminal painter: comment
//! cards (word wrap) and long diff-code lines (hard chunking). Touches
//! nothing under `crates/core` on purpose - wrapping is presentation, not
//! diff semantics, and this keeps the wasm leg and the committed
//! `web/dist` out of the blast radius of any change here.
//!
//! `wrap_line` is a direct port of `ambidiff-nvim`'s
//! `lua/ambidiff/render.lua` `wrap_line` (nvim always wraps cards; this is
//! the behavioural reference both the TUI and nvim converge on).

use ambidiff_core::sanitize::sanitize_line;
use unicode_width::UnicodeWidthChar;

/// Tab stop used when expanding tabs before measuring or wrapping.
pub(super) const TAB_WIDTH: usize = 4;
/// Chrome painted on every comment-card body/response row.
pub(super) const CARD_BORDER: &str = "  \u{2502} ";
/// Display width of [`CARD_BORDER`].
pub(super) const CARD_PREFIX: usize = 4;
/// Narrowest card body wrapped to, whatever the pane width (matches nvim's
/// `MIN_CARD_WIDTH`).
pub(super) const MIN_CARD_WIDTH: usize = 20;

/// Display-cell width of `s`, folding unicode-width over every char.
pub(super) fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Expand tabs against a running display column (tab stops every
/// `TAB_WIDTH` cells), carrying `column` across calls for text painted as
/// several consecutive spans of one logical row.
pub(super) fn expand_tabs(text: &str, column: &mut usize) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\t' {
            let advance = TAB_WIDTH - (*column % TAB_WIDTH);
            out.extend(std::iter::repeat_n(' ', advance));
            *column += advance;
        } else {
            out.push(c);
            *column += c.width().unwrap_or(0);
        }
    }
    out
}

fn flush(cur: &mut String, out: &mut Vec<String>) {
    while cur.ends_with(' ') {
        cur.pop();
    }
    out.push(std::mem::take(cur));
}

/// Tokenize `line` into an optional leading run of spaces followed by runs
/// of "non-space characters plus their trailing spaces" - mirrors Lua's
/// `line:match("^ +")` plus `line:gmatch("[^ ]+ *")`.
fn tokenize(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut i = match bytes.iter().position(|&b| b != b' ') {
        Some(0) => 0,
        Some(first) => {
            tokens.push(&line[..first]);
            first
        }
        None => {
            if !line.is_empty() {
                tokens.push(line);
            }
            return tokens;
        }
    };
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] != b' ' {
            i += 1;
        }
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        tokens.push(&line[start..i]);
    }
    tokens
}

/// Greedy word wrap of ONE sanitized, tab-free line into pieces of at most
/// `width` cells. Splits on ASCII spaces keeping trailing spaces with the
/// word; a token wider than `width` breaks per character (never mid-char);
/// trailing spaces are stripped from every piece; leading indent survives
/// on the first piece only; always returns >= 1 piece; `width` clamps to
/// >= 1 so it always makes progress.
pub(super) fn wrap_line(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;

    for token in tokenize(line) {
        let word = token.trim_end_matches(' ');
        let spaces = token.len() - word.len();
        let ww = display_width(word);
        if cur_w > 0 && cur_w + ww > width {
            flush(&mut cur, &mut out);
            cur_w = 0;
        }
        if ww > width {
            for ch in word.chars() {
                let cw = ch.width().unwrap_or(0);
                if cur_w > 0 && cur_w + cw > width {
                    flush(&mut cur, &mut out);
                    cur_w = 0;
                }
                cur.push(ch);
                cur_w += cw;
            }
            for _ in 0..spaces {
                cur.push(' ');
            }
            cur_w += spaces;
        } else {
            cur.push_str(token);
            cur_w += ww + spaces;
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// Card text -> painted rows: sanitize, expand tabs, then wrap when
/// `Some`. `None` = wrap off, one row per physical line, byte-identical to
/// today. Uses `str::lines()`, so a trailing newline adds no blank row.
pub(super) fn card_lines(text: &str, width: Option<usize>) -> Vec<String> {
    match width {
        None => text.lines().map(str::to_string).collect(),
        Some(w) => {
            let mut out = Vec::new();
            for line in text.lines() {
                let clean = sanitize_line(line);
                let mut column = 0usize;
                let expanded = expand_tabs(&clean, &mut column);
                out.extend(wrap_line(&expanded, w));
            }
            out
        }
    }
}

/// Byte ranges cutting `text` into chunks of at most `width` cells,
/// counting tabs to the next stop exactly as `styled_content` expands them
/// (column resets per chunk, as the painter does). Hard chunking, no word
/// awareness: `DRow::View.seg` must stay a byte range because
/// `styled_content` indexes highlight/word-diff/search spans by byte offset
/// into the original text.
///
/// Known approximation, deliberate: this measures the raw cell text, so an
/// ANSI escape's printable parameter bytes are counted although
/// `sanitize_line` drops them at paint time. That can only make a chunk
/// narrower, never overflow, and git plumbing runs `--no-color`, so
/// escapes do not appear in diff text in practice.
pub(super) fn chunk_ranges(text: &str, width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![(0, 0)];
    }
    let mut out = Vec::new();
    let mut chunk_start = 0usize;
    let mut column = 0usize;
    for (byte_idx, ch) in text.char_indices() {
        let cw = if ch == '\t' {
            TAB_WIDTH - (column % TAB_WIDTH)
        } else {
            ch.width().unwrap_or(0)
        };
        if column > 0 && column + cw > width {
            out.push((chunk_start, byte_idx));
            chunk_start = byte_idx;
            column = 0;
        }
        column += cw;
    }
    out.push((chunk_start, text.len()));
    out
}

/// Display width of `text` with tabs expanded to `TAB_WIDTH`-cell stops
/// from column 0, matching what `chunk_ranges` measures against `width`.
pub(super) fn display_width_expanded(text: &str) -> usize {
    let mut column = 0usize;
    for ch in text.chars() {
        if ch == '\t' {
            column += TAB_WIDTH - (column % TAB_WIDTH);
        } else {
            column += ch.width().unwrap_or(0);
        }
    }
    column
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_prefix_matches_border_width() {
        assert_eq!(display_width(CARD_BORDER), CARD_PREFIX);
    }

    #[test]
    fn wrap_line_token_exactly_width_fits_alone() {
        assert_eq!(wrap_line("hello", 5), vec!["hello"]);
    }

    #[test]
    fn wrap_line_token_one_cell_over_wraps() {
        assert_eq!(wrap_line("helloo", 5), vec!["hello", "o"]);
    }

    #[test]
    fn wrap_line_over_long_token_breaks_per_character() {
        let pieces = wrap_line("aaaaaaaaaa", 3);
        assert_eq!(pieces, vec!["aaa", "aaa", "aaa", "a"]);
        for p in &pieces {
            assert!(display_width(p) <= 3);
        }
    }

    #[test]
    fn wrap_line_cjk_pieces_stay_within_budget_and_round_trip() {
        let text = "你好世界你好世界";
        let pieces = wrap_line(text, 5);
        for p in &pieces {
            assert!(display_width(p) <= 5, "piece {p:?} exceeds budget");
        }
        assert_eq!(pieces.concat(), text);
    }

    #[test]
    fn wrap_line_two_cell_char_at_width_one_emitted_alone() {
        let pieces = wrap_line("你a", 1);
        // Progress is guaranteed: no piece can be empty, and every source
        // character appears somewhere.
        assert!(pieces.iter().all(|p| !p.is_empty()));
        assert_eq!(pieces.concat().chars().count(), 2);
    }

    #[test]
    fn wrap_line_trailing_spaces_stripped() {
        assert_eq!(wrap_line("hi   ", 20), vec!["hi"]);
    }

    #[test]
    fn wrap_line_empty_line_is_one_empty_piece() {
        assert_eq!(wrap_line("", 20), vec![""]);
    }

    #[test]
    fn wrap_line_leading_indent_survives_on_first_piece_only() {
        let pieces = wrap_line("  indented words here now", 12);
        assert!(pieces[0].starts_with("  "));
        for p in &pieces[1..] {
            assert!(!p.starts_with("  "));
        }
    }

    #[test]
    fn card_lines_expands_tabs_to_four_cell_stops_before_wrapping() {
        let pieces = card_lines("a\tb", Some(20));
        assert_eq!(pieces, vec!["a   b"]);
    }

    #[test]
    fn card_lines_strips_ansi_before_measuring() {
        let pieces = card_lines("\x1b[31mred\x1b[0m text", Some(20));
        assert_eq!(pieces, vec!["red text"]);
    }

    #[test]
    fn card_lines_blank_line_between_paragraphs() {
        assert_eq!(card_lines("a\n\nb\n", Some(20)), vec!["a", "", "b"]);
        assert_eq!(card_lines("a\n\nb\n", None), vec!["a", "", "b"]);
    }

    #[test]
    fn card_lines_none_is_byte_identical_to_lines() {
        let text = "one\ntwo\nthree";
        let expected: Vec<String> = text.lines().map(str::to_string).collect();
        assert_eq!(card_lines(text, None), expected);
    }

    #[test]
    fn chunk_ranges_reassemble_the_input_exactly() {
        let text = "the quick brown fox jumps over";
        let ranges = chunk_ranges(text, 6);
        let mut rebuilt = String::new();
        for (start, end) in &ranges {
            rebuilt.push_str(&text[*start..*end]);
        }
        assert_eq!(rebuilt, text);
    }

    #[test]
    fn chunk_ranges_every_chunk_fits_the_budget() {
        let text = "the quick brown fox jumps over the lazy dog";
        for (start, end) in chunk_ranges(text, 7) {
            assert!(display_width_expanded(&text[start..end]) <= 7);
        }
    }

    #[test]
    fn chunk_ranges_counts_tabs_to_next_stop() {
        let text = "\tabc";
        let ranges = chunk_ranges(text, 4);
        // A tab at column 0 costs 4 cells and fills the first chunk alone.
        assert_eq!(ranges.first().map(|&(s, e)| &text[s..e]), Some("\t"));
    }

    #[test]
    fn chunk_ranges_width_one_makes_progress() {
        let text = "abcdef";
        let ranges = chunk_ranges(text, 1);
        assert_eq!(ranges.len(), 6);
        for (start, end) in &ranges {
            assert!(end > start);
        }
    }
}
