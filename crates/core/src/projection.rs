//! The review projection: one place that derives placements, per-file
//! tallies, filtering, the file tree, and the overview list together from a
//! complete snapshot of the review and the changed-file list (S1, B15).
//!
//! Every frontend used to assemble these independently (and inconsistently:
//! the TUI, the stdio engine, and the wasm painter each built their own
//! placements + tallies, and the browser rebuilt tallies a fourth time from
//! whatever it had). `ReviewProjection` computes placements and per-file
//! counts once in [`ReviewProjection::new`]; every other accessor reads from
//! that, so a filtered tree, a file's comment list, and the review-level
//! overview can never disagree about what the review currently says.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::anchor::{Placement, place_comments};
use crate::model::FileEntry;
use crate::review::{Comment, ReviewFile};
use crate::tree::{TreeRow, build_filtered_file_tree, flatten_tree};

/// Which files the tree shows. Cycles All -> Annotated -> Unreviewed -> All.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileFilter {
    All,
    Annotated,
    Unreviewed,
}

impl FileFilter {
    /// Human label for the filter chip.
    pub fn label(self) -> &'static str {
        match self {
            FileFilter::All => "All",
            FileFilter::Annotated => "Annotated",
            FileFilter::Unreviewed => "Unreviewed",
        }
    }

    /// The filter one step around the cycle.
    pub fn next(self) -> FileFilter {
        match self {
            FileFilter::All => FileFilter::Annotated,
            FileFilter::Annotated => FileFilter::Unreviewed,
            FileFilter::Unreviewed => FileFilter::All,
        }
    }
}

/// Open/reopened vs total comment tally for one file (or one grouping).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct FileCounts {
    pub todo: usize,
    pub total: usize,
}

impl FileCounts {
    fn add(&mut self, is_todo: bool) {
        self.total += 1;
        if is_todo {
            self.todo += 1;
        }
    }

    fn combined(a: FileCounts, b: FileCounts) -> FileCounts {
        FileCounts {
            todo: a.todo + b.todo,
            total: a.total + b.total,
        }
    }
}

/// A comment placed on a current file, with its rename badge (if the file
/// was renamed since the comment was written) and its index in
/// `review.comments` (kept for callers that need to reference the source
/// comment; skipped on the wire).
pub struct PlacedComment<'a> {
    pub index: usize,
    pub comment: &'a Comment,
    pub was_path: Option<&'a str>,
}

/// A review-level or unattached comment, for the overview pane.
pub struct OverviewComment<'a> {
    pub index: usize,
    pub comment: &'a Comment,
    pub unattached: bool,
}

/// An owned copy of [`OverviewComment`], for the wire ([`ProjectionSnapshot`]
/// crosses process/wasm boundaries and cannot borrow).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewCommentOwned {
    #[serde(skip)]
    pub index: usize,
    pub comment: Comment,
    pub unattached: bool,
}

/// Everything one frontend needs to paint the file list at once: filter,
/// canonical file list, filtered tree, per-file tallies, and the overview.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionSnapshot {
    pub filter: FileFilter,
    pub files: Vec<FileEntry>,
    pub tree: Vec<TreeRow>,
    pub counts: BTreeMap<String, FileCounts>,
    pub review_level_comments: usize,
    pub unattached_comments: usize,
    pub overview: Vec<OverviewCommentOwned>,
}

/// The review projected against the current changed-file list: placements
/// and per-file tallies computed once, everything else derived from them.
pub struct ReviewProjection<'a> {
    review: &'a ReviewFile,
    files: &'a [FileEntry],
    filter: FileFilter,
    placements: Vec<Placement>,
    counts: BTreeMap<String, FileCounts>,
    review_only: FileCounts,
    unattached_only: FileCounts,
}

impl<'a> ReviewProjection<'a> {
    pub fn new(review: &'a ReviewFile, files: &'a [FileEntry], filter: FileFilter) -> Self {
        let placements = place_comments(&review.comments, files);
        let mut counts: BTreeMap<String, FileCounts> = BTreeMap::new();
        let mut review_only = FileCounts::default();
        let mut unattached_only = FileCounts::default();
        for (comment, placement) in review.comments.iter().zip(&placements) {
            let is_todo = comment.status.is_todo();
            match placement {
                Placement::File { path, .. } => {
                    counts.entry(path.clone()).or_default().add(is_todo)
                }
                Placement::Review => review_only.add(is_todo),
                Placement::Unattached => unattached_only.add(is_todo),
            }
        }
        ReviewProjection {
            review,
            files,
            filter,
            placements,
            counts,
            review_only,
            unattached_only,
        }
    }

    pub fn filter(&self) -> FileFilter {
        self.filter
    }

    pub fn files(&self) -> &'a [FileEntry] {
        self.files
    }

    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    pub fn counts_for(&self, path: &str) -> FileCounts {
        self.counts.get(path).copied().unwrap_or_default()
    }

    /// The tree's "(review)" row: review-level and unattached comments
    /// bundled together, since the UI shows them as one group.
    pub fn review_level(&self) -> FileCounts {
        FileCounts::combined(self.review_only, self.unattached_only)
    }

    /// Review-level comments alone (`path: null`), without the unattached
    /// group.
    pub fn review_only(&self) -> FileCounts {
        self.review_only
    }

    /// Unattached comments alone (files gone beyond git's rename detection).
    pub fn unattached(&self) -> FileCounts {
        self.unattached_only
    }

    pub fn passes_filter(&self, file_index: usize) -> bool {
        let Some(entry) = self.files.get(file_index) else {
            return false;
        };
        match self.filter {
            FileFilter::All => true,
            FileFilter::Annotated => self.counts_for(&entry.path).total > 0,
            FileFilter::Unreviewed => self.counts_for(&entry.path).total == 0,
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        (0..self.files.len())
            .filter(|&i| self.passes_filter(i))
            .collect()
    }

    /// Canonical file indices that pass the current filter, in tree order.
    pub fn visible_files(&self) -> Vec<usize> {
        self.tree_rows(&BTreeSet::new())
            .into_iter()
            .filter_map(|row| row.file_index)
            .collect()
    }

    /// Comments placed on `path`, in comment order.
    ///
    /// `was_path` borrows from the comment's own recorded `path`, not from
    /// `self.placements`: the rename badge is always exactly the comment's
    /// original path (that is what a `Placement::File`'s `was_path` was
    /// built from in the first place), and that field is owned by the
    /// review (lifetime `'a`), never by this projection's own scratch
    /// state (which only lives as long as `&self`).
    pub fn file_comments(&self, path: &str) -> Vec<PlacedComment<'a>> {
        self.review
            .comments
            .iter()
            .zip(&self.placements)
            .enumerate()
            .filter_map(|(index, (comment, placement))| match placement {
                Placement::File { path: p, was_path } if p == path => Some(PlacedComment {
                    index,
                    comment,
                    was_path: was_path
                        .is_some()
                        .then_some(comment.path.as_deref())
                        .flatten(),
                }),
                _ => None,
            })
            .collect()
    }

    /// The filtered file tree: directories with no passing descendant are
    /// pruned entirely (not just their files hidden), and every leaf's
    /// `file_index` is canonical (its position in the full changed-file
    /// list), never renumbered within the filtered subset.
    pub fn tree_rows(&self, collapsed: &BTreeSet<String>) -> Vec<TreeRow> {
        let indices = self.visible_indices();
        let tree = build_filtered_file_tree(self.files, &indices);
        let mut rows = flatten_tree(&tree, collapsed);
        for row in &mut rows {
            if let Some(file_index) = row.file_index {
                let counts = self.counts_for(&self.files[file_index].path);
                row.comments_todo = counts.todo;
                row.comments_total = counts.total;
            }
        }
        rows
    }

    /// Review-level comments first, then unattached, each in comment order.
    pub fn overview(&self) -> Vec<OverviewComment<'a>> {
        let mut out = Vec::new();
        for (index, (comment, placement)) in self
            .review
            .comments
            .iter()
            .zip(&self.placements)
            .enumerate()
        {
            if matches!(placement, Placement::Review) {
                out.push(OverviewComment {
                    index,
                    comment,
                    unattached: false,
                });
            }
        }
        for (index, (comment, placement)) in self
            .review
            .comments
            .iter()
            .zip(&self.placements)
            .enumerate()
        {
            if matches!(placement, Placement::Unattached) {
                out.push(OverviewComment {
                    index,
                    comment,
                    unattached: true,
                });
            }
        }
        out
    }

    /// Everything a frontend needs to paint the file list in one message.
    pub fn snapshot(&self, collapsed: &BTreeSet<String>) -> ProjectionSnapshot {
        ProjectionSnapshot {
            filter: self.filter,
            files: self.files.to_vec(),
            tree: self.tree_rows(collapsed),
            counts: self.counts.clone(),
            review_level_comments: self.review_only.total,
            unattached_comments: self.unattached_only.total,
            overview: self
                .overview()
                .into_iter()
                .map(|o| OverviewCommentOwned {
                    index: o.index,
                    comment: o.comment.clone(),
                    unattached: o.unattached,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::Status;
    use crate::model::FileStatus;
    use crate::review::Source;
    use serde_json::Map;

    fn entry(path: &str, old_path: Option<&str>) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            old_path: old_path.map(str::to_string),
            status: if old_path.is_some() {
                FileStatus::Renamed
            } else {
                FileStatus::Modified
            },
            adds: None,
            dels: None,
        }
    }

    fn review_with(comments: Vec<Comment>) -> ReviewFile {
        let mut review = ReviewFile::new("fixture".into(), Source::git(None), "t");
        review.comments = comments;
        review
    }

    fn comment(id: &str, path: Option<&str>, status: Status) -> Comment {
        Comment {
            id: id.to_string(),
            rev: 1,
            status,
            path: path.map(str::to_string),
            side: None,
            line: None,
            end_line: None,
            snippet: None,
            body: "b".to_string(),
            response: None,
            author: "a".to_string(),
            created_at: "t".to_string(),
            updated_at: "t".to_string(),
            extra: Map::new(),
        }
    }

    #[test]
    fn counts_partition_by_placement_and_status() {
        let files = vec![entry("a.rs", None)];
        let review = review_with(vec![
            comment("c1", Some("a.rs"), Status::Open),
            comment("c2", Some("a.rs"), Status::Resolved),
            comment("c3", None, Status::Open),
            comment("c4", Some("gone.rs"), Status::Open),
        ]);
        let projection = ReviewProjection::new(&review, &files, FileFilter::All);
        assert_eq!(
            projection.counts_for("a.rs"),
            FileCounts { todo: 1, total: 2 }
        );
        assert_eq!(projection.unattached(), FileCounts { todo: 1, total: 1 });
        assert_eq!(
            projection.review_level(),
            FileCounts { todo: 2, total: 2 },
            "review-level combines review + unattached"
        );
    }

    #[test]
    fn filter_partitions_annotated_from_unreviewed() {
        let files = vec![entry("a.rs", None), entry("b.rs", None)];
        let review = review_with(vec![comment("c1", Some("a.rs"), Status::Open)]);

        let annotated = ReviewProjection::new(&review, &files, FileFilter::Annotated);
        assert_eq!(annotated.visible_files(), vec![0]);

        let unreviewed = ReviewProjection::new(&review, &files, FileFilter::Unreviewed);
        assert_eq!(unreviewed.visible_files(), vec![1]);

        let all = ReviewProjection::new(&review, &files, FileFilter::All);
        assert_eq!(all.visible_files(), vec![0, 1]);
    }

    #[test]
    fn filter_cycle_and_labels() {
        assert_eq!(FileFilter::All.next(), FileFilter::Annotated);
        assert_eq!(FileFilter::Annotated.next(), FileFilter::Unreviewed);
        assert_eq!(FileFilter::Unreviewed.next(), FileFilter::All);
        assert_eq!(FileFilter::All.label(), "All");
    }

    #[test]
    fn tree_rows_keep_canonical_file_index_under_a_filter() {
        let files = vec![entry("src/a.rs", None), entry("src/b.rs", None)];
        let review = review_with(vec![comment("c1", Some("src/b.rs"), Status::Open)]);
        let projection = ReviewProjection::new(&review, &files, FileFilter::Annotated);
        let rows = projection.tree_rows(&BTreeSet::new());
        let leaf = rows.iter().find(|r| !r.is_dir).expect("one leaf");
        assert_eq!(leaf.path, "src/b.rs");
        assert_eq!(leaf.file_index, Some(1));
        assert_eq!((leaf.comments_todo, leaf.comments_total), (1, 1));
    }

    #[test]
    fn file_comments_reports_rename_badge_and_comment_order() {
        let files = vec![entry("new.rs", Some("old.rs"))];
        let review = review_with(vec![
            comment("c1", Some("old.rs"), Status::Open),
            comment("c2", Some("old.rs"), Status::Resolved),
        ]);
        let projection = ReviewProjection::new(&review, &files, FileFilter::All);
        let placed = projection.file_comments("new.rs");
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[0].comment.id, "c1");
        assert_eq!(placed[0].was_path, Some("old.rs"));
        assert_eq!(placed[1].comment.id, "c2");
    }

    #[test]
    fn overview_lists_review_level_before_unattached() {
        let files = vec![entry("a.rs", None)];
        let review = review_with(vec![
            comment("c-unattached", Some("gone.rs"), Status::Open),
            comment("c-review", None, Status::Open),
        ]);
        let projection = ReviewProjection::new(&review, &files, FileFilter::All);
        let overview = projection.overview();
        assert_eq!(overview.len(), 2);
        assert_eq!(overview[0].comment.id, "c-review");
        assert!(!overview[0].unattached);
        assert_eq!(overview[1].comment.id, "c-unattached");
        assert!(overview[1].unattached);
    }

    #[test]
    fn snapshot_bundles_filter_tree_counts_and_overview() {
        let files = vec![entry("a.rs", None)];
        let review = review_with(vec![comment("c1", Some("a.rs"), Status::Open)]);
        let projection = ReviewProjection::new(&review, &files, FileFilter::All);
        let snapshot = projection.snapshot(&BTreeSet::new());
        assert_eq!(snapshot.filter, FileFilter::All);
        assert_eq!(snapshot.files, files);
        assert_eq!(snapshot.tree.len(), 1);
        assert_eq!(snapshot.counts["a.rs"], FileCounts { todo: 1, total: 1 });
        assert_eq!(snapshot.review_level_comments, 0);
        assert_eq!(snapshot.unattached_comments, 0);

        let json = serde_json::to_value(&snapshot).expect("json");
        assert_eq!(json["reviewLevelComments"], 0);
        assert!(json["tree"].as_array().is_some());
    }
}
