//! TUI application state and update logic. Rendering lives in `render`;
//! everything here is derivation glue over `crate::application::Application`
//! plus UI state. Diff semantics themselves live entirely in
//! `ambidiff-core`: this file only tracks cursor/scroll/overlay state and
//! asks the shared application for review state and operations.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use ambidiff_core::anchor::{ActiveCell, RowAnchor, anchor_target};
use ambidiff_core::highlight::ThemeChoice;
use ambidiff_core::model::FileDiffKind;
use ambidiff_core::projection::{FileCounts, FileFilter, ReviewProjection};
use ambidiff_core::protocol::{CommentAddRequest, CommentEditRequest, LifecycleRequest};
use ambidiff_core::review::{Action, Actor, Comment, Side};
use ambidiff_core::rows::{Row, ViewMode};
use ambidiff_core::search::{SearchMatch, search_rows};
use ambidiff_core::store::Store;
use ambidiff_core::tree::TreeRow;
use ambidiff_core::view::ViewOptions;
use ambidiff_core::view_state::ViewState;

use crate::application::{Application, ReviewCommand, Snapshot};

use super::editor::Editor;
use super::render;
use super::theme::{self, Theme};

/// How long a transient status message stays on the status bar.
pub const STATUS_TTL: Duration = Duration::from_secs(4);

/// What the diff pane is currently showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTarget {
    /// The review overview: review-level and unattached comments.
    Overview,
    /// A listed file, by path (stable across a refresh even when the
    /// file's index in the changed set moves).
    File(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Diff,
}

/// Which side triggered a refresh, so it can decide whether the view needs
/// reloading (a diff refresh always does; a review refresh does only if the
/// generation moved, e.g. a source-changing edit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshKind {
    Review,
    Diff,
}

/// Where `resolve_target` landed a previous file selection (B11): a stale
/// selection never silently becomes another file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetResolution {
    /// The overview was showing; stays the overview.
    Overview,
    /// The same path is still listed.
    Same,
    /// The path is gone, but a listed file's rename origin matches it.
    Followed { to: String },
    /// The path is gone and nothing renamed from it.
    Vanished { from: String },
}

/// Resolve a previous file target against a freshly listed changed-file set.
fn resolve_target(
    prev: &FileTarget,
    files: &[ambidiff_core::model::FileEntry],
) -> TargetResolution {
    match prev {
        FileTarget::Overview => TargetResolution::Overview,
        FileTarget::File(path) => {
            if files.iter().any(|f| &f.path == path) {
                TargetResolution::Same
            } else if let Some(entry) = files
                .iter()
                .find(|f| f.old_path.as_deref() == Some(path.as_str()))
            {
                TargetResolution::Followed {
                    to: entry.path.clone(),
                }
            } else {
                TargetResolution::Vanished { from: path.clone() }
            }
        }
    }
}

/// One 1-line display row: what the diff pane actually paints.
#[derive(Debug, Clone, PartialEq)]
pub enum DRow {
    /// File title banner.
    Banner,
    /// Secondary notice line (binary placeholder, too-large, warnings).
    Notice(String),
    /// A view row; `seg` selects a byte range of the cell text for wrap
    /// continuation lines (None = whole line / first chunk).
    View {
        idx: usize,
        seg: Option<(usize, usize)>,
        continuation: bool,
    },
    /// Comment card header for `App::pane[c]`.
    CommentHead {
        comment: usize,
    },
    /// One body/response line of a comment card.
    CommentLine {
        comment: usize,
        text: String,
    },
    /// Closing border of a comment card.
    CommentFoot {
        comment: usize,
    },
    /// Section header for the unattached group (overview only).
    SectionHead(String),
    Blank,
}

/// A pending editor overlay and what saving it will do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorIntent {
    NewComment {
        path: Option<String>,
        side: Option<Side>,
        line: Option<u32>,
    },
    EditBody {
        id: String,
    },
    AddressResponse {
        id: String,
    },
}

pub struct EditorState {
    pub editor: Editor,
    pub intent: EditorIntent,
    pub title: String,
    /// Set when the last save attempt failed; the draft survives so the
    /// author can fix it and retry (B19). Cleared on the next keystroke.
    pub last_error: Option<String>,
}

pub enum Overlay {
    None,
    Help,
    Editor(Box<EditorState>),
    ConfirmDelete { id: String },
}

pub struct SearchState {
    pub query: String,
    pub typing: bool,
    pub matches: Vec<SearchMatch>,
    pub current: usize,
}

/// One comment shown in the current pane: an owned slice of what
/// `ViewState::file_projection` / `ReviewProjection::overview` computed,
/// since both borrow from a `Snapshot` this struct cannot also borrow from
/// while `App` holds it mutably across frames.
struct PaneComment {
    /// Index into `snapshot.review.comments`.
    index: usize,
    anchor: RowAnchor,
    was_path: Option<String>,
}

/// Everything the file list needs to paint, derived once per snapshot/
/// filter/collapse change from `ReviewProjection` (S1, B15): a filtered
/// tree, unattached/review-level overview list, and per-file tallies that
/// stay available even for a file the current filter hides (the banner of
/// an already-open file must still show its own counts).
#[derive(Default)]
struct Projected {
    tree_rows: Vec<TreeRow>,
    /// (comment index, unattached) pairs, review-level first.
    overview: Vec<(usize, bool)>,
    review_counts: FileCounts,
    file_counts: std::collections::BTreeMap<String, FileCounts>,
}

pub struct App {
    app: Application,
    snapshot: Snapshot,

    filter: FileFilter,
    collapsed: BTreeSet<String>,
    projected: Projected,
    tree_cursor: usize,

    target: FileTarget,
    view: Option<ViewState>,
    pane: Vec<PaneComment>,
    display: Vec<DRow>,
    cursor: usize,
    scroll: usize,
    active_cell: ActiveCell,

    mode: ViewMode,
    word_diff: bool,
    wrap: bool,
    line_numbers: bool,
    show_tree: bool,
    theme: Theme,
    focus: Focus,

    search: Option<SearchState>,
    overlay: Overlay,
    status_msg: Option<(String, Instant)>,
    /// The last layout `render::draw` computed; mouse hit-testing reads
    /// this instead of recomputing geometry (B25).
    last_layout: Option<render::Layout>,
    quit: bool,
}

/// Cursor position preserved across a refresh (B11/B12): the row's own key
/// when it still exists, else the semantic (side, line) it anchored to, else
/// a raw clamp. Captured before a reload, replayed after.
struct CursorMemento {
    target: FileTarget,
    row_key: Option<String>,
    semantic: Option<(Side, u32)>,
    cell: ActiveCell,
    viewport_offset: usize,
    cursor: usize,
}

impl App {
    pub fn open(store: Store, split: bool, light: bool) -> anyhow::Result<App> {
        let mut application = Application::open(store);
        // Arm the watch BEFORE the first load: a write landing between the
        // load and a later watch start would be absorbed into the baseline
        // and never converge on screen. A watch failure is non-fatal: it
        // only means live updates stop working, not that the review is
        // unusable.
        let mut startup_flash = None;
        if let Err(err) = application.start_watch() {
            startup_flash = Some(format!("watch not started: {err}"));
        }
        let snapshot = application.load()?.clone();

        let mut app = App {
            app: application,
            snapshot,
            filter: FileFilter::All,
            collapsed: BTreeSet::new(),
            projected: Projected::default(),
            tree_cursor: 0,
            target: FileTarget::Overview,
            view: None,
            pane: Vec::new(),
            display: Vec::new(),
            cursor: 0,
            scroll: 0,
            active_cell: ActiveCell::Auto,
            mode: if split {
                ViewMode::Split
            } else {
                ViewMode::Unified
            },
            word_diff: true,
            wrap: false,
            line_numbers: true,
            show_tree: true,
            theme: if light { theme::light() } else { theme::dark() },
            focus: Focus::Diff,
            search: None,
            overlay: Overlay::None,
            status_msg: None,
            last_layout: None,
            quit: false,
        };
        app.reproject();
        let first = app
            .projected
            .tree_rows
            .iter()
            .find_map(|r| r.file_index)
            .map(|idx| app.snapshot.files[idx].path.clone());
        match first {
            Some(path) => app.open_target(FileTarget::File(path)),
            None => app.open_target(FileTarget::Overview),
        }
        if let Some(msg) = startup_flash {
            app.flash(&msg);
        }
        Ok(app)
    }

    // ------------------------------------------------------ accessors

    pub fn review(&self) -> &ambidiff_core::review::ReviewFile {
        &self.snapshot.review
    }

    pub fn read_only(&self) -> bool {
        self.snapshot.read_only
    }

    pub fn files(&self) -> &[ambidiff_core::model::FileEntry] {
        &self.snapshot.files
    }

    pub fn tree_rows(&self) -> &[TreeRow] {
        &self.projected.tree_rows
    }

    pub fn review_level_counts(&self) -> FileCounts {
        self.projected.review_counts
    }

    pub fn file_comment_counts(&self, path: &str) -> (usize, usize) {
        let counts = self
            .projected
            .file_counts
            .get(path)
            .copied()
            .unwrap_or_default();
        (counts.todo, counts.total)
    }

    pub fn target(&self) -> &FileTarget {
        &self.target
    }

    pub fn view(&self) -> Option<&ViewState> {
        self.view.as_ref()
    }

    pub fn display(&self) -> &[DRow] {
        &self.display
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn tree_cursor(&self) -> usize {
        self.tree_cursor
    }

    pub fn filter(&self) -> FileFilter {
        self.filter
    }

    pub fn mode(&self) -> ViewMode {
        self.mode
    }

    pub fn word_diff(&self) -> bool {
        self.word_diff
    }

    pub fn wrap(&self) -> bool {
        self.wrap
    }

    pub fn line_numbers(&self) -> bool {
        self.line_numbers
    }

    pub fn show_tree(&self) -> bool {
        self.show_tree
    }

    pub fn theme(&self) -> Theme {
        self.theme
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
    }

    pub fn search(&self) -> Option<&SearchState> {
        self.search.as_ref()
    }

    pub fn overlay(&self) -> &Overlay {
        &self.overlay
    }

    pub fn overlay_mut(&mut self) -> &mut Overlay {
        &mut self.overlay
    }

    pub fn open_help(&mut self) {
        self.overlay = Overlay::Help;
    }

    pub fn open_confirm_delete(&mut self, id: String) {
        self.overlay = Overlay::ConfirmDelete { id };
    }

    pub fn quit(&self) -> bool {
        self.quit
    }

    pub fn set_quit(&mut self) {
        self.quit = true;
    }

    /// Store this frame's computed layout; when the wrap budget
    /// (`content_width`) changed and wrap is on, rebuilds the wrapped rows
    /// so they reflow to the new width (B25).
    pub fn apply_layout(&mut self, layout: render::Layout) {
        let changed =
            self.last_layout.as_ref().map(|l| l.content_width) != Some(layout.content_width);
        self.last_layout = Some(layout);
        if changed && self.wrap {
            self.rebuild_display_preserving_cursor();
        }
    }

    pub fn set_scroll(&mut self, scroll: usize) {
        self.scroll = scroll;
    }

    pub fn set_active_cell(&mut self, cell: ActiveCell) {
        self.active_cell = cell;
    }

    pub fn pane_comment(&self, pane_idx: usize) -> Option<(&Comment, &RowAnchor, Option<&str>)> {
        let pane = self.pane.get(pane_idx)?;
        let comment = self.snapshot.review.comments.get(pane.index)?;
        Some((comment, &pane.anchor, pane.was_path.as_deref()))
    }

    // ---------------------------------------------------------- projection

    /// Rebuild the filtered tree, overview, and per-file tallies from the
    /// current snapshot + filter + collapsed set (S1, B15).
    fn reproject(&mut self) {
        let projection =
            ReviewProjection::new(&self.snapshot.review, &self.snapshot.files, self.filter);
        let tree_rows = projection.tree_rows(&self.collapsed);
        let overview = projection
            .overview()
            .into_iter()
            .map(|o| (o.index, o.unattached))
            .collect();
        let review_counts = projection.review_level();
        let file_counts = self
            .snapshot
            .files
            .iter()
            .map(|f| (f.path.clone(), projection.counts_for(&f.path)))
            .collect();
        self.projected = Projected {
            tree_rows,
            overview,
            review_counts,
            file_counts,
        };
        if self.tree_cursor >= self.tree_len() {
            self.tree_cursor = self.tree_len().saturating_sub(1);
        }
    }

    /// Tree pane rows: one overview row plus the filtered file tree.
    pub fn tree_len(&self) -> usize {
        self.projected.tree_rows.len() + 1
    }

    fn visible_file_order(&self) -> Vec<usize> {
        self.projected
            .tree_rows
            .iter()
            .filter_map(|r| r.file_index)
            .collect()
    }

    // -------------------------------------------------------------- watch

    /// Drain watch refreshes; returns true when something changed.
    pub fn poll_watch(&mut self) -> bool {
        let pending = self.app.poll();
        if pending.diff {
            self.refresh(RefreshKind::Diff);
        } else if pending.review {
            self.refresh(RefreshKind::Review);
        }
        pending.diff || pending.review
    }

    pub fn flash(&mut self, msg: &str) {
        self.status_msg = Some((msg.to_string(), Instant::now()));
    }

    pub fn status_msg(&self) -> Option<&(String, Instant)> {
        self.status_msg.as_ref()
    }

    /// Clear a status message once it has aged out, reporting whether the
    /// status bar therefore needs one more repaint.
    pub fn take_expired_status(&mut self) -> bool {
        if self
            .status_msg
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= STATUS_TTL)
        {
            self.status_msg = None;
            return true;
        }
        false
    }

    // ---------------------------------------------------------- refresh

    fn memento(&self) -> CursorMemento {
        let row_key = match self.display.get(self.cursor) {
            Some(DRow::View { idx, .. }) => self
                .view
                .as_ref()
                .and_then(|v| v.view().rows.get(*idx))
                .map(|r| r.key().to_string()),
            Some(
                DRow::CommentHead { comment }
                | DRow::CommentLine { comment, .. }
                | DRow::CommentFoot { comment },
            ) => self
                .pane_comment(*comment)
                .map(|(c, ..)| format!("comment:{}", c.id)),
            _ => None,
        };
        let semantic = match self.display.get(self.cursor) {
            Some(DRow::View { idx, .. }) => self
                .view
                .as_ref()
                .and_then(|v| v.anchor_target(*idx, self.active_cell)),
            _ => None,
        };
        CursorMemento {
            target: self.target.clone(),
            row_key,
            semantic,
            cell: self.active_cell,
            viewport_offset: self.cursor.saturating_sub(self.scroll),
            cursor: self.cursor,
        }
    }

    /// Reload the review (and, when dirty, the diff listing), reconcile the
    /// current file target against the new listing (B11), reload the view
    /// if needed (B12), and restore the cursor from a memento.
    fn refresh(&mut self, kind: RefreshKind) {
        let memento = self.memento();
        match self.app.load() {
            Ok(snapshot) => self.snapshot = snapshot.clone(),
            Err(err) => {
                self.flash(&format!("reload failed: {err}"));
                return;
            }
        }
        self.reproject();

        let resolution = resolve_target(&memento.target, &self.snapshot.files);
        let (new_target, vanished_notice) = match resolution {
            TargetResolution::Overview | TargetResolution::Same => (memento.target.clone(), None),
            TargetResolution::Followed { to } => (FileTarget::File(to), None),
            TargetResolution::Vanished { from } => (
                FileTarget::Overview,
                Some(format!("{from} is no longer in the diff")),
            ),
        };
        let target_changed = new_target != self.target;
        self.target = new_target;

        let generation_changed = match &self.view {
            Some(view) => view.generation() != self.app.generation(),
            None => matches!(self.target, FileTarget::File(_)),
        };
        if kind == RefreshKind::Diff || generation_changed || target_changed {
            self.reload_view();
        }
        self.rebuild_display();
        self.restore_cursor(&memento);
        self.sync_tree_cursor();

        match vanished_notice {
            Some(notice) => self.flash(&notice),
            None => self.flash(match kind {
                RefreshKind::Diff => "diff refreshed",
                RefreshKind::Review => "review updated",
            }),
        }
    }

    /// Force a manual refresh (the `r` command): re-lists the diff even when
    /// nothing marked it dirty.
    pub fn manual_refresh(&mut self) {
        if let Err(err) = self.app.refresh() {
            self.flash(&format!("refresh failed: {err}"));
            return;
        }
        self.refresh(RefreshKind::Diff);
        self.flash("refreshed");
    }

    fn locate(&self, memento: &CursorMemento) -> usize {
        if let Some(key) = &memento.row_key
            && let Some(pos) = self.display_position_of_key(key)
        {
            return pos;
        }
        if let Some(target) = memento.semantic
            && let Some(view) = &self.view
        {
            for (idx, row) in view.view().rows.iter().enumerate() {
                let hit = anchor_target(row, memento.cell) == Some(target)
                    || anchor_target(row, ActiveCell::Auto) == Some(target);
                if hit && let Some(pos) = self.display.iter().position(
                    |d| matches!(d, DRow::View { idx: i, continuation: false, .. } if *i == idx),
                ) {
                    return pos;
                }
            }
        }
        memento.cursor.min(self.display.len().saturating_sub(1))
    }

    fn restore_cursor(&mut self, memento: &CursorMemento) {
        let pos = self.locate(memento);
        self.cursor = pos;
        self.scroll = pos.saturating_sub(memento.viewport_offset);
    }

    fn display_position_of_key(&self, key: &str) -> Option<usize> {
        self.display.iter().position(|row| match row {
            DRow::View {
                idx,
                continuation: false,
                ..
            } => self
                .view
                .as_ref()
                .and_then(|v| v.view().rows.get(*idx))
                .is_some_and(|r| r.key() == key),
            DRow::CommentHead { comment } => self
                .pane_comment(*comment)
                .is_some_and(|(c, ..)| format!("comment:{}", c.id) == key),
            _ => false,
        })
    }

    fn sync_tree_cursor(&mut self) {
        match &self.target {
            FileTarget::Overview => self.tree_cursor = 0,
            FileTarget::File(path) => {
                if let Some(pos) = self.snapshot.files.iter().position(|f| &f.path == path)
                    && let Some(row) = self
                        .projected
                        .tree_rows
                        .iter()
                        .position(|r| r.file_index == Some(pos))
                {
                    self.tree_cursor = row + 1;
                }
            }
        }
    }

    fn view_options(&self) -> ViewOptions {
        ViewOptions {
            mode: self.mode,
            word_diff: self.word_diff,
            highlight: Some(self.theme.choice),
        }
    }

    /// Reload (or build) the view for the current target at the current
    /// options and generation, then re-apply any expansions the caller
    /// still wants (B10/B12).
    fn reload_view(&mut self) {
        let FileTarget::File(path) = self.target.clone() else {
            self.view = None;
            return;
        };
        match self.app.file_diff(&path) {
            Ok((entry, diff)) => {
                let opts = self.view_options();
                let generation = self.app.generation();
                match &mut self.view {
                    Some(view) if view.path() == path => view.reload(entry, diff, generation),
                    _ => self.view = Some(ViewState::new(entry, diff, opts, generation)),
                }
                self.apply_pending_expansions();
            }
            Err(err) => {
                self.view = None;
                self.flash(&format!("diff failed: {err}"));
            }
        }
    }

    fn apply_pending_expansions(&mut self) {
        let Some(view) = &mut self.view else { return };
        let pending = view.pending_expansions();
        if pending.is_empty() {
            return;
        }
        let path = view.path().to_string();
        match self.app.content(Side::New, &path) {
            Ok(Some(content)) => {
                let generation = self.app.generation();
                for gap in pending {
                    if let Err(err) = view.expand(&gap.id, &content, generation) {
                        self.flash(&format!("expand failed: {err}"));
                        break;
                    }
                }
            }
            Ok(None) | Err(_) => self.flash("cannot restore expansions: file unreadable"),
        }
    }

    pub fn open_target(&mut self, target: FileTarget) {
        self.target = target;
        self.cursor = 0;
        self.scroll = 0;
        self.search = None;
        self.reload_view();
        self.rebuild_display();
        self.sync_tree_cursor();
    }

    /// Expand the gap at a display row, remembering it across reloads.
    pub fn expand_gap_at_cursor(&mut self) {
        let Some(DRow::View { idx, .. }) = self.display.get(self.cursor).cloned() else {
            return;
        };
        let Some(view) = &self.view else { return };
        let Some(Row::Gap { gap, .. }) = view.view().rows.get(idx) else {
            self.flash("not a collapsed gap");
            return;
        };
        let gap_id = gap.id.clone();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        match self.app.expand(view, &gap_id) {
            Ok(_) => {
                view.mark_wanted(&gap_id);
                self.rebuild_display_preserving_cursor();
            }
            Err(err) => self.flash(&format!("expand failed: {err}")),
        }
    }

    // ------------------------------------------------------------- display

    fn cursor_row_key(&self) -> Option<String> {
        match self.display.get(self.cursor)? {
            DRow::View { idx, .. } => {
                Some(self.view.as_ref()?.view().rows.get(*idx)?.key().to_string())
            }
            DRow::CommentHead { comment }
            | DRow::CommentLine { comment, .. }
            | DRow::CommentFoot { comment } => self
                .pane_comment(*comment)
                .map(|(c, ..)| format!("comment:{}", c.id)),
            _ => None,
        }
    }

    pub fn rebuild_display_preserving_cursor(&mut self) {
        let key = self.cursor_row_key();
        let offset = self.cursor.saturating_sub(self.scroll);
        self.rebuild_display();
        if let Some(key) = key
            && let Some(pos) = self.display_position_of_key(&key)
        {
            self.cursor = pos;
            self.scroll = pos.saturating_sub(offset);
            return;
        }
        self.cursor = self.cursor.min(self.display.len().saturating_sub(1));
        self.scroll = self.scroll.min(self.cursor);
    }

    fn rebuild_display(&mut self) {
        let mut display = Vec::new();
        self.pane.clear();

        display.push(DRow::Banner);
        for warning in &self.snapshot.warnings {
            display.push(DRow::Notice(format!("warning: {warning}")));
        }
        if let Some(err) = &self.snapshot.source_error {
            display.push(DRow::Notice(format!("source: {err}")));
        }

        match self.target.clone() {
            FileTarget::Overview => self.build_overview_display(&mut display),
            FileTarget::File(_) => self.build_file_display(&mut display),
        }

        self.display = display;
        if self.cursor >= self.display.len() {
            self.cursor = self.display.len().saturating_sub(1);
        }
        self.refresh_search_matches();
    }

    fn push_comment_card(display: &mut Vec<DRow>, comment: &Comment, pane_idx: usize) {
        display.push(DRow::CommentHead { comment: pane_idx });
        for line in comment.body.lines() {
            display.push(DRow::CommentLine {
                comment: pane_idx,
                text: line.to_string(),
            });
        }
        if let Some(response) = &comment.response {
            display.push(DRow::CommentLine {
                comment: pane_idx,
                text: format!("\u{21b3} {response}"),
            });
        }
        display.push(DRow::CommentFoot { comment: pane_idx });
    }

    fn build_overview_display(&mut self, display: &mut Vec<DRow>) {
        let review_level: Vec<usize> = self
            .projected
            .overview
            .iter()
            .filter(|(_, unattached)| !unattached)
            .map(|(idx, _)| *idx)
            .collect();
        let unattached: Vec<usize> = self
            .projected
            .overview
            .iter()
            .filter(|(_, unattached)| *unattached)
            .map(|(idx, _)| *idx)
            .collect();

        if review_level.is_empty() && unattached.is_empty() {
            display.push(DRow::Notice(
                "no review-level comments; press R to add one".to_string(),
            ));
        }

        for idx in review_level {
            let pane_idx = self.pane.len();
            self.pane.push(PaneComment {
                index: idx,
                anchor: RowAnchor {
                    comment_id: self.snapshot.review.comments[idx].id.clone(),
                    row: None,
                    outdated: false,
                    clamped: false,
                },
                was_path: None,
            });
            Self::push_comment_card(display, &self.snapshot.review.comments[idx], pane_idx);
        }

        if !unattached.is_empty() {
            display.push(DRow::Blank);
            display.push(DRow::SectionHead(format!(
                "unattached ({}) - files no longer in this diff",
                unattached.len()
            )));
            for idx in unattached {
                let pane_idx = self.pane.len();
                self.pane.push(PaneComment {
                    index: idx,
                    anchor: RowAnchor {
                        comment_id: self.snapshot.review.comments[idx].id.clone(),
                        row: None,
                        outdated: false,
                        clamped: false,
                    },
                    was_path: None,
                });
                let comment = &self.snapshot.review.comments[idx];
                Self::push_comment_card(display, comment, pane_idx);
                if let Some(snippet) = &comment.snippet {
                    display.push(DRow::CommentLine {
                        comment: pane_idx,
                        text: format!("snippet: {snippet}"),
                    });
                }
            }
        }
    }

    fn build_file_display(&mut self, display: &mut Vec<DRow>) {
        let Some(view) = &self.view else {
            display.push(DRow::Notice("no diff available".to_string()));
            return;
        };

        match &view.view().kind {
            FileDiffKind::Binary { desc } => {
                display.push(DRow::Notice(desc.clone()));
            }
            FileDiffKind::TooLarge { adds, dels } => {
                display.push(DRow::Notice(format!(
                    "diff too large to render (+{adds} -{dels} lines); skipped"
                )));
            }
            FileDiffKind::Text => {}
        }

        let projection =
            ReviewProjection::new(&self.snapshot.review, &self.snapshot.files, self.filter);
        let file_projection = view.file_projection(&projection);
        self.pane = file_projection
            .comments
            .iter()
            .map(|ac| PaneComment {
                index: ac.index,
                anchor: ac.anchor.clone(),
                was_path: ac.was_path.map(str::to_string),
            })
            .collect();

        // File-level comments (no row) come right under the banner.
        for (pane_idx, pane) in self.pane.iter().enumerate() {
            if pane.anchor.row.is_none() {
                let comment = &self.snapshot.review.comments[pane.index];
                Self::push_comment_card(display, comment, pane_idx);
            }
        }

        let width = self.effective_text_width();
        for (row_idx, row) in view.view().rows.iter().enumerate() {
            match row {
                Row::Unified { cell, .. } if self.wrap && cell.text.len() > width => {
                    let mut start = 0;
                    let mut first = true;
                    while start < cell.text.len() {
                        let end =
                            floor_char_boundary(&cell.text, (start + width).min(cell.text.len()));
                        let end = if end <= start { cell.text.len() } else { end };
                        display.push(DRow::View {
                            idx: row_idx,
                            seg: Some((start, end)),
                            continuation: !first,
                        });
                        first = false;
                        start = end;
                    }
                }
                _ => display.push(DRow::View {
                    idx: row_idx,
                    seg: None,
                    continuation: false,
                }),
            }
            for (pane_idx, pane) in self.pane.iter().enumerate() {
                if pane.anchor.row == Some(row_idx) {
                    let comment = &self.snapshot.review.comments[pane.index];
                    Self::push_comment_card(display, comment, pane_idx);
                }
            }
        }
    }

    fn effective_text_width(&self) -> usize {
        self.last_layout
            .as_ref()
            .map(|l| l.content_width)
            .unwrap_or(120)
            .max(20)
    }

    // ------ navigation ------

    pub fn move_cursor(&mut self, delta: i64) {
        let len = self.display.len();
        if len == 0 {
            return;
        }
        let next = (self.cursor as i64 + delta).clamp(0, len as i64 - 1) as usize;
        self.cursor = next;
        self.active_cell = ActiveCell::Auto;
    }

    pub fn cursor_to(&mut self, pos: usize) {
        self.cursor = pos.min(self.display.len().saturating_sub(1));
        self.active_cell = ActiveCell::Auto;
    }

    pub fn jump_next(&mut self, pred: impl Fn(&DRow) -> bool, forward: bool) {
        let len = self.display.len();
        if len == 0 {
            return;
        }
        let range: Box<dyn Iterator<Item = usize>> = if forward {
            Box::new((self.cursor + 1)..len)
        } else {
            Box::new((0..self.cursor).rev())
        };
        for i in range {
            if pred(&self.display[i]) {
                self.cursor = i;
                return;
            }
        }
        self.flash("no more");
    }

    pub fn is_hunk_start(&self, row: &DRow) -> bool {
        matches!(row, DRow::View { idx, continuation: false, .. }
            if self.view.as_ref().is_some_and(|v| matches!(v.view().rows.get(*idx), Some(Row::HunkHeader { .. }))))
    }

    pub fn jump_hunk(&mut self, forward: bool) {
        let positions: Vec<usize> = self
            .display
            .iter()
            .enumerate()
            .filter(|(_, r)| self.is_hunk_start(r))
            .map(|(i, _)| i)
            .collect();
        let next = if forward {
            positions.iter().find(|&&p| p > self.cursor)
        } else {
            positions.iter().rev().find(|&&p| p < self.cursor)
        };
        match next {
            Some(&p) => self.cursor = p,
            None => self.flash("no more hunks"),
        }
    }

    pub fn next_file(&mut self, forward: bool) {
        let order = self.visible_file_order();
        if order.is_empty() {
            self.open_target(FileTarget::Overview);
            return;
        }
        let current = match &self.target {
            FileTarget::Overview => None,
            FileTarget::File(path) => order
                .iter()
                .position(|&i| self.snapshot.files.get(i).is_some_and(|f| &f.path == path)),
        };
        let next = match (current, forward) {
            (None, true) => Some(0),
            (None, false) => Some(order.len() - 1),
            (Some(pos), true) => {
                if pos + 1 < order.len() {
                    Some(pos + 1)
                } else {
                    None
                }
            }
            (Some(pos), false) => pos.checked_sub(1),
        };
        match next {
            Some(pos) => self.open_target(FileTarget::File(
                self.snapshot.files[order[pos]].path.clone(),
            )),
            None => {
                if forward {
                    self.flash("last file");
                } else {
                    self.open_target(FileTarget::Overview);
                }
            }
        }
    }

    // ------ tree ------

    pub fn tree_move(&mut self, delta: i64) {
        let next =
            (self.tree_cursor as i64 + delta).clamp(0, self.tree_len().saturating_sub(1) as i64);
        self.tree_cursor = next as usize;
    }

    pub fn cursor_to_tree(&mut self, pos: usize) {
        self.tree_cursor = pos.min(self.tree_len().saturating_sub(1));
    }

    pub fn tree_top(&mut self) {
        self.tree_cursor = 0;
    }

    pub fn tree_bottom(&mut self) {
        self.tree_cursor = self.tree_len().saturating_sub(1);
    }

    pub fn tree_select(&mut self) {
        if self.tree_cursor == 0 {
            self.open_target(FileTarget::Overview);
            self.focus = Focus::Diff;
            return;
        }
        let Some(row) = self.projected.tree_rows.get(self.tree_cursor - 1).cloned() else {
            return;
        };
        if row.is_dir {
            if self.collapsed.contains(&row.path) {
                self.collapsed.remove(&row.path);
            } else {
                self.collapsed.insert(row.path.clone());
            }
            let cursor = self.tree_cursor;
            self.reproject();
            self.tree_cursor = cursor.min(self.tree_len().saturating_sub(1));
        } else if let Some(idx) = row.file_index {
            let path = self.snapshot.files[idx].path.clone();
            self.open_target(FileTarget::File(path));
            self.focus = Focus::Diff;
        }
    }

    pub fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
        self.reproject();
        let label = self.filter.label().to_string();
        self.flash(&format!("filter: {label}"));
    }

    // ------ toggles ------

    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            ViewMode::Unified => ViewMode::Split,
            ViewMode::Split => ViewMode::Unified,
        };
        let opts = self.view_options_with(self.mode);
        if let Some(view) = &mut self.view {
            view.set_options(opts);
        }
        self.rebuild_display_preserving_cursor();
    }

    fn view_options_with(&self, mode: ViewMode) -> ViewOptions {
        ViewOptions {
            mode,
            word_diff: self.word_diff,
            highlight: Some(self.theme.choice),
        }
    }

    pub fn toggle_word_diff(&mut self) {
        self.word_diff = !self.word_diff;
        let opts = self.view_options();
        if let Some(view) = &mut self.view {
            view.set_options(opts);
        }
        self.rebuild_display_preserving_cursor();
    }

    pub fn toggle_theme(&mut self) {
        self.theme = match self.theme.choice {
            ThemeChoice::Dark => theme::light(),
            ThemeChoice::Light => theme::dark(),
        };
        let opts = self.view_options();
        if let Some(view) = &mut self.view {
            view.set_options(opts);
        }
        self.rebuild_display_preserving_cursor();
    }

    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.rebuild_display_preserving_cursor();
    }

    pub fn toggle_tree(&mut self) {
        self.show_tree = !self.show_tree;
    }

    pub fn toggle_line_numbers(&mut self) {
        self.line_numbers = !self.line_numbers;
    }

    // ------ search ------

    pub fn start_search(&mut self) {
        self.search = Some(SearchState {
            query: String::new(),
            typing: true,
            matches: Vec::new(),
            current: 0,
        });
    }

    pub fn cancel_search(&mut self) {
        self.search = None;
    }

    pub fn search_typing(&mut self) -> Option<&mut SearchState> {
        self.search.as_mut().filter(|s| s.typing)
    }

    pub fn stop_search_typing(&mut self) {
        if let Some(s) = &mut self.search {
            s.typing = false;
        }
    }

    pub fn refresh_search_matches(&mut self) {
        let Some(view) = &self.view else {
            if let Some(s) = &mut self.search {
                s.matches.clear();
            }
            return;
        };
        if let Some(s) = &mut self.search {
            s.matches = search_rows(&view.view().rows, &s.query);
            if s.current >= s.matches.len() {
                s.current = 0;
            }
        }
    }

    /// Jump the cursor to the display row showing search match `i`.
    pub fn goto_match(&mut self, i: usize) {
        let Some(s) = &self.search else { return };
        let Some(m) = s.matches.get(i) else { return };
        let row_idx = m.row;
        if let Some(pos) = self.display.iter().position(
            |d| matches!(d, DRow::View { idx, continuation: false, .. } if *idx == row_idx),
        ) {
            self.cursor = pos;
        }
    }

    pub fn search_step(&mut self, forward: bool) {
        let Some(s) = &mut self.search else {
            self.flash("no search");
            return;
        };
        if s.matches.is_empty() {
            self.flash("no matches");
            return;
        }
        let len = s.matches.len();
        s.current = if forward {
            (s.current + 1) % len
        } else {
            (s.current + len - 1) % len
        };
        let current = s.current;
        self.goto_match(current);
    }

    // ------ comments ------

    /// The comment index (into `review.comments`) under the cursor, if any.
    pub fn comment_at_cursor(&self) -> Option<usize> {
        match self.display.get(self.cursor)? {
            DRow::CommentHead { comment }
            | DRow::CommentLine { comment, .. }
            | DRow::CommentFoot { comment } => self.pane.get(*comment).map(|p| p.index),
            _ => None,
        }
    }

    /// The (path, side, line) target for a new line comment at the cursor,
    /// honouring which split-mode cell last had focus (B18).
    pub fn line_target_at_cursor(&self) -> Option<(String, Side, u32)> {
        let DRow::View { idx, .. } = self.display.get(self.cursor)? else {
            return None;
        };
        let view = self.view.as_ref()?;
        let (side, line) = view.anchor_target(*idx, self.active_cell)?;
        Some((view.path().to_string(), side, line))
    }

    pub fn open_comment_editor(&mut self, intent: EditorIntent) {
        if self.snapshot.read_only {
            let reason = self
                .snapshot
                .read_only_reason
                .clone()
                .unwrap_or_else(|| "newer schema".to_string());
            self.flash(&format!("review file is read-only ({reason})"));
            return;
        }
        let (title, initial, single) = match &intent {
            EditorIntent::NewComment { path, line, .. } => (
                match (path, line) {
                    (Some(p), Some(l)) => format!("comment on {p}:{l}"),
                    (Some(p), None) => format!("comment on {p}"),
                    _ => "comment on review".to_string(),
                },
                String::new(),
                false,
            ),
            EditorIntent::EditBody { id } => {
                let body = self
                    .snapshot
                    .review
                    .find_comment(id)
                    .map(|c| c.body.clone())
                    .unwrap_or_default();
                (format!("edit {id}"), body, false)
            }
            EditorIntent::AddressResponse { id } => {
                (format!("response for {id} (optional)"), String::new(), true)
            }
        };
        let editor = if initial.is_empty() {
            Editor::new(single)
        } else {
            Editor::with_text(&initial, single)
        };
        self.overlay = Overlay::Editor(Box::new(EditorState {
            editor,
            intent,
            title,
            last_error: None,
        }));
    }

    /// Called on every keystroke inside the editor overlay: a fresh edit
    /// clears any previous save error (B19).
    pub fn clear_editor_error(&mut self) {
        if let Overlay::Editor(state) = &mut self.overlay {
            state.last_error = None;
        }
    }

    pub fn discard_editor(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Save the active editor overlay. On success, closes the overlay and
    /// refreshes; on failure, keeps the draft open with the error recorded
    /// so the author can fix it and retry (B19).
    pub fn save_editor(&mut self) {
        let Overlay::Editor(state) = &mut self.overlay else {
            return;
        };
        let text = state.editor.text();
        let requires_body = !matches!(state.intent, EditorIntent::AddressResponse { .. });
        if requires_body && text.trim().is_empty() {
            state.last_error = Some("empty text discarded".to_string());
            self.flash("save failed");
            return;
        }
        let (cmd, ok_msg) = match &state.intent {
            EditorIntent::NewComment { path, side, line } => (
                ReviewCommand::Add(CommentAddRequest {
                    path: path.clone(),
                    side: *side,
                    line: *line,
                    end_line: None,
                    body: text,
                    author: None,
                }),
                "comment added",
            ),
            EditorIntent::EditBody { id } => (
                ReviewCommand::Edit(CommentEditRequest {
                    id: id.clone(),
                    body: text,
                }),
                "comment updated",
            ),
            EditorIntent::AddressResponse { id } => {
                let response = (!text.trim().is_empty()).then_some(text);
                (
                    ReviewCommand::Lifecycle {
                        action: Action::Address,
                        actor: Actor::Agent,
                        req: LifecycleRequest {
                            id: id.clone(),
                            response,
                        },
                    },
                    "response recorded",
                )
            }
        };
        match self.app.execute(cmd) {
            Ok(_) => {
                self.overlay = Overlay::None;
                self.refresh(RefreshKind::Review);
                self.flash(ok_msg);
            }
            Err(err) => {
                if let Overlay::Editor(state) = &mut self.overlay {
                    state.last_error = Some(err.to_string());
                }
                self.flash("save failed");
            }
        }
    }

    pub fn apply_lifecycle(&mut self, id: &str, action: Action, response: Option<String>) {
        let actor = match action {
            Action::Address => Actor::Agent,
            _ => Actor::Human,
        };
        match self.app.execute(ReviewCommand::Lifecycle {
            action,
            actor,
            req: LifecycleRequest {
                id: id.to_string(),
                response,
            },
        }) {
            Ok(_) => self.refresh(RefreshKind::Review),
            Err(err) => self.flash(&format!("save failed: {err}")),
        }
    }

    pub fn delete_comment(&mut self, id: &str) {
        match self.app.execute(ReviewCommand::Delete(
            ambidiff_core::protocol::CommentDeleteRequest { id: id.to_string() },
        )) {
            Ok(_) => self.refresh(RefreshKind::Review),
            Err(err) => self.flash(&format!("save failed: {err}")),
        }
    }
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use ambidiff_core::model::{FileEntry, FileStatus};

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

    #[test]
    fn resolve_target_same_path_stays_same() {
        let files = vec![entry("a.rs", None), entry("b.rs", None)];
        let prev = FileTarget::File("a.rs".to_string());
        assert_eq!(resolve_target(&prev, &files), TargetResolution::Same);
    }

    #[test]
    fn resolve_target_follows_a_rename() {
        let files = vec![entry("new.rs", Some("old.rs"))];
        let prev = FileTarget::File("old.rs".to_string());
        assert_eq!(
            resolve_target(&prev, &files),
            TargetResolution::Followed {
                to: "new.rs".to_string()
            }
        );
    }

    #[test]
    fn resolve_target_reports_vanished_when_nothing_matches() {
        let files = vec![entry("other.rs", None)];
        let prev = FileTarget::File("gone.rs".to_string());
        assert_eq!(
            resolve_target(&prev, &files),
            TargetResolution::Vanished {
                from: "gone.rs".to_string()
            }
        );
    }

    #[test]
    fn resolve_target_overview_stays_overview() {
        let files = vec![entry("a.rs", None)];
        assert_eq!(
            resolve_target(&FileTarget::Overview, &files),
            TargetResolution::Overview
        );
    }
}
