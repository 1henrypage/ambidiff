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
//!
//! In a stack review the projection is scoped to the selected target
//! ([`ReviewProjection::scoped`]): comments made on another live target
//! count nowhere here, comments whose target left the stack join the
//! unattached group with a "was on" badge, and per-target tallies for the
//! strip come from the same pass.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::anchor::{Placement, TargetScope, place_comments_scoped};
use crate::model::FileEntry;
use crate::review::{Comment, ReviewFile};
use crate::stack::TargetId;
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
    /// The label of the target this comment was made on when that target
    /// has left the stack (the "was on" badge).
    pub was_on: Option<&'a str>,
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
    pub was_on: Option<String>,
}

/// Tally of the comments made on one live target (its own comments only,
/// wherever they place), for the target strip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetCounts {
    pub id: TargetId,
    pub counts: FileCounts,
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
    pub selected: Option<TargetId>,
    pub target_counts: Vec<TargetCounts>,
    pub untargeted_comments: usize,
    pub was_on_comments: usize,
}

/// The review projected against the current changed-file list: placements
/// and per-file tallies computed once, everything else derived from them.
pub struct ReviewProjection<'a> {
    review: &'a ReviewFile,
    files: &'a [FileEntry],
    filter: FileFilter,
    scope: TargetScope,
    placements: Vec<Placement>,
    counts: BTreeMap<String, FileCounts>,
    review_only: FileCounts,
    unattached_only: FileCounts,
    was_on: FileCounts,
    untargeted: FileCounts,
    target_counts: Vec<TargetCounts>,
}

impl<'a> ReviewProjection<'a> {
    /// The unscoped projection: every comment placed by path (non-stack
    /// reviews, and callers that have no selection).
    pub fn new(review: &'a ReviewFile, files: &'a [FileEntry], filter: FileFilter) -> Self {
        ReviewProjection::scoped(review, files, filter, TargetScope::default())
    }

    /// The projection as seen from `scope.selected`: placements follow
    /// [`place_comments_scoped`], `OtherTarget` comments count nowhere,
    /// `WasOn` comments count as unattached (and in `was_on`), and every
    /// live target gets its own tally.
    pub fn scoped(
        review: &'a ReviewFile,
        files: &'a [FileEntry],
        filter: FileFilter,
        scope: TargetScope,
    ) -> Self {
        let placements = place_comments_scoped(&review.comments, files, &scope);
        let mut counts: BTreeMap<String, FileCounts> = BTreeMap::new();
        let mut review_only = FileCounts::default();
        let mut unattached_only = FileCounts::default();
        let mut was_on = FileCounts::default();
        let mut untargeted = FileCounts::default();
        let mut target_counts: Vec<TargetCounts> = scope
            .live
            .iter()
            .map(|id| TargetCounts {
                id: id.clone(),
                counts: FileCounts::default(),
            })
            .collect();
        for (comment, placement) in review.comments.iter().zip(&placements) {
            let is_todo = comment.status.is_todo();
            match &comment.target {
                None => untargeted.add(is_todo),
                Some(target) => {
                    if let Some(tally) = target_counts.iter_mut().find(|t| &t.id == target) {
                        tally.counts.add(is_todo);
                    }
                }
            }
            match placement {
                Placement::File { path, .. } => {
                    counts.entry(path.clone()).or_default().add(is_todo)
                }
                Placement::Review => review_only.add(is_todo),
                Placement::Unattached => unattached_only.add(is_todo),
                Placement::WasOn { .. } => {
                    unattached_only.add(is_todo);
                    was_on.add(is_todo);
                }
                Placement::OtherTarget => {}
            }
        }
        ReviewProjection {
            review,
            files,
            filter,
            scope,
            placements,
            counts,
            review_only,
            unattached_only,
            was_on,
            untargeted,
            target_counts,
        }
    }

    pub fn filter(&self) -> FileFilter {
        self.filter
    }

    /// The target this projection is scoped to (`None` outside stacks).
    pub fn selected(&self) -> Option<&TargetId> {
        self.scope.selected.as_ref()
    }

    /// Comments whose target has left the stack (a subset of `unattached`).
    pub fn was_on(&self) -> FileCounts {
        self.was_on
    }

    /// Comments without a target (legacy, or a non-stack review): they show
    /// on every target.
    pub fn untargeted(&self) -> FileCounts {
        self.untargeted
    }

    /// One tally per live target, in stack order, counting the comments
    /// made on that target wherever they place.
    pub fn target_counts(&self) -> &[TargetCounts] {
        &self.target_counts
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

    /// Review-level comments first, then the unattached group (files gone
    /// from the diff, and comments whose target left the stack), each in
    /// comment order.
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
                    was_on: None,
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
            match placement {
                Placement::Unattached => out.push(OverviewComment {
                    index,
                    comment,
                    unattached: true,
                    was_on: None,
                }),
                Placement::WasOn { .. } => out.push(OverviewComment {
                    index,
                    comment,
                    unattached: true,
                    // The badge text is the comment's own recorded target,
                    // which the review owns (lifetime `'a`).
                    was_on: comment.target.as_ref().map(TargetId::label),
                }),
                _ => {}
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
                    was_on: o.was_on.map(str::to_string),
                })
                .collect(),
            selected: self.scope.selected.clone(),
            target_counts: self.target_counts.clone(),
            untargeted_comments: self.untargeted.total,
            was_on_comments: self.was_on.total,
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
            target: None,
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

    fn tagged(id: &str, path: Option<&str>, status: Status, target: Option<TargetId>) -> Comment {
        let mut c = comment(id, path, status);
        c.target = target;
        c
    }

    fn branch(name: &str) -> TargetId {
        TargetId::Branch { name: name.into() }
    }

    fn scope() -> TargetScope {
        TargetScope {
            selected: Some(branch("auth-2")),
            live: vec![branch("auth-1"), branch("auth-2"), TargetId::Stack],
        }
    }

    fn stack_review() -> ReviewFile {
        review_with(vec![
            tagged("c-legacy", Some("a.rs"), Status::Open, None),
            tagged(
                "c-selected",
                Some("a.rs"),
                Status::Open,
                Some(branch("auth-2")),
            ),
            tagged(
                "c-other",
                Some("a.rs"),
                Status::Open,
                Some(branch("auth-1")),
            ),
            tagged(
                "c-gone",
                Some("a.rs"),
                Status::Reopened,
                Some(branch("auth-0")),
            ),
            tagged("c-review", None, Status::Addressed, Some(branch("auth-2"))),
            tagged(
                "c-review-gone",
                None,
                Status::Open,
                Some(TargetId::Worktree),
            ),
            tagged(
                "c-unattached",
                Some("gone.rs"),
                Status::Resolved,
                Some(branch("auth-2")),
            ),
        ])
    }

    #[test]
    fn scoped_counts_hide_other_targets_and_bucket_was_on_as_unattached() {
        let files = vec![entry("a.rs", None)];
        let review = stack_review();
        let projection = ReviewProjection::scoped(&review, &files, FileFilter::All, scope());
        assert_eq!(
            projection.placements().len(),
            review.comments.len(),
            "parallel"
        );
        assert_eq!(
            projection.counts_for("a.rs"),
            FileCounts { todo: 2, total: 2 },
            "legacy + selected; other and gone do not count on the file"
        );
        assert_eq!(projection.review_only(), FileCounts { todo: 0, total: 1 });
        assert_eq!(
            projection.unattached(),
            FileCounts { todo: 2, total: 3 },
            "gone.rs + two was-on comments"
        );
        assert_eq!(projection.was_on(), FileCounts { todo: 2, total: 2 });
        assert_eq!(projection.untargeted(), FileCounts { todo: 1, total: 1 });
        assert_eq!(projection.selected(), Some(&branch("auth-2")));
        assert_eq!(
            projection.target_counts(),
            &[
                TargetCounts {
                    id: branch("auth-1"),
                    counts: FileCounts { todo: 1, total: 1 }
                },
                TargetCounts {
                    id: branch("auth-2"),
                    counts: FileCounts { todo: 1, total: 3 }
                },
                TargetCounts {
                    id: TargetId::Stack,
                    counts: FileCounts::default()
                },
            ],
            "own comments only, in live order"
        );
        let ids: Vec<&str> = projection
            .file_comments("a.rs")
            .iter()
            .map(|p| p.comment.id.as_str())
            .collect();
        assert_eq!(ids, vec!["c-legacy", "c-selected"]);
    }

    #[test]
    fn scoped_overview_lists_review_then_unattached_with_was_on_badges() {
        let files = vec![entry("a.rs", None)];
        let review = stack_review();
        let projection = ReviewProjection::scoped(&review, &files, FileFilter::All, scope());
        let overview: Vec<(&str, bool, Option<&str>)> = projection
            .overview()
            .iter()
            .map(|o| (o.comment.id.as_str(), o.unattached, o.was_on))
            .collect();
        assert_eq!(
            overview,
            vec![
                ("c-review", false, None),
                ("c-gone", true, Some("auth-0")),
                ("c-review-gone", true, Some("worktree")),
                ("c-unattached", true, None),
            ]
        );
        let snapshot = projection.snapshot(&BTreeSet::new());
        let json = serde_json::to_value(&snapshot).expect("json");
        assert_eq!(
            json["selected"],
            serde_json::json!({"kind": "branch", "name": "auth-2"})
        );
        assert_eq!(json["targetCounts"][1]["counts"]["total"], 3);
        assert_eq!(json["untargetedComments"], 1);
        assert_eq!(json["wasOnComments"], 2);
        assert_eq!(json["overview"][1]["wasOn"], "auth-0");
        assert_eq!(json["overview"][3]["wasOn"], serde_json::Value::Null);
    }

    #[test]
    fn unscoped_projection_equals_the_empty_scope_and_shows_every_comment() {
        let files = vec![entry("a.rs", None)];
        let review = stack_review();
        let plain = ReviewProjection::new(&review, &files, FileFilter::All);
        let empty =
            ReviewProjection::scoped(&review, &files, FileFilter::All, TargetScope::default());
        assert_eq!(plain.placements(), empty.placements());
        assert_eq!(plain.counts_for("a.rs"), FileCounts { todo: 4, total: 4 });
        assert_eq!(plain.was_on(), FileCounts::default());
        assert!(plain.target_counts().is_empty());
        assert_eq!(plain.selected(), None);
        let snapshot = plain.snapshot(&BTreeSet::new());
        assert_eq!(snapshot.selected, None);
        assert_eq!(snapshot.was_on_comments, 0);
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
