//! TUI end-to-end through a real PTY (expectrl): journey-scoped, few, and
//! tolerant of timing. Asserts against contiguous painted strings (single
//! render spans), never against escape-sequence layout.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use expectrl::Session;

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
    assert!(out.status.success(), "git {args:?}");
}

fn write(root: &Path, path: &str, content: &str) {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(full, content).expect("write");
}

fn scratch_repo() -> (tempfile::TempDir, PathBuf) {
    let (dir, root) = repo_with(&[(
        "src/app.ts",
        "const one = 1;\nconst two = 2;\nconst three = 3;\nconst four = 4;\n",
    )]);
    write(
        &root,
        "src/app.ts",
        "const one = 1;\nconst two = 2000;\nconst three = 3;\nconst four = 4;\n",
    );
    (dir, root)
}

/// A repo whose file is long enough for a 10+ row cursor jump: every line is
/// completely rewritten (no line is byte-identical on both sides), so git's
/// diff shows one hunk with no gaps: a block of 15 old-side removes followed
/// by a block of 15 new-side adds, in order -- a fully predictable row
/// layout for the count-prefix and goto-line journey.
fn tall_scratch_repo() -> (tempfile::TempDir, PathBuf) {
    let original: String = (1..=15).map(|i| format!("const l{i} = {i};\n")).collect();
    let modified: String = (1..=15).map(|i| format!("const m{i} = {i}00;\n")).collect();
    let (dir, root) = repo_with(&[("src/app.ts", &original)]);
    write(&root, "src/app.ts", &modified);
    (dir, root)
}

fn spawn_tui(root: &Path) -> Session {
    spawn_tui_with(root, &[])
}

/// Spawn the TUI with extra `tui` subcommand args (e.g. `--split`).
fn spawn_tui_with(root: &Path, args: &[&str]) -> Session {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ambidiff"));
    cmd.arg("tui")
        .args(args)
        .current_dir(root)
        .env("TERM", "xterm-256color")
        .env("AMBIDIFF_AUTHOR", "pty");
    let mut session = Session::spawn(cmd).expect("spawn tui in pty");
    session.set_expect_timeout(Some(Duration::from_secs(15)));
    session
}

/// A scratch git repo with `files` committed as the base, ready for `init`
/// with `--base HEAD`.
fn repo_with(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    for (path, content) in files {
        write(&root, path, content);
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "base"]);
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args(["init", "--review", "pty-test", "--base", "HEAD"])
        .current_dir(&root)
        .output()
        .expect("init");
    assert!(out.status.success());
    (dir, root)
}

/// Resize the PTY's window; the app's own `Resize` event handling then
/// recomputes layout on the next frame.
fn resize(session: &mut Session, cols: u16, rows: u16) {
    session
        .get_process_mut()
        .set_window_size(cols, rows)
        .expect("resize pty");
    std::thread::sleep(Duration::from_millis(150));
}

/// Send an SGR mouse click (press + release) at 0-based terminal
/// coordinates, matching the coordinate space `ratatui::layout::Rect` and
/// `crossterm::event::MouseEvent` both use.
fn click(session: &mut Session, col: u16, row: u16) {
    let press = format!("\x1b[<0;{};{}M", col + 1, row + 1);
    let release = format!("\x1b[<0;{};{}m", col + 1, row + 1);
    send(session, &press);
    send(session, &release);
}

/// Overwrite `.ambidiff.json` with a JSON value, simulating an external
/// agent or CLI edit landing on disk while the TUI is running. Retries the
/// read: the store writes atomically (temp + rename) but a read landing in
/// the middle of that swap can transiently fail or race, and this is an
/// external writer with no lock to coordinate through.
fn rewrite_review(root: &Path, mutate: impl FnOnce(&mut serde_json::Value)) {
    let path = root.join(".ambidiff.json");
    let mut parsed = None;
    retry_until(
        || {
            let Ok(content) = std::fs::read_to_string(&path) else {
                return false;
            };
            let Ok(value) = serde_json::from_str(&content) else {
                return false;
            };
            parsed = Some(value);
            true
        },
        Duration::from_secs(5),
    );
    let mut value: serde_json::Value = parsed.expect("review json read");
    mutate(&mut value);
    std::fs::write(
        &path,
        serde_json::to_string(&value).expect("serialize review"),
    )
    .expect("rewrite review");
}

/// Retry `f` until it returns `true` or the deadline passes; used for
/// filesystem races (a lock sidecar briefly held by another process).
fn retry_until(mut f: impl FnMut() -> bool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(f(), "condition never became true within {timeout:?}");
}

/// Wait for the spawned TUI to exit after `q`, bounded. Keeps draining the
/// PTY's output while waiting: a full kernel pty buffer would otherwise
/// block the child's next `write` (a repaint) forever, so it would never
/// get back to polling stdin to notice `q` -- an apparent hang that is
/// actually just a full pipe, not an application bug.
fn wait_for_exit(session: &mut Session) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = [0u8; 65536];
    loop {
        if !session.is_alive().unwrap_or(false) {
            return;
        }
        match session.try_read(&mut buf) {
            Ok(0) | Err(_) => std::thread::sleep(Duration::from_millis(20)),
            Ok(_) => {}
        }
        assert!(
            Instant::now() < deadline,
            "session never exited after q within 10s"
        );
    }
}

/// Strip ANSI escape sequences and whitespace so assertions survive
/// ratatui's cell-diffed output (unchanged cells are skipped with cursor
/// moves, splitting words across escape sequences).
fn normalize(stream: &str) -> String {
    let mut out = String::with_capacity(stream.len());
    let bytes = stream.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i < bytes.len() && (bytes[i] == b'[' || bytes[i] == b']') {
                i += 1;
                while i < bytes.len() && !bytes[i].is_ascii_alphabetic() && bytes[i] != 0x07 {
                    i += 1;
                }
                i += 1;
            }
            continue;
        }
        let c = stream[i..].chars().next().unwrap_or(' ');
        if !c.is_whitespace() && !c.is_control() {
            out.push(c);
        }
        i += c.len_utf8();
    }
    out
}

/// Read from the PTY until `needle` (ignoring layout) shows up in the
/// accumulated stream.
fn wait_for(session: &mut Session, needle: &str, ctx: &str) {
    let want = normalize(needle);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut seen = String::new();
    let mut buf = [0u8; 8192];
    while Instant::now() < deadline {
        match session.try_read(&mut buf) {
            Ok(0) => std::thread::sleep(Duration::from_millis(50)),
            Ok(n) => {
                seen.push_str(&String::from_utf8_lossy(&buf[..n]));
                if normalize(&seen).contains(&want) {
                    return;
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    std::fs::write("/tmp/ambidiff-pty-stream.txt", &seen).ok();
    panic!(
        "{ctx}: never saw {needle:?} (full stream at /tmp/ambidiff-pty-stream.txt); tail: {:?}",
        seen.chars().rev().take(200).collect::<String>()
    );
}

/// Like `wait_for`, but succeeds once ALL of `needles` have appeared
/// (order-independent) in the accumulated stream. Two assertions that can
/// legitimately land in the very same repaint (e.g. a status flash and the
/// banner change it accompanies) must be checked against one accumulated
/// buffer, since a plain sequence of `wait_for` calls would let the first
/// call consume-and-discard bytes the second call needed.
fn wait_for_all(session: &mut Session, needles: &[&str], ctx: &str) {
    let wants: Vec<String> = needles.iter().map(|n| normalize(n)).collect();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut seen = String::new();
    let mut buf = [0u8; 8192];
    while Instant::now() < deadline {
        match session.try_read(&mut buf) {
            Ok(0) => std::thread::sleep(Duration::from_millis(50)),
            Ok(n) => {
                seen.push_str(&String::from_utf8_lossy(&buf[..n]));
                let normalized = normalize(&seen);
                if wants.iter().all(|w| normalized.contains(w)) {
                    return;
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    std::fs::write("/tmp/ambidiff-pty-stream.txt", &seen).ok();
    panic!(
        "{ctx}: never saw all of {needles:?} (full stream at /tmp/ambidiff-pty-stream.txt); tail: {:?}",
        seen.chars().rev().take(200).collect::<String>()
    );
}

fn send(session: &mut Session, text: &str) {
    // Drain whatever output is already sitting in the pty first: a repaint
    // the app is mid-`write`ing blocks on a full kernel pty buffer, and
    // while blocked it cannot get back to reading stdin -- so several
    // sends in a row with nothing draining between them can silently
    // coalesce into one processing burst with only the LAST repaint ever
    // reaching the terminal (and, worse, a mouse hit-test in that burst
    // would read a stale `last_layout` from before an intervening key
    // changed it). Draining here keeps each send's effects visible on
    // their own.
    drain(session);
    session.write_all(text.as_bytes()).expect("send keys");
    session.flush().expect("flush");
    std::thread::sleep(Duration::from_millis(150));
}

/// Best-effort non-blocking drain of whatever the pty has buffered.
fn drain(session: &mut Session) {
    let mut buf = [0u8; 65536];
    loop {
        match session.try_read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

#[test]
fn open_navigate_comment_quit_persists() {
    let (_dir, root) = scratch_repo();
    let mut session = spawn_tui(&root);
    wait_for(&mut session, "src/app.ts", "initial render");

    // Cursor starts on the banner; rows: banner, hunk, ctx, del, add...
    send(&mut session, "jjj"); // land on the removed line (old side)
    send(&mut session, "c"); // open the comment editor
    wait_for(&mut session, "comment on src/app.ts", "editor overlay");
    send(&mut session, "why the jump to 2000??");
    send(&mut session, "\x13"); // ctrl-s saves
    wait_for(
        &mut session,
        "why the jump to 2000??",
        "comment card rendered",
    );
    send(&mut session, "q");
    wait_for_exit(&mut session);

    let content = std::fs::read_to_string(root.join(".ambidiff.json")).expect("review file");
    let value: serde_json::Value = serde_json::from_str(&content).expect("json");
    let comments = value["comments"].as_array().expect("comments");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["body"], "why the jump to 2000??");
    assert_eq!(comments[0]["path"], "src/app.ts");
    assert_eq!(comments[0]["status"], "open");
    // Cursor was on the removed line: side rule anchors old-side.
    assert_eq!(comments[0]["side"], "old");
    assert_eq!(comments[0]["line"], 2);
    assert_eq!(comments[0]["snippet"], "const two = 2;");
}

/// `D` on a live comment still confirms; `D` on a resolved one deletes at
/// once. This is the rigorous form of "no dialog appeared": if a
/// confirmation had been required for the second `D`, one keypress alone
/// could not have deleted anything, so the review file is the assertion
/// throughout -- including for the resolve step, since ratatui's
/// cell-diffed repaints can skip rewriting a cell whose column position
/// happens to keep the same byte across frames, splitting a short status
/// word across escape sequences in the raw PTY stream (`tui_pty.rs:521`).
#[test]
fn delete_skips_confirmation_once_resolved() {
    let (_dir, root) = scratch_repo();
    let mut session = spawn_tui(&root);
    wait_for(&mut session, "src/app.ts", "initial render");

    send(&mut session, "jjj"); // land on the removed line (old side)
    send(&mut session, "c");
    wait_for(&mut session, "comment on src/app.ts", "editor overlay");
    send(&mut session, "still live");
    send(&mut session, "\x13"); // ctrl-s saves
    wait_for(&mut session, "still live", "comment card rendered");

    send(&mut session, "."); // jump onto the comment card
    send(&mut session, "D");
    wait_for(
        &mut session,
        "delete comment",
        "confirm dialog appears for an open comment",
    );
    send(&mut session, "n"); // cancel

    let content = std::fs::read_to_string(root.join(".ambidiff.json")).expect("review file");
    let value: serde_json::Value = serde_json::from_str(&content).expect("json");
    assert_eq!(
        value["comments"].as_array().expect("comments").len(),
        1,
        "cancelling the dialog must not delete the comment"
    );

    send(&mut session, "x"); // resolve
    retry_until(
        || {
            let Ok(content) = std::fs::read_to_string(root.join(".ambidiff.json")) else {
                return false;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
                return false;
            };
            value["comments"][0]["status"] == "resolved"
        },
        Duration::from_secs(10),
    );

    send(&mut session, "D"); // a single D: no dialog for a resolved comment
    retry_until(
        || {
            let Ok(content) = std::fs::read_to_string(root.join(".ambidiff.json")) else {
                return false;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
                return false;
            };
            value["comments"]
                .as_array()
                .is_some_and(std::vec::Vec::is_empty)
        },
        Duration::from_secs(10),
    );

    send(&mut session, "q");
    wait_for_exit(&mut session);
}

/// A watch-driven diff reload must keep the cursor on the same logical
/// line. Observable: the comment editor's title names the cursor line, so
/// opening it before and after the reload must show the same target.
#[test]
fn watch_reload_preserves_cursor_line() {
    let (_dir, root) = scratch_repo();
    let mut session = spawn_tui(&root);
    wait_for(&mut session, "src/app.ts", "initial render");

    // banner, hunk, ctx(1), del(2), add(2): land on the add row (line 2).
    send(&mut session, "jjjj");
    send(&mut session, "c");
    wait_for(
        &mut session,
        "comment on src/app.ts:2",
        "editor names line 2",
    );
    send(&mut session, "\x1b"); // esc closes the editor

    // External edit far from the cursor: appending a line changes the diff.
    let content = std::fs::read_to_string(root.join("src/app.ts")).expect("read");
    std::fs::write(
        root.join("src/app.ts"),
        format!("{content}const five = 5;\n"),
    )
    .expect("write");
    wait_for(&mut session, "diff refreshed", "watch reload");

    // Same logical line under the cursor after the reload.
    send(&mut session, "c");
    wait_for(&mut session, "comment on src/app.ts:2", "cursor preserved");
    send(&mut session, "\x1b");
    send(&mut session, "q");
    wait_for_exit(&mut session);
}

/// Vim-style count prefixes (`10j`) and the `:<num>` goto-line motion,
/// TUI-only. Observable: the comment editor's title names the cursor line,
/// exactly as `watch_reload_preserves_cursor_line` does.
///
/// Rows in `tall_scratch_repo`'s single hunk: banner(0), hunk header(1),
/// 15 removes old1..old15 (rows 2..=16), 15 adds new1..new15 (rows 17..=31).
#[test]
fn count_prefix_and_goto_line_navigate() {
    let (_dir, root) = tall_scratch_repo();
    let mut session = spawn_tui(&root);
    wait_for(&mut session, "src/app.ts", "initial render");

    // "10j" from the banner (row 0) lands on row 10: remove #9 (old side).
    send(&mut session, "10j");
    send(&mut session, "c");
    wait_for(
        &mut session,
        "comment on src/app.ts:9",
        "count-prefixed cursorDown",
    );
    send(&mut session, "\x1b"); // esc closes the editor

    // `:7` addresses new-side line 7 directly (an add row).
    send(&mut session, ":");
    send(&mut session, "7");
    send(&mut session, "\r");
    send(&mut session, "c");
    wait_for(&mut session, "comment on src/app.ts:7", "goto-line");
    send(&mut session, "\x1b");
    send(&mut session, "q");
    wait_for_exit(&mut session);
}

/// The cross-frontend sync journey: an external CLI write to .ambidiff.json
/// while the TUI runs must converge on screen without any keypress.
#[test]
fn external_review_edit_converges_live() {
    let (_dir, root) = scratch_repo();
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args([
            "comment",
            "add",
            "-p",
            "src/app.ts",
            "-l",
            "2",
            "-m",
            "seed comment",
        ])
        .current_dir(&root)
        .env("AMBIDIFF_AUTHOR", "henry")
        .output()
        .expect("seed");
    assert!(out.status.success());
    let listed = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args(["comment", "list", "--todo", "--json"])
        .current_dir(&root)
        .output()
        .expect("list");
    let value: serde_json::Value = serde_json::from_slice(&listed.stdout).expect("json");
    let id = value["comments"][0]["id"].as_str().expect("id").to_string();

    let mut session = spawn_tui(&root);
    wait_for(&mut session, "seed comment", "seed comment rendered");

    // The agent addresses it from outside while the TUI stays untouched.
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args(["comment", "addressed", &id, "-m", "tuned the constant"])
        .current_dir(&root)
        .output()
        .expect("addressed");
    assert!(out.status.success());

    wait_for(
        &mut session,
        "tuned the constant",
        "response converged on screen",
    );
    send(&mut session, "q");
    wait_for_exit(&mut session);
}

/// B11: a diff refresh that resorts the changed-file list (a new file lands
/// alphabetically earlier) must never silently move the selection to
/// whatever is now first.
#[test]
fn earlier_sorting_file_added_keeps_selection() {
    let (_dir, root) = repo_with(&[("src/m.ts", "const m = 1;\n")]);
    write(&root, "src/m.ts", "const m = 2;\n");
    let mut session = spawn_tui(&root);
    wait_for(&mut session, "src/m.ts", "initial render on the only file");

    write(&root, "src/a.ts", "brand new file\n");
    wait_for(
        &mut session,
        "diff refreshed",
        "watch reload after new file",
    );

    send(&mut session, "jj"); // land on the removed line
    send(&mut session, "c");
    wait_for(
        &mut session,
        "comment on src/m.ts",
        "selection stays on src/m.ts, not the now-earlier src/a.ts",
    );
    send(&mut session, "\x1b");
    send(&mut session, "q");
    wait_for_exit(&mut session);
}

/// B11/B12: a file that drops out of the changed set entirely falls back to
/// the overview with a notice naming it, never a silent switch to whatever
/// file happens to occupy its old slot.
#[test]
fn vanished_file_falls_back_to_overview_with_notice() {
    let (_dir, root) = repo_with(&[("src/only.ts", "const a = 1;\n")]);
    write(&root, "src/only.ts", "const a = 2;\n");
    let mut session = spawn_tui(&root);
    wait_for(&mut session, "src/only.ts", "initial render");

    // Revert to match the base commit exactly: the file drops out of the
    // changed set entirely (no rename to follow).
    write(&root, "src/only.ts", "const a = 1;\n");
    wait_for_all(
        &mut session,
        &["src/only.ts is no longer in the diff", "review pty-test"],
        "vanished notice and overview fallback land in the same repaint",
    );
    send(&mut session, "q");
    wait_for_exit(&mut session);
}

/// B19: a failed save (the review turned read-only underneath the draft)
/// keeps the draft open with the error surfaced, and a retry after the
/// review becomes writable again succeeds with the SAME draft text.
#[test]
fn save_failure_keeps_draft() {
    let (_dir, root) = scratch_repo();
    let mut session = spawn_tui(&root);
    wait_for(&mut session, "src/app.ts", "initial render");

    send(&mut session, "jjj");
    send(&mut session, "c");
    wait_for(&mut session, "comment on src/app.ts", "editor overlay");
    send(&mut session, "will this survive a bad rewrite?");

    rewrite_review(&root, |v| v["ambidiff"] = serde_json::json!(99));
    send(&mut session, "\x13"); // ctrl-s: this save must fail
    wait_for(
        &mut session,
        "newer ambidiff",
        "save failure surfaces in the still-open draft",
    );

    // Restore a supported schema and retry: the same draft now saves. Wait
    // on the rendered comment body, not the "comment added" flash: a short
    // status message is exactly the kind of text ratatui's cell-diffing can
    // partially skip repainting when it overlaps a prior frame's message.
    rewrite_review(&root, |v| v["ambidiff"] = serde_json::json!(1));
    send(&mut session, "\x13");
    wait_for(
        &mut session,
        "will this survive a bad rewrite?",
        "retry succeeds and the draft's own text renders as a saved comment",
    );
    send(&mut session, "q");
    wait_for_exit(&mut session);

    let content = std::fs::read_to_string(root.join(".ambidiff.json")).expect("review file");
    let value: serde_json::Value = serde_json::from_str(&content).expect("json");
    let comments = value["comments"].as_array().expect("comments");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["body"], "will this survive a bad rewrite?");
}

/// B18: clicking the LEFT half of a split-mode paired row (the removed
/// line) must anchor a new comment to the old side, never the new side,
/// even though the row also carries a new-side cell.
#[test]
fn split_left_cell_comment_is_old_side() {
    let (_dir, root) = scratch_repo();
    let mut session = spawn_tui_with(&root, &["--split"]);
    resize(&mut session, 80, 24);
    wait_for(&mut session, "src/app.ts", "initial render");

    // Row 3 (banner, hunk header, context, removed) at col 45: left of the
    // separator, which split_geometry places at column 56 for this layout.
    click(&mut session, 45, 3);
    send(&mut session, "c");
    wait_for(
        &mut session,
        "comment on src/app.ts",
        "editor opens for the clicked cell",
    );
    send(&mut session, "left cell click");
    send(&mut session, "\x13");
    wait_for(
        &mut session,
        "left cell click",
        "saved comment body renders",
    );

    // Cards wrap in split mode too, not just unified. A resize forces a
    // full repaint (ratatui only redraws changed cells otherwise, and this
    // short card's own text may not move at all once wrapped), so the
    // card reappearing here proves wrap didn't break rendering in split
    // mode rather than merely that unchanged cells were left alone.
    send(&mut session, "W");
    resize(&mut session, 79, 24);
    wait_for(
        &mut session,
        "left cell click",
        "card still renders once wrapped in split mode",
    );

    send(&mut session, "q");
    wait_for_exit(&mut session);

    let content = std::fs::read_to_string(root.join(".ambidiff.json")).expect("review file");
    let value: serde_json::Value = serde_json::from_str(&content).expect("json");
    let comments = value["comments"].as_array().expect("comments");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["side"], "old");
    assert_eq!(comments[0]["line"], 2);
}

/// B25: a tree click must be translated through the tree's own scroll
/// offset, not the raw row under the cursor, so a click after scrolling
/// still lands on the file it visually points at.
#[test]
fn scrolled_tree_click_selects_correct_row() {
    let files: Vec<(String, String)> = (0..30)
        .map(|i| (format!("f{i:02}.ts"), format!("const v = {i};\n")))
        .collect();
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let (_dir, root) = repo_with(&file_refs);
    for (path, _) in &files {
        let full = root.join(path);
        let content = std::fs::read_to_string(&full).expect("read");
        std::fs::write(&full, format!("{content}// touched\n")).expect("modify");
    }
    let mut session = spawn_tui(&root);
    resize(&mut session, 80, 24);
    wait_for(&mut session, "f00.ts", "initial render lists the tree");

    send(&mut session, "\t"); // focus the tree
    send(&mut session, "G"); // scroll to the bottom
    // The click's hit-test reads the LAST PAINTED layout, so it must be
    // certain a repaint reflecting the scrolled tree has actually reached
    // the terminal before clicking -- otherwise the click can land inside
    // the same input burst as `G` and be hit-tested against the
    // pre-scroll layout. f29 only becomes visible once that repaint has
    // happened.
    wait_for(
        &mut session,
        "f29.ts",
        "tree repaints scrolled to the bottom",
    );
    click(&mut session, 5, 5);
    // The click opens the file with the cursor on its banner row; move onto
    // the one changed line before commenting.
    send(&mut session, "jj");
    send(&mut session, "c");
    wait_for(
        &mut session,
        "comment on f12.ts",
        "scrolled click resolves the canonical (unscrolled) file index",
    );
    send(&mut session, "\x1b");
    send(&mut session, "q");
    wait_for_exit(&mut session);
}

/// B25: resizing the terminal must reflow already-wrapped rows to the new
/// width rather than leaving them chunked for the old one.
#[test]
fn resize_reflows_wrapped_rows() {
    // A naive byte-position wrap can split any single fixed marker exactly
    // on a chunk boundary for a given width; repeating a short marker
    // throughout the line means at least one occurrence survives intact
    // regardless of where either width's chunk boundaries happen to fall.
    let marker_run: String = std::iter::repeat_n("xxxxxxxxxxxxxxxxxxxxxxxxxxMARK", 10).collect();
    let long_line = format!("const s = \"{marker_run}\";\n");
    let (_dir, root) = repo_with(&[("src/long.ts", "const s = \"short\";\n")]);
    write(&root, "src/long.ts", &long_line);
    let mut session = spawn_tui(&root);
    resize(&mut session, 100, 24);
    wait_for(&mut session, "src/long.ts", "initial render");

    send(&mut session, "W"); // toggle wrap
    wait_for(&mut session, "MARK", "wrap reveals a marker at 100 cols");

    // Drain the still-buffered width-100 frame first: otherwise a leftover
    // (pre-resize) MARK occurrence could satisfy the next wait_for before
    // the reflowed, narrower repaint ever arrives.
    drain(&mut session);
    resize(&mut session, 40, 24);
    wait_for(
        &mut session,
        "MARK",
        "reflow keeps a marker reachable after a narrower resize",
    );
    send(&mut session, "q");
    wait_for_exit(&mut session);
}

/// Wrap applies to comment cards too: a long card body must reflow to the
/// new pane width on resize, the same way a long code line does.
#[test]
fn wrap_reflows_comment_cards_on_resize() {
    let (_dir, root) = scratch_repo();
    let words: String = (1..=40).map(|i| format!("w{i:02} ")).collect();
    let body = format!("{words}ZULUTAIL");
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args(["comment", "add", "-p", "src/app.ts", "-m", &body])
        .current_dir(&root)
        .output()
        .expect("comment add");
    assert!(out.status.success(), "comment add failed: {out:?}");

    let mut session = spawn_tui(&root);
    resize(&mut session, 100, 24);
    wait_for(&mut session, "src/app.ts", "initial render");

    send(&mut session, "W"); // toggle wrap
    wait_for(
        &mut session,
        "ZULUTAIL",
        "wrapped card reveals its tail word at 100 cols",
    );

    // Drain the still-buffered width-100 frame first, same reasoning as
    // resize_reflows_wrapped_rows: otherwise a leftover occurrence could
    // satisfy the next wait_for before the reflowed repaint arrives.
    drain(&mut session);
    resize(&mut session, 60, 24);
    wait_for(
        &mut session,
        "ZULUTAIL",
        "card reflows and keeps its tail word reachable after a narrower resize",
    );
    send(&mut session, "q");
    wait_for_exit(&mut session);
}

/// B25: below the tree's visibility threshold, the tree is hidden and a
/// click that would have hit a tree row must be read as a diff-pane click
/// instead, never a tree selection.
#[test]
fn narrow_layout_hides_tree_and_ignores_tree_clicks() {
    let (_dir, root) = repo_with(&[
        ("src/app.ts", "const one = 1;\n"),
        ("src/zzz.ts", "const z = 1;\n"),
    ]);
    write(&root, "src/app.ts", "const one = 11;\n");
    write(&root, "src/zzz.ts", "const z = 2;\n");
    let mut session = spawn_tui(&root);
    resize(&mut session, 50, 24); // narrower than the 55-col tree threshold
    wait_for(
        &mut session,
        "src/app.ts",
        "initial render on the first sorted file",
    );

    // At this width the tree is hidden; a click at coordinates that would
    // have hit the "src/zzz.ts" tree row must be read as a diff click.
    click(&mut session, 2, 2);
    send(&mut session, "c");
    wait_for(
        &mut session,
        "comment on src/app.ts",
        "narrow layout ignores the tree click",
    );
    send(&mut session, "\x1b");
    send(&mut session, "q");
    wait_for_exit(&mut session);
}

/// B12: a review edit that changes the diff `source` (not just a comment)
/// must reconfigure the comparison and bump the generation, so the TUI
/// reloads the view against the NEW comparison rather than continuing to
/// show the stale one.
#[test]
fn source_change_reconfigures_listing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    // Each commit's content is a wholly distinct word, not a one-character
    // variant of the others: ratatui's cell-diffing repaints only the
    // cells that actually changed, so a one-digit edit ("one = 1" -> "one
    // = 2") would retransmit just that single character, never the
    // surrounding text, and a `wait_for` for the whole line would then
    // never see it as one contiguous run.
    write(&root, "src/app.ts", "FIRST_MARKER_ONE\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "c1"]);
    write(&root, "src/app.ts", "SECOND_MARKER_TWO\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "c2"]);
    write(&root, "src/app.ts", "THIRD_MARKER_THREE\n");
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args(["init", "--review", "pty-test", "--base", "HEAD~1"])
        .current_dir(&root)
        .output()
        .expect("init");
    assert!(out.status.success());

    let mut session = spawn_tui(&root);
    wait_for(
        &mut session,
        "FIRST_MARKER_ONE",
        "initial diff compares against HEAD~1",
    );

    rewrite_review(&root, |v| v["source"]["base"] = serde_json::json!("HEAD"));
    wait_for(
        &mut session,
        "SECOND_MARKER_TWO",
        "source change reconfigures the comparison",
    );
    send(&mut session, "q");
    wait_for_exit(&mut session);
}

#[test]
fn initial_source_failure_is_not_an_empty_review() {
    let (_dir, root) = scratch_repo();
    rewrite_review(&root, |v| {
        v["source"]["base"] = serde_json::json!("no-such-ref")
    });
    let mut session = spawn_tui(&root);
    wait_for_all(
        &mut session,
        &[
            "source unavailable",
            "[source error]",
            "no-such-ref",
            "press r to retry",
        ],
        "a broken source is diagnosed in the tree, the status bar and the overview, with a way out",
    );
    send(&mut session, "q");
    wait_for_exit(&mut session);
}

#[test]
fn clean_tree_shows_no_changes() {
    let (_dir, root) = repo_with(&[("src/app.ts", "const one = 1;\n")]);
    let mut session = spawn_tui(&root);
    wait_for(&mut session, "no changes", "a clean tree is explained");
    send(&mut session, "q");
    wait_for_exit(&mut session);
}

/// Add a review-level comment via the real CLI, returning its id.
fn cli_add_comment(root: &Path, body: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args(["comment", "add", "-m", body, "--json"])
        .current_dir(root)
        .output()
        .expect("cli comment add");
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    value["id"].as_str().expect("id").to_string()
}

/// Address a comment via the real CLI.
fn cli_address_comment(root: &Path, id: &str, response: &str) {
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args(["comment", "addressed", id, "-m", response, "--json"])
        .current_dir(root)
        .output()
        .expect("cli comment addressed");
    assert!(out.status.success());
}

fn review_comment_status(root: &Path, id: &str) -> Option<String> {
    let content = std::fs::read_to_string(root.join(".ambidiff.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value["comments"]
        .as_array()?
        .iter()
        .find(|c| c["id"] == id)?["status"]
        .as_str()
        .map(str::to_string)
}

/// `X` resolves every addressed comment in one action, leaving untouched
/// comments alone: the first PTY test to drive the confirm overlay at all.
#[test]
fn resolve_addressed_confirms_and_flips_only_addressed_comments() {
    let (_dir, root) = scratch_repo();
    let addressed_id = cli_add_comment(&root, "fix the jump to 2000");
    let open_id = cli_add_comment(&root, "still need to look at this");
    cli_address_comment(&root, &addressed_id, "done, see the fix");

    let mut session = spawn_tui(&root);
    wait_for(&mut session, "src/app.ts", "initial render");

    send(&mut session, "X");
    wait_for(
        &mut session,
        "resolve 1 addressed comments?",
        "confirm dialog shows the addressed count",
    );
    send(&mut session, "y");
    wait_for_all(
        &mut session,
        &["resolved 1 addressed comments"],
        "flash confirms the batch resolve",
    );

    retry_until(
        || review_comment_status(&root, &addressed_id).as_deref() == Some("resolved"),
        Duration::from_secs(10),
    );
    assert_eq!(
        review_comment_status(&root, &open_id).as_deref(),
        Some("open"),
        "untouched comment stays open"
    );
    let content = std::fs::read_to_string(root.join(".ambidiff.json")).expect("review file");
    let value: serde_json::Value = serde_json::from_str(&content).expect("json");
    assert_eq!(
        value["revision"], 1,
        "resolving addressed comments does not bump revision"
    );

    send(&mut session, "q");
    wait_for_exit(&mut session);
}

/// `Esc` on the confirm dialog leaves the review file byte-for-byte
/// unchanged.
#[test]
fn resolve_addressed_esc_cancels_without_writing() {
    let (_dir, root) = scratch_repo();
    let addressed_id = cli_add_comment(&root, "fix the jump to 2000");
    cli_address_comment(&root, &addressed_id, "done, see the fix");

    let mut session = spawn_tui(&root);
    wait_for(&mut session, "src/app.ts", "initial render");
    let before = std::fs::read_to_string(root.join(".ambidiff.json")).expect("review file before");

    send(&mut session, "X");
    wait_for(
        &mut session,
        "resolve 1 addressed comments?",
        "confirm dialog shows the addressed count",
    );
    send(&mut session, "\x1b"); // Esc cancels

    let after = std::fs::read_to_string(root.join(".ambidiff.json")).expect("review file after");
    assert_eq!(
        before, after,
        "cancelling the dialog must not write anything"
    );

    send(&mut session, "q");
    wait_for_exit(&mut session);
}

/// `X` with nothing addressed flashes and never opens the confirm dialog.
#[test]
fn resolve_addressed_with_nothing_addressed_flashes_without_confirming() {
    let (_dir, root) = scratch_repo();
    let mut session = spawn_tui(&root);
    wait_for(&mut session, "src/app.ts", "initial render");

    send(&mut session, "X");
    wait_for_all(
        &mut session,
        &["no addressed comments to resolve"],
        "flash instead of a confirm dialog",
    );

    send(&mut session, "q");
    wait_for_exit(&mut session);
}

// ---------------------------------------------------------------------
// Stack targets and single-commit review
// ---------------------------------------------------------------------

/// `main` (base) with `src/a.ts` = BASE_ALPHA; PR `auth-1` rewrites it to
/// AUTH_ONE_BRAVO; PR `auth-2` adds `src/b.ts` = AUTH_TWO_CHARLIE. Every
/// marker is a wholly distinct word so ratatui's cell-diffing repaints it
/// as one contiguous run. Returns without a review file.
fn stack_repo_without_review() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    write(&root, "src/a.ts", "BASE_ALPHA\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "base"]);
    git(&root, &["checkout", "-q", "-b", "auth-1"]);
    write(&root, "src/a.ts", "AUTH_ONE_BRAVO\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "add bravo"]);
    git(&root, &["checkout", "-q", "-b", "auth-2"]);
    write(&root, "src/b.ts", "AUTH_TWO_CHARLIE\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "add charlie"]);
    (dir, root)
}

fn stack_repo() -> (tempfile::TempDir, PathBuf) {
    let (dir, root) = stack_repo_without_review();
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args([
            "init",
            "--review",
            "pty-stack",
            "--stack",
            "--upstream",
            "main",
        ])
        .current_dir(&root)
        .output()
        .expect("init");
    assert!(out.status.success());
    (dir, root)
}

fn review_json(root: &Path) -> serde_json::Value {
    let content = std::fs::read_to_string(root.join(".ambidiff.json")).expect("review file");
    serde_json::from_str(&content).expect("json")
}

#[test]
fn strip_lists_the_stack_and_target_keys_switch_the_diff() {
    let (_dir, root) = stack_repo();
    let mut session = spawn_tui(&root);
    // The topmost PR opens selected; the strip names both PRs.
    wait_for_all(
        &mut session,
        &[
            "1 auth-1",
            "[2 auth-2]",
            "AUTH_TWO_CHARLIE",
            "target:auth-2",
        ],
        "initial stack render",
    );
    send(&mut session, "(");
    wait_for_all(
        &mut session,
        &["[1 auth-1]", "AUTH_ONE_BRAVO", "target:auth-1"],
        "( selects the PR below",
    );
    send(&mut session, "(");
    wait_for(&mut session, "bottom of stack", "( at the bottom flashes");
    send(&mut session, ")");
    wait_for_all(
        &mut session,
        &["[2 auth-2]", "AUTH_TWO_CHARLIE"],
        ") selects the PR above",
    );

    // The picker: k moves to auth-1, enter selects it.
    send(&mut session, "p");
    wait_for_all(
        &mut session,
        &["targets (trunk main)", "add bravo", "add charlie"],
        "picker lists subjects",
    );
    send(&mut session, "k");
    send(&mut session, "\r");
    wait_for_all(
        &mut session,
        &["[1 auth-1]", "AUTH_ONE_BRAVO"],
        "picker selection applied",
    );

    // A comment saved while looking at PR 1 is tagged with it.
    send(&mut session, "jjj"); // banner, hunk, removed, added
    send(&mut session, "c");
    wait_for(
        &mut session,
        "comment on src/a.ts:1 [auth-1]",
        "editor names the target",
    );
    send(&mut session, "bravo??");
    send(&mut session, "\x13");
    wait_for(&mut session, "bravo??", "comment card rendered");
    send(&mut session, "q");
    wait_for_exit(&mut session);

    let review = review_json(&root);
    let comments = review["comments"].as_array().expect("comments");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["path"], "src/a.ts");
    assert_eq!(
        comments[0]["target"],
        serde_json::json!({"kind": "branch", "name": "auth-1"})
    );
    assert_eq!(comments[0]["snippet"], "AUTH_ONE_BRAVO");
}

#[test]
fn clicking_the_strip_selects_that_target() {
    let (_dir, root) = stack_repo();
    let mut session = spawn_tui(&root);
    resize(&mut session, 100, 30);
    wait_for_all(
        &mut session,
        &["1 auth-1", "AUTH_TWO_CHARLIE"],
        "initial render",
    );
    // The strip is the first row; " 1 auth-1 " spans its first ten cells.
    click(&mut session, 3, 0);
    wait_for_all(
        &mut session,
        &["[1 auth-1]", "AUTH_ONE_BRAVO", "target:auth-1"],
        "strip click selects auth-1",
    );
    send(&mut session, "q");
    wait_for_exit(&mut session);
}

#[test]
fn an_external_restack_keeps_the_comment_on_its_branch() {
    let (_dir, root) = stack_repo();
    let out = Command::new(env!("CARGO_BIN_EXE_ambidiff"))
        .args([
            "comment",
            "add",
            "-p",
            "src/a.ts",
            "-l",
            "1",
            "-m",
            "SEEDED_NOTE",
            "--target",
            "auth-1",
            "--json",
        ])
        .current_dir(&root)
        .env("AMBIDIFF_AUTHOR", "pty")
        .output()
        .expect("seed comment");
    assert!(out.status.success());

    let mut session = spawn_tui(&root);
    wait_for(&mut session, "AUTH_TWO_CHARLIE", "initial render");
    send(&mut session, "(");
    wait_for_all(
        &mut session,
        &["AUTH_ONE_BRAVO", "SEEDED_NOTE"],
        "comment on auth-1",
    );

    // Fold a fix into the bottom PR from the top and restack; every tip
    // moves but the branch names stay.
    write(&root, "src/a.ts", "AUTH_ONE_BRAVO\nAUTH_ONE_DELTA\n");
    git(&root, &["commit", "-qa", "--fixup", "auth-1"]);
    let out = Command::new("git")
        .args([
            "rebase",
            "-q",
            "-i",
            "--autosquash",
            "--update-refs",
            "main",
        ])
        .current_dir(&root)
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("rebase");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Only the new line is repainted (ratatui skips unchanged cells), so
    // force fresh paints to prove the rest: the picker lists auth-1 with
    // its one comment, and landing the cursor on the card re-sends it.
    wait_for(
        &mut session,
        "AUTH_ONE_DELTA",
        "the watch picks up the restack",
    );
    send(&mut session, "p");
    wait_for_all(
        &mut session,
        &[
            "targets (trunk main)",
            "1 auth-1",
            "\u{25cb}1/1",
            "add bravo",
        ],
        "auth-1 keeps its comment after the restack",
    );
    send(&mut session, "\x1b");
    send(&mut session, "G");
    send(&mut session, "kk");
    wait_for(
        &mut session,
        "SEEDED_NOTE",
        "the comment card is still on auth-1",
    );
    send(&mut session, "q");
    wait_for_exit(&mut session);
    let review = review_json(&root);
    assert_eq!(review["comments"][0]["target"]["name"], "auth-1");
}

#[test]
fn a_commit_review_opens_without_a_review_file_and_the_first_comment_creates_it() {
    let (_dir, root) = stack_repo_without_review();
    assert!(!root.join(".ambidiff.json").exists());
    let mut session = spawn_tui_with(&root, &["--commit", "auth-1"]);
    // Wide enough that the overview banner is not clipped by the tree pane.
    resize(&mut session, 120, 30);
    wait_for_all(
        &mut session,
        &["AUTH_ONE_BRAVO", "[unsaved]", "commit:"],
        "commit review opens unsaved",
    );
    // No strip: the diff banner is the first row.
    send(&mut session, "{"); // overview
    wait_for(
        &mut session,
        "add bravo",
        "overview banner names the commit",
    );
    send(&mut session, "}"); // back to the file
    wait_for(&mut session, "AUTH_ONE_BRAVO", "file again");
    send(&mut session, "jjj");
    send(&mut session, "c");
    wait_for(
        &mut session,
        "comment on src/a.ts:1",
        "editor without a target suffix",
    );
    send(&mut session, "first note");
    send(&mut session, "\x13");
    wait_for(&mut session, "first note", "comment card rendered");
    send(&mut session, "q");
    wait_for_exit(&mut session);

    let review = review_json(&root);
    assert_eq!(
        review["source"],
        serde_json::json!({"kind": "git", "commit": "auth-1"})
    );
    assert_eq!(review["comments"][0]["body"], "first note");
    assert!(review["comments"][0].get("target").is_none());
    let exclude = std::fs::read_to_string(root.join(".git/info/exclude")).expect("exclude");
    assert!(exclude.contains(".ambidiff.json"), "{exclude}");
}
