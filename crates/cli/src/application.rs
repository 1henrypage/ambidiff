//! The native application: the one place CLI verbs, the stdio engine, the
//! web server, and the TUI go for review state and review operations.
//!
//! It owns the store, the resolved git source, the watch controller, and
//! the last valid snapshot (review + changed-file listing + diagnostics),
//! and it executes typed review commands with author resolution, snippet
//! capture, id and time injection, and warning propagation. Terminal and
//! DOM concepts never appear here; transports map `AppError` into their
//! own envelopes through `AppError::kind`.
//!
//! Ordering invariant (the absorbed-write race): `start_watch` arms the
//! watch and computes its baselines BEFORE `load` takes the held snapshot,
//! so a write between the two is both loaded and notified. A change of the
//! review's `source` configuration reconfigures the git source and the
//! watch signature together and bumps the generation, so every cached view
//! is reloaded against the new comparison.
//!
//! Re-pinning: a git source pins its comparison when opened. Every dirty
//! load re-resolves it and re-opens the source when the live resolution
//! moved (a rebase moved the merge base of a branch review; a commit
//! review of `HEAD` moved on), so a session never keeps diffing against a
//! stale pin until restart. A re-resolution failure keeps the pin and is
//! reported as the snapshot's source error, never a silent fallback.
//!
//! Stacks: the selected target is PER PROCESS (this struct's `selected`),
//! never written to the review file. Selecting a target rebuilds the git
//! source exactly like a `source` change does; every dirty load in stack
//! mode re-opens unconditionally so a restack (tips moved, a PR merged
//! away) refreshes the target strip even when the selected diff is the
//! same. A selection whose branch left the stack falls back to the
//! whole-stack target with a warning, never to a neighbouring PR.
//!
//! Unsaved reviews: `ambidiff --commit X` / `--stack` open without a review
//! file (`open_ephemeral`); the pending review is served as if loaded and
//! the first mutation creates `.ambidiff.json` (with the git exclusions),
//! after which the store is the only truth.

use std::collections::BTreeSet;
use std::path::Path;

use ambidiff_core::anchor::TargetScope;
#[cfg(test)]
use ambidiff_core::commands::{COMMANDS, CommandSpec};
use ambidiff_core::git_source::{GitSource, RawDiff};
use ambidiff_core::model::{FileDiff, FileEntry};
use ambidiff_core::projection::{FileFilter, ReviewProjection};
use ambidiff_core::protocol::{
    CommentAddRequest, CommentDeleteRequest, CommentEditRequest, DecodeError, LifecycleRequest,
};
use ambidiff_core::review::{
    Action, Actor, Comment, LoadOutcome, ReviewError, ReviewFile, Side, Source,
    validate_comment_target, validate_new_comment,
};
use ambidiff_core::source::{
    CommitSummary, CompareSpec, Comparison, DiffSource, FileDiffRequest, ListingState, SkippedPath,
    SourceError, SourceMode,
};
use ambidiff_core::stack::{Target, TargetId};
use ambidiff_core::store::{Store, StoreError};
use ambidiff_core::sys::{generate_comment_id, now_rfc3339};
use ambidiff_core::view::ViewOptions;
use ambidiff_core::view_state::{ExpandError, ExpansionResult, ViewState};
use ambidiff_core::watch::{
    Refresh, SignatureFn, WatchConfig, WatchController, review_file_signature,
};

use crate::context::resolve_author;

/// Context lines requested from the source for every served view.
pub const CONTEXT: u32 = 3;

/// The warning an unsaved review carries until its first mutation.
pub const UNSAVED_WARNING: &str = "unsaved review: the first comment creates .ambidiff.json";

/// The last valid state every transport serves from.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub review: ReviewFile,
    pub warnings: Vec<String>,
    pub read_only: bool,
    pub read_only_reason: Option<String>,
    /// Sorted by path (the source guarantees it).
    pub files: Vec<FileEntry>,
    pub skipped: Vec<SkippedPath>,
    /// The listing could not be produced (or the source could not be
    /// opened); `files` then holds the previous listing when `listing` is
    /// stale, nothing when it is unavailable.
    pub source_error: Option<String>,
    /// Whether `files` is this load's listing, a kept previous one, or
    /// nothing at all: a failed listing is never shown as an empty one.
    pub listing: ListingState,
    pub comparison: Option<Comparison>,
    /// The stack's targets in order (empty outside stack reviews).
    pub targets: Vec<Target>,
    /// The target this process is looking at (`None` outside stacks).
    pub selected: Option<TargetId>,
    /// The trunk the stack sits on (stack reviews only).
    pub trunk: Option<String>,
    /// The reviewed commit (single-commit reviews only).
    pub commit: Option<CommitSummary>,
    /// Bumped on every diff refresh and source reconfiguration; views built
    /// at an older generation must reload.
    pub generation: u64,
}

impl Snapshot {
    pub fn entry(&self, path: &str) -> Option<&FileEntry> {
        self.files.iter().find(|e| e.path == path)
    }

    /// The target scope every projection of this snapshot uses.
    pub fn scope(&self) -> TargetScope {
        TargetScope {
            selected: self.selected.clone(),
            live: self.targets.iter().map(|t| t.id.clone()).collect(),
        }
    }

    pub fn projection(&self, filter: FileFilter) -> ReviewProjection<'_> {
        ReviewProjection::scoped(&self.review, &self.files, filter, self.scope())
    }

    pub fn is_stack(&self) -> bool {
        self.review.source.is_stack()
    }

    pub fn target(&self, id: &TargetId) -> Option<&Target> {
        self.targets.iter().find(|t| &t.id == id)
    }
}

/// Coalesced watch hints since the last poll.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pending {
    pub review: bool,
    pub diff: bool,
}

/// A typed review operation. Every transport decodes into one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewCommand {
    Add(CommentAddRequest),
    Edit(CommentEditRequest),
    Delete(CommentDeleteRequest),
    Lifecycle {
        action: Action,
        actor: Actor,
        req: LifecycleRequest,
    },
    ResolveAddressed {
        actor: Actor,
    },
    RevBump,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutcomeValue {
    /// Boxed: a comment is an order of magnitude larger than the other
    /// variants.
    Comment(Box<Comment>),
    Deleted(String),
    ResolvedAddressed(Vec<String>),
    Revision(u32),
}

/// A command's value plus the warnings raised while executing it (salvage
/// warnings from the locked load, snippet-capture problems).
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub value: OutcomeValue,
    pub warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Store(StoreError),
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Review(#[from] ReviewError),
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error(transparent)]
    Expand(#[from] ExpandError),
    #[error("{path:?} is not in the changed set")]
    NotInChangedSet { path: String },
    #[error("no gap {gap_id:?} in {path:?}")]
    NoSuchGap { path: String, gap_id: String },
    #[error("no usable diff source: {reason}")]
    NoSource { reason: String },
    #[error("review not loaded")]
    NotLoaded,
    #[error("this review is not a stack review; there is no target to select")]
    NotStackReview,
    #[error("target {target} is not in the stack")]
    UnknownTarget { target: String },
}

impl From<StoreError> for AppError {
    /// A domain rejection raised inside `Store::mutate` is the caller's
    /// fault (invalid input, unknown id, illegal transition), not a storage
    /// failure: unwrap it so the kind and the transport code say so.
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::Review(e) => AppError::Review(e),
            other => AppError::Store(other),
        }
    }
}

impl AppError {
    /// Stable lowerCamel kind for wire error envelopes.
    pub fn kind(&self) -> &'static str {
        match self {
            AppError::Store(_) => "store",
            AppError::Source(e) if e.is_invalid_path() => "invalidPath",
            AppError::Source(_) => "source",
            AppError::Review(_) => "review",
            AppError::Decode(_) => "decode",
            AppError::Expand(ExpandError::NoSuchGap { .. }) | AppError::NoSuchGap { .. } => {
                "noSuchGap"
            }
            AppError::Expand(_) => "expand",
            AppError::NotInChangedSet { .. } => "notInChangedSet",
            AppError::NoSource { .. } => "noSource",
            AppError::NotLoaded => "notLoaded",
            AppError::NotStackReview => "notStackReview",
            AppError::UnknownTarget { .. } => "unknownTarget",
        }
    }

    /// True for the caller's fault (bad payload, unknown id, illegal
    /// transition, unsafe path, unknown file, gap, or target): transports
    /// answer with their invalid-input code instead of an internal error.
    pub fn is_invalid_input(&self) -> bool {
        matches!(
            self.kind(),
            "decode"
                | "review"
                | "invalidPath"
                | "notInChangedSet"
                | "noSuchGap"
                | "notStackReview"
                | "unknownTarget"
        )
    }
}

/// The listing state after a failed listing attempt: a previous listing
/// (fresh or already stale) is kept and marked stale; nothing to keep means
/// unavailable.
fn listing_after_failure(previous: Option<ListingState>) -> ListingState {
    match previous {
        Some(ListingState::Fresh | ListingState::Stale) => ListingState::Stale,
        Some(ListingState::Unavailable) | None => ListingState::Unavailable,
    }
}

/// Add the review file and its sidecars to `.git/info/exclude` when `root`
/// is a git working tree. A failure is reported as a warning, never hidden:
/// the review exists, and the user can fix the exclusion before committing.
pub fn exclude_review_from_git(root: &Path) -> Option<String> {
    if !GitSource::is_repo(root) {
        return None;
    }
    GitSource::ensure_git_exclude(root)
        .err()
        .map(|err| format!("could not add the review sidecars to .git/info/exclude: {err}"))
}

/// What the git source is currently built for: the review's source kind
/// and the resolved comparison spec (including the per-process target).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Configured {
    kind: String,
    spec: CompareSpec,
}

pub struct Application {
    store: Store,
    source: Option<GitSource>,
    configured: Option<Configured>,
    source_error: Option<String>,
    /// Warnings from the last source (re)open, e.g. a target fallback.
    source_warnings: Vec<String>,
    /// The stack target this process looks at; never persisted.
    selected: Option<TargetId>,
    /// An unsaved review, served until the first mutation creates the file.
    pending: Option<ReviewFile>,
    watch: Option<WatchController>,
    generation: u64,
    diff_dirty: bool,
    snapshot: Option<Snapshot>,
}

impl Application {
    /// No IO happens here; call `start_watch` (optional) and `load`.
    pub fn open(store: Store) -> Application {
        Application {
            store,
            source: None,
            configured: None,
            source_error: None,
            source_warnings: Vec::new(),
            selected: None,
            pending: None,
            watch: None,
            generation: 0,
            diff_dirty: true,
            snapshot: None,
        }
    }

    /// Open on a root that has no review file yet: `review` is served as
    /// the loaded review (with [`UNSAVED_WARNING`]) and the first mutation
    /// creates `.ambidiff.json` from it.
    pub fn open_ephemeral(store: Store, review: ReviewFile) -> Application {
        let mut app = Application::open(store);
        app.pending = Some(review);
        app
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// True while the review has not been written to disk yet.
    pub fn is_unsaved(&self) -> bool {
        self.pending.is_some() && !self.store.exists()
    }

    pub fn last_diff_error(&self) -> Option<String> {
        self.watch.as_ref().and_then(|w| w.last_diff_error())
    }

    fn review_signature_fn(&self) -> SignatureFn {
        let path = self.store.review_path();
        std::sync::Arc::new(move || review_file_signature(&path))
    }

    fn diff_signature_fn(&self) -> SignatureFn {
        match (&self.source, &self.source_error) {
            (Some(source), _) => {
                let source = source.clone();
                std::sync::Arc::new(move || source.try_signature().map_err(|e| e.to_string()))
            }
            (None, error) => {
                let reason = error
                    .clone()
                    .unwrap_or_else(|| "no diff source configured".to_string());
                std::sync::Arc::new(move || Err(reason.clone()))
            }
        }
    }

    fn swap_watch_signature(&self) {
        if let Some(watch) = &self.watch {
            watch.set_diff_signature(self.diff_signature_fn());
        }
    }

    /// The review as the store has it, or the pending (unsaved) review
    /// when the file does not exist yet.
    fn load_outcome(&self) -> Result<LoadOutcome, AppError> {
        match self.store.load() {
            Ok(outcome) => Ok(outcome),
            Err(StoreError::NotInitialized { .. }) if self.pending.is_some() => {
                let review = self.pending.clone().expect("checked");
                Ok(LoadOutcome {
                    review,
                    warnings: vec![UNSAVED_WARNING.to_string()],
                    read_only: false,
                    read_only_reason: None,
                    retained: Default::default(),
                })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Arm the watch: an unheld pre-read of the review selects the source,
    /// then the controller baselines both signatures synchronously. Must run
    /// BEFORE `load` so a write in between is detected, never absorbed.
    pub fn start_watch(&mut self) -> Result<(), AppError> {
        if self.watch.is_some() {
            return Ok(());
        }
        let outcome = self.load_outcome()?;
        self.reconfigure(&outcome.review.source);
        let config = WatchConfig::new(self.store.root().to_path_buf(), self.store.review_path());
        let controller = WatchController::start_checked(
            config,
            self.diff_signature_fn(),
            self.review_signature_fn(),
        );
        self.watch = Some(controller);
        Ok(())
    }

    /// Rebuild the git source when the review's source configuration (or
    /// this process's target selection) changed, swap the watch's diff
    /// signature synchronously, and mark the diff dirty. Returns whether a
    /// rebuild happened. Errors become `source_error`; there is never a
    /// silent fallback.
    fn reconfigure(&mut self, source: &Source) -> bool {
        let key = Configured {
            kind: source.kind.clone(),
            spec: CompareSpec::from_source(source, self.selected.clone()),
        };
        if self.configured.as_ref() == Some(&key) {
            return false;
        }
        self.configured = Some(key);
        self.reopen_source();
        self.diff_dirty = true;
        true
    }

    /// (Re)open the git source for the current configuration. In stack
    /// mode a selected target that left the stack falls back to the
    /// whole-stack target (never a neighbouring PR) with a warning; the
    /// resolved selection is adopted as this process's selection.
    fn reopen_source(&mut self) {
        let Some(configured) = self.configured.clone() else {
            return;
        };
        let root = self.store.root().to_path_buf();
        self.source_warnings.clear();
        let (git, error) = if configured.kind != "git" {
            (
                None,
                Some(format!("unsupported source kind {:?}", configured.kind)),
            )
        } else if !GitSource::is_repo(&root) {
            (None, Some("not a git repository".to_string()))
        } else {
            match GitSource::open_with(&root, configured.spec.clone()) {
                Ok(git) => (Some(git), None),
                Err(SourceError::TargetNotInStack { target }) => {
                    self.open_fallback_target(&root, &configured.spec, &target)
                }
                Err(e) => (None, Some(e.to_string())),
            }
        };
        if let Some(git) = &git
            && let Some(selected) = git.selected_target()
        {
            self.selected = Some(selected.clone());
            if let Some(configured) = &mut self.configured {
                configured.spec.target = Some(selected.clone());
            }
        }
        self.source = git;
        self.source_error = error;
        self.swap_watch_signature();
    }

    fn open_fallback_target(
        &mut self,
        root: &Path,
        spec: &CompareSpec,
        gone: &str,
    ) -> (Option<GitSource>, Option<String>) {
        let upstream = match &spec.mode {
            SourceMode::Stack { upstream } => upstream.clone(),
            _ => None,
        };
        let fallback = match GitSource::discover_stack(root, upstream.as_deref()) {
            Ok(stack) => match stack.fallback_target() {
                Some(target) => target,
                None => {
                    return (
                        None,
                        Some(SourceError::EmptyStack { trunk: stack.trunk }.to_string()),
                    );
                }
            },
            Err(e) => return (None, Some(e.to_string())),
        };
        match GitSource::open_with(root, CompareSpec::stack(upstream, Some(fallback.clone()))) {
            Ok(git) => {
                self.source_warnings
                    .push(format!("target {gone} left the stack; showing {fallback}"));
                (Some(git), None)
            }
            Err(e) => (None, Some(e.to_string())),
        }
    }

    fn is_stack_configured(&self) -> bool {
        self.configured
            .as_ref()
            .is_some_and(|c| c.kind == "git" && c.spec.is_stack())
    }

    /// Re-resolve the pinned comparison and re-open the source when the
    /// live resolution differs (see the module docs). Returns the error to
    /// surface when re-resolution fails; the pin is kept in that case.
    fn repin_source(&mut self) -> Option<String> {
        let source = self.source.as_ref()?;
        let moved = source
            .resolve_current()
            .map(|live| live != *source.comparison());
        match moved {
            Ok(true) => {
                self.reopen_source();
                None
            }
            Ok(false) => None,
            Err(e) => Some(format!("comparison could not be re-resolved: {e}")),
        }
    }

    /// Take the held snapshot: the review (errors keep the previous
    /// snapshot), the source reconfigured when its configuration changed,
    /// and the listing re-taken when the diff is dirty (a listing failure
    /// keeps the previous files and records the error). A dirty load also
    /// re-resolves the comparison: stack mode re-opens unconditionally so
    /// the targets follow a restack; every other mode re-opens only when
    /// the live resolution moved.
    pub fn load(&mut self) -> Result<&Snapshot, AppError> {
        let outcome = self.load_outcome()?;
        let reopened = self.reconfigure(&outcome.review.source);
        let repin_error = if self.diff_dirty && !reopened {
            if self.is_stack_configured() {
                self.reopen_source();
                None
            } else {
                self.repin_source()
            }
        } else {
            None
        };

        let previous = self.snapshot.take();
        let (files, skipped, source_error, listing, generation) = match previous {
            Some(p) if !self.diff_dirty => {
                (p.files, p.skipped, p.source_error, p.listing, p.generation)
            }
            previous => {
                let (files, skipped, error, listing) = match &self.source {
                    Some(source) => match source.listing() {
                        Ok(listing) => {
                            (listing.entries, listing.skipped, None, ListingState::Fresh)
                        }
                        Err(e) => {
                            let listing =
                                listing_after_failure(previous.as_ref().map(|p| p.listing));
                            let (files, skipped) = match (listing, previous) {
                                (ListingState::Stale, Some(p)) => (p.files, p.skipped),
                                _ => (Vec::new(), Vec::new()),
                            };
                            (files, skipped, Some(e.to_string()), listing)
                        }
                    },
                    // No source: the comparison itself is unusable, so a
                    // previous listing (taken against another comparison)
                    // is not worth keeping.
                    None => (
                        Vec::new(),
                        Vec::new(),
                        self.source_error.clone(),
                        ListingState::Unavailable,
                    ),
                };
                self.generation += 1;
                self.diff_dirty = false;
                (files, skipped, error, listing, self.generation)
            }
        };
        let comparison = self.source.as_ref().map(|s| s.comparison().clone());
        let (targets, selected, trunk) = match self.source.as_ref().and_then(|s| s.stack()) {
            Some(stack) => (
                stack.targets.clone(),
                self.source
                    .as_ref()
                    .and_then(|s| s.selected_target().cloned()),
                Some(stack.trunk.clone()),
            ),
            None => (Vec::new(), None, None),
        };
        let commit = self
            .source
            .as_ref()
            .and_then(|s| s.commit_summary().cloned());
        // A listing can succeed while the re-pin or the watch's signature
        // computation fails (a deleted base ref, a git deadline under
        // load): surface that too, so the frontends never show a silent
        // stale state.
        let source_error = source_error
            .or(repin_error)
            .or_else(|| self.last_diff_error());
        let mut warnings = outcome.warnings;
        if let Some(conflict) = outcome.review.source.conflict_warning() {
            warnings.push(conflict);
        }
        warnings.extend(self.source_warnings.iter().cloned());
        self.snapshot = Some(Snapshot {
            review: outcome.review,
            warnings,
            read_only: outcome.read_only,
            read_only_reason: outcome.read_only_reason,
            files,
            skipped,
            source_error,
            listing,
            comparison,
            targets,
            selected,
            trunk,
            commit,
            generation,
        });
        Ok(self.snapshot.as_ref().expect("just set"))
    }

    /// The review alone, salvage-mode, zero git: for `status`, `list`,
    /// `show`, and review-only broadcasts.
    pub fn load_review(&self) -> Result<LoadOutcome, AppError> {
        self.load_outcome()
    }

    /// Force a diff re-listing on the next load (manual refresh).
    pub fn refresh(&mut self) -> Result<&Snapshot, AppError> {
        self.diff_dirty = true;
        self.load()
    }

    /// Select the stack target this process looks at. Requires a stack
    /// review and a target the stack currently has; rebuilds the source,
    /// swaps the watch signature, and re-lists (the generation bumps, so
    /// every cached view reloads). Same mechanics as a `source` change.
    pub fn select_target(&mut self, id: TargetId) -> Result<&Snapshot, AppError> {
        let snapshot = self.loaded()?;
        if !snapshot.is_stack() {
            return Err(AppError::NotStackReview);
        }
        if snapshot.target(&id).is_none() {
            return Err(AppError::UnknownTarget {
                target: id.label().to_string(),
            });
        }
        self.selected = Some(id);
        self.diff_dirty = true;
        self.load()
    }

    /// Drain and coalesce the watch's hints; a diff hint marks the diff
    /// dirty so the next `load` re-lists.
    pub fn poll(&mut self) -> Pending {
        let mut pending = Pending::default();
        if let Some(watch) = &self.watch {
            while let Some(refresh) = watch.try_recv() {
                match refresh {
                    Refresh::Review => pending.review = true,
                    Refresh::Diff => pending.diff = true,
                }
            }
        }
        if pending.diff {
            self.diff_dirty = true;
        }
        pending
    }

    fn loaded(&self) -> Result<&Snapshot, AppError> {
        self.snapshot.as_ref().ok_or(AppError::NotLoaded)
    }

    fn source(&self) -> Result<&GitSource, AppError> {
        self.source.as_ref().ok_or_else(|| AppError::NoSource {
            reason: self
                .source_error
                .clone()
                .unwrap_or_else(|| "not configured".to_string()),
        })
    }

    fn entry(&self, path: &str) -> Result<FileEntry, AppError> {
        self.loaded()?
            .entry(path)
            .cloned()
            .ok_or_else(|| AppError::NotInChangedSet {
                path: path.to_string(),
            })
    }

    /// One listed file's parsed diff (a placeholder diff for binary and
    /// oversized files), membership-checked.
    pub fn file_diff(&self, path: &str) -> Result<(FileEntry, FileDiff), AppError> {
        let entry = self.entry(path)?;
        let diff = self
            .source()?
            .file_diff(&FileDiffRequest::for_entry(&entry, CONTEXT))?;
        Ok((entry, diff))
    }

    /// A fresh view state for a listed file at the current generation.
    pub fn file(&self, path: &str, opts: ViewOptions) -> Result<ViewState, AppError> {
        let (entry, diff) = self.file_diff(path)?;
        Ok(ViewState::new(entry, diff, opts, self.generation))
    }

    /// The raw diff bytes the browser parses itself.
    pub fn raw_file(&self, path: &str) -> Result<(FileEntry, RawDiff), AppError> {
        let entry = self.entry(path)?;
        let raw = self
            .source()?
            .file_diff_raw(&FileDiffRequest::for_entry(&entry, CONTEXT))?;
        Ok((entry, raw))
    }

    /// Full content of a listed file on one side; the old side of a rename
    /// reads its origin path.
    pub fn content(&self, side: Side, path: &str) -> Result<Option<String>, AppError> {
        let entry = self.entry(path)?;
        let side_path = match side {
            Side::Old => entry.old_path.as_deref().unwrap_or(path),
            Side::New => path,
        };
        Ok(self.source()?.read_side(side, side_path)?)
    }

    /// Expand one collapsed gap with the new-side content at the current
    /// generation; the view must be from this generation.
    pub fn expand(&self, view: &mut ViewState, gap_id: &str) -> Result<ExpansionResult, AppError> {
        if view.gap(gap_id).is_none() {
            return Err(AppError::NoSuchGap {
                path: view.path().to_string(),
                gap_id: gap_id.to_string(),
            });
        }
        let content = self.new_side_content(view.path())?;
        Ok(view.expand(gap_id, &content, self.generation)?)
    }

    /// Re-apply remembered expansions to a freshly built view with one
    /// content read.
    pub fn restore_expansions(
        &self,
        view: &mut ViewState,
        wanted: &BTreeSet<String>,
    ) -> Result<(), AppError> {
        for gap_id in wanted {
            view.mark_wanted(gap_id);
        }
        let pending = view.pending_expansions();
        if pending.is_empty() {
            return Ok(());
        }
        let content = self.new_side_content(view.path())?;
        for gap in pending {
            view.expand(&gap.id, &content, self.generation)?;
        }
        Ok(())
    }

    fn new_side_content(&self, path: &str) -> Result<String, AppError> {
        self.content(Side::New, path)?
            .ok_or_else(|| AppError::NoSource {
                reason: format!("cannot read the new side of {path:?}"),
            })
    }

    /// Make sure the git source matches the review's current configuration
    /// (verbs that never `load` still capture snippets against it), and
    /// hand back that configuration.
    fn ensure_source(&mut self) -> Result<Source, AppError> {
        if let Some(snapshot) = &self.snapshot
            && self.configured.is_some()
        {
            return Ok(snapshot.review.source.clone());
        }
        let outcome = self.load_outcome()?;
        self.reconfigure(&outcome.review.source);
        Ok(outcome.review.source)
    }

    /// The git source to capture a snippet against for `target`: the open
    /// one when the target matches (or there is none), otherwise a second
    /// source opened for that target, since agents comment on any PR of
    /// the stack without a live view of it. A target that is not in the
    /// stack yields `None` (the snippet is skipped with a warning, never a
    /// rejection).
    fn source_for(
        &self,
        target: Option<&TargetId>,
        warnings: &mut Vec<String>,
    ) -> Option<GitSource> {
        let Some(source) = &self.source else {
            warnings.push(format!(
                "snippet not captured: {}",
                self.source_error
                    .as_deref()
                    .unwrap_or("no diff source configured")
            ));
            return None;
        };
        let Some(target) = target else {
            return Some(source.clone());
        };
        if !source.spec().is_stack() || source.selected_target() == Some(target) {
            return Some(source.clone());
        }
        let upstream = match &source.spec().mode {
            SourceMode::Stack { upstream } => upstream.clone(),
            _ => None,
        };
        let spec = CompareSpec::stack(upstream, Some(target.clone()));
        match GitSource::open_with(self.store.root(), spec) {
            Ok(other) => Some(other),
            Err(e) => {
                warnings.push(format!("snippet not captured: {e}"));
                None
            }
        }
    }

    /// The captured line for a new line comment: the origin path for the
    /// old side of a rename, only the starting line (the documented snippet
    /// semantics), read against the COMMENT's target. An unsafe path
    /// rejects the comment; any other read problem becomes a warning and
    /// the comment lands without a snippet.
    fn capture_snippet(
        &self,
        path: &str,
        side: Side,
        line: u32,
        target: Option<&TargetId>,
        warnings: &mut Vec<String>,
    ) -> Result<Option<String>, AppError> {
        let Some(source) = self.source_for(target, warnings) else {
            return Ok(None);
        };
        let side_path = match side {
            Side::Old => self
                .snapshot
                .as_ref()
                .and_then(|s| s.entry(path))
                .and_then(|e| e.old_path.clone())
                .unwrap_or_else(|| path.to_string()),
            Side::New => path.to_string(),
        };
        match source.read_side_lines(side, &side_path, line, line) {
            Ok(Some(lines)) => Ok(lines.into_iter().next()),
            Ok(None) => Ok(None),
            Err(e) if e.is_invalid_path() => Err(e.into()),
            Err(e) => {
                warnings.push(format!("snippet not captured: {e}"));
                Ok(None)
            }
        }
    }

    /// Locked read-modify-write through the store, or, for an unsaved
    /// review, creation of the file from the pending review with `f`
    /// applied. The closure may run twice only in the narrow race where
    /// another process creates the file first (then the ordinary locked
    /// path runs it against that file).
    fn mutate<T>(
        &mut self,
        mut f: impl FnMut(&mut ReviewFile) -> Result<T, ReviewError>,
    ) -> Result<(T, Vec<String>), AppError> {
        if let Some(pending) = &self.pending {
            if !self.store.exists() {
                let mut review = pending.clone();
                let value = f(&mut review)?;
                review.validate()?;
                match self.store.init(&review) {
                    Ok(()) => {
                        self.pending = None;
                        let warnings = exclude_review_from_git(self.store.root())
                            .into_iter()
                            .collect();
                        return Ok((value, warnings));
                    }
                    Err(StoreError::AlreadyInitialized { .. }) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            self.pending = None;
        }
        Ok(self.store.mutate(f)?)
    }

    /// Execute a review command under the store lock. Author resolution
    /// and validation happen outside the lock; snippet capture, id and
    /// time injection, and the domain operation inside it.
    pub fn execute(&mut self, cmd: ReviewCommand) -> Result<Outcome, AppError> {
        let source = self.ensure_source()?;
        let mut warnings = Vec::new();
        let (review, value, load_warnings) = match cmd {
            ReviewCommand::Add(req) => {
                let author = resolve_author(req.author.clone());
                let mut new = req.into_new_comment(author);
                validate_new_comment(&new)?;
                validate_comment_target(&new, &source)?;
                new.snippet = match (&new.path, new.line, new.side) {
                    (Some(path), Some(line), Some(side)) => {
                        self.capture_snippet(path, side, line, new.target.as_ref(), &mut warnings)?
                    }
                    _ => None,
                };
                let ((review, comment), load_warnings) = self.mutate(|review| {
                    let ids: Vec<&str> = review.comments.iter().map(|c| c.id.as_str()).collect();
                    let id = generate_comment_id(&ids);
                    let now = now_rfc3339();
                    let comment = review.try_add_comment(new.clone(), id, &now)?.clone();
                    Ok((review.clone(), comment))
                })?;
                (
                    review,
                    OutcomeValue::Comment(Box::new(comment)),
                    load_warnings,
                )
            }
            ReviewCommand::Edit(req) => {
                let ((review, comment), load_warnings) = self.mutate(|review| {
                    let now = now_rfc3339();
                    let comment = review.edit_comment(&req.id, &req.body, &now)?.clone();
                    Ok((review.clone(), comment))
                })?;
                (
                    review,
                    OutcomeValue::Comment(Box::new(comment)),
                    load_warnings,
                )
            }
            ReviewCommand::Delete(req) => {
                let ((review, deleted), load_warnings) = self.mutate(|review| {
                    let now = now_rfc3339();
                    let deleted = review.delete_comment(&req.id, &now)?;
                    Ok((review.clone(), deleted.id))
                })?;
                (review, OutcomeValue::Deleted(deleted), load_warnings)
            }
            ReviewCommand::Lifecycle { action, actor, req } => {
                let ((review, comment), load_warnings) = self.mutate(|review| {
                    let now = now_rfc3339();
                    let comment = review
                        .apply_lifecycle(&req.id, action, actor, req.response.clone(), &now)?
                        .clone();
                    Ok((review.clone(), comment))
                })?;
                (
                    review,
                    OutcomeValue::Comment(Box::new(comment)),
                    load_warnings,
                )
            }
            ReviewCommand::ResolveAddressed { actor } => {
                let ((review, ids), load_warnings) = self.mutate(|review| {
                    let now = now_rfc3339();
                    let ids = review.resolve_addressed(actor, &now)?;
                    Ok((review.clone(), ids))
                })?;
                (review, OutcomeValue::ResolvedAddressed(ids), load_warnings)
            }
            ReviewCommand::RevBump => {
                let ((review, revision), load_warnings) = self.mutate(|review| {
                    let now = now_rfc3339();
                    let revision = review.try_rev_bump(&now)?;
                    Ok((review.clone(), revision))
                })?;
                (review, OutcomeValue::Revision(revision), load_warnings)
            }
        };
        if let Some(snapshot) = &mut self.snapshot {
            snapshot.review = review;
        }
        warnings.extend(load_warnings);
        Ok(Outcome { value, warnings })
    }

    /// Stop watching; joins the watch thread.
    pub fn stop(self) {
        if let Some(watch) = self.watch {
            watch.stop();
        }
    }
}

// ------------------------------------------------------ command inventory

/// Where a command is fulfilled.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Entirely inside the frontend (navigation, view toggles, search).
    Client,
    /// Needs the engine: the stdio method and browser message type it maps
    /// to (a frontend without a chord for it does not need the mapping).
    Server,
}

/// One row of the capability inventory: how each advertised command is
/// fulfilled per transport. Tested against the command table and against
/// the transports' dispatch tables so an advertised command cannot lack a
/// handler again (findings S5 and B17).
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub struct Capability {
    pub id: &'static str,
    pub reach: Reach,
    pub stdio: Option<&'static str>,
    pub web: Option<&'static str>,
}

#[cfg(test)]
macro_rules! cap {
    ($id:literal, client) => {
        Capability {
            id: $id,
            reach: Reach::Client,
            stdio: None,
            web: None,
        }
    };
    ($id:literal, server, stdio: $stdio:literal, web: $web:literal) => {
        Capability {
            id: $id,
            reach: Reach::Server,
            stdio: Some($stdio),
            web: Some($web),
        }
    };
}

#[cfg(test)]
pub const CAPABILITIES: &[Capability] = &[
    // Fulfilled by the frontend alone (navigation, view toggles, search).
    cap!("ambidiff.nav.cursorDown", client),
    cap!("ambidiff.nav.cursorUp", client),
    cap!("ambidiff.nav.pageDown", client),
    cap!("ambidiff.nav.pageUp", client),
    cap!("ambidiff.nav.top", client),
    cap!("ambidiff.nav.bottom", client),
    cap!("ambidiff.nav.nextHunk", client),
    cap!("ambidiff.nav.prevHunk", client),
    cap!("ambidiff.nav.nextFile", client),
    cap!("ambidiff.nav.prevFile", client),
    cap!("ambidiff.nav.nextComment", client),
    cap!("ambidiff.nav.prevComment", client),
    cap!("ambidiff.nav.focusSwitch", client),
    cap!("ambidiff.nav.gotoLine", client),
    cap!("ambidiff.view.toggleLayout", client),
    cap!("ambidiff.view.toggleWordDiff", client),
    cap!("ambidiff.view.toggleTree", client),
    cap!("ambidiff.view.toggleWrap", client),
    cap!("ambidiff.view.toggleLineNumbers", client),
    cap!("ambidiff.view.toggleTheme", client),
    cap!("ambidiff.view.cycleFilter", client),
    cap!("ambidiff.view.expand", server, stdio: "expand", web: "getSrc"),
    cap!("ambidiff.view.refresh", server, stdio: "files", web: "refresh"),
    cap!("ambidiff.review.comment", server, stdio: "comment.add", web: "comment.add"),
    cap!("ambidiff.review.commentFile", server, stdio: "comment.add", web: "comment.add"),
    cap!("ambidiff.review.commentReview", server, stdio: "comment.add", web: "comment.add"),
    cap!("ambidiff.review.address", server, stdio: "comment.address", web: "comment.address"),
    cap!("ambidiff.review.resolve", server, stdio: "comment.resolve", web: "comment.resolve"),
    cap!("ambidiff.review.reopen", server, stdio: "comment.reopen", web: "comment.reopen"),
    cap!(
        "ambidiff.review.resolveAddressed",
        server,
        stdio: "comment.resolveAddressed",
        web: "comment.resolveAddressed"
    ),
    cap!("ambidiff.review.editComment", server, stdio: "comment.edit", web: "comment.edit"),
    cap!("ambidiff.review.deleteComment", server, stdio: "comment.delete", web: "comment.delete"),
    cap!("ambidiff.target.next", server, stdio: "target.select", web: "target.select"),
    cap!("ambidiff.target.prev", server, stdio: "target.select", web: "target.select"),
    cap!("ambidiff.target.pick", server, stdio: "target.select", web: "target.select"),
    cap!("ambidiff.search.start", client),
    cap!("ambidiff.search.next", client),
    cap!("ambidiff.search.prev", client),
    cap!("ambidiff.app.help", client),
    cap!("ambidiff.app.quit", client),
];

/// The command-table entry a capability row describes.
#[cfg(test)]
pub fn spec_of(capability: &Capability) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|c| c.id == capability.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use ambidiff_core::source::Endpoint;

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_stdout(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(root: &Path, path: &str, content: &str) {
        std::fs::write(root.join(path), content).expect("write");
    }

    /// `B - U (main)` and `B - F (feature, checked out)`, with a review
    /// file selecting `source`.
    fn feature_checkout(source: Source) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        write(&root, "shared.txt", "shared\n");
        write(&root, "feature.txt", "feature v1\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "B"]);
        git(&root, &["checkout", "-qb", "feature"]);
        write(&root, "feature.txt", "feature v2\n");
        git(&root, &["commit", "-qam", "F"]);
        git(&root, &["checkout", "-q", "main"]);
        write(&root, "upstream.txt", "u\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "U"]);
        git(&root, &["checkout", "-q", "feature"]);
        Store::new(&root)
            .init(&ReviewFile::new("t".into(), source, &now_rfc3339()))
            .expect("init");
        assert_eq!(
            exclude_review_from_git(&root),
            None,
            "exclude the review file"
        );
        (dir, root)
    }

    /// The real startup order: arm the watch, then take the first load.
    fn open_loaded(root: &Path) -> Application {
        let mut app = Application::open(Store::new(root));
        app.start_watch().expect("watch");
        app.load().expect("load");
        app
    }

    fn paths(snapshot: &Snapshot) -> Vec<&str> {
        snapshot.files.iter().map(|e| e.path.as_str()).collect()
    }

    fn old_oid(snapshot: &Snapshot) -> String {
        match &snapshot.comparison {
            Some(Comparison {
                old: Endpoint::Commit { oid },
                ..
            }) => oid.clone(),
            other => panic!("expected a commit on the old side, got {other:?}"),
        }
    }

    #[test]
    fn listing_after_failure_keeps_a_previous_listing_as_stale_and_nothing_as_unavailable() {
        assert_eq!(
            listing_after_failure(Some(ListingState::Fresh)),
            ListingState::Stale
        );
        assert_eq!(
            listing_after_failure(Some(ListingState::Stale)),
            ListingState::Stale
        );
        assert_eq!(
            listing_after_failure(Some(ListingState::Unavailable)),
            ListingState::Unavailable
        );
        assert_eq!(listing_after_failure(None), ListingState::Unavailable);
    }

    #[test]
    fn a_first_load_failure_is_unavailable_and_a_later_one_keeps_files_as_stale() {
        let (_dir, root) = feature_checkout(Source::git(Some("no-such-ref".into())));
        let app = open_loaded(&root);
        let snapshot = app.snapshot().expect("loaded");
        assert_eq!(snapshot.listing, ListingState::Unavailable);
        assert!(snapshot.files.is_empty());
        assert!(
            snapshot
                .source_error
                .as_deref()
                .is_some_and(|e| e.contains("no-such-ref")),
            "{:?}",
            snapshot.source_error
        );
        app.stop();

        let (_dir, root) = feature_checkout(Source::git(Some("main".into())));
        let mut app = open_loaded(&root);
        assert_eq!(app.snapshot().expect("loaded").listing, ListingState::Fresh);
        let git_dir = root.join(".git");
        let parked = root.join(".git-parked");
        std::fs::rename(&git_dir, &parked).expect("park .git");
        let snapshot = app.refresh().expect("refresh");
        assert_eq!(snapshot.listing, ListingState::Stale);
        assert_eq!(
            paths(snapshot),
            vec!["feature.txt"],
            "the previous files are kept"
        );
        assert!(snapshot.source_error.is_some());
        std::fs::rename(&parked, &git_dir).expect("restore .git");
        let snapshot = app.refresh().expect("refresh");
        assert_eq!(snapshot.listing, ListingState::Fresh);
        assert_eq!(snapshot.source_error, None);
        app.stop();
    }

    #[test]
    fn dirty_load_repins_a_moved_merge_base() {
        let (_dir, root) = feature_checkout(Source::git(Some("main".into())));
        let mut app = open_loaded(&root);
        let snapshot = app.snapshot().expect("loaded");
        assert_eq!(
            old_oid(snapshot),
            git_stdout(&root, &["rev-parse", "main~1"])
        );
        assert_eq!(paths(snapshot), vec!["feature.txt"]);
        let generation = snapshot.generation;

        git(&root, &["rebase", "-q", "main"]);
        let snapshot = app.refresh().expect("refresh");
        assert_eq!(
            old_oid(snapshot),
            git_stdout(&root, &["rev-parse", "main"]),
            "the pin follows the merge base to main's tip"
        );
        assert_eq!(
            paths(snapshot),
            vec!["feature.txt"],
            "upstream.txt never appears as a deletion"
        );
        assert!(snapshot.generation > generation, "views must reload");
        assert_eq!(snapshot.source_error, None);
        app.stop();
    }

    #[test]
    fn dirty_load_keeps_the_pinned_source_when_re_resolution_fails() {
        let (_dir, root) = feature_checkout(Source::git(Some("main".into())));
        let mut app = open_loaded(&root);
        let before = app.snapshot().expect("loaded").clone();

        git(&root, &["branch", "-D", "main"]);
        let snapshot = app.refresh().expect("refresh");
        let error = snapshot
            .source_error
            .clone()
            .expect("the failure is surfaced");
        assert!(
            error.contains("could not be re-resolved") && error.contains("main"),
            "{error}"
        );
        assert_eq!(snapshot.comparison, before.comparison, "the pin is kept");
        assert_eq!(paths(snapshot), paths(&before), "the listing still works");
        app.stop();
    }

    #[test]
    fn commit_mode_follows_head_on_a_dirty_load() {
        let (_dir, root) = feature_checkout(Source::git_commit("HEAD"));
        let mut app = open_loaded(&root);
        let first = app
            .snapshot()
            .expect("loaded")
            .commit
            .clone()
            .expect("commit");
        assert_eq!(first.oid, git_stdout(&root, &["rev-parse", "HEAD"]));
        assert_eq!(first.subject, "F");

        write(&root, "feature.txt", "feature v3\n");
        git(&root, &["commit", "-qam", "F2"]);
        let snapshot = app.refresh().expect("refresh");
        let commit = snapshot.commit.clone().expect("commit");
        assert_eq!(commit.oid, git_stdout(&root, &["rev-parse", "HEAD"]));
        assert_eq!(commit.subject, "F2");
        assert_eq!(snapshot.source_error, None);
        app.stop();
    }

    #[test]
    fn capability_table_is_exhaustive() {
        let advertised: BTreeSet<&str> = COMMANDS.iter().map(|c| c.id).collect();
        let covered: BTreeSet<&str> = CAPABILITIES.iter().map(|c| c.id).collect();
        assert_eq!(covered, advertised, "one capability row per command id");
        assert_eq!(CAPABILITIES.len(), COMMANDS.len(), "no duplicate rows");
    }

    #[test]
    fn server_commands_with_nvim_chords_have_stdio_methods() {
        for cap in CAPABILITIES.iter().filter(|c| c.reach == Reach::Server) {
            let spec = spec_of(cap).expect("known id");
            assert!(cap.web.is_some(), "{}: browser message type", cap.id);
            if !spec.nvim.is_empty() {
                assert!(cap.stdio.is_some(), "{}: stdio method", cap.id);
            }
        }
    }

    #[test]
    fn every_stdio_capability_is_dispatched() {
        for cap in CAPABILITIES {
            if let Some(method) = cap.stdio {
                assert!(
                    crate::engine::METHODS.contains(&method),
                    "{}: engine does not dispatch {method:?}",
                    cap.id
                );
            }
        }
    }

    #[test]
    fn every_web_capability_is_a_message_type() {
        for cap in CAPABILITIES {
            if let Some(kind) = cap.web {
                assert!(
                    crate::web::MESSAGE_TYPES.contains(&kind),
                    "{}: web server does not handle {kind:?}",
                    cap.id
                );
            }
        }
    }

    #[test]
    fn tui_dispatches_every_command_with_a_tui_chord() {
        let dispatch = include_str!("tui/mod.rs");
        for spec in COMMANDS.iter().filter(|c| !c.tui.is_empty()) {
            assert!(
                dispatch.contains(&format!("\"{}\"", spec.id)),
                "{}: no TUI dispatch arm",
                spec.id
            );
        }
    }

    #[test]
    fn error_kinds_partition_invalid_input_from_failures() {
        let invalid = AppError::NotInChangedSet { path: "x".into() };
        assert!(invalid.is_invalid_input());
        assert_eq!(invalid.kind(), "notInChangedSet");
        let decode = AppError::Decode(DecodeError::Missing {
            field: "body".into(),
        });
        assert!(decode.is_invalid_input());
        let store = AppError::Store(StoreError::NotInitialized { path: "p".into() });
        assert!(!store.is_invalid_input());
        assert_eq!(store.kind(), "store");
        assert_eq!(AppError::NotLoaded.kind(), "notLoaded");
        assert!(!AppError::NotLoaded.is_invalid_input());
        assert_eq!(AppError::NotStackReview.kind(), "notStackReview");
        assert!(AppError::NotStackReview.is_invalid_input());
        let unknown = AppError::UnknownTarget {
            target: "auth-9".into(),
        };
        assert_eq!(unknown.kind(), "unknownTarget");
        assert!(unknown.is_invalid_input());
    }
}
