//! View assembly: the single entry point every frontend calls to get a
//! paintable file view. Runs identically native and in wasm; frontends only
//! paint what comes out of here (the drift firewall).

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

use crate::highlight::{HighlightMap, ThemeChoice, highlight_diff};
use crate::model::{FileDiff, FileDiffKind, FileEntry, FileStatus};
use crate::rows::{BuildOptions, CellKind, Row, ViewMode, build_rows};

/// Options controlling view derivation.
#[derive(Debug, Clone, Copy)]
pub struct ViewOptions {
    pub mode: ViewMode,
    pub word_diff: bool,
    /// Compute and attach syntax highlight spans for this theme.
    pub highlight: Option<ThemeChoice>,
}

impl Default for ViewOptions {
    fn default() -> Self {
        ViewOptions {
            mode: ViewMode::Unified,
            word_diff: true,
            highlight: None,
        }
    }
}

/// A fully derived, paint-ready view of one file's diff.
///
/// Serialises by hand rather than deriving: `kind` used to be flattened
/// alongside the view's own `adds`/`dels`, so a too-large placeholder (whose
/// `FileDiffKind::TooLarge` variant carries its own `adds`/`dels`) emitted
/// each field twice on the wire -- `to_string` produced duplicate JSON keys
/// and `to_value` silently collapsed them to zero (B16). Each field is
/// written exactly once here: `kind` contributes only its tag (plus `desc`
/// for binary), and `adds`/`dels` always come from the view's own fields,
/// which [`FileView::assemble`] fills with the preflight counts for
/// too-large diffs.
#[derive(Debug, Clone)]
pub struct FileView {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub kind: FileDiffKind,
    pub mode: ViewMode,
    pub adds: u64,
    pub dels: u64,
    pub old_missing_newline: bool,
    pub new_missing_newline: bool,
    pub rows: Vec<Row>,
}

impl Serialize for FileView {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("path", &self.path)?;
        if let Some(old_path) = &self.old_path {
            map.serialize_entry("oldPath", old_path)?;
        }
        map.serialize_entry("status", &self.status)?;
        match &self.kind {
            FileDiffKind::Text => {
                map.serialize_entry("kind", "text")?;
            }
            FileDiffKind::Binary { desc } => {
                map.serialize_entry("kind", "binary")?;
                map.serialize_entry("desc", desc)?;
            }
            FileDiffKind::TooLarge { .. } => {
                map.serialize_entry("kind", "toolarge")?;
            }
        }
        map.serialize_entry("mode", &self.mode)?;
        map.serialize_entry("adds", &self.adds)?;
        map.serialize_entry("dels", &self.dels)?;
        map.serialize_entry("oldMissingNewline", &self.old_missing_newline)?;
        map.serialize_entry("newMissingNewline", &self.new_missing_newline)?;
        map.serialize_entry("rows", &self.rows)?;
        map.end()
    }
}

impl FileView {
    /// Build the complete view for one file: the one placeholder-capable
    /// path every frontend and transport calls, whether the diff is text,
    /// binary, or a too-large preflight placeholder ([`FileDiff::placeholder`]).
    pub fn assemble(entry: &FileEntry, diff: &FileDiff, opts: ViewOptions) -> FileView {
        let mut rows = build_rows(
            diff,
            BuildOptions {
                mode: opts.mode,
                word_diff: opts.word_diff,
            },
        );

        if let Some(theme) = opts.highlight {
            let map = highlight_diff(&entry.path, diff, theme);
            attach_highlights(&mut rows, &map);
        }

        // Each wire count comes from exactly one source: the preflight
        // numstat counts for a too-large placeholder (its hunks are empty,
        // so `count_changes` would otherwise report zero), the hunk tally
        // for everything else.
        let (adds, dels) = match &diff.kind {
            FileDiffKind::TooLarge { adds, dels } => (*adds, *dels),
            _ => diff.count_changes(),
        };
        FileView {
            path: entry.path.clone(),
            old_path: entry.old_path.clone(),
            status: entry.status,
            kind: diff.kind.clone(),
            mode: opts.mode,
            adds,
            dels,
            old_missing_newline: diff.old_missing_newline,
            new_missing_newline: diff.new_missing_newline,
            rows,
        }
    }
}

/// Build the complete view for one file. Alias for [`FileView::assemble`]
/// kept for existing callers.
pub fn build_file_view(entry: &FileEntry, diff: &FileDiff, opts: ViewOptions) -> FileView {
    FileView::assemble(entry, diff, opts)
}

/// Attach highlight spans to row cells by (side, line number) lookup.
/// Removed cells read the old-side map; added and context cells the new-side
/// (split left cells read old-side context spans so per-side lexical state
/// stays exact).
pub fn attach_highlights(rows: &mut [Row], map: &HighlightMap) {
    for row in rows.iter_mut() {
        match row {
            Row::Unified { cell, .. } => {
                let Some(line) = cell.line else { continue };
                let spans = match cell.kind {
                    CellKind::Remove => map.old.get(&line),
                    CellKind::Add | CellKind::Context => map.new.get(&line),
                    CellKind::Empty => None,
                };
                if let Some(spans) = spans {
                    cell.hl = spans.clone();
                }
            }
            Row::Split { left, right, .. } => {
                if let Some(line) = left.line
                    && left.kind != CellKind::Empty
                    && let Some(spans) = map.old.get(&line)
                {
                    left.hl = spans.clone();
                }
                if let Some(line) = right.line
                    && right.kind != CellKind::Empty
                    && let Some(spans) = map.new.get(&line)
                {
                    right.hl = spans.clone();
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileStatus;
    use crate::parser::parse_file_diff;

    fn entry(path: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            adds: None,
            dels: None,
        }
    }

    #[test]
    fn view_carries_counts_and_rows() {
        let raw = "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        let diff = parse_file_diff(raw).expect("parse");
        let view = build_file_view(&entry("f.rs"), &diff, ViewOptions::default());
        assert_eq!((view.adds, view.dels), (1, 1));
        assert_eq!(view.rows.len(), 5);
    }

    #[test]
    fn highlights_attach_to_matching_cells() {
        let raw = "@@ -1,2 +1,2 @@\n fn main() {}\n-let a = 1;\n+let a = 2;\n";
        let diff = parse_file_diff(raw).expect("parse");
        let view = build_file_view(
            &entry("m.rs"),
            &diff,
            ViewOptions {
                mode: ViewMode::Split,
                word_diff: true,
                highlight: Some(ThemeChoice::Dark),
            },
        );
        let mut saw_hl = false;
        for row in &view.rows {
            if let Row::Split { left, right, .. } = row {
                saw_hl |= !left.hl.is_empty() || !right.hl.is_empty();
                for span in &left.hl {
                    assert!(span.end as usize <= left.text.len(), "left span in bounds");
                }
                for span in &right.hl {
                    assert!(
                        span.end as usize <= right.text.len(),
                        "right span in bounds"
                    );
                }
            }
        }
        assert!(saw_hl, "rust diff must highlight");
    }

    #[test]
    fn too_large_placeholder_view_carries_counts_exactly_once() {
        let diff = FileDiff::placeholder(FileDiffKind::TooLarge {
            adds: 12_000,
            dels: 20,
        });
        let view = build_file_view(&entry("vendor/bundle.js"), &diff, ViewOptions::default());
        assert_eq!((view.adds, view.dels), (12_000, 20));
        assert!(view.rows.is_empty());

        // Serialised text must carry each wire key once: `to_string` used to
        // emit "adds"/"dels" twice (once from the flattened TooLarge variant,
        // once from the view's own fields) which is exactly B16's defect.
        let text = serde_json::to_string(&view).expect("serialize");
        assert_eq!(text.matches("\"adds\"").count(), 1);
        assert_eq!(text.matches("\"dels\"").count(), 1);

        let json = serde_json::to_value(&view).expect("json");
        assert_eq!(json["kind"], "toolarge");
        assert_eq!(json["adds"], 12000);
        assert_eq!(json["dels"], 20);
        assert!(json.get("desc").is_none());
    }

    #[test]
    fn view_serializes_with_camel_case_kind_tag() {
        let raw = "diff --git a/x.png b/x.png\nBinary files a/x.png and b/x.png differ\n";
        let diff = parse_file_diff(raw).expect("parse");
        let view = build_file_view(&entry("x.png"), &diff, ViewOptions::default());
        let json = serde_json::to_value(&view).expect("json");
        assert_eq!(json["kind"], "binary");
        assert_eq!(json["path"], "x.png");
        assert!(json["rows"].as_array().expect("rows").is_empty());
    }
}
