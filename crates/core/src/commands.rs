//! The command table: one semantics table, three keymaps.
//!
//! Every user action has a stable id; each frontend maps its own default
//! chords onto the same ids (hunk's indirection). The TUI dispatches from
//! this table, the engine serves it to the nvim plugin, and the web painter
//! embeds it, so help overlays and rebinding stay consistent everywhere.

use serde::Serialize;

/// One action in the shared command table.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSpec {
    /// Stable id, e.g. `ambidiff.nav.nextHunk`.
    pub id: &'static str,
    pub name: &'static str,
    pub desc: &'static str,
    /// Default chords per frontend.
    pub tui: &'static [&'static str],
    pub nvim: &'static [&'static str],
    pub web: &'static [&'static str],
}

macro_rules! cmd {
    ($id:literal, $name:literal, $desc:literal, tui: $tui:expr, nvim: $nvim:expr, web: $web:expr) => {
        CommandSpec {
            id: $id,
            name: $name,
            desc: $desc,
            tui: $tui,
            nvim: $nvim,
            web: $web,
        }
    };
}

/// The full command table, grouped by area.
pub const COMMANDS: &[CommandSpec] = &[
    // Navigation
    cmd!("ambidiff.nav.cursorDown", "Cursor down", "Move the cursor one row down",
        tui: &["j", "Down"], nvim: &["j"], web: &["j", "ArrowDown"]),
    cmd!("ambidiff.nav.cursorUp", "Cursor up", "Move the cursor one row up",
        tui: &["k", "Up"], nvim: &["k"], web: &["k", "ArrowUp"]),
    cmd!("ambidiff.nav.pageDown", "Page down", "Scroll one page down",
        tui: &["C-d", "PageDown"], nvim: &["<C-d>"], web: &["PageDown"]),
    cmd!("ambidiff.nav.pageUp", "Page up", "Scroll one page up",
        tui: &["C-u", "PageUp"], nvim: &["<C-u>"], web: &["PageUp"]),
    cmd!("ambidiff.nav.top", "Go to top", "Jump to the first row",
        tui: &["g"], nvim: &["gg"], web: &["g"]),
    cmd!("ambidiff.nav.bottom", "Go to bottom", "Jump to the last row",
        tui: &["G"], nvim: &["G"], web: &["G"]),
    cmd!("ambidiff.nav.nextHunk", "Next hunk", "Jump to the next hunk",
        tui: &["]"], nvim: &["]h"], web: &["]"]),
    cmd!("ambidiff.nav.prevHunk", "Previous hunk", "Jump to the previous hunk",
        tui: &["["], nvim: &["[h"], web: &["["]),
    cmd!("ambidiff.nav.nextFile", "Next file", "Open the next changed file",
        tui: &["}"], nvim: &["]f"], web: &["}"]),
    cmd!("ambidiff.nav.prevFile", "Previous file", "Open the previous changed file",
        tui: &["{"], nvim: &["[f"], web: &["{"]),
    cmd!("ambidiff.nav.nextComment", "Next comment", "Jump to the next comment",
        tui: &["."], nvim: &["]c"], web: &["."]),
    cmd!("ambidiff.nav.prevComment", "Previous comment", "Jump to the previous comment",
        tui: &[","], nvim: &["[c"], web: &[","]),
    cmd!("ambidiff.nav.focusSwitch", "Switch focus", "Switch focus between tree and diff",
        tui: &["Tab"], nvim: &[], web: &["Tab"]),
    // View toggles
    cmd!("ambidiff.view.toggleLayout", "Toggle layout", "Switch between unified and side-by-side",
        tui: &["s"], nvim: &["gs"], web: &["s"]),
    cmd!("ambidiff.view.toggleWordDiff", "Toggle word diff", "Toggle intra-line word highlighting",
        tui: &["w"], nvim: &["gw"], web: &["w"]),
    cmd!("ambidiff.view.toggleTree", "Toggle file tree", "Show or hide the file tree",
        tui: &["t"], nvim: &[], web: &["t"]),
    cmd!("ambidiff.view.toggleWrap", "Toggle wrap", "Toggle long-line wrapping",
        tui: &["W"], nvim: &[], web: &["W"]),
    cmd!("ambidiff.view.toggleLineNumbers", "Toggle line numbers", "Show or hide line numbers",
        tui: &["L"], nvim: &[], web: &["L"]),
    cmd!("ambidiff.view.toggleTheme", "Toggle theme", "Switch between light and dark themes",
        tui: &["T"], nvim: &[], web: &["T"]),
    cmd!("ambidiff.view.cycleFilter", "Cycle file filter", "Cycle all / annotated / unreviewed files",
        tui: &["f"], nvim: &[], web: &["f"]),
    cmd!("ambidiff.view.expand", "Expand gap", "Expand the collapsed lines under the cursor",
        tui: &["Enter"], nvim: &["za"], web: &["Enter"]),
    cmd!("ambidiff.view.refresh", "Refresh", "Re-read the diff and review file now",
        tui: &["r"], nvim: &["gr"], web: &["r"]),
    // Review actions
    cmd!("ambidiff.review.comment", "Comment on line", "Add a comment on the cursor line",
        tui: &["c"], nvim: &["gc"], web: &["c"]),
    cmd!("ambidiff.review.commentFile", "Comment on file", "Add a file-level comment",
        tui: &["F"], nvim: &["gF"], web: &["F"]),
    cmd!("ambidiff.review.commentReview", "Comment on review", "Add a review-level comment",
        tui: &["R"], nvim: &["gR"], web: &["R"]),
    cmd!("ambidiff.review.address", "Mark addressed", "Mark the comment at the cursor addressed",
        tui: &["a"], nvim: &["ga"], web: &["a"]),
    cmd!("ambidiff.review.resolve", "Resolve", "Resolve the comment at the cursor (human only)",
        tui: &["x"], nvim: &["gx"], web: &["x"]),
    cmd!("ambidiff.review.reopen", "Reopen", "Reopen the comment at the cursor (human only)",
        tui: &["o"], nvim: &["go"], web: &["o"]),
    cmd!("ambidiff.review.editComment", "Edit comment", "Edit the comment body at the cursor",
        tui: &["e"], nvim: &["ge"], web: &["e"]),
    cmd!("ambidiff.review.deleteComment", "Delete comment", "Delete the comment at the cursor",
        tui: &["D"], nvim: &["gD"], web: &["D"]),
    // Search
    cmd!("ambidiff.search.start", "Search", "Search within the diff",
        tui: &["/"], nvim: &["/"], web: &["/"]),
    cmd!("ambidiff.search.next", "Next match", "Jump to the next search match",
        tui: &["n"], nvim: &["n"], web: &["n"]),
    cmd!("ambidiff.search.prev", "Previous match", "Jump to the previous search match",
        tui: &["N"], nvim: &["N"], web: &["N"]),
    // Meta
    cmd!("ambidiff.app.help", "Help", "Show the help overlay",
        tui: &["?"], nvim: &["g?"], web: &["?"]),
    cmd!("ambidiff.app.quit", "Quit", "Quit ambidiff",
        tui: &["q"], nvim: &[], web: &[]),
];

/// Look up a command by id.
pub fn find(id: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|c| c.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_and_namespaced() {
        let mut seen = HashSet::new();
        for c in COMMANDS {
            assert!(c.id.starts_with("ambidiff."), "{}", c.id);
            assert!(seen.insert(c.id), "duplicate id {}", c.id);
        }
    }

    #[test]
    fn tui_chords_do_not_collide() {
        let mut seen = HashSet::new();
        for c in COMMANDS {
            for chord in c.tui {
                assert!(
                    seen.insert(*chord),
                    "tui chord {chord} bound twice ({})",
                    c.id
                );
            }
        }
    }

    #[test]
    fn nvim_chords_do_not_collide() {
        let mut seen = HashSet::new();
        for c in COMMANDS {
            for chord in c.nvim {
                assert!(
                    seen.insert(*chord),
                    "nvim chord {chord} bound twice ({})",
                    c.id
                );
            }
        }
    }

    #[test]
    fn find_resolves_known_ids() {
        assert!(find("ambidiff.nav.nextHunk").is_some());
        assert!(find("ambidiff.nope").is_none());
    }
}
