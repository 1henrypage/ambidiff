//! Syntax highlighting: syntect (fancy-regex mode, pure Rust, wasm-clean)
//! over revdiff's pseudo-file reconstruction.
//!
//! The diff's context+added lines form a "new" pseudo file and context+removed
//! lines an "old" pseudo file; each is highlighted as a whole file so
//! multi-line tokens (strings, comments) carry correct lexical state, then
//! spans map back to side line numbers. Frontends paint the resulting RGB
//! spans verbatim, which keeps the three frontends pixel-consistent.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::model::{FileDiff, FileDiffKind, LineKind};

/// Total pseudo-file size above which highlighting is skipped entirely.
const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;

/// Per-line length above which the line gets no spans (minified content).
const MAX_HIGHLIGHT_LINE_BYTES: usize = 2000;

/// An RGB color serialized as `#rrggbb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Serialize for Rgb {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2))
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let hex = s.strip_prefix('#').unwrap_or(&s);
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(serde::de::Error::custom("expected #rrggbb"));
        }
        let v = u32::from_str_radix(hex, 16).map_err(serde::de::Error::custom)?;
        Ok(Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
    }
}

/// One syntax highlight span; byte offsets into the line content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HlSpan {
    pub start: u32,
    pub end: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fg: Option<Rgb>,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub bold: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub italic: bool,
}

/// Built-in theme choice; frontends map their own light/dark setting to this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    Dark,
    Light,
}

impl ThemeChoice {
    fn syntect_name(self) -> &'static str {
        match self {
            ThemeChoice::Dark => "base16-ocean.dark",
            ThemeChoice::Light => "InspiredGitHub",
        }
    }
}

/// Highlight spans for one file diff, keyed by side line number. BTreeMap so
/// serialization order is deterministic (the wasm parity harness hashes it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightMap {
    pub old: BTreeMap<u32, Vec<HlSpan>>,
    pub new: BTreeMap<u32, Vec<HlSpan>>,
}

fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static SET: OnceLock<ThemeSet> = OnceLock::new();
    SET.get_or_init(ThemeSet::load_defaults)
}

fn file_extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => name,
    }
}

/// Extensions syntect's bundled set does not know, mapped to the closest
/// bundled syntax. Only near-exact supersets/relatives are listed: a wrong
/// grammar paints worse than no grammar. TypeScript is a syntactic superset
/// of JavaScript (type annotations degrade to identifiers), which is the
/// same fallback other syntect-based diff tools use. Precise grammars for
/// these arrive with the planned tree-sitter upgrade.
const EXTENSION_ALIASES: &[(&str, &str)] = &[
    ("ts", "js"),
    ("mts", "js"),
    ("cts", "js"),
    ("tsx", "js"),
    ("jsx", "js"),
    ("mjs", "js"),
    ("cjs", "js"),
    ("jsonc", "json"),
    ("json5", "json"),
    ("vue", "html"),
    ("svelte", "html"),
    ("astro", "html"),
    ("kt", "java"),
    ("kts", "java"),
];

/// Resolve a syntax for `path`, trying the real extension, then the alias
/// table, then a first-line shebang/modeline sniff.
fn find_syntax<'a>(
    ss: &'a SyntaxSet,
    path: &str,
    first_line: Option<&str>,
) -> Option<&'a syntect::parsing::SyntaxReference> {
    let ext = file_extension(path);
    if let Some(syntax) = ss.find_syntax_by_extension(ext) {
        return Some(syntax);
    }
    if let Some((_, alias)) = EXTENSION_ALIASES
        .iter()
        .find(|(from, _)| from.eq_ignore_ascii_case(ext))
        && let Some(syntax) = ss.find_syntax_by_extension(alias)
    {
        return Some(syntax);
    }
    first_line.and_then(|line| ss.find_syntax_by_first_line(line))
}

/// Compute highlight spans for a text diff. Returns an empty map when the
/// language is unknown, the diff is binary/too large, or content exceeds the
/// size caps.
pub fn highlight_diff(path: &str, diff: &FileDiff, theme: ThemeChoice) -> HighlightMap {
    if diff.kind != FileDiffKind::Text {
        return HighlightMap::default();
    }

    // Guard on the size of the pseudo-files BEFORE allocating them: a
    // context line contributes to both sides, so its length counts twice.
    // Rejecting here means an oversized diff never pays for the
    // reconstruction it would only throw away.
    let mut estimated_bytes: usize = 0;
    for hunk in &diff.hunks {
        for line in &hunk.lines {
            let len = line.content.len() + 1; // +1 for the newline pushed below
            match line.kind {
                LineKind::Context => estimated_bytes += len * 2,
                LineKind::Add | LineKind::Remove => estimated_bytes += len,
            }
        }
    }
    if estimated_bytes > MAX_HIGHLIGHT_BYTES {
        return HighlightMap::default();
    }

    // Reconstruct pseudo files with parallel side-line-number tracking.
    let mut new_content = String::new();
    let mut old_content = String::new();
    let mut new_line_nums: Vec<u32> = Vec::new();
    let mut old_line_nums: Vec<u32> = Vec::new();
    for hunk in &diff.hunks {
        for line in &hunk.lines {
            match line.kind {
                LineKind::Add => {
                    new_content.push_str(&line.content);
                    new_content.push('\n');
                    new_line_nums.push(line.new_num);
                }
                LineKind::Remove => {
                    old_content.push_str(&line.content);
                    old_content.push('\n');
                    old_line_nums.push(line.old_num);
                }
                LineKind::Context => {
                    new_content.push_str(&line.content);
                    new_content.push('\n');
                    new_line_nums.push(line.new_num);
                    old_content.push_str(&line.content);
                    old_content.push('\n');
                    old_line_nums.push(line.old_num);
                }
            }
        }
    }

    if new_content.len() + old_content.len() > MAX_HIGHLIGHT_BYTES {
        return HighlightMap::default();
    }

    let ss = syntax_set();
    let Some(syntax) = find_syntax(ss, path, new_content.lines().next()) else {
        return HighlightMap::default();
    };
    let theme = &theme_set().themes[theme.syntect_name()];

    HighlightMap {
        new: highlight_pseudo_file(&new_content, &new_line_nums, syntax, theme, ss),
        old: highlight_pseudo_file(&old_content, &old_line_nums, syntax, theme, ss),
    }
}

fn highlight_pseudo_file(
    content: &str,
    line_nums: &[u32],
    syntax: &syntect::parsing::SyntaxReference,
    theme: &syntect::highlighting::Theme,
    ss: &SyntaxSet,
) -> BTreeMap<u32, Vec<HlSpan>> {
    let mut out = BTreeMap::new();
    if content.is_empty() {
        return out;
    }
    let mut hl = HighlightLines::new(syntax, theme);
    // split_inclusive keeps the trailing newline syntect's -newlines syntax
    // set expects on every line.
    for (idx, line) in content.split_inclusive('\n').enumerate() {
        let Some(&line_num) = line_nums.get(idx) else {
            break;
        };
        if line.len() > MAX_HIGHLIGHT_LINE_BYTES {
            continue;
        }
        let Ok(regions) = hl.highlight_line(line, ss) else {
            continue;
        };
        let spans = regions_to_spans(&regions);
        if !spans.is_empty() {
            out.insert(line_num, spans);
        }
    }
    out
}

/// Convert syntect styled regions into byte-offset spans, merging adjacent
/// regions with identical style and dropping the trailing newline byte.
fn regions_to_spans(regions: &[(syntect::highlighting::Style, &str)]) -> Vec<HlSpan> {
    let mut spans: Vec<HlSpan> = Vec::new();
    let mut offset: u32 = 0;
    for (style, text) in regions {
        let text = text.strip_suffix('\n').unwrap_or(text);
        let len = text.len() as u32;
        if len == 0 {
            offset += len;
            continue;
        }
        let fg = style.foreground;
        let span = HlSpan {
            start: offset,
            end: offset + len,
            fg: Some(Rgb(fg.r, fg.g, fg.b)),
            bold: style.font_style.contains(FontStyle::BOLD),
            italic: style.font_style.contains(FontStyle::ITALIC),
        };
        offset += len;
        match spans.last_mut() {
            Some(prev)
                if prev.end == span.start
                    && prev.fg == span.fg
                    && prev.bold == span.bold
                    && prev.italic == span.italic =>
            {
                prev.end = span.end;
            }
            _ => spans.push(span),
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_file_diff;

    #[test]
    fn rgb_serde_round_trips_as_hex() {
        let rgb = Rgb(0x12, 0xab, 0xff);
        let json = serde_json::to_string(&rgb).expect("serialize");
        assert_eq!(json, "\"#12abff\"");
        let back: Rgb = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, rgb);
    }

    #[test]
    fn file_extension_extraction() {
        assert_eq!(file_extension("src/main.rs"), "rs");
        assert_eq!(file_extension("a/b/Makefile"), "Makefile");
        assert_eq!(file_extension(".gitignore"), ".gitignore");
        assert_eq!(file_extension("x.test.ts"), "ts");
    }

    #[test]
    fn highlights_rust_diff_on_both_sides() {
        let raw = "@@ -1,3 +1,3 @@\n fn main() {\n-    let x = 1;\n+    let x = 2;\n }\n";
        let diff = parse_file_diff(raw).expect("parse");
        let map = highlight_diff("src/main.rs", &diff, ThemeChoice::Dark);
        // Context line 1 highlighted on both sides; changed line on each side.
        assert!(map.new.contains_key(&1), "context line on new side");
        assert!(map.old.contains_key(&1), "context line on old side");
        assert!(map.old.contains_key(&2), "removed line on old side");
        assert!(map.new.contains_key(&2), "added line on new side");
        // Spans must be in-bounds and ordered.
        for spans in map.new.values() {
            let mut prev_end = 0;
            for s in spans {
                assert!(s.start >= prev_end);
                assert!(s.end > s.start);
                prev_end = s.end;
            }
        }
    }

    #[test]
    fn typescript_falls_back_to_javascript_grammar() {
        let raw = "@@ -1,2 +1,2 @@\n const routes = [];\n-export function f(x: string) {}\n+export function g(x: string) {}\n";
        let diff = parse_file_diff(raw).expect("parse");
        for path in ["a.ts", "a.tsx", "a.mts", "comp.jsx"] {
            let map = highlight_diff(path, &diff, ThemeChoice::Dark);
            assert!(
                !map.new.is_empty(),
                "{path} must highlight via the alias table"
            );
        }
    }

    #[test]
    fn alias_table_entries_all_resolve() {
        let ss = syntax_set();
        for (from, alias) in EXTENSION_ALIASES {
            assert!(
                ss.find_syntax_by_extension(alias).is_some(),
                "alias target {alias:?} for {from:?} is not a bundled syntax"
            );
            assert!(
                ss.find_syntax_by_extension(from).is_none(),
                "{from:?} is bundled now; drop its alias"
            );
        }
    }

    #[test]
    fn shebang_first_line_resolves_extensionless_scripts() {
        let raw = "@@ -1,2 +1,2 @@\n #!/usr/bin/env python3\n-x = 1\n+x = 2\n";
        let diff = parse_file_diff(raw).expect("parse");
        let map = highlight_diff("scripts/run", &diff, ThemeChoice::Dark);
        assert!(!map.new.is_empty(), "shebang sniff must find a grammar");
    }

    #[test]
    fn unknown_language_returns_empty_map() {
        let raw = "@@ -1 +1 @@\n-a\n+b\n";
        let diff = parse_file_diff(raw).expect("parse");
        let map = highlight_diff("noext_unknown", &diff, ThemeChoice::Dark);
        assert!(map.new.is_empty() && map.old.is_empty());
    }

    #[test]
    fn light_and_dark_themes_differ() {
        let raw = "@@ -1 +1 @@\n-let x = 1;\n+let x = 2;\n";
        let diff = parse_file_diff(raw).expect("parse");
        let dark = highlight_diff("a.rs", &diff, ThemeChoice::Dark);
        let light = highlight_diff("a.rs", &diff, ThemeChoice::Light);
        assert_ne!(dark, light);
    }

    #[test]
    fn oversized_pseudo_file_is_rejected_before_reconstruction() {
        // A single context line long enough to blow the budget once counted
        // on both sides (but not on one side alone) must still be rejected;
        // proves the estimate accounts for context lines counting twice.
        let big = "x".repeat(MAX_HIGHLIGHT_BYTES / 2 + 10);
        let raw = format!("@@ -1,2 +1,2 @@\n {big}\n-a\n+b\n");
        let diff = parse_file_diff(&raw).expect("parse");
        let map = highlight_diff("a.rs", &diff, ThemeChoice::Dark);
        assert!(map.new.is_empty() && map.old.is_empty());
    }

    #[test]
    fn binary_diff_gets_no_highlights() {
        let raw = "diff --git a/x.png b/x.png\nBinary files a/x.png and b/x.png differ\n";
        let diff = parse_file_diff(raw).expect("parse");
        let map = highlight_diff("x.png", &diff, ThemeChoice::Dark);
        assert!(map.new.is_empty());
    }

    #[test]
    fn deterministic_across_runs() {
        let raw = "@@ -1,2 +1,2 @@\n import os\n-print(1)\n+print(2)\n";
        let diff = parse_file_diff(raw).expect("parse");
        let a =
            serde_json::to_string(&highlight_diff("x.py", &diff, ThemeChoice::Dark)).expect("json");
        let b =
            serde_json::to_string(&highlight_diff("x.py", &diff, ThemeChoice::Dark)).expect("json");
        assert_eq!(a, b);
    }
}
