//! Terminal UI: ratatui event loop dispatching through the shared command
//! table, painting the core's row model.

mod app;
mod editor;
mod render;
mod theme;

use std::time::Duration;

use ambidiff_core::review::Action;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;

use crate::args::TuiArgs;
use crate::context::resolve_store;
use app::{App, DRow, EditorIntent, FileTarget, Focus, Overlay};
use render::Hit;

pub fn run(args: TuiArgs) -> Result<i32> {
    let store = resolve_store()?;
    let mut app = App::open(store, args.split, args.light)?;

    let mut terminal = ratatui::try_init()?;
    // Restore the terminal even if the loop panics.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        event_loop(&mut terminal, &mut app)
    }));
    ratatui::restore();
    match result {
        Ok(r) => r.map(|_| 0),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    crossterm::execute!(std::io::stdout(), event::EnableMouseCapture)?;
    let result = (|| -> Result<()> {
        // Repaint only when something changed: an input event, a watch
        // refresh, a resize, or an expiring status message. An idle viewer
        // must not burn CPU or flood the terminal with redundant frames.
        let mut dirty = true;
        loop {
            if dirty {
                terminal.draw(|frame| render::draw(frame, app))?;
                dirty = false;
            }
            if app.quit() {
                return Ok(());
            }
            // Poll input with a short timeout so watch refreshes surface.
            if event::poll(Duration::from_millis(120))? {
                loop {
                    match event::read()? {
                        Event::Key(key) if key.kind != event::KeyEventKind::Release => {
                            handle_key(app, key);
                            dirty = true;
                        }
                        Event::Mouse(mouse) => {
                            let size = terminal.size()?;
                            handle_mouse(app, mouse, Rect::new(0, 0, size.width, size.height));
                            dirty = true;
                        }
                        Event::Resize(..) => dirty = true,
                        _ => {}
                    }
                    // Drain queued events before repainting.
                    if !event::poll(Duration::from_millis(0))? {
                        break;
                    }
                }
            }
            dirty |= app.poll_watch();
            // A status message that just aged out changes the status bar.
            dirty |= app.take_expired_status();
        }
    })();
    let _ = crossterm::execute!(std::io::stdout(), event::DisableMouseCapture);
    result
}

/// Translate a key event to a command-table chord string.
fn chord_of(key: &KeyEvent) -> Option<String> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    Some(match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                format!("C-{c}")
            } else {
                c.to_string()
            }
        }
        KeyCode::Down => "Down".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        _ => return None,
    })
}

/// Resolve a chord to a command id via the shared table.
fn command_for_chord(chord: &str) -> Option<&'static str> {
    ambidiff_core::commands::COMMANDS
        .iter()
        .find(|c| c.tui.contains(&chord))
        .map(|c| c.id)
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // Overlays capture input first.
    match app.overlay() {
        Overlay::Help => {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
            ) {
                app.discard_editor();
            }
            return;
        }
        Overlay::ConfirmDelete { id } => {
            let id = id.clone();
            match key.code {
                KeyCode::Char('y') => {
                    app.discard_editor();
                    app.delete_comment(&id);
                }
                KeyCode::Esc | KeyCode::Char('n') => app.discard_editor(),
                _ => {}
            }
            return;
        }
        Overlay::Editor(_) => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match (key.code, ctrl) {
                (KeyCode::Esc, _) => app.discard_editor(),
                (KeyCode::Char('s'), true) => app.save_editor(),
                (KeyCode::Enter, _) => {
                    let single = matches!(app.overlay(), Overlay::Editor(state) if state.editor.is_single_line());
                    if single {
                        app.save_editor();
                    } else {
                        editor_mut(app, |e| e.newline());
                        app.clear_editor_error();
                    }
                }
                (KeyCode::Backspace, _) => {
                    editor_mut(app, |e| e.backspace());
                    app.clear_editor_error();
                }
                (KeyCode::Delete, _) => {
                    editor_mut(app, |e| e.delete());
                    app.clear_editor_error();
                }
                (KeyCode::Left, _) => editor_mut(app, |e| e.left()),
                (KeyCode::Right, _) => editor_mut(app, |e| e.right()),
                (KeyCode::Up, _) => editor_mut(app, |e| e.up()),
                (KeyCode::Down, _) => editor_mut(app, |e| e.down()),
                (KeyCode::Home, _) => editor_mut(app, |e| e.home()),
                (KeyCode::End, _) => editor_mut(app, |e| e.end()),
                (KeyCode::Char(c), false) => {
                    editor_mut(app, |e| e.insert_char(c));
                    app.clear_editor_error();
                }
                _ => {}
            }
            return;
        }
        Overlay::None => {}
    }

    // Search typing mode.
    if let Some(search) = app.search() {
        if search.typing {
            match key.code {
                KeyCode::Esc => {
                    app.cancel_search();
                    app.rebuild_display_preserving_cursor();
                }
                KeyCode::Enter => {
                    app.stop_search_typing();
                    let has = app.search().is_some_and(|s| !s.matches.is_empty());
                    if has {
                        let current = app.search().map(|s| s.current).unwrap_or(0);
                        app.goto_match(current);
                    } else {
                        app.flash("no matches");
                    }
                }
                KeyCode::Backspace => {
                    if let Some(s) = app.search_typing() {
                        s.query.pop();
                    }
                    app.refresh_search_matches();
                }
                KeyCode::Char(c) => {
                    if let Some(s) = app.search_typing() {
                        s.query.push(c);
                    }
                    app.refresh_search_matches();
                }
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Esc {
            app.cancel_search();
            return;
        }
    } else if key.code == KeyCode::Esc {
        return;
    }

    let Some(chord) = chord_of(&key) else { return };

    // Tree focus gets its own basic motion first.
    if app.focus() == Focus::Tree {
        match chord.as_str() {
            "j" | "Down" => {
                app.tree_move(1);
                return;
            }
            "k" | "Up" => {
                app.tree_move(-1);
                return;
            }
            "Enter" | "l" => {
                app.tree_select();
                return;
            }
            "g" => {
                app.tree_top();
                return;
            }
            "G" => {
                app.tree_bottom();
                return;
            }
            _ => {}
        }
    }

    let Some(command) = command_for_chord(&chord) else {
        return;
    };
    run_command(app, command);
}

/// Mutate the active editor's text buffer, if the overlay is an editor.
fn editor_mut(app: &mut App, f: impl FnOnce(&mut editor::Editor)) {
    if let Overlay::Editor(state) = app.overlay_mut() {
        f(&mut state.editor);
    }
}

fn run_command(app: &mut App, command: &str) {
    match command {
        "ambidiff.nav.cursorDown" => app.move_cursor(1),
        "ambidiff.nav.cursorUp" => app.move_cursor(-1),
        "ambidiff.nav.pageDown" => app.move_cursor(20),
        "ambidiff.nav.pageUp" => app.move_cursor(-20),
        "ambidiff.nav.top" => app.cursor_to(0),
        "ambidiff.nav.bottom" => app.cursor_to(usize::MAX),
        "ambidiff.nav.nextHunk" => app.jump_hunk(true),
        "ambidiff.nav.prevHunk" => app.jump_hunk(false),
        "ambidiff.nav.nextFile" => app.next_file(true),
        "ambidiff.nav.prevFile" => app.next_file(false),
        "ambidiff.nav.nextComment" => {
            app.jump_next(|r| matches!(r, DRow::CommentHead { .. }), true)
        }
        "ambidiff.nav.prevComment" => {
            app.jump_next(|r| matches!(r, DRow::CommentHead { .. }), false)
        }
        "ambidiff.nav.focusSwitch" => {
            app.set_focus(match app.focus() {
                Focus::Tree => Focus::Diff,
                Focus::Diff => Focus::Tree,
            });
        }
        "ambidiff.view.toggleLayout" => app.toggle_mode(),
        "ambidiff.view.toggleWordDiff" => app.toggle_word_diff(),
        "ambidiff.view.toggleTree" => app.toggle_tree(),
        "ambidiff.view.toggleWrap" => app.toggle_wrap(),
        "ambidiff.view.toggleLineNumbers" => app.toggle_line_numbers(),
        "ambidiff.view.toggleTheme" => app.toggle_theme(),
        "ambidiff.view.cycleFilter" => app.cycle_filter(),
        "ambidiff.view.expand" => app.expand_gap_at_cursor(),
        "ambidiff.view.refresh" => app.manual_refresh(),
        "ambidiff.review.comment" => {
            if let Some((path, side, line)) = app.line_target_at_cursor() {
                app.open_comment_editor(EditorIntent::NewComment {
                    path: Some(path),
                    side: Some(side),
                    line: Some(line),
                });
            } else {
                app.flash("cursor is not on a diff line (F for file, R for review)");
            }
        }
        "ambidiff.review.commentFile" => match app.target().clone() {
            FileTarget::File(path) => {
                app.open_comment_editor(EditorIntent::NewComment {
                    path: Some(path),
                    side: None,
                    line: None,
                });
            }
            FileTarget::Overview => app.flash("open a file first"),
        },
        "ambidiff.review.commentReview" => {
            app.open_comment_editor(EditorIntent::NewComment {
                path: None,
                side: None,
                line: None,
            });
        }
        "ambidiff.review.address" => {
            if let Some(idx) = app.comment_at_cursor() {
                let id = app.review().comments[idx].id.clone();
                app.open_comment_editor(EditorIntent::AddressResponse { id });
            } else {
                app.flash("cursor is not on a comment");
            }
        }
        "ambidiff.review.resolve" => {
            if let Some(idx) = app.comment_at_cursor() {
                let id = app.review().comments[idx].id.clone();
                app.apply_lifecycle(&id, Action::Resolve, None);
            } else {
                app.flash("cursor is not on a comment");
            }
        }
        "ambidiff.review.reopen" => {
            if let Some(idx) = app.comment_at_cursor() {
                let id = app.review().comments[idx].id.clone();
                app.apply_lifecycle(&id, Action::Reopen, None);
            } else {
                app.flash("cursor is not on a comment");
            }
        }
        "ambidiff.review.editComment" => {
            if let Some(idx) = app.comment_at_cursor() {
                let id = app.review().comments[idx].id.clone();
                app.open_comment_editor(EditorIntent::EditBody { id });
            } else {
                app.flash("cursor is not on a comment");
            }
        }
        "ambidiff.review.deleteComment" => {
            if let Some(idx) = app.comment_at_cursor() {
                let id = app.review().comments[idx].id.clone();
                app.open_confirm_delete(id);
            } else {
                app.flash("cursor is not on a comment");
            }
        }
        "ambidiff.search.start" => app.start_search(),
        "ambidiff.search.next" => app.search_step(true),
        "ambidiff.search.prev" => app.search_step(false),
        "ambidiff.app.help" => app.open_help(),
        "ambidiff.app.quit" => app.set_quit(),
        _ => {}
    }
}

/// Mouse input is hit-tested against the layout for the CURRENT state and
/// terminal size (a pure computation), not the one cached at the last
/// repaint: several events can arrive in one burst before a repaint, and a
/// key among them may have moved the tree scroll or hidden the tree.
fn handle_mouse(app: &mut App, mouse: event::MouseEvent, area: Rect) {
    match mouse.kind {
        MouseEventKind::ScrollDown => app.move_cursor(3),
        MouseEventKind::ScrollUp => app.move_cursor(-3),
        MouseEventKind::Down(MouseButton::Left) => {
            let layout = render::current_layout(app, area);
            let hit = layout.hit(mouse.column, mouse.row);
            app.apply_layout(layout);
            match hit {
                Hit::Tree(row) => {
                    app.set_focus(Focus::Tree);
                    if row < app.tree_len() {
                        app.cursor_to_tree(row);
                        app.tree_select();
                    }
                }
                Hit::Diff { offset, cell } => {
                    app.set_focus(Focus::Diff);
                    let target = app.scroll() + offset;
                    if target < app.display().len() {
                        app.cursor_to(target);
                        app.set_active_cell(cell);
                    }
                }
                Hit::None => {}
            }
        }
        _ => {}
    }
}
