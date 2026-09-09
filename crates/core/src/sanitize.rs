//! Terminal-injection hardening for untrusted text (diff content, VCS
//! metadata, review comment bodies) before it reaches a terminal renderer.
//!
//! Ported from revdiff's `SanitizeCommitText`. Strips ANSI CSI sequences,
//! stray ESC bytes, C0 controls (except TAB), DEL, CR (cursor-to-column-0
//! overwrite attacks), C1 code points (8-bit CSI/OSC), and invalid UTF-8
//! bytes, while preserving printable runes including CJK and emoji.
//!
//! Input here is already line-split, so LF is treated as unsafe too (unlike
//! the revdiff original, which sanitized multi-line bodies).

/// True when `r` must not reach the terminal.
fn is_unsafe_char(r: char) -> bool {
    match r {
        '\t' => false,
        c if (c as u32) < 0x20 => true,
        '\u{7f}' => true,
        c if ('\u{80}'..='\u{9f}').contains(&c) => true,
        _ => false,
    }
}

fn has_unsafe_content(s: &str) -> bool {
    s.chars().any(is_unsafe_char)
}

/// Strip a complete ANSI CSI sequence starting at `bytes[i]` (which must be
/// ESC). Returns the index just past the sequence, or `i + 1` when the ESC
/// does not open a CSI sequence (the bare ESC byte is dropped by the caller's
/// unsafe-char handling).
fn skip_csi(bytes: &[u8], i: usize) -> usize {
    debug_assert_eq!(bytes[i], 0x1b);
    if bytes.get(i + 1) != Some(&b'[') {
        return i + 1;
    }
    let mut j = i + 2;
    // parameter bytes
    while j < bytes.len() && matches!(bytes[j], b'0'..=b'9' | b';' | b'?') {
        j += 1;
    }
    // intermediate bytes
    while j < bytes.len() && (0x20..=0x2f).contains(&bytes[j]) {
        j += 1;
    }
    // final byte
    if j < bytes.len() && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'~') {
        return j + 1;
    }
    // Unterminated or malformed: drop only the ESC.
    i + 1
}

/// Sanitize one line of untrusted text for terminal display. Returns the
/// input unchanged (borrowed) on the fast path.
///
/// Invalid UTF-8 never reaches this function: byte ingestion goes through
/// `String::from_utf8_lossy`, whose U+FFFD replacement is printable and safe
/// (this is where revdiff's raw-byte 8-bit CSI hole is closed in Rust).
pub fn sanitize_line(s: &str) -> std::borrow::Cow<'_, str> {
    if !has_unsafe_content(s) {
        return std::borrow::Cow::Borrowed(s);
    }

    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i = skip_csi(bytes, i);
            continue;
        }
        // skip_csi always lands on a char boundary (CSI bodies are ASCII),
        // so decoding one char from s[i..] is safe.
        let Some(c) = s[i..].chars().next() else {
            break;
        };
        if !is_unsafe_char(c) {
            out.push(c);
        }
        i += c.len_utf8();
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_borrowed_unchanged() {
        let s = "normal text with tab\tand CJK \u{4f60}\u{597d} and emoji \u{1F600}";
        match sanitize_line(s) {
            std::borrow::Cow::Borrowed(b) => assert_eq!(b, s),
            std::borrow::Cow::Owned(_) => panic!("expected borrowed fast path"),
        }
    }

    #[test]
    fn ansi_csi_sequences_are_stripped() {
        assert_eq!(sanitize_line("a\x1b[31mred\x1b[0mb"), "aredb");
        assert_eq!(sanitize_line("x\x1b[2Jy"), "xy"); // clear screen
        assert_eq!(sanitize_line("x\x1b[?25lz"), "xz"); // hide cursor
    }

    #[test]
    fn bare_esc_and_non_csi_escapes_are_dropped() {
        assert_eq!(sanitize_line("a\x1bb"), "ab");
        assert_eq!(sanitize_line("a\x1b]0;title\x07b"), "a]0;titleb"); // OSC: ESC dropped, BEL dropped
    }

    #[test]
    fn carriage_return_is_stripped() {
        // CR lets crafted content overwrite earlier text on the line.
        assert_eq!(sanitize_line("safe\rEVIL"), "safeEVIL");
    }

    #[test]
    fn c0_controls_except_tab_are_stripped() {
        assert_eq!(sanitize_line("a\x07b\x08c\x0bd\x0ce"), "abcde");
        assert_eq!(sanitize_line("keep\ttab"), "keep\ttab");
    }

    #[test]
    fn del_and_c1_code_points_are_stripped() {
        assert_eq!(sanitize_line("a\u{7f}b"), "ab");
        // U+009B is the single-char CSI; must not survive.
        assert_eq!(sanitize_line("a\u{9b}31mb"), "a31mb");
    }

    #[test]
    fn unterminated_csi_drops_only_the_esc() {
        assert_eq!(sanitize_line("a\x1b[12;"), "a[12;");
    }

    #[test]
    fn multibyte_chars_survive_around_stripping() {
        assert_eq!(
            sanitize_line("\u{4f60}\x1b[31m\u{597d}"),
            "\u{4f60}\u{597d}"
        );
    }
}
