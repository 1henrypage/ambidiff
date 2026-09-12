//! TUI painting: everything visual, derived strictly from `App` state.
//!
//! `layout` (B25) is the one place pane geometry is computed: `draw` calls
//! it once per frame through `current_layout`, mouse hit-testing calls the
//! same function for the present state instead of trusting a cached frame,
//! and `split_geometry` is shared by the split-mode painter and the
//! layout/hit-test code so they can never disagree about where the
//! separator column sits.

use ambidiff_core::anchor::ActiveCell;
use ambidiff_core::highlight::HlSpan;
use ambidiff_core::model::FileStatus;
use ambidiff_core::review::{Comment, Status};
use ambidiff_core::rows::{Cell, CellKind, Row, ViewMode};
use ambidiff_core::sanitize::sanitize_line;
use ambidiff_core::search::MatchCell;
use ambidiff_core::worddiff::Range as WordRange;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout as RLayout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthChar;

use super::app::{App, DRow, FileTarget, Focus, Overlay};
use super::theme::Theme;
use super::wrap;

const TREE_WIDTH: u16 = 34;

/// Sanitise one piece of metadata before it reaches the screen (B26): tree
/// names, notices, section heads, the review name, banner path/was-path,
/// comment id/author, the status message, the search query, editor title
/// and error, and the confirm dialog id all pass through here.
fn clean(s: &str) -> String {
    sanitize_line(s).into_owned()
}

// --------------------------------------------------------------- layout

/// What `layout` needs to know to place the panes: purely a function of UI
/// state, no rendering side effects.
pub struct LayoutInputs {
    pub show_tree: bool,
    pub line_numbers: bool,
    pub mode: ViewMode,
    pub tree_cursor: usize,
    /// The largest line number the current view could show; drives gutter
    /// width. Zero when nothing is open.
    pub max_line_number: u32,
}

/// Where a mouse click lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// A tree row, already translated by the tree's scroll offset.
    Tree(usize),
    /// A diff-pane row, plus which split cell (if any) was clicked.
    Diff {
        offset: usize,
        cell: ActiveCell,
    },
    None,
}

/// One frame's computed pane geometry: paint, hit-test, and reflow all read
/// from this single result instead of recomputing rectangles independently
/// (B25).
#[derive(Debug, Clone)]
pub struct Layout {
    pub tree: Option<Rect>,
    pub diff: Rect,
    pub status: Rect,
    pub tree_scroll: usize,
    pub gutter: usize,
    /// Absolute column of the split separator, when in split mode.
    pub split_sep_col: Option<u16>,
    pub content_width: usize,
    /// Wrap budget for comment card bodies: `pane_width - CARD_PREFIX`, not
    /// `content_width - CARD_PREFIX` - a card has no gutter or sign column,
    /// so its true budget is the pane width minus its own border chrome.
    pub card_width: usize,
}

impl Layout {
    /// Translate a terminal (x, y) into a pane hit. The hidden tree is
    /// never hit even if the mouse lands where it would otherwise be.
    pub fn hit(&self, x: u16, y: u16) -> Hit {
        if let Some(tree) = self.tree
            && x >= tree.x
            && x < tree.x + tree.width
            && y >= tree.y
            && y < tree.y + tree.height
        {
            return Hit::Tree(self.tree_scroll + (y - tree.y) as usize);
        }
        if x >= self.diff.x
            && x < self.diff.x + self.diff.width
            && y >= self.diff.y
            && y < self.diff.y + self.diff.height
        {
            let offset = (y - self.diff.y) as usize;
            let cell = match self.split_sep_col {
                Some(sep) if x < sep => ActiveCell::Left,
                Some(_) => ActiveCell::Right,
                None => ActiveCell::Auto,
            };
            return Hit::Diff { offset, cell };
        }
        Hit::None
    }
}

/// Split-mode geometry shared by the painter and the layout/hit-test code:
/// gutter digit width, each half's width, the text budget per cell, and the
/// separator's column offset relative to the diff area's own left edge.
pub fn split_geometry(width: usize, line_numbers: bool) -> (usize, usize, usize, u16) {
    let digits = if line_numbers { 4 } else { 0 };
    let half = width.saturating_sub(1) / 2;
    let text_budget = half.saturating_sub(digits + 3).max(8);
    let sep_col = (digits + 3 + text_budget) as u16;
    (digits, half, text_budget, sep_col)
}

fn gutter_width_for(line_numbers: bool, max_line_number: u32) -> usize {
    if !line_numbers {
        return 2;
    }
    let digits = max_line_number.max(1).to_string().len().max(3);
    digits * 2 + 3
}

/// Compute this frame's pane geometry. Pure: no terminal or `App` state
/// beyond `inputs` and the area ratatui gave us.
pub fn layout(inputs: &LayoutInputs, area: Rect) -> Layout {
    let vertical = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let body = vertical[0];
    let status = vertical[1];

    let (tree, diff) = if inputs.show_tree && body.width > TREE_WIDTH + 20 {
        let chunks = RLayout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(TREE_WIDTH), Constraint::Min(10)])
            .split(body);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, body)
    };

    let gutter = gutter_width_for(inputs.line_numbers, inputs.max_line_number);
    let content_width = (diff.width as usize).saturating_sub(gutter + 3).max(20);
    let card_width = (diff.width as usize)
        .saturating_sub(wrap::CARD_PREFIX)
        .max(wrap::MIN_CARD_WIDTH);
    let split_sep_col = match inputs.mode {
        ViewMode::Split => {
            let (_, _, _, sep) = split_geometry(diff.width as usize, inputs.line_numbers);
            Some(diff.x + sep)
        }
        ViewMode::Unified => None,
    };
    let tree_height = tree.map(|t| t.height as usize).unwrap_or(0);
    let tree_scroll = inputs
        .tree_cursor
        .saturating_sub(tree_height.saturating_sub(1));

    Layout {
        tree,
        diff,
        status,
        tree_scroll,
        gutter,
        split_sep_col,
        content_width,
        card_width,
    }
}

fn max_line_number(app: &App) -> u32 {
    app.view()
        .map(|v| {
            v.view()
                .rows
                .iter()
                .filter_map(|r| match r {
                    Row::Unified {
                        old_num, new_num, ..
                    } => *old_num.max(new_num),
                    Row::Split { left, right, .. } => left.line.max(right.line),
                    _ => None,
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// The layout for the app's current state in `area`: the one computation
/// painting and hit testing share, so a click is never tested against
/// geometry from before an intervening key changed it.
pub fn current_layout(app: &App, area: Rect) -> Layout {
    let inputs = LayoutInputs {
        show_tree: app.show_tree(),
        line_numbers: app.line_numbers(),
        mode: app.mode(),
        tree_cursor: app.tree_cursor(),
        max_line_number: max_line_number(app),
    };
    layout(&inputs, area)
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(app.theme().bg).fg(app.theme().fg)),
        area,
    );

    let computed = current_layout(app, area);

    if let Some(tree_area) = computed.tree {
        draw_tree(frame, app, tree_area, computed.tree_scroll);
    }
    draw_diff(frame, app, computed.diff, computed.gutter);
    draw_status_bar(frame, app, computed.status);

    match app.overlay() {
        Overlay::Help => draw_help(frame, app, area),
        Overlay::Editor(_) => draw_editor(frame, app, area),
        Overlay::ConfirmDelete { id } => draw_confirm(frame, app, area, &id.clone()),
        Overlay::None => {
            if let Some(search) = app.search()
                && search.typing
            {
                let query = search.query.clone();
                draw_search_bar(frame, app, computed.status, &query);
            }
        }
    }

    let status_area = computed.status;
    app.apply_layout(computed);
    let _ = status_area;
}

// ---------- tree ----------

fn status_char(status: FileStatus) -> (&'static str, bool) {
    match status {
        FileStatus::Added => ("A", true),
        FileStatus::Modified => ("M", false),
        FileStatus::Deleted => ("D", false),
        FileStatus::Renamed => ("R", false),
        FileStatus::Copied => ("C", false),
        FileStatus::Untracked => ("?", true),
        FileStatus::Other => ("T", false),
    }
}

fn draw_tree(frame: &mut Frame, app: &App, area: Rect, scroll: usize) {
    let theme = app.theme();
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(theme.comment_border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;

    let mut lines: Vec<Line> = Vec::new();
    for pos in scroll..(scroll + height).min(app.tree_len()) {
        let selected = pos == app.tree_cursor();
        let focused = app.focus() == Focus::Tree;
        let base = if selected {
            Style::default()
                .bg(theme.tree_selected_bg)
                .add_modifier(if focused {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                })
        } else {
            Style::default()
        };

        if pos == 0 {
            let counts = app.review_level_counts();
            let marker = if matches!(app.target(), FileTarget::Overview) {
                "\u{25b8} "
            } else {
                "  "
            };
            let mut spans = vec![
                Span::styled(marker.to_string(), base.fg(theme.hunk_fg)),
                Span::styled("(review)", base.fg(theme.fg)),
            ];
            if counts.total > 0 {
                let style = if counts.todo > 0 {
                    base.fg(theme.open_fg)
                } else {
                    base.fg(theme.dim)
                };
                spans.push(Span::styled(
                    format!("  {}/{}", counts.todo, counts.total),
                    style,
                ));
            }
            lines.push(Line::from(spans).style(base));
            continue;
        }

        let Some(row) = app.tree_rows().get(pos - 1) else {
            continue;
        };
        let indent = "  ".repeat(row.depth + 1);
        let mut spans: Vec<Span> = vec![Span::styled(indent, base)];
        if row.is_dir {
            let arrow = if row.collapsed {
                "\u{25b8} "
            } else {
                "\u{25be} "
            };
            spans.push(Span::styled(arrow.to_string(), base.fg(theme.dim)));
            spans.push(Span::styled(
                clean(&row.name),
                base.fg(theme.fg).add_modifier(Modifier::BOLD),
            ));
        } else if let Some(fi) = row.file_index {
            let entry = &app.files()[fi];
            let (sc, is_add) = status_char(entry.status);
            let sc_style = if is_add {
                base.fg(theme.add_sign)
            } else if entry.status == FileStatus::Deleted {
                base.fg(theme.remove_sign)
            } else {
                base.fg(theme.addressed_fg)
            };
            let current = matches!(app.target(), FileTarget::File(p) if p == &entry.path);
            spans.push(Span::styled(format!("{sc} "), sc_style));
            spans.push(Span::styled(
                clean(&row.name),
                if current {
                    base.fg(theme.hunk_fg).add_modifier(Modifier::BOLD)
                } else {
                    base.fg(theme.fg)
                },
            ));
            let (todo, total) = app.file_comment_counts(&entry.path);
            if total > 0 {
                let style = if todo > 0 {
                    base.fg(theme.open_fg)
                } else {
                    base.fg(theme.resolved_fg)
                };
                spans.push(Span::styled(format!(" \u{25cb}{todo}/{total}"), style));
            }
            if let (Some(a), Some(d)) = (entry.adds, entry.dels) {
                spans.push(Span::styled(format!(" +{a}"), base.fg(theme.add_sign)));
                spans.push(Span::styled(format!(" -{d}"), base.fg(theme.remove_sign)));
            }
        }
        lines.push(Line::from(spans).style(base));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------- diff pane ----------

fn draw_diff(frame: &mut Frame, app: &mut App, area: Rect, gutter: usize) {
    let height = area.height as usize;
    let mut scroll = app.scroll();
    let mut cursor = app.cursor();
    if cursor < scroll {
        scroll = cursor;
    } else if height > 0 && cursor >= scroll + height {
        scroll = cursor + 1 - height;
    }
    let _ = &mut cursor;
    app.set_scroll(scroll);

    let mut lines: Vec<Line> = Vec::new();
    let end = (scroll + height).min(app.display().len());
    for pos in scroll..end {
        lines.push(render_drow(app, pos, area.width as usize, gutter));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_drow(app: &App, pos: usize, width: usize, gutter: usize) -> Line<'static> {
    let theme = app.theme();
    let is_cursor = pos == app.cursor() && app.focus() == Focus::Diff;
    let cursor_style = if is_cursor {
        Style::default().bg(theme.cursor_bg)
    } else {
        Style::default()
    };

    match &app.display()[pos] {
        DRow::Banner => banner_line(app),
        DRow::Notice(text) => Line::from(Span::styled(
            format!("  {}", clean(text)),
            cursor_style.fg(theme.warn_fg),
        ))
        .style(cursor_style),
        DRow::Blank => Line::default().style(cursor_style),
        DRow::SectionHead(text) => Line::from(Span::styled(
            format!("\u{2500}\u{2500} {} ", clean(text)),
            cursor_style.fg(theme.dim).add_modifier(Modifier::BOLD),
        ))
        .style(cursor_style),
        DRow::CommentHead { comment } => comment_head_line(app, *comment, is_cursor, width),
        DRow::CommentLine { comment: _, text } => {
            let card_bg = if is_cursor {
                theme.cursor_bg
            } else {
                theme.comment_bg
            };
            let clean_text = clean(text);
            let mut spans = vec![
                Span::styled(
                    wrap::CARD_BORDER.to_string(),
                    Style::default().fg(theme.comment_border).bg(card_bg),
                ),
                Span::styled(clean_text, Style::default().fg(theme.fg).bg(card_bg)),
            ];
            pad_line(&mut spans, width, card_bg);
            Line::from(spans)
        }
        DRow::CommentFoot { comment: _ } => {
            let card_bg = if is_cursor {
                theme.cursor_bg
            } else {
                theme.comment_bg
            };
            let mut spans = vec![Span::styled(
                "  \u{2514}\u{2500}".to_string(),
                Style::default().fg(theme.comment_border).bg(card_bg),
            )];
            pad_line(&mut spans, width, card_bg);
            Line::from(spans)
        }
        DRow::View {
            idx,
            seg,
            continuation,
        } => view_row_line(app, *idx, *seg, *continuation, is_cursor, width, gutter),
    }
}

fn banner_line(app: &App) -> Line<'static> {
    let theme = app.theme();
    let style = Style::default().bg(theme.banner_bg);
    let mut spans: Vec<Span> = Vec::new();
    match app.target() {
        FileTarget::Overview => {
            spans.push(Span::styled(
                format!(" review {} ", clean(&app.review().review)),
                style.fg(theme.fg).add_modifier(Modifier::BOLD),
            ));
            let counts = app.review().counts();
            spans.push(Span::styled(
                format!(
                    " rev {}  \u{25cb}{} \u{21ba}{} \u{25d0}{} \u{25cf}{}",
                    app.review().revision,
                    counts.open,
                    counts.reopened,
                    counts.addressed,
                    counts.resolved
                ),
                style.fg(theme.dim),
            ));
        }
        FileTarget::File(path) => {
            if let Some(entry) = app.files().iter().find(|e| &e.path == path) {
                spans.push(Span::styled(
                    format!(" {} ", clean(&entry.path)),
                    style.fg(theme.fg).add_modifier(Modifier::BOLD),
                ));
                if let Some(old) = &entry.old_path {
                    spans.push(Span::styled(
                        format!("(was {}) ", clean(old)),
                        style.fg(theme.warn_fg),
                    ));
                }
                if let Some(view) = app.view() {
                    spans.push(Span::styled(
                        format!("+{} ", view.view().adds),
                        style.fg(theme.add_sign),
                    ));
                    spans.push(Span::styled(
                        format!("-{} ", view.view().dels),
                        style.fg(theme.remove_sign),
                    ));
                }
                let (todo, total) = app.file_comment_counts(&entry.path);
                if total > 0 {
                    spans.push(Span::styled(
                        format!(" \u{25cb} {todo}/{total} comments"),
                        style.fg(if todo > 0 {
                            theme.open_fg
                        } else {
                            theme.resolved_fg
                        }),
                    ));
                }
            }
        }
    }
    Line::from(spans).style(style)
}

fn status_glyph(status: Status, theme: &Theme) -> Span<'static> {
    let (glyph, color) = match status {
        Status::Open => ("\u{25cb}", theme.open_fg),
        Status::Addressed => ("\u{25d0}", theme.addressed_fg),
        Status::Resolved => ("\u{25cf}", theme.resolved_fg),
        Status::Reopened => ("\u{21ba}", theme.reopened_fg),
    };
    Span::styled(glyph.to_string(), Style::default().fg(color))
}

fn comment_head_line(app: &App, pane_idx: usize, is_cursor: bool, width: usize) -> Line<'static> {
    let theme = app.theme();
    let card_bg = if is_cursor {
        theme.cursor_bg
    } else {
        theme.comment_bg
    };
    let base = Style::default().bg(card_bg);
    let Some((comment, anchor, _was_path)) = app.pane_comment(pane_idx) else {
        return Line::default();
    };
    let comment: &Comment = comment;

    let mut spans = vec![
        Span::styled(
            "  \u{250c}\u{2500} ".to_string(),
            base.fg(theme.comment_border),
        ),
        status_glyph(comment.status, &theme).style(base.fg(match comment.status {
            Status::Open => theme.open_fg,
            Status::Addressed => theme.addressed_fg,
            Status::Resolved => theme.resolved_fg,
            Status::Reopened => theme.reopened_fg,
        })),
        Span::styled(
            format!(" {} ", clean(&comment.id)),
            base.fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{} ", comment.status.as_str()), base.fg(theme.dim)),
        Span::styled(format!("rev {} ", comment.rev), base.fg(theme.dim)),
        Span::styled(
            format!("by {} ", clean(&comment.author)),
            base.fg(theme.dim),
        ),
    ];
    if comment.is_question() {
        spans.push(Span::styled(
            "[question] ".to_string(),
            base.fg(theme.warn_fg).add_modifier(Modifier::BOLD),
        ));
    }
    if anchor.outdated {
        spans.push(Span::styled(
            "[outdated] ".to_string(),
            base.fg(theme.warn_fg).add_modifier(Modifier::BOLD),
        ));
    }
    if anchor.clamped {
        spans.push(Span::styled("[moved] ".to_string(), base.fg(theme.warn_fg)));
    }
    if let (Some(line), Some(end)) = (comment.line, comment.end_line) {
        spans.push(Span::styled(format!("L{line}-{end} "), base.fg(theme.dim)));
    }
    pad_line(&mut spans, width, card_bg);
    Line::from(spans)
}

/// Pad a line's spans with `bg` out to `width` display columns so card and
/// banner backgrounds span the full pane instead of ending mid-row.
fn pad_line(spans: &mut Vec<Span<'static>>, width: usize, bg: ratatui::style::Color) {
    let used: usize = spans.iter().map(|s| wrap::display_width(&s.content)).sum();
    if used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
}

/// Extra emphasis ranges for search matches on a given row/cell.
fn search_ranges_for(app: &App, row_idx: usize, cell: MatchCell) -> Vec<WordRange> {
    let Some(search) = app.search() else {
        return Vec::new();
    };
    search
        .matches
        .iter()
        .filter(|m| m.row == row_idx && m.cell == cell)
        .map(|m| WordRange {
            start: m.start,
            end: m.end,
        })
        .collect()
}

fn view_row_line(
    app: &App,
    idx: usize,
    seg: Option<(usize, usize)>,
    continuation: bool,
    is_cursor: bool,
    width: usize,
    gutter: usize,
) -> Line<'static> {
    let theme = app.theme();
    let Some(view) = app.view() else {
        return Line::default();
    };
    let Some(row) = view.view().rows.get(idx) else {
        return Line::default();
    };

    match row {
        Row::HunkHeader { text, .. } => {
            let style = Style::default().fg(theme.hunk_fg).bg(if is_cursor {
                theme.cursor_bg
            } else {
                theme.bg
            });
            Line::from(Span::styled(clean(text), style)).style(style)
        }
        Row::Gap { gap, .. } => {
            let style = Style::default().fg(theme.gap_fg).bg(if is_cursor {
                theme.cursor_bg
            } else {
                theme.bg
            });
            let noun = if gap.count == 1 { "line" } else { "lines" };
            Line::from(Span::styled(
                format!(
                    "{:>pad$} \u{22ef} {} unchanged {noun} \u{22ef}  (enter expands)",
                    "",
                    gap.count,
                    pad = gutter.saturating_sub(1)
                ),
                style,
            ))
            .style(style)
        }
        Row::Unified {
            old_num,
            new_num,
            cell,
            ..
        } => unified_line(
            app,
            idx,
            cell,
            *old_num,
            *new_num,
            seg,
            continuation,
            is_cursor,
            gutter,
        ),
        Row::Split { left, right, .. } => split_line(app, idx, left, right, is_cursor, width),
    }
}

fn cell_bg(kind: CellKind, theme: &Theme, is_cursor: bool) -> ratatui::style::Color {
    match kind {
        CellKind::Add => theme.add_bg,
        CellKind::Remove => theme.remove_bg,
        CellKind::Context | CellKind::Empty => {
            if is_cursor {
                theme.cursor_bg
            } else {
                theme.bg
            }
        }
    }
}

fn word_bg(kind: CellKind, theme: &Theme) -> ratatui::style::Color {
    match kind {
        CellKind::Add => theme.add_word_bg,
        CellKind::Remove => theme.remove_word_bg,
        _ => theme.bg,
    }
}

fn sign_span(kind: CellKind, theme: &Theme, bg: ratatui::style::Color) -> Span<'static> {
    let (sign, color) = match kind {
        CellKind::Add => ("+", theme.add_sign),
        CellKind::Remove => ("-", theme.remove_sign),
        CellKind::Context => (" ", theme.dim),
        CellKind::Empty => (" ", theme.dim),
    };
    Span::styled(format!("{sign} "), Style::default().fg(color).bg(bg))
}

#[allow(clippy::too_many_arguments)]
fn unified_line(
    app: &App,
    row_idx: usize,
    cell: &Cell,
    old_num: Option<u32>,
    new_num: Option<u32>,
    seg: Option<(usize, usize)>,
    continuation: bool,
    is_cursor: bool,
    gutter: usize,
) -> Line<'static> {
    let theme = app.theme();
    let bg = cell_bg(cell.kind, &theme, is_cursor);
    let mut spans: Vec<Span> = Vec::new();

    if app.line_numbers() {
        let digits = (gutter - 3) / 2;
        let fmt_num = |n: Option<u32>| match n {
            Some(n) => format!("{n:>digits$}"),
            None => " ".repeat(digits),
        };
        let gutter_bg = if is_cursor { theme.cursor_bg } else { bg };
        let gutter_style = Style::default()
            .fg(if is_cursor { theme.fg } else { theme.gutter })
            .bg(gutter_bg);
        if continuation {
            spans.push(Span::styled(" ".repeat(digits * 2 + 1), gutter_style));
        } else {
            spans.push(Span::styled(
                format!("{} {}", fmt_num(old_num), fmt_num(new_num)),
                gutter_style,
            ));
        }
        spans.push(Span::styled(" ".to_string(), gutter_style));
    } else {
        spans.push(Span::styled(
            "  ".to_string(),
            Style::default().bg(if is_cursor { theme.cursor_bg } else { bg }),
        ));
    }

    spans.push(sign_span(
        if continuation {
            CellKind::Context
        } else {
            cell.kind
        },
        &theme,
        bg,
    ));
    let search = search_ranges_for(app, row_idx, MatchCell::Unified);
    spans.extend(styled_content(
        &cell.text,
        &cell.hl,
        &cell.word_ranges,
        &search,
        seg,
        bg,
        word_bg(cell.kind, &theme),
        &theme,
    ));
    Line::from(spans).style(Style::default().bg(bg))
}

fn split_line(
    app: &App,
    row_idx: usize,
    left: &Cell,
    right: &Cell,
    is_cursor: bool,
    width: usize,
) -> Line<'static> {
    let theme = app.theme();
    let (digits, _half, text_budget, _sep) = split_geometry(width, app.line_numbers());

    let mut spans: Vec<Span> = Vec::new();
    for (side_idx, cell) in [left, right].into_iter().enumerate() {
        let bg = cell_bg(cell.kind, &theme, is_cursor);
        if app.line_numbers() {
            let num = match cell.line {
                Some(n) => format!("{n:>digits$}"),
                None => " ".repeat(digits),
            };
            spans.push(Span::styled(
                num,
                Style::default()
                    .fg(if is_cursor { theme.fg } else { theme.gutter })
                    .bg(bg),
            ));
        }
        spans.push(Span::styled(" ".to_string(), Style::default().bg(bg)));
        spans.push(sign_span(cell.kind, &theme, bg));

        let cell_match = if side_idx == 0 {
            MatchCell::Left
        } else {
            MatchCell::Right
        };
        let search = search_ranges_for(app, row_idx, cell_match);
        let content = styled_content(
            &cell.text,
            &cell.hl,
            &cell.word_ranges,
            &search,
            None,
            bg,
            word_bg(cell.kind, &theme),
            &theme,
        );
        let (mut fitted, used) = fit_spans(content, text_budget);
        if used < text_budget {
            fitted.push(Span::styled(
                " ".repeat(text_budget - used),
                Style::default().bg(bg),
            ));
        }
        spans.extend(fitted);
        if side_idx == 0 {
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(theme.comment_border).bg(if is_cursor {
                    theme.cursor_bg
                } else {
                    theme.bg
                }),
            ));
        }
    }
    Line::from(spans)
}

/// Truncate styled spans to a display-width budget (CJK-aware); returns the
/// fitted spans and the width consumed.
fn fit_spans(spans: Vec<Span<'static>>, budget: usize) -> (Vec<Span<'static>>, usize) {
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        if used >= budget {
            break;
        }
        let mut text = String::new();
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > budget {
                break;
            }
            text.push(c);
            used += w;
        }
        let truncated = text.len() < span.content.len();
        if !text.is_empty() {
            out.push(Span::styled(text, span.style));
        }
        if truncated {
            break;
        }
    }
    (out, used)
}

/// Compose one cell's styled content spans: syntax fg + word-diff bg +
/// search bg, sanitized and tab-expanded, optionally sliced to `seg`.
#[allow(clippy::too_many_arguments)]
fn styled_content(
    text: &str,
    hl: &[HlSpan],
    word_ranges: &[WordRange],
    search_ranges: &[WordRange],
    seg: Option<(usize, usize)>,
    base_bg: ratatui::style::Color,
    emphasis_bg: ratatui::style::Color,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let (start, end) = seg.unwrap_or((0, text.len()));
    let window = &text[start..end.min(text.len())];

    let mut bounds: Vec<usize> = vec![0, window.len()];
    let mut push = |offset: usize| {
        if offset > start && offset < end {
            bounds.push(offset - start);
        }
    };
    for span in hl {
        push(span.start as usize);
        push(span.end as usize);
    }
    for range in word_ranges.iter().chain(search_ranges) {
        push(range.start as usize);
        push(range.end as usize);
    }
    bounds.sort_unstable();
    bounds.dedup();
    bounds.retain(|&b| window.is_char_boundary(b));

    let mut out = Vec::new();
    let mut column = 0usize;
    for pair in bounds.windows(2) {
        let (seg_start, seg_end) = (pair[0], pair[1]);
        if seg_start >= seg_end {
            continue;
        }
        let abs = seg_start + start;
        let fg = hl
            .iter()
            .find(|s| (s.start as usize) <= abs && abs < s.end as usize)
            .and_then(|s| {
                s.fg.map(|rgb| ratatui::style::Color::Rgb(rgb.0, rgb.1, rgb.2))
            });
        let in_word = word_ranges
            .iter()
            .any(|r| (r.start as usize) <= abs && abs < r.end as usize);
        let in_search = search_ranges
            .iter()
            .any(|r| (r.start as usize) <= abs && abs < r.end as usize);
        let bg = if in_search {
            theme.search_bg
        } else if in_word {
            emphasis_bg
        } else {
            base_bg
        };
        let mut style = Style::default().bg(bg);
        if let Some(fg) = fg {
            style = style.fg(fg);
        } else {
            style = style.fg(theme.fg);
        }
        let raw = &window[seg_start..seg_end];
        let clean = sanitize_line(raw);
        let expanded = wrap::expand_tabs(&clean, &mut column);
        out.push(Span::styled(expanded, style));
    }
    out
}

// ---------- status bar ----------

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme();
    let style = Style::default().bg(theme.status_bg).fg(theme.status_fg);
    let counts = app.review().counts();

    let mut left = format!(
        " {} rev {}  \u{25cb}{} \u{21ba}{} \u{25d0}{} \u{25cf}{}",
        clean(&app.review().review),
        app.review().revision,
        counts.open,
        counts.reopened,
        counts.addressed,
        counts.resolved
    );
    if app.read_only() {
        left.push_str("  [read-only]");
    }
    if app.filter() != ambidiff_core::projection::FileFilter::All {
        left.push_str(&format!("  filter:{}", app.filter().label()));
    }
    let mut toggles = String::new();
    toggles.push_str(match app.mode() {
        ViewMode::Unified => " unified",
        ViewMode::Split => " split",
    });
    if app.word_diff() {
        toggles.push_str(" word");
    }
    if app.wrap() {
        toggles.push_str(" wrap");
    }

    let message = app
        .status_msg()
        .filter(|(_, at)| at.elapsed() < super::app::STATUS_TTL)
        .map(|(m, _)| format!("  {}", clean(m)))
        .unwrap_or_default();

    let right = format!("{toggles}  ? help ");
    let pad = (area.width as usize)
        .saturating_sub(left.len() + message.len() + right.len())
        .max(1);
    let line = Line::from(vec![
        Span::styled(left, style),
        Span::styled(message, style.fg(theme.warn_fg)),
        Span::styled(" ".repeat(pad), style),
        Span::styled(right, style.fg(theme.dim)),
    ]);
    frame.render_widget(Paragraph::new(line).style(style), area);
}

fn draw_search_bar(frame: &mut Frame, app: &App, area: Rect, query: &str) {
    let theme = app.theme();
    let count = app.search().map(|s| s.matches.len()).unwrap_or(0);
    let line = Line::from(vec![
        Span::styled(
            format!(" /{}", clean(query)),
            Style::default().bg(theme.status_bg).fg(theme.fg),
        ),
        Span::styled(
            format!("  ({count} matches)  enter:go esc:cancel"),
            Style::default().bg(theme.status_bg).fg(theme.dim),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme.status_bg)),
        area,
    );
}

// ---------- overlays ----------

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme();
    let popup = centered(area, 74, area.height.saturating_sub(4).min(38));
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" ambidiff help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.comment_border))
        .style(Style::default().bg(theme.overlay_bg).fg(theme.fg));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines = Vec::new();
    for spec in ambidiff_core::commands::COMMANDS {
        if spec.tui.is_empty() {
            continue;
        }
        let chords = spec.tui.join(" / ");
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {chords:>12}  "),
                Style::default().fg(theme.hunk_fg),
            ),
            Span::styled(format!("{:<22}", spec.name), Style::default().fg(theme.fg)),
            Span::styled(spec.desc.to_string(), Style::default().fg(theme.dim)),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        " esc or ? closes help \u{b7} wrap: comment cards always, code rows unified only",
        Style::default().fg(theme.dim),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_editor(frame: &mut Frame, app: &App, area: Rect) {
    let Overlay::Editor(state) = app.overlay() else {
        return;
    };
    let theme = app.theme();
    let extra = if state.last_error.is_some() { 1 } else { 0 };
    let height = (state.editor.lines().len() as u16 + 4 + extra).clamp(5, area.height / 2);
    let popup = centered(area, 72, height);
    frame.render_widget(Clear, popup);
    let hint = if state.editor.is_single_line() {
        " enter saves \u{b7} esc cancels "
    } else {
        " ctrl-s saves \u{b7} esc cancels "
    };
    let title = clean(&state.title);
    let bottom = match &state.last_error {
        Some(err) => format!(" {} ", clean(err)),
        None => hint.to_string(),
    };
    let border_color = if state.last_error.is_some() {
        theme.remove_sign
    } else {
        theme.comment_border
    };
    let block = Block::default()
        .title(format!(" {title} "))
        .title_bottom(bottom)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme.overlay_bg).fg(theme.fg));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let (cur_row, cur_col) = state.editor.cursor();
    let visible = inner.height as usize;
    let scroll = cur_row.saturating_sub(visible.saturating_sub(1));
    let lines: Vec<Line> = state
        .editor
        .lines()
        .iter()
        .skip(scroll)
        .take(visible)
        .map(|l| Line::from(l.clone()))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);

    let line = &state.editor.lines()[cur_row];
    let col: usize = line
        .chars()
        .take(cur_col)
        .map(|c| c.width().unwrap_or(0))
        .sum();
    frame.set_cursor_position((
        inner.x + (col as u16).min(inner.width.saturating_sub(1)),
        inner.y + (cur_row - scroll) as u16,
    ));
}

fn draw_confirm(frame: &mut Frame, app: &App, area: Rect, id: &str) {
    let theme = app.theme();
    let popup = centered(area, 50, 5);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" delete comment ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.remove_sign))
        .style(Style::default().bg(theme.overlay_bg).fg(theme.fg));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let lines = vec![
        Line::from(format!("delete {} permanently?", clean(id))),
        Line::from(Span::styled(
            "y confirms \u{b7} esc cancels (prefer resolve)",
            Style::default().fg(theme.dim),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn inputs(show_tree: bool, mode: ViewMode, width: u16) -> (LayoutInputs, Rect) {
        (
            LayoutInputs {
                show_tree,
                line_numbers: true,
                mode,
                tree_cursor: 0,
                max_line_number: 5,
            },
            Rect::new(0, 0, width, 24),
        )
    }

    #[test]
    fn layout_hides_tree_at_54_shows_at_55() {
        let (inp, area) = inputs(true, ViewMode::Unified, 54);
        assert!(layout(&inp, area).tree.is_none());
        let (inp, area) = inputs(true, ViewMode::Unified, 55);
        assert!(layout(&inp, area).tree.is_some());
    }

    #[test]
    fn layout_gutter_widths() {
        assert_eq!(gutter_width_for(true, 5), 9); // 1 digit * 2 + 3
        assert_eq!(gutter_width_for(true, 1000), 4 * 2 + 3);
        assert_eq!(gutter_width_for(false, 1000), 2);
    }

    #[test]
    fn layout_split_separator_column() {
        let (inp, area) = inputs(false, ViewMode::Split, 80);
        let l = layout(&inp, area);
        assert!(l.split_sep_col.is_some());
        let (inp, area) = inputs(false, ViewMode::Unified, 80);
        let l = layout(&inp, area);
        assert!(l.split_sep_col.is_none());
    }

    #[test]
    fn hit_maps_scrolled_tree_and_split_cells() {
        let (inp, area) = inputs(true, ViewMode::Split, 100);
        let mut inp = inp;
        inp.tree_cursor = 20;
        let l = layout(&inp, area);
        match l.hit(2, 5) {
            Hit::Tree(row) => assert!(row > 0, "scrolled tree row must account for the offset"),
            other => panic!("expected a tree hit, got {other:?}"),
        }
        let sep = l.split_sep_col.expect("split mode has a separator");
        match l.hit(sep - 1, 0) {
            Hit::Diff { cell, .. } => assert_eq!(cell, ActiveCell::Left),
            other => panic!("expected a diff hit, got {other:?}"),
        }
        match l.hit(sep + 1, 0) {
            Hit::Diff { cell, .. } => assert_eq!(cell, ActiveCell::Right),
            other => panic!("expected a diff hit, got {other:?}"),
        }
    }

    #[test]
    fn draw_paints_the_overview_banner_on_a_test_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ambidiff_core::store::Store::new(dir.path());
        let review = ambidiff_core::review::ReviewFile::new(
            "r".into(),
            ambidiff_core::review::Source::git(None),
            "t",
        );
        store.init(&review).expect("init");
        let mut app = super::super::app::App::open(store, false, false).expect("open");

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw(f, &mut app)).expect("draw");
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            content.contains("review r"),
            "banner should show the review name; got: {content}"
        );
    }

    /// The one assertion nothing else can make: every body row of a wrapped
    /// card starts with the card border and the closing row starts with the
    /// foot glyph - catching a continuation row that lost its left rule.
    #[test]
    fn wrapped_card_rows_keep_the_left_border_unbroken() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ambidiff_core::store::Store::new(dir.path());
        let mut review = ambidiff_core::review::ReviewFile::new(
            "r".into(),
            ambidiff_core::review::Source::git(None),
            "t",
        );
        review.comments.push(ambidiff_core::review::Comment {
            id: "c-1".into(),
            rev: 1,
            status: ambidiff_core::review::Status::Open,
            path: None,
            side: None,
            line: None,
            end_line: None,
            snippet: None,
            body: "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi \
                omicron pi rho sigma tau"
                .into(),
            response: None,
            author: "t".into(),
            created_at: "t".into(),
            updated_at: "t".into(),
            extra: Default::default(),
        });
        store.init(&review).expect("init");
        let mut app = super::super::app::App::open(store, false, false).expect("open");
        app.toggle_wrap();

        let backend = TestBackend::new(50, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw(f, &mut app)).expect("draw");

        let buf = terminal.backend().buffer();
        let width = buf.area.width;
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();

        let body_rows: Vec<&String> = rows
            .iter()
            .filter(|r| r.trim_start_matches(' ').starts_with('\u{2502}'))
            .collect();
        assert!(
            !body_rows.is_empty(),
            "expected at least one wrapped card row; rows: {rows:?}"
        );
        for row in &body_rows {
            assert!(
                row.starts_with("  \u{2502} "),
                "wrapped card row lost its border: {row:?}"
            );
        }

        let foot_row = rows.iter().find(|r| r.trim_start().starts_with('\u{2514}'));
        assert!(
            foot_row.is_some_and(|r| r.starts_with("  \u{2514}\u{2500}")),
            "card foot must keep its border: {foot_row:?}"
        );
    }
}
