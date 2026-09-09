//! Per-file view state: the assembled [`FileView`] plus the expansion and
//! generation bookkeeping every frontend needs, derived once instead of
//! three times (S1). Replaces the pattern where the browser expanded a gap
//! by mutating a cached view and then immediately reloaded the raw diff over
//! it, discarding the expansion it had just computed (B10): a caller now
//! reads the result straight off [`ViewState::expand`] and the cached view
//! it left behind, never by reloading.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::anchor::{
    ActiveCell, AnchoredComment, anchor_file_comments, anchor_target as anchor_target_of,
};
use crate::model::{FileDiff, FileEntry};
use crate::projection::ReviewProjection;
use crate::review::Side;
use crate::rows::{GapInfo, Row, expansion_rows_from_content};
use crate::search::{SearchMatch, search_rows};
use crate::view::{FileView, ViewOptions};

/// Why [`ViewState::expand`] could not apply an expansion.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandError {
    #[error("no gap {gap_id:?} in the current view")]
    NoSuchGap { gap_id: String },
    #[error("gap {gap_id:?} is internally inconsistent")]
    InvalidGap { gap_id: String },
    #[error("stale content: view is at generation {expected}, content is from {got}")]
    StaleContent { expected: u64, got: u64 },
}

/// The result of successfully expanding one gap: the gap that was expanded,
/// where its rows now sit in the view, and the rows themselves -- enough for
/// a frontend to splice them into its own copy of the row stream, or simply
/// to repaint from the (already updated) cached [`FileView`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpansionResult {
    pub gap: GapInfo,
    pub at: usize,
    pub rows: Vec<Row>,
}

/// Everything one frontend needs to paint one file: the assembled view, and
/// its comments already anchored into that view's rows.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileProjection<'a> {
    pub view: &'a FileView,
    pub comments: Vec<AnchoredComment<'a>>,
}

/// One held expansion: the content it was built from and the generation
/// that content belongs to, so a same-generation reload can replay it and a
/// new-generation reload knows to drop it (the diff changed underneath it).
#[derive(Debug, Clone)]
struct HeldExpansion {
    content: String,
    content_generation: u64,
}

/// A cached, paint-ready view of one file plus its expansion state, held
/// across calls so expanding a gap or changing view options never needs to
/// re-parse the diff.
pub struct ViewState {
    entry: FileEntry,
    diff: FileDiff,
    opts: ViewOptions,
    generation: u64,
    /// Held content for gaps the caller has actually expanded, replayed
    /// whenever the view is rebuilt (options change, same-generation
    /// reload).
    expansions: BTreeMap<String, HeldExpansion>,
    /// Gap ids the caller wants expanded, independent of whether content is
    /// currently held for them. Survives a generation bump so the caller
    /// knows which gaps to re-supply content for after the file changes.
    wanted: BTreeSet<String>,
    view: FileView,
}

impl ViewState {
    pub fn new(entry: FileEntry, diff: FileDiff, opts: ViewOptions, generation: u64) -> ViewState {
        let view = FileView::assemble(&entry, &diff, opts);
        ViewState {
            entry,
            diff,
            opts,
            generation,
            expansions: BTreeMap::new(),
            wanted: BTreeSet::new(),
            view,
        }
    }

    pub fn path(&self) -> &str {
        &self.entry.path
    }

    pub fn entry(&self) -> &FileEntry {
        &self.entry
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn options(&self) -> ViewOptions {
        self.opts
    }

    pub fn view(&self) -> &FileView {
        &self.view
    }

    /// Rebuild the view under new options, then replay every held expansion
    /// over the fresh row stream so an option change (word-diff off, split
    /// vs unified, theme) never collapses gaps the caller had already
    /// opened.
    pub fn set_options(&mut self, opts: ViewOptions) {
        self.opts = opts;
        self.rebuild_and_reapply();
    }

    /// Fold in a reloaded diff (the watch controller reread the file). Same
    /// generation means the content did not actually change (only metadata,
    /// or a redundant reload): held expansions are replayed as-is. A new
    /// generation means the diff may have changed underneath any held
    /// expansion content, so that content is dropped -- but `wanted` is
    /// kept, so the caller knows which gaps still need re-expanding once it
    /// has fresh content for them (B10).
    pub fn reload(&mut self, entry: FileEntry, diff: FileDiff, generation: u64) {
        self.entry = entry;
        self.diff = diff;
        if generation != self.generation {
            self.expansions.clear();
            self.generation = generation;
        }
        self.rebuild_and_reapply();
    }

    fn rebuild_and_reapply(&mut self) {
        self.view = FileView::assemble(&self.entry, &self.diff, self.opts);
        let held: Vec<(String, HeldExpansion)> = self
            .expansions
            .iter()
            .map(|(id, held)| (id.clone(), held.clone()))
            .collect();
        for (gap_id, held) in held {
            if held.content_generation != self.generation {
                // Invariant: `expand` only ever stores content stamped with
                // the CURRENT generation, and a generation bump clears
                // `expansions` outright, so this never actually fires; kept
                // as a defensive backstop against a future caller storing
                // stale content some other way.
                continue;
            }
            let Some((idx, gap)) = self.find_gap(&gap_id) else {
                continue; // the gap no longer exists in the rebuilt view
            };
            let rows = expansion_rows_from_content(
                &gap,
                &held.content,
                self.opts.mode,
                hunk_for_gap(&gap, &self.diff),
            );
            self.view.rows.splice(idx..=idx, rows);
        }
    }

    fn find_gap(&self, id: &str) -> Option<(usize, GapInfo)> {
        self.view
            .rows
            .iter()
            .enumerate()
            .find_map(|(idx, row)| match row {
                Row::Gap { gap, .. } if gap.id == id => Some((idx, gap.clone())),
                _ => None,
            })
    }

    pub fn gap(&self, id: &str) -> Option<&GapInfo> {
        self.view.rows.iter().find_map(|row| match row {
            Row::Gap { gap, .. } if gap.id == id => Some(gap),
            _ => None,
        })
    }

    pub fn wanted(&self) -> &BTreeSet<String> {
        &self.wanted
    }

    pub fn mark_wanted(&mut self, gap_id: &str) {
        self.wanted.insert(gap_id.to_string());
    }

    /// Gaps the caller wants expanded that are still collapsed in the
    /// current view -- what the caller needs to fetch content for (again,
    /// after a generation bump dropped the previous content).
    pub fn pending_expansions(&self) -> Vec<GapInfo> {
        self.wanted
            .iter()
            .filter_map(|id| self.gap(id).cloned())
            .collect()
    }

    /// Expand one collapsed gap using `content` (the new-side file text) at
    /// `content_generation`. Splices the resulting rows into the cached view
    /// so the caller can simply repaint from [`ViewState::view`] afterwards
    /// -- no reload can silently undo this (B10).
    pub fn expand(
        &mut self,
        gap_id: &str,
        content: &str,
        content_generation: u64,
    ) -> Result<ExpansionResult, ExpandError> {
        if content_generation != self.generation {
            return Err(ExpandError::StaleContent {
                expected: self.generation,
                got: content_generation,
            });
        }
        let Some((idx, gap)) = self.find_gap(gap_id) else {
            return Err(ExpandError::NoSuchGap {
                gap_id: gap_id.to_string(),
            });
        };
        if !gap.is_consistent() {
            return Err(ExpandError::InvalidGap {
                gap_id: gap_id.to_string(),
            });
        }
        let hunk = hunk_for_gap(&gap, &self.diff);
        let rows = expansion_rows_from_content(&gap, content, self.opts.mode, hunk);
        self.view.rows.splice(idx..=idx, rows.clone());
        self.expansions.insert(
            gap_id.to_string(),
            HeldExpansion {
                content: content.to_string(),
                content_generation,
            },
        );
        Ok(ExpansionResult { gap, at: idx, rows })
    }

    /// This file's comments, anchored into the current view's rows.
    pub fn file_projection<'a>(&'a self, projection: &ReviewProjection<'a>) -> FileProjection<'a> {
        let placed = projection.file_comments(self.path());
        FileProjection {
            view: &self.view,
            comments: anchor_file_comments(&self.view.rows, &placed),
        }
    }

    pub fn search(&self, query: &str) -> Vec<SearchMatch> {
        search_rows(&self.view.rows, query)
    }

    pub fn anchor_target(&self, row: usize, cell: ActiveCell) -> Option<(Side, u32)> {
        self.view
            .rows
            .get(row)
            .and_then(|r| anchor_target_of(r, cell))
    }
}

/// The hunk a gap's expansion rows belong to: the gap's own index for a
/// `before:<idx>` gap, the last hunk for `trailing`.
fn hunk_for_gap(gap: &GapInfo, diff: &FileDiff) -> u32 {
    match gap.id.strip_prefix("before:").and_then(|s| s.parse().ok()) {
        Some(idx) => idx,
        None => diff.hunks.len().saturating_sub(1) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileStatus;
    use crate::parser::parse_file_diff;
    use crate::review::{Comment, ReviewFile, Source, Status};
    use crate::rows::ViewMode;
    use serde_json::Map;

    fn entry(path: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            adds: None,
            dels: None,
        }
    }

    fn text_diff(raw: &str) -> FileDiff {
        parse_file_diff(raw).expect("parse")
    }

    const RAW: &str = "@@ -1,2 +1,2 @@\n a\n-b\n+B\n";

    fn state_with_trailing_gap() -> ViewState {
        let mut diff = text_diff(RAW);
        diff.old_total_lines = Some(6);
        ViewState::new(entry("f.rs"), diff, ViewOptions::default(), 1)
    }

    #[test]
    fn new_state_assembles_the_view_immediately() {
        let state = state_with_trailing_gap();
        assert_eq!(state.path(), "f.rs");
        assert!(
            state
                .view()
                .rows
                .iter()
                .any(|r| matches!(r, Row::Gap { .. }))
        );
    }

    #[test]
    fn expand_splices_rows_and_the_view_reflects_it_without_reload() {
        // B10: the expanded state must be visible directly off the cached
        // view; nothing here reloads the raw diff to get it.
        let mut state = state_with_trailing_gap();
        let content = "a\nB\nc\nd\ne\nf\n";
        let result = state.expand("trailing", content, 1).expect("expand ok");
        assert_eq!(result.rows.len(), 4); // lines 3..6
        assert!(
            !state
                .view()
                .rows
                .iter()
                .any(|r| matches!(r, Row::Gap { .. }))
        );
        assert_eq!(state.view().rows.len(), result.at + result.rows.len());
    }

    #[test]
    fn expand_rejects_stale_content_generation() {
        let mut state = state_with_trailing_gap();
        let err = state
            .expand("trailing", "x\n", 2)
            .expect_err("generation mismatch must fail");
        assert_eq!(
            err,
            ExpandError::StaleContent {
                expected: 1,
                got: 2
            }
        );
    }

    #[test]
    fn expand_rejects_an_unknown_gap() {
        let mut state = state_with_trailing_gap();
        let err = state
            .expand("before:99", "x\n", 1)
            .expect_err("no such gap");
        assert_eq!(
            err,
            ExpandError::NoSuchGap {
                gap_id: "before:99".into()
            }
        );
    }

    #[test]
    fn set_options_rebuilds_and_replays_held_expansions() {
        let mut state = state_with_trailing_gap();
        state
            .expand("trailing", "a\nB\nc\nd\ne\nf\n", 1)
            .expect("expand");
        state.set_options(ViewOptions {
            mode: ViewMode::Split,
            ..ViewOptions::default()
        });
        assert_eq!(state.view().mode, ViewMode::Split);
        assert!(
            !state
                .view()
                .rows
                .iter()
                .any(|r| matches!(r, Row::Gap { .. }))
        );
    }

    #[test]
    fn reload_same_generation_replays_expansions() {
        let mut state = state_with_trailing_gap();
        state
            .expand("trailing", "a\nB\nc\nd\ne\nf\n", 1)
            .expect("expand");
        let mut diff = text_diff(RAW);
        diff.old_total_lines = Some(6);
        state.reload(entry("f.rs"), diff, 1);
        assert!(
            !state
                .view()
                .rows
                .iter()
                .any(|r| matches!(r, Row::Gap { .. }))
        );
    }

    #[test]
    fn reload_new_generation_drops_content_but_keeps_wanted() {
        let mut state = state_with_trailing_gap();
        state.mark_wanted("trailing");
        state
            .expand("trailing", "a\nB\nc\nd\ne\nf\n", 1)
            .expect("expand");
        let mut diff = text_diff("@@ -1,2 +1,2 @@\n a\n-b\n+B2\n");
        diff.old_total_lines = Some(6);
        state.reload(entry("f.rs"), diff, 2);
        // The gap reappears (content dropped, not replayed) and is still
        // reported pending because it stayed in `wanted`.
        assert!(
            state
                .view()
                .rows
                .iter()
                .any(|r| matches!(r, Row::Gap { .. }))
        );
        let pending = state.pending_expansions();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "trailing");
    }

    #[test]
    fn pending_expansions_excludes_gaps_never_marked_wanted() {
        let state = state_with_trailing_gap();
        assert!(state.pending_expansions().is_empty());
    }

    #[test]
    fn file_projection_anchors_this_files_comments() {
        let state = state_with_trailing_gap();
        let mut review = ReviewFile::new("r".into(), Source::git(None), "t");
        review.comments.push(Comment {
            id: "c1".into(),
            rev: 1,
            status: Status::Open,
            path: Some("f.rs".into()),
            side: Some(Side::New),
            line: Some(2),
            end_line: None,
            snippet: Some("B".into()),
            body: "b".into(),
            response: None,
            author: "a".into(),
            created_at: "t".into(),
            updated_at: "t".into(),
            extra: Map::new(),
        });
        let files = vec![entry("f.rs")];
        let projection = ReviewProjection::new(&review, &files, crate::projection::FileFilter::All);
        let fp = state.file_projection(&projection);
        assert_eq!(fp.comments.len(), 1);
        assert!(!fp.comments[0].anchor.outdated);
    }

    #[test]
    fn search_reads_the_current_view() {
        let state = state_with_trailing_gap();
        assert_eq!(state.search("B").len(), 1);
    }

    #[test]
    fn anchor_target_delegates_to_the_row_at_that_index() {
        let state = state_with_trailing_gap();
        // Row 0 is the hunk header, row 1 is context "a".
        assert_eq!(
            state.anchor_target(1, ActiveCell::Auto),
            Some((Side::New, 1))
        );
        assert_eq!(state.anchor_target(9999, ActiveCell::Auto), None);
    }
}
