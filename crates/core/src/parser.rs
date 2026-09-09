//! Unified-diff parser: raw `git diff` output for a single file into the
//! structured [`FileDiff`] model.
//!
//! Ported from revdiff's `parseUnifiedDiff` semantics with two deliberate
//! changes: hunks stay structured (gap rows are derived later by the row
//! builder from hunk metadata, not inlined as divider lines), and overlong
//! lines are truncated with a marker instead of failing the whole parse.
//! Tolerance rules are preserved: unknown prefixes render as context, header
//! noise is skipped, and malformed input never panics.

use crate::model::{DiffLine, FileDiff, FileDiffKind, Hunk, LineKind};

/// Maximum accepted content-line length in bytes; longer lines are truncated
/// at a char boundary and suffixed with a marker. Mirrors revdiff's 1 MiB
/// scanner cap, but degrades instead of erroring.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Marker appended to a truncated overlong line.
pub const TRUNCATION_MARKER: &str = " [line truncated]";

/// Typed parse failure. The parser is tolerant by design; only structurally
/// unusable hunk headers are errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("unparseable hunk header: {header:?}")]
    HunkHeader { header: String },
    /// A hunk header is numerically well-formed but its start+len extent
    /// exceeds 2^32, which would overflow every downstream u32 line number
    /// (B24). Rejected at parse time instead of panicking in the row
    /// builder.
    #[error("hunk header extent overflows u32: {header:?}")]
    Extent { header: String },
}

/// Parsed `@@ -a[,b] +c[,d] @@[ context]` header.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HunkHeader {
    old_start: u32,
    old_len: u32,
    new_start: u32,
    new_len: u32,
    context: String,
}

/// Parse a hunk header line. Returns None when the line is not shaped like a
/// hunk header at all; Err when it is shaped like one but the numbers are
/// unusable (overflow).
fn parse_hunk_header(line: &str) -> Result<Option<HunkHeader>, ParseError> {
    let Some(rest) = line.strip_prefix("@@ -") else {
        return Ok(None);
    };
    let err = || ParseError::HunkHeader {
        header: line.to_string(),
    };

    // -a[,b]
    let Some(space) = rest.find(' ') else {
        return Ok(None);
    };
    let (old_part, rest) = rest.split_at(space);
    let Some(rest) = rest.strip_prefix(" +") else {
        return Ok(None);
    };
    // +c[,d] terminated by " @@"
    let Some(at) = rest.find(" @@") else {
        return Ok(None);
    };
    let (new_part, tail) = rest.split_at(at);
    let context = tail.strip_prefix(" @@").unwrap_or("").trim().to_string();

    let parse_pair = |part: &str| -> Result<Option<(u32, u32)>, ParseError> {
        let (start_s, len_s) = match part.split_once(',') {
            Some((s, l)) => (s, Some(l)),
            None => (part, None),
        };
        if start_s.is_empty() || !start_s.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(None);
        }
        let start: u32 = start_s.parse().map_err(|_| err())?;
        let len = match len_s {
            // Omitted length means 1 per the unified diff spec.
            None => 1,
            Some(l) => {
                if l.is_empty() || !l.bytes().all(|b| b.is_ascii_digit()) {
                    return Ok(None);
                }
                l.parse().map_err(|_| err())?
            }
        };
        Ok(Some((start, len)))
    };

    let Some((old_start, old_len)) = parse_pair(old_part)? else {
        return Ok(None);
    };
    let Some((new_start, new_len)) = parse_pair(new_part)? else {
        return Ok(None);
    };

    // Header extent check: start + len must fit in the 2^32 range every
    // downstream u32 line number lives in. A structurally valid but
    // pathological header (e.g. start = u32::MAX, len = 2) is rejected here
    // rather than overflowing arithmetic later (B24).
    const EXTENT_LIMIT: u64 = 1u64 << 32;
    if (old_start as u64) + (old_len as u64) > EXTENT_LIMIT
        || (new_start as u64) + (new_len as u64) > EXTENT_LIMIT
    {
        return Err(ParseError::Extent {
            header: line.to_string(),
        });
    }

    Ok(Some(HunkHeader {
        old_start,
        old_len,
        new_start,
        new_len,
        context,
    }))
}

/// True for git's "Binary files X and Y differ" marker.
fn is_binary_marker(line: &str) -> bool {
    line.starts_with("Binary files ") && line.ends_with(" differ")
}

/// Truncate content to [`MAX_LINE_BYTES`] at a char boundary, appending
/// [`TRUNCATION_MARKER`] when truncation happened.
fn cap_line(content: &str) -> String {
    if content.len() <= MAX_LINE_BYTES {
        return content.to_string();
    }
    let mut cut = MAX_LINE_BYTES;
    while cut > 0 && !content.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = content[..cut].to_string();
    out.push_str(TRUNCATION_MARKER);
    out
}

/// Parse raw unified diff output for a single file.
///
/// Multi-file input is tolerated: parsing stops at the second `diff --git`
/// boundary and returns the first file's diff. Binary markers produce a
/// [`FileDiffKind::Binary`] placeholder whose description reflects whether
/// the file was added or deleted.
pub fn parse_file_diff(raw: &str) -> Result<FileDiff, ParseError> {
    let mut diff = FileDiff::empty();
    let mut hunks: Vec<Hunk> = Vec::new();

    let mut in_header = true;
    let mut seen_file_boundary = false;
    let mut is_new_file = false;
    let mut is_deleted_file = false;
    // Running counters within the current hunk; saturating so pathological
    // input cannot overflow-panic in debug builds.
    let mut old_num: u32 = 0;
    let mut new_num: u32 = 0;

    // split('\n') keeps CR bytes (CRLF content stays intact in the model) but
    // yields a trailing empty artifact when the input ends with a newline.
    let mut lines: Vec<&str> = raw.split('\n').collect();
    if raw.ends_with('\n') {
        lines.pop();
    }

    for line in lines {
        if line.starts_with("diff --git ") {
            if seen_file_boundary && (!in_header || !hunks.is_empty()) {
                // Second file section: single-file parser stops here.
                break;
            }
            if seen_file_boundary && in_header {
                break;
            }
            seen_file_boundary = true;
            continue;
        }

        if in_header {
            if line.starts_with("new file mode") {
                is_new_file = true;
                continue;
            }
            if line.starts_with("deleted file mode") {
                is_deleted_file = true;
                continue;
            }
            if is_binary_marker(line) || line == "GIT binary patch" {
                let desc = if is_new_file {
                    "(new binary file)"
                } else if is_deleted_file {
                    "(deleted binary file)"
                } else {
                    "(binary file)"
                };
                diff.kind = FileDiffKind::Binary {
                    desc: desc.to_string(),
                };
                diff.hunks = hunks;
                return Ok(diff);
            }
            match parse_hunk_header(line)? {
                Some(_) => {
                    in_header = false;
                    // fall through to hunk handling below
                }
                None => continue,
            }
        }

        if let Some(header) = parse_hunk_header(line)? {
            old_num = header.old_start;
            new_num = header.new_start;
            hunks.push(Hunk {
                old_start: header.old_start,
                old_len: header.old_len,
                new_start: header.new_start,
                new_len: header.new_len,
                context: header.context,
                lines: Vec::new(),
            });
            continue;
        }

        let Some(hunk) = hunks.last_mut() else {
            // Content before any hunk header (should not happen; tolerated).
            continue;
        };

        if line.starts_with("\\ No newline at end of file") || line.starts_with("\\ ") {
            // The marker applies to the file version of the preceding line:
            // after '-' the old file lacks a newline, after '+' the new file,
            // after context both.
            match hunk.lines.last().map(|l| l.kind) {
                Some(LineKind::Remove) => diff.old_missing_newline = true,
                Some(LineKind::Add) => diff.new_missing_newline = true,
                Some(LineKind::Context) => {
                    diff.old_missing_newline = true;
                    diff.new_missing_newline = true;
                }
                None => {}
            }
            continue;
        }

        if line.is_empty() {
            // Some diff producers emit blank context lines without the
            // leading space; treat as empty context (revdiff rule).
            hunk.lines.push(DiffLine {
                old_num,
                new_num,
                kind: LineKind::Context,
                content: String::new(),
            });
            old_num = old_num.saturating_add(1);
            new_num = new_num.saturating_add(1);
            continue;
        }

        // Prefix dispatch is byte-based: a tolerated junk line may start
        // with a multi-byte char, where split_at(1) would panic.
        let (prefix, content) = match line.as_bytes()[0] {
            b @ (b'+' | b'-' | b' ') => (b, &line[1..]),
            _ => (0u8, line),
        };
        match prefix {
            b'+' => {
                hunk.lines.push(DiffLine {
                    old_num: 0,
                    new_num,
                    kind: LineKind::Add,
                    content: cap_line(content),
                });
                new_num = new_num.saturating_add(1);
            }
            b'-' => {
                hunk.lines.push(DiffLine {
                    old_num,
                    new_num: 0,
                    kind: LineKind::Remove,
                    content: cap_line(content),
                });
                old_num = old_num.saturating_add(1);
            }
            b' ' => {
                hunk.lines.push(DiffLine {
                    old_num,
                    new_num,
                    kind: LineKind::Context,
                    content: cap_line(content),
                });
                old_num = old_num.saturating_add(1);
                new_num = new_num.saturating_add(1);
            }
            _ => {
                // Unknown prefix: keep the whole line as context (tolerance).
                hunk.lines.push(DiffLine {
                    old_num,
                    new_num,
                    kind: LineKind::Context,
                    content: cap_line(line),
                });
                old_num = old_num.saturating_add(1);
                new_num = new_num.saturating_add(1);
            }
        }
    }

    diff.hunks = hunks;
    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(diff: &FileDiff) -> Vec<LineKind> {
        diff.hunks
            .iter()
            .flat_map(|h| h.lines.iter().map(|l| l.kind))
            .collect()
    }

    #[test]
    fn parses_plain_modification() {
        let raw = "diff --git a/f.txt b/f.txt\n\
                   index 000..111 100644\n\
                   --- a/f.txt\n\
                   +++ b/f.txt\n\
                   @@ -1,3 +1,3 @@\n \
                   one\n\
                   -two\n\
                   +TWO\n \
                   three\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert_eq!(diff.kind, FileDiffKind::Text);
        assert_eq!(diff.hunks.len(), 1);
        let h = &diff.hunks[0];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (1, 3, 1, 3)
        );
        assert_eq!(
            kinds(&diff),
            vec![
                LineKind::Context,
                LineKind::Remove,
                LineKind::Add,
                LineKind::Context
            ]
        );
        assert_eq!(
            h.lines[1],
            DiffLine {
                old_num: 2,
                new_num: 0,
                kind: LineKind::Remove,
                content: "two".into()
            }
        );
        assert_eq!(
            h.lines[2],
            DiffLine {
                old_num: 0,
                new_num: 2,
                kind: LineKind::Add,
                content: "TWO".into()
            }
        );
        assert_eq!(
            h.lines[3],
            DiffLine {
                old_num: 3,
                new_num: 3,
                kind: LineKind::Context,
                content: "three".into()
            }
        );
    }

    #[test]
    fn parses_hunk_header_function_context() {
        let raw = "@@ -10,2 +10,2 @@ fn main() {\n x\n-y\n+z\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert_eq!(diff.hunks[0].context, "fn main() {");
    }

    #[test]
    fn omitted_hunk_lengths_default_to_one() {
        let raw = "@@ -5 +7 @@\n-a\n+b\n";
        let diff = parse_file_diff(raw).expect("parse");
        let h = &diff.hunks[0];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (5, 1, 7, 1)
        );
        assert_eq!(h.lines[0].old_num, 5);
        assert_eq!(h.lines[1].new_num, 7);
    }

    #[test]
    fn insertion_only_hunk_at_start() {
        let raw = "@@ -0,0 +1,2 @@\n+a\n+b\n";
        let diff = parse_file_diff(raw).expect("parse");
        let h = &diff.hunks[0];
        assert_eq!((h.old_start, h.old_len), (0, 0));
        assert_eq!(h.lines[0].new_num, 1);
        assert_eq!(h.lines[1].new_num, 2);
    }

    #[test]
    fn new_binary_file_placeholder() {
        let raw = "diff --git a/x.png b/x.png\n\
                   new file mode 100644\n\
                   index 000..111\n\
                   Binary files /dev/null and b/x.png differ\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert_eq!(
            diff.kind,
            FileDiffKind::Binary {
                desc: "(new binary file)".into()
            }
        );
    }

    #[test]
    fn deleted_binary_file_placeholder() {
        let raw = "diff --git a/x.png b/x.png\n\
                   deleted file mode 100644\n\
                   Binary files a/x.png and /dev/null differ\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert_eq!(
            diff.kind,
            FileDiffKind::Binary {
                desc: "(deleted binary file)".into()
            }
        );
    }

    #[test]
    fn git_binary_patch_is_binary() {
        let raw = "diff --git a/x.bin b/x.bin\nindex 000..111\nGIT binary patch\nliteral 5\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert!(matches!(diff.kind, FileDiffKind::Binary { .. }));
    }

    #[test]
    fn no_newline_markers_set_side_flags() {
        // Removed line then marker: old side lacks newline.
        let raw = "@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert!(diff.old_missing_newline);
        assert!(!diff.new_missing_newline);

        let raw = "@@ -1 +1 @@\n-old\n+new\n\\ No newline at end of file\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert!(!diff.old_missing_newline);
        assert!(diff.new_missing_newline);

        let raw = "@@ -1,2 +1,2 @@\n-a\n+b\n last\n\\ No newline at end of file\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert!(diff.old_missing_newline);
        assert!(diff.new_missing_newline);
    }

    #[test]
    fn empty_unprefixed_line_is_empty_context() {
        let raw = "@@ -1,3 +1,3 @@\n a\n\n b\n";
        let diff = parse_file_diff(raw).expect("parse");
        let h = &diff.hunks[0];
        assert_eq!(h.lines[1].kind, LineKind::Context);
        assert_eq!(h.lines[1].content, "");
        assert_eq!(h.lines[1].old_num, 2);
        assert_eq!(h.lines[2].old_num, 3);
    }

    #[test]
    fn crlf_content_keeps_carriage_return() {
        let raw = "@@ -1 +1 @@\n-a\r\n+b\r\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert_eq!(diff.hunks[0].lines[0].content, "a\r");
        assert_eq!(diff.hunks[0].lines[1].content, "b\r");
    }

    #[test]
    fn unknown_prefix_is_tolerated_as_context() {
        let raw = "@@ -1,2 +1,2 @@\n a\n?weird\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert_eq!(diff.hunks[0].lines[1].kind, LineKind::Context);
        assert_eq!(diff.hunks[0].lines[1].content, "?weird");
    }

    #[test]
    fn second_file_section_stops_the_parse() {
        let raw = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+y\n\
                   diff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-p\n+q\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(diff.hunks[0].lines.len(), 2);
    }

    #[test]
    fn multiple_hunks_track_numbers_independently() {
        let raw = "@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -10,2 +10,2 @@\n j\n-k\n+K\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert_eq!(diff.hunks.len(), 2);
        assert_eq!(diff.hunks[1].lines[0].old_num, 10);
        assert_eq!(diff.hunks[1].lines[1].old_num, 11);
        assert_eq!(diff.hunks[1].lines[2].new_num, 11);
    }

    #[test]
    fn overlong_line_is_truncated_with_marker() {
        let big = "x".repeat(MAX_LINE_BYTES + 100);
        let raw = format!("@@ -1 +1 @@\n-{big}\n+short\n");
        let diff = parse_file_diff(&raw).expect("parse");
        let content = &diff.hunks[0].lines[0].content;
        assert!(content.ends_with(TRUNCATION_MARKER));
        assert_eq!(content.len(), MAX_LINE_BYTES + TRUNCATION_MARKER.len());
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        // A multi-byte char straddling the cap must not split.
        let mut big = "y".repeat(MAX_LINE_BYTES - 1);
        big.push('\u{1F600}'); // 4 bytes, crosses the boundary
        let raw = format!("@@ -1 +1 @@\n-{big}\n");
        let diff = parse_file_diff(&raw).expect("parse");
        assert!(diff.hunks[0].lines[0].content.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn overflowing_hunk_number_is_typed_error() {
        let raw = "@@ -99999999999999999999,1 +1,1 @@\n x\n";
        let err = parse_file_diff(raw).expect_err("overflow must error");
        assert!(matches!(err, ParseError::HunkHeader { .. }));
    }

    #[test]
    fn header_at_the_extent_limit_is_accepted() {
        // start + len == 2^32 exactly: at the boundary, not over it.
        let raw = "@@ -4294967295,0 +1,1 @@\n+a\n";
        let diff = parse_file_diff(raw).expect("boundary extent accepted");
        assert_eq!(diff.hunks[0].old_start, u32::MAX);
    }

    #[test]
    fn header_extent_beyond_u32_range_is_a_typed_error() {
        let raw = "@@ -4294967295,2 +1,1 @@\n x\n";
        let err = parse_file_diff(raw).expect_err("extent overflow must error");
        assert!(matches!(err, ParseError::Extent { .. }));
    }

    #[test]
    fn non_numeric_hunk_shape_is_not_a_header() {
        // Shaped like a header but with junk numbers: skipped as header noise,
        // not an error (never reached body phase).
        let raw = "@@ -a,b +c,d @@\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert!(diff.hunks.is_empty());
    }

    #[test]
    fn empty_input_yields_empty_diff() {
        let diff = parse_file_diff("").expect("parse");
        assert!(diff.hunks.is_empty());
        assert_eq!(diff.kind, FileDiffKind::Text);
    }

    #[test]
    fn header_only_diff_yields_no_hunks() {
        // Mode-change-only diffs have headers but no hunks.
        let raw = "diff --git a/x b/x\nold mode 100644\nnew mode 100755\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert!(diff.hunks.is_empty());
    }

    #[test]
    fn unicode_and_cjk_content_round_trips() {
        let raw = "@@ -1,2 +1,2 @@\n \u{4f60}\u{597d}\n-caf\u{e9} \u{1F600}\n+caf\u{e9}!\n";
        let diff = parse_file_diff(raw).expect("parse");
        assert_eq!(diff.hunks[0].lines[0].content, "\u{4f60}\u{597d}");
        assert_eq!(diff.hunks[0].lines[1].content, "caf\u{e9} \u{1F600}");
    }
}
