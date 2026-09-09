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

use std::collections::BTreeSet;

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
    validate_new_comment,
};
use ambidiff_core::source::{Comparison, DiffSource, FileDiffRequest, SkippedPath, SourceError};
use ambidiff_core::store::{Store, StoreError};
use ambidiff_core::sys::{generate_comment_id, now_rfc3339};
use ambidiff_core::view::ViewOptions;
use ambidiff_core::view_state::{ExpandError, ExpansionResult, ViewState};
use ambidiff_core::watch::{Refresh, SignatureFn, WatchConfig, WatchController};

use crate::context::resolve_author;

/// Context lines requested from the source for every served view.
pub const CONTEXT: u32 = 3;

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
    /// opened); `files` then holds the previous listing.
    pub source_error: Option<String>,
    pub comparison: Option<Comparison>,
    /// Bumped on every diff refresh and source reconfiguration; views built
    /// at an older generation must reload.
    pub generation: u64,
}

impl Snapshot {
    pub fn entry(&self, path: &str) -> Option<&FileEntry> {
        self.files.iter().find(|e| e.path == path)
    }

    pub fn projection(&self, filter: FileFilter) -> ReviewProjection<'_> {
        ReviewProjection::new(&self.review, &self.files, filter)
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
    RevBump,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutcomeValue {
    /// Boxed: a comment is an order of magnitude larger than the other
    /// variants.
    Comment(Box<Comment>),
    Deleted(String),
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
        }
    }

    /// True for the caller's fault (bad payload, unknown id, illegal
    /// transition, unsafe path, unknown file or gap): transports answer with
    /// their invalid-input code instead of an internal error.
    pub fn is_invalid_input(&self) -> bool {
        matches!(
            self.kind(),
            "decode" | "review" | "invalidPath" | "notInChangedSet" | "noSuchGap"
        )
    }
}

/// The parts of `source` that select a git comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceKey {
    kind: String,
    base: Option<String>,
    staged: bool,
}

impl SourceKey {
    fn of(source: &Source) -> SourceKey {
        SourceKey {
            kind: source.kind.clone(),
            base: source.base.clone().filter(|b| !b.is_empty()),
            staged: source
                .extra
                .get("staged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }
    }
}

pub struct Application {
    store: Store,
    source: Option<GitSource>,
    source_key: Option<SourceKey>,
    source_error: Option<String>,
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
            source_key: None,
            source_error: None,
            watch: None,
            generation: 0,
            diff_dirty: true,
            snapshot: None,
        }
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

    pub fn last_diff_error(&self) -> Option<String> {
        self.watch.as_ref().and_then(|w| w.last_diff_error())
    }

    fn review_signature_fn(&self) -> SignatureFn {
        let path = self.store.review_path();
        std::sync::Arc::new(move || {
            std::fs::read(&path)
                .map(|bytes| ambidiff_core::util::fnv1a64(&bytes))
                .map_err(|e| format!("read {}: {e}", path.display()))
        })
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

    /// Arm the watch: an unheld pre-read of the review selects the source,
    /// then the controller baselines both signatures synchronously. Must run
    /// BEFORE `load` so a write in between is detected, never absorbed.
    pub fn start_watch(&mut self) -> Result<(), AppError> {
        if self.watch.is_some() {
            return Ok(());
        }
        let outcome = self.store.load()?;
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

    /// Rebuild the git source for a (changed) source configuration, swap the
    /// watch's diff signature synchronously, and mark the diff dirty. Errors
    /// become `source_error`; there is never a silent fallback.
    fn reconfigure(&mut self, source: &Source) {
        let key = SourceKey::of(source);
        if self.source_key.as_ref() == Some(&key) {
            return;
        }
        let root = self.store.root().to_path_buf();
        let (git, error) = if key.kind != "git" {
            (
                None,
                Some(format!("unsupported source kind {:?}", key.kind)),
            )
        } else if !GitSource::is_repo(&root) {
            (None, Some("not a git repository".to_string()))
        } else {
            match GitSource::open(&root, key.base.clone(), key.staged) {
                Ok(git) => (Some(git), None),
                Err(e) => (None, Some(e.to_string())),
            }
        };
        self.source = git;
        self.source_error = error;
        self.source_key = Some(key);
        self.diff_dirty = true;
        if let Some(watch) = &self.watch {
            watch.set_diff_signature(self.diff_signature_fn());
        }
    }

    /// Take the held snapshot: the review (errors keep the previous
    /// snapshot), the source reconfigured when its configuration changed,
    /// and the listing re-taken when the diff is dirty (a listing failure
    /// keeps the previous files and records the error).
    pub fn load(&mut self) -> Result<&Snapshot, AppError> {
        let outcome = self.store.load()?;
        self.reconfigure(&outcome.review.source);

        let previous = self.snapshot.take();
        let (files, skipped, source_error, generation) = match previous {
            Some(p) if !self.diff_dirty => (p.files, p.skipped, p.source_error, p.generation),
            previous => {
                let (files, skipped, error) = match &self.source {
                    Some(source) => match source.listing() {
                        Ok(listing) => (listing.entries, listing.skipped, None),
                        Err(e) => {
                            let (files, skipped) =
                                previous.map(|p| (p.files, p.skipped)).unwrap_or_default();
                            (files, skipped, Some(e.to_string()))
                        }
                    },
                    None => (Vec::new(), Vec::new(), self.source_error.clone()),
                };
                self.generation += 1;
                self.diff_dirty = false;
                (files, skipped, error, self.generation)
            }
        };
        let comparison = self.source.as_ref().map(|s| s.comparison().clone());
        // A listing can succeed while the watch's signature computation
        // fails (a git deadline under load): surface that too, so the
        // frontends never show a silent stale state.
        let source_error = source_error.or_else(|| self.last_diff_error());
        self.snapshot = Some(Snapshot {
            review: outcome.review,
            warnings: outcome.warnings,
            read_only: outcome.read_only,
            read_only_reason: outcome.read_only_reason,
            files,
            skipped,
            source_error,
            comparison,
            generation,
        });
        Ok(self.snapshot.as_ref().expect("just set"))
    }

    /// The review alone, salvage-mode, zero git: for `status`, `list`,
    /// `show`, and review-only broadcasts.
    pub fn load_review(&self) -> Result<LoadOutcome, AppError> {
        Ok(self.store.load()?)
    }

    /// Force a diff re-listing on the next load (manual refresh).
    pub fn refresh(&mut self) -> Result<&Snapshot, AppError> {
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
    /// (verbs that never `load` still capture snippets against it).
    fn ensure_source(&mut self) -> Result<(), AppError> {
        if self.source_key.is_none() {
            let outcome = self.store.load()?;
            self.reconfigure(&outcome.review.source);
        }
        Ok(())
    }

    /// The captured line for a new line comment: the origin path for the
    /// old side of a rename, only the starting line (the documented snippet
    /// semantics). An unsafe path rejects the comment; any other read
    /// problem becomes a warning and the comment lands without a snippet.
    fn capture_snippet(
        &self,
        path: &str,
        side: Side,
        line: u32,
        warnings: &mut Vec<String>,
    ) -> Result<Option<String>, AppError> {
        let Some(source) = &self.source else {
            warnings.push(format!(
                "snippet not captured: {}",
                self.source_error
                    .as_deref()
                    .unwrap_or("no diff source configured")
            ));
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

    /// Execute a review command under the store lock. Author resolution
    /// and validation happen outside the lock; snippet capture, id and
    /// time injection, and the domain operation inside it.
    pub fn execute(&mut self, cmd: ReviewCommand) -> Result<Outcome, AppError> {
        self.ensure_source()?;
        let mut warnings = Vec::new();
        let (review, value, load_warnings) = match cmd {
            ReviewCommand::Add(req) => {
                let author = resolve_author(req.author.clone());
                let new = req.into_new_comment(author);
                validate_new_comment(&new)?;
                let snippet = match (&new.path, new.line, new.side) {
                    (Some(path), Some(line), Some(side)) => {
                        self.capture_snippet(path, side, line, &mut warnings)?
                    }
                    _ => None,
                };
                let ((review, comment), load_warnings) = self.store.mutate(|review| {
                    let mut new = new;
                    new.snippet = snippet;
                    let ids: Vec<&str> = review.comments.iter().map(|c| c.id.as_str()).collect();
                    let id = generate_comment_id(&ids);
                    let now = now_rfc3339();
                    let comment = review.try_add_comment(new, id, &now)?.clone();
                    Ok((review.clone(), comment))
                })?;
                (
                    review,
                    OutcomeValue::Comment(Box::new(comment)),
                    load_warnings,
                )
            }
            ReviewCommand::Edit(req) => {
                let ((review, comment), load_warnings) = self.store.mutate(|review| {
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
                let ((review, deleted), load_warnings) = self.store.mutate(|review| {
                    let now = now_rfc3339();
                    let deleted = review.delete_comment(&req.id, &now)?;
                    Ok((review.clone(), deleted.id))
                })?;
                (review, OutcomeValue::Deleted(deleted), load_warnings)
            }
            ReviewCommand::Lifecycle { action, actor, req } => {
                let ((review, comment), load_warnings) = self.store.mutate(|review| {
                    let now = now_rfc3339();
                    let comment = review
                        .apply_lifecycle(&req.id, action, actor, req.response, &now)?
                        .clone();
                    Ok((review.clone(), comment))
                })?;
                (
                    review,
                    OutcomeValue::Comment(Box::new(comment)),
                    load_warnings,
                )
            }
            ReviewCommand::RevBump => {
                let ((review, revision), load_warnings) = self.store.mutate(|review| {
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
    cap!("ambidiff.review.editComment", server, stdio: "comment.edit", web: "comment.edit"),
    cap!("ambidiff.review.deleteComment", server, stdio: "comment.delete", web: "comment.delete"),
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
    }
}
