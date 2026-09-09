//! Diff source abstraction. Git is the only v1 implementation; the trait is
//! the seam future FilesSource/PatchSource adapters plug into (revdiff's
//! pattern), and the in-memory fake used by tests.
//!
//! This module is pure: the endpoint/comparison vocabulary, the error type,
//! and the trait are shared by every frontend including wasm. Only
//! `git_source` (native-only) and `rootio` (native+unix) touch the
//! filesystem or spawn a process.

use serde::Serialize;

use crate::model::{FileDiff, FileEntry};
use crate::review::Side;

/// A request for one file's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiffRequest {
    /// Current path (new side for renames).
    pub path: String,
    /// Rename origin when known.
    pub old_path: Option<String>,
    /// Context lines around hunks.
    pub context: u32,
}

impl FileDiffRequest {
    pub fn for_entry(entry: &FileEntry, context: u32) -> Self {
        FileDiffRequest {
            path: entry.path.clone(),
            old_path: entry.old_path.clone(),
            context,
        }
    }

    /// The path this request addresses on `side`: the rename origin on the
    /// old side when known, the current path otherwise.
    pub fn path_on(&self, side: Side) -> &str {
        match side {
            Side::Old => self.old_path.as_deref().unwrap_or(&self.path),
            Side::New => &self.path,
        }
    }
}

/// One endpoint of a [`Comparison`]: a commit tree, the well-known empty
/// tree (an unborn `HEAD`), the index, or the working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Endpoint {
    Commit { oid: String },
    EmptyTree { oid: String },
    Index,
    Worktree,
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::Commit { oid } => write!(f, "{oid}"),
            Endpoint::EmptyTree { .. } => write!(f, "empty"),
            Endpoint::Index => write!(f, "index"),
            Endpoint::Worktree => write!(f, "worktree"),
        }
    }
}

/// The two endpoints a diff is taken between, resolved once per `open`
/// (`GitSource` re-resolves independently for `try_signature`, never
/// mutating the cached value).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Comparison {
    pub old: Endpoint,
    pub new: Endpoint,
}

impl Comparison {
    pub fn endpoint(&self, side: Side) -> &Endpoint {
        match side {
            Side::Old => &self.old,
            Side::New => &self.new,
        }
    }

    /// True when the new side is the live working tree: untracked files
    /// participate, and reads go through the filesystem rather than a git
    /// object.
    pub fn new_is_worktree(&self) -> bool {
        matches!(self.new, Endpoint::Worktree)
    }
}

impl std::fmt::Display for Comparison {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}..{}", self.old, self.new)
    }
}

/// Why a candidate root-relative path was rejected before any syscall
/// touched it, or partway through the walk (`SymlinkComponent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PathReason {
    Empty,
    Absolute,
    ParentComponent,
    CurrentComponent,
    EmptyComponent,
    TrailingSlash,
    ContainsNul,
    SymlinkComponent,
}

/// Why a changed path was skipped from a [`Listing`] rather than reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SkipReason {
    /// Either side of the path is not valid UTF-8; `SkippedPath::display`
    /// carries a `\xNN`-escaped rendering rather than a guess.
    NonUtf8,
}

/// A changed path git reported that could not be turned into a [`FileEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedPath {
    pub display: String,
    pub reason: SkipReason,
}

/// The result of listing changed files: entries the viewer can open, plus
/// paths that were skipped and why (surfaced as warnings, never silently
/// dropped).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Listing {
    /// Sorted by path.
    pub entries: Vec<FileEntry>,
    pub skipped: Vec<SkippedPath>,
}

/// Errors from a diff source.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("not a git repository: {root}")]
    NotARepo { root: String },
    #[error("git {args} failed: {stderr}")]
    GitFailed { args: String, stderr: String },
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid ref: {spec}")]
    InvalidRef { spec: String },
    #[error("no merge base between {left} and {right}")]
    NoMergeBase { left: String, right: String },
    #[error("unsupported comparison {base}: {reason}")]
    UnsupportedComparison { base: String, reason: String },
    #[error("invalid path {path}: {reason:?}")]
    InvalidPath { path: String, reason: PathReason },
    #[error("{context} exceeds {limit_bytes} bytes")]
    TooLarge { context: String, limit_bytes: u64 },
    #[error("git {args} timed out after {seconds}s")]
    Timeout { args: String, seconds: u64 },
}

impl SourceError {
    pub fn is_invalid_path(&self) -> bool {
        matches!(self, SourceError::InvalidPath { .. })
    }
}

/// A pluggable diff source.
pub trait DiffSource {
    /// List changed files with statuses and numstat counts, plus any paths
    /// skipped as unrepresentable (non-UTF-8).
    fn listing(&self) -> Result<Listing, SourceError>;
    /// Produce one file's parsed diff.
    fn file_diff(&self, req: &FileDiffRequest) -> Result<FileDiff, SourceError>;
    /// Read a whole side's content of `path` (the path AS IT EXISTS on that
    /// side: the rename origin for the old side of a rename). `Ok(None)`
    /// means the path does not exist on that side (e.g. an added file's old
    /// side, or an untracked file's old side).
    fn read_side(&self, side: Side, path: &str) -> Result<Option<String>, SourceError>;
    /// Read a 1-based inclusive line range of a side's content, without
    /// requiring the whole file. `Ok(None)` means the path does not exist on
    /// that side.
    fn read_side_lines(
        &self,
        side: Side,
        path: &str,
        first: u32,
        last: u32,
    ) -> Result<Option<Vec<String>>, SourceError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileStatus;

    fn entry() -> FileEntry {
        FileEntry {
            path: "new.rs".into(),
            old_path: Some("old.rs".into()),
            status: FileStatus::Renamed,
            adds: Some(1),
            dels: Some(2),
        }
    }

    #[test]
    fn path_on_uses_rename_origin_for_old_side_only() {
        let req = FileDiffRequest::for_entry(&entry(), 3);
        assert_eq!(req.path_on(Side::Old), "old.rs");
        assert_eq!(req.path_on(Side::New), "new.rs");
    }

    #[test]
    fn path_on_falls_back_to_path_without_a_rename() {
        let req = FileDiffRequest {
            path: "a.rs".into(),
            old_path: None,
            context: 3,
        };
        assert_eq!(req.path_on(Side::Old), "a.rs");
        assert_eq!(req.path_on(Side::New), "a.rs");
    }

    #[test]
    fn comparison_display_and_endpoint_accessors() {
        let cmp = Comparison {
            old: Endpoint::Commit {
                oid: "deadbeef".into(),
            },
            new: Endpoint::Worktree,
        };
        assert_eq!(cmp.to_string(), "deadbeef..worktree");
        assert_eq!(
            cmp.endpoint(Side::Old),
            &Endpoint::Commit {
                oid: "deadbeef".into()
            }
        );
        assert_eq!(cmp.endpoint(Side::New), &Endpoint::Worktree);
        assert!(cmp.new_is_worktree());

        let staged = Comparison {
            old: Endpoint::Index,
            new: Endpoint::Index,
        };
        assert_eq!(staged.to_string(), "index..index");
        assert!(!staged.new_is_worktree());

        let unborn = Comparison {
            old: Endpoint::EmptyTree {
                oid: "4b825dc".into(),
            },
            new: Endpoint::Index,
        };
        assert_eq!(unborn.to_string(), "empty..index");
    }

    #[test]
    fn a_trait_implementation_serves_listing_and_read_side_directly() {
        struct Fake;
        impl DiffSource for Fake {
            fn listing(&self) -> Result<Listing, SourceError> {
                Ok(Listing {
                    entries: vec![entry()],
                    skipped: vec![],
                })
            }
            fn file_diff(&self, _req: &FileDiffRequest) -> Result<FileDiff, SourceError> {
                Ok(FileDiff::empty())
            }
            fn read_side(&self, side: Side, _path: &str) -> Result<Option<String>, SourceError> {
                Ok(match side {
                    Side::Old => Some("old content".into()),
                    Side::New => None,
                })
            }
            fn read_side_lines(
                &self,
                _side: Side,
                _path: &str,
                _first: u32,
                _last: u32,
            ) -> Result<Option<Vec<String>>, SourceError> {
                Ok(None)
            }
        }
        let fake = Fake;
        assert_eq!(fake.listing().expect("listing").entries, vec![entry()]);
        assert_eq!(
            fake.read_side(Side::Old, "x").expect("read old"),
            Some("old content".into())
        );
        assert_eq!(fake.read_side(Side::New, "x").expect("read new"), None);
    }
}
