//! Shared diff model types produced by the parser and consumed by every
//! derivation (rows, word-diff, highlighting, anchors). This is the neutral
//! vocabulary all three frontends speak.

use serde::{Deserialize, Serialize};

/// The change direction of a single diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineKind {
    Context,
    Add,
    Remove,
}

/// One parsed line of a unified diff, without its +/-/space prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    /// 1-based line number in the old version; 0 for additions.
    pub old_num: u32,
    /// 1-based line number in the new version; 0 for removals.
    pub new_num: u32,
    pub kind: LineKind,
    pub content: String,
}

/// One hunk of a unified diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    /// 1-based first line of the hunk on the old side. For insertion-only
    /// hunks git reports the line BEFORE the insertion point (may be 0 when
    /// inserting at the top of the file).
    pub old_start: u32,
    /// Number of old-side lines covered (0 for insertion-only hunks).
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// Function context git appends after the trailing `@@`, already trimmed.
    pub context: String,
    pub lines: Vec<DiffLine>,
}

/// What kind of content a file diff carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FileDiffKind {
    /// Regular text diff with hunks.
    Text,
    /// Binary file; `desc` is a human-readable placeholder such as
    /// "(new binary file)".
    Binary { desc: String },
    /// Diff skipped because it exceeds the size preflight threshold.
    TooLarge { adds: u64, dels: u64 },
}

/// A parsed single-file diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub kind: FileDiffKind,
    pub hunks: Vec<Hunk>,
    /// Total line count of the old version when known; enables the trailing
    /// gap row after the last hunk. None means unknown (no trailing gap).
    pub old_total_lines: Option<u32>,
    /// True when the old version does not end with a newline.
    pub old_missing_newline: bool,
    /// True when the new version does not end with a newline.
    pub new_missing_newline: bool,
}

impl FileDiff {
    pub fn empty() -> Self {
        FileDiff {
            kind: FileDiffKind::Text,
            hunks: Vec::new(),
            old_total_lines: None,
            old_missing_newline: false,
            new_missing_newline: false,
        }
    }

    /// Tally added and removed line counts across all hunks. Widened to u64
    /// so a pathological hunk count near u32::MAX cannot overflow (B24).
    pub fn count_changes(&self) -> (u64, u64) {
        let mut adds: u64 = 0;
        let mut removes: u64 = 0;
        for hunk in &self.hunks {
            for line in &hunk.lines {
                match line.kind {
                    LineKind::Add => adds += 1,
                    LineKind::Remove => removes += 1,
                    LineKind::Context => {}
                }
            }
        }
        (adds, removes)
    }

    /// A placeholder diff carrying only `kind` (binary or too-large): no
    /// hunks, so it builds rows-free views through the same assembly path
    /// text diffs use (the one placeholder-capable view path, B16).
    pub fn placeholder(kind: FileDiffKind) -> Self {
        FileDiff {
            kind,
            hunks: Vec::new(),
            old_total_lines: None,
            old_missing_newline: false,
            new_missing_newline: false,
        }
    }
}

/// The change status of a file in a diff source listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    /// Type change or any status this model does not distinguish further.
    Other,
}

/// One changed file as reported by a diff source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    /// Path relative to the review root (new/current path for renames).
    pub path: String,
    /// Rename or copy origin; None for non-renames.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub old_path: Option<String>,
    pub status: FileStatus,
    /// Added line count from the numstat preflight; None for binary files.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub adds: Option<u64>,
    /// Removed line count from the numstat preflight; None for binary files.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub dels: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_changes_tallies_adds_and_removes_only() {
        let diff = FileDiff {
            kind: FileDiffKind::Text,
            hunks: vec![Hunk {
                old_start: 1,
                old_len: 2,
                new_start: 1,
                new_len: 2,
                context: String::new(),
                lines: vec![
                    DiffLine {
                        old_num: 1,
                        new_num: 1,
                        kind: LineKind::Context,
                        content: "a".into(),
                    },
                    DiffLine {
                        old_num: 2,
                        new_num: 0,
                        kind: LineKind::Remove,
                        content: "b".into(),
                    },
                    DiffLine {
                        old_num: 0,
                        new_num: 2,
                        kind: LineKind::Add,
                        content: "c".into(),
                    },
                ],
            }],
            old_total_lines: None,
            old_missing_newline: false,
            new_missing_newline: false,
        };
        assert_eq!(diff.count_changes(), (1, 1));
    }
}
