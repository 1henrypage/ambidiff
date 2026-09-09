//! CLI end-to-end: the full agent round-trip in a scratch git repo, driven
//! through the spawned binary exactly as a human and an agent would run it.
//!
//! Journey: init -> human comments (line/file/review level) -> agent lists
//! and addresses with responses -> rename-through-review -> human reopens
//! and resolves -> pass bookkeeping -> agent-setup idempotency. File state
//! is asserted at every step.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ambidiff_core::anchor::{Placement, place_comments};
use ambidiff_core::git_source::GitSource;
use ambidiff_core::review::parse_review;
use ambidiff_core::source::DiffSource;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ambidiff")
}

fn run_in(root: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .current_dir(root)
        .env("AMBIDIFF_AUTHOR", "henry")
        .output()
        .expect("spawn ambidiff")
}

fn ok_json(root: &Path, args: &[&str]) -> serde_json::Value {
    let out = run_in(root, args);
    assert!(
        out.status.success(),
        "ambidiff {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "ambidiff {args:?} produced invalid JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

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

fn write(root: &Path, path: &str, content: &str) {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(full, content).expect("write");
}

fn scratch_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    write(
        &root,
        "src/login.ts",
        "import { audit } from \"./audit\";\nimport { db } from \"./db\";\n\nfunction login(user) {\n  if (user == null) return;\n  audit(user);\n  db.session.create(user);\n  db.session.touch(user);\n  return user.token;\n}\n",
    );
    write(&root, "docs/notes.md", "# Notes\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "base"]);
    (dir, root)
}

fn review_file(root: &Path) -> ambidiff_core::review::ReviewFile {
    let content = std::fs::read_to_string(root.join(".ambidiff.json")).expect("review file");
    let outcome = parse_review(&content).expect("parseable");
    assert!(
        outcome.warnings.is_empty(),
        "clean file: {:?}",
        outcome.warnings
    );
    outcome.review
}

#[test]
fn full_agent_round_trip() {
    let (_dir, root) = scratch_repo();
    // The worktree gets edits the reviewer will comment on.
    write(
        &root,
        "src/login.ts",
        "import { audit } from \"./audit\";\nimport { db } from \"./db\";\n\nfunction login(user) {\n  if (user != null) return;\n  audit(user);\n  db.session.create(user);\n  db.session.touch(user);\n  return user.token;\n}\n",
    );

    // init
    let init = ok_json(
        &root,
        &["init", "--review", "auth-fix", "--base", "HEAD", "--json"],
    );
    assert_eq!(init["review"], "auth-fix");
    assert!(root.join(".ambidiff.json").is_file());
    let exclude = std::fs::read_to_string(root.join(".git/info/exclude")).expect("exclude");
    assert!(exclude.contains(".ambidiff.json"));

    // Fresh review: status exits 0.
    let out = run_in(&root, &["status"]);
    assert_eq!(out.status.code(), Some(0));

    // Human comments: line-level (captures snippet), file-level, review-level.
    let line_comment = ok_json(
        &root,
        &[
            "comment",
            "add",
            "-p",
            "src/login.ts",
            "-l",
            "5",
            "-m",
            "null check inverted?? explain",
            "--json",
        ],
    );
    let line_id = line_comment["id"].as_str().expect("id").to_string();
    assert_eq!(line_comment["status"], "open");
    assert_eq!(line_comment["rev"], 1);
    assert_eq!(line_comment["side"], "new");
    assert_eq!(
        line_comment["snippet"], "  if (user != null) return;",
        "snippet captured from the working tree at comment time"
    );
    assert_eq!(line_comment["author"], "henry");

    let file_comment = ok_json(
        &root,
        &[
            "comment",
            "add",
            "-p",
            "docs/notes.md",
            "-m",
            "document the auth flow",
            "--json",
        ],
    );
    let file_id = file_comment["id"].as_str().expect("id").to_string();
    assert_eq!(file_comment["line"], serde_json::Value::Null);

    let review_comment = ok_json(
        &root,
        &[
            "comment",
            "add",
            "-m",
            "overall: split this into two PRs next time",
            "--json",
        ],
    );
    assert_eq!(review_comment["path"], serde_json::Value::Null);
    let review_id = review_comment["id"].as_str().expect("id").to_string();

    // status now exits 10 (actionable comments exist).
    let out = run_in(&root, &["status", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(10),
        "exit contract: 10 = agent has work"
    );
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(status["todo"], 3);
    assert_eq!(status["counts"]["open"], 3);
    assert_eq!(status["revision"], 1);

    // AGENT SIDE: list the to-do, address each with a response.
    let todo = ok_json(&root, &["comment", "list", "--todo", "--json"]);
    let todo_items = todo["comments"].as_array().expect("array");
    assert_eq!(todo_items.len(), 3);

    for item in todo_items {
        let id = item["id"].as_str().expect("id");
        let is_question = item["body"].as_str().expect("body").contains("??");
        let response = if is_question {
            "it was intentional: early-return guards the audit call"
        } else {
            "done"
        };
        let addressed = ok_json(
            &root,
            &["comment", "addressed", id, "-m", response, "--json"],
        );
        assert_eq!(addressed["status"], "addressed");
        assert_eq!(addressed["response"], response);
    }

    // Agent finished: nothing to do, exit 0, pass 1 complete.
    let out = run_in(&root, &["status"]);
    assert_eq!(out.status.code(), Some(0));

    // RENAME THROUGH REVIEW: move and edit the commented file.
    std::fs::create_dir_all(root.join("src/auth")).expect("mkdir");
    git(&root, &["mv", "src/login.ts", "src/auth/login.ts"]);
    write(
        &root,
        "src/auth/login.ts",
        "import { audit } from \"./audit\";\nimport { db } from \"./db\";\n\nfunction login(user) {\n  if (user != null) return;\n  audit(user);\n  trace(user);\n  db.session.create(user);\n  db.session.touch(user);\n  return user.token;\n}\n",
    );

    let review = review_file(&root);
    let source = GitSource::open(&root, Some("HEAD".to_string()), false).expect("open");
    let files = source.listing().expect("changed files").entries;
    let placements = place_comments(&review.comments, &files);
    let line_idx = review
        .comments
        .iter()
        .position(|c| c.id == line_id)
        .expect("line comment present");
    assert_eq!(
        placements[line_idx],
        Placement::File {
            path: "src/auth/login.ts".to_string(),
            was_path: Some("src/login.ts".to_string()),
        },
        "comment follows the rename with a was-badge"
    );

    // Lifecycle by id is untouched by the rename: human reopens the line
    // comment (starts pass 2), the agent re-addresses it.
    let reopened = ok_json(&root, &["comment", "reopen", &line_id, "--json"]);
    assert_eq!(reopened["status"], "reopened");
    assert_eq!(reopened["rev"], 1, "origin pass is provenance, not mutated");
    let review = review_file(&root);
    assert_eq!(
        review.revision, 2,
        "reopen after completed pass starts pass 2"
    );

    let out = run_in(&root, &["status"]);
    assert_eq!(out.status.code(), Some(10));

    ok_json(
        &root,
        &[
            "comment",
            "addressed",
            &line_id,
            "-m",
            "renamed and guarded; added trace",
            "--json",
        ],
    );

    // HUMAN verdicts: resolve everything.
    for id in [&line_id, &file_id, &review_id] {
        let resolved = ok_json(&root, &["comment", "resolve", id, "--json"]);
        assert_eq!(resolved["status"], "resolved");
    }
    let out = run_in(&root, &["status", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(status["counts"]["resolved"], 3);

    // A fresh comment now opens pass 3.
    ok_json(
        &root,
        &["comment", "add", "-m", "one more sweep please", "--json"],
    );
    assert_eq!(review_file(&root).revision, 3);

    // Manual escape hatch.
    let bumped = ok_json(&root, &["rev", "bump", "--json"]);
    assert_eq!(bumped["revision"], 4);
}

#[test]
fn illegal_lifecycle_transitions_fail_at_the_cli() {
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);
    let comment = ok_json(&root, &["comment", "add", "-m", "check this", "--json"]);
    let id = comment["id"].as_str().expect("id");

    // Reopening an open comment is illegal.
    let out = run_in(&root, &["comment", "reopen", id]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("already open"));

    // Addressing twice is illegal.
    ok_json(&root, &["comment", "addressed", id, "--json"]);
    let out = run_in(&root, &["comment", "addressed", id]);
    assert_eq!(out.status.code(), Some(1));

    // Unknown id is a clean error.
    let out = run_in(&root, &["comment", "resolve", "c-nope"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no comment"));
}

#[test]
fn validation_rejects_incoherent_anchors() {
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);

    let out = run_in(
        &root,
        &["comment", "add", "-l", "3", "-m", "line without path"],
    );
    assert_eq!(out.status.code(), Some(1));

    let out = run_in(
        &root,
        &[
            "comment",
            "add",
            "-p",
            "src/login.ts",
            "-l",
            "5",
            "--end-line",
            "3",
            "-m",
            "bad range",
        ],
    );
    assert_eq!(out.status.code(), Some(1));

    let out = run_in(&root, &["comment", "add", "-m", "   "]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn agent_setup_is_idempotent_and_check_aware() {
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);
    write(&root, "CLAUDE.md", "# Existing instructions\n");

    // Check before setup: reports work needed via exit 1.
    let out = run_in(&root, &["agent-setup", "--check"]);
    assert_eq!(out.status.code(), Some(1));

    let out = run_in(&root, &["agent-setup"]);
    assert_eq!(out.status.code(), Some(0));
    let agents = std::fs::read_to_string(root.join("AGENTS.md")).expect("AGENTS.md");
    assert!(agents.contains("BEGIN ambidiff"));
    assert!(agents.contains("NEVER run `ambidiff comment resolve`"));
    let claude = std::fs::read_to_string(root.join("CLAUDE.md")).expect("CLAUDE.md");
    assert!(claude.starts_with("# Existing instructions"));
    assert!(claude.contains("BEGIN ambidiff"));

    // Second run: no duplicate blocks, exit 0 on check.
    let out = run_in(&root, &["agent-setup"]);
    assert_eq!(out.status.code(), Some(0));
    let agents2 = std::fs::read_to_string(root.join("AGENTS.md")).expect("AGENTS.md");
    assert_eq!(agents2.matches("BEGIN ambidiff").count(), 1);
    assert_eq!(agents2, agents);
    let out = run_in(&root, &["agent-setup", "--check"]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn verbs_work_from_subdirectories() {
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);
    let sub = root.join("src");
    let out = run_in(&sub, &["comment", "add", "-m", "from a subdir", "--json"]);
    assert!(out.status.success());
    assert!(root.join(".ambidiff.json").is_file());
    assert!(!sub.join(".ambidiff.json").exists());
}

#[test]
fn init_refuses_double_init_and_verbs_require_init() {
    let (_dir, root) = scratch_repo();
    let out = run_in(&root, &["status"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("ambidiff init"));

    ok_json(&root, &["init", "--json"]);
    let out = run_in(&root, &["init"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("already exists"));
}

#[test]
fn salvage_warnings_surface_but_do_not_block() {
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);
    // An agent writes a malformed record directly into the file.
    let path = root.join(".ambidiff.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    value["comments"] = serde_json::json!([
        {"id": "c-good", "rev": 1, "status": "open", "path": null, "line": null,
         "body": "fine", "author": "a", "createdAt": "t", "updatedAt": "t"},
        {"id": "c-broken", "status": "banana"}
    ]);
    std::fs::write(&path, serde_json::to_string_pretty(&value).expect("json")).expect("write");

    let out = run_in(&root, &["status", "--json"]);
    assert_eq!(out.status.code(), Some(10), "good comment still counts");
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(status["todo"], 1);
    assert_eq!(status["quarantined"], 1);
    assert!(!status["warnings"].as_array().expect("warnings").is_empty());

    // Mutations keep working and preserve the quarantined record.
    ok_json(&root, &["comment", "addressed", "c-good", "--json"]);
    let content = std::fs::read_to_string(&path).expect("read");
    assert!(
        content.contains("c-broken"),
        "quarantine is preserved on write"
    );
}

fn err_of(root: &Path, args: &[&str]) -> (i32, String) {
    let out = run_in(root, args);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn stdout_of(root: &Path, args: &[&str]) -> (i32, Vec<u8>) {
    let out = run_in(root, args);
    (out.status.code().unwrap_or(-1), out.stdout)
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn agent_setup_refuses_a_non_utf8_agents_md_and_leaves_its_bytes_intact() {
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);
    let bytes = b"# agents\n\xff\xfe binary tail\n".to_vec();
    std::fs::write(root.join("AGENTS.md"), &bytes).expect("write");
    let (code, stderr) = err_of(&root, &["agent-setup"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("AGENTS.md"), "names the target: {stderr}");
    assert_eq!(
        std::fs::read(root.join("AGENTS.md")).expect("read"),
        bytes,
        "the file is byte-identical"
    );
    // --check is read-only too and reports the same problem.
    let (code, _) = err_of(&root, &["agent-setup", "--check"]);
    assert_eq!(code, 1);
    assert_eq!(std::fs::read(root.join("AGENTS.md")).expect("read"), bytes);
}

#[cfg(unix)]
#[test]
fn agent_setup_refuses_a_symlinked_claude_md_before_writing_anything() {
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);
    let elsewhere = root.join("docs/real-claude.md");
    write(&root, "docs/real-claude.md", "# elsewhere\n");
    std::os::unix::fs::symlink(&elsewhere, root.join("CLAUDE.md")).expect("symlink");
    let (code, stderr) = err_of(&root, &["agent-setup"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("CLAUDE.md"), "names the target: {stderr}");
    assert!(
        !root.join("AGENTS.md").exists(),
        "preflight fails before AGENTS.md is created"
    );
    assert_eq!(
        std::fs::read_to_string(&elsewhere).expect("read"),
        "# elsewhere\n",
        "the symlink target is untouched"
    );
}

#[test]
fn agent_setup_refuses_malformed_block_delimiters() {
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);
    let content = "# Project\n<!-- BEGIN ambidiff (v0) -->\nhalf a block\n";
    write(&root, "AGENTS.md", content);
    let (code, stderr) = err_of(&root, &["agent-setup"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("AGENTS.md") && stderr.contains("END"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("AGENTS.md")).expect("read"),
        content
    );
}

#[cfg(unix)]
#[test]
fn agent_setup_preserves_mode_and_surrounding_content() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);
    write(
        &root,
        "AGENTS.md",
        "before\n\n<!-- BEGIN ambidiff (v0) -->\nstale\n<!-- END ambidiff -->\n\nafter \u{e9}\n",
    );
    std::fs::set_permissions(
        root.join("AGENTS.md"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("chmod");
    let out = run_in(&root, &["agent-setup"]);
    assert_eq!(out.status.code(), Some(0));
    let after = std::fs::read_to_string(root.join("AGENTS.md")).expect("read");
    assert!(after.starts_with("before\n\n<!-- BEGIN ambidiff (v"));
    assert!(
        after.ends_with("<!-- END ambidiff -->\n\nafter \u{e9}\n"),
        "{after}"
    );
    assert!(!after.contains("stale"));
    assert_eq!(mode_of(&root.join("AGENTS.md")), 0o600, "mode bits survive");
}

#[cfg(unix)]
#[test]
fn init_reports_an_exclude_write_failure_as_a_warning_and_still_succeeds() {
    let (_dir, root) = scratch_repo();
    // A symlinked exclude file is refused (never followed or replaced).
    write(&root, "elsewhere.txt", "keep\n");
    let exclude = root.join(".git/info/exclude");
    std::fs::create_dir_all(exclude.parent().expect("parent")).expect("mkdir");
    let _ = std::fs::remove_file(&exclude);
    std::os::unix::fs::symlink(root.join("elsewhere.txt"), &exclude).expect("symlink");

    let out = run_in(&root, &["init", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(
        value["review"],
        root.file_name().and_then(|n| n.to_str()).expect("name")
    );
    let warnings = value["warnings"].as_array().expect("warnings array");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().is_some_and(|w| w.contains("exclude"))),
        "{value}"
    );
    assert!(root.join(".ambidiff.json").is_file());
    assert_eq!(
        std::fs::read_to_string(root.join("elsewhere.txt")).expect("read"),
        "keep\n",
        "the symlink target was not rewritten"
    );

    // The human form puts the same warning on stderr.
    let (_dir2, root2) = scratch_repo();
    let exclude2 = root2.join(".git/info/exclude");
    std::fs::create_dir_all(exclude2.parent().expect("parent")).expect("mkdir");
    let _ = std::fs::remove_file(&exclude2);
    std::os::unix::fs::symlink(root2.join("nope.txt"), &exclude2).expect("symlink");
    let (code, stderr) = err_of(&root2, &["init"]);
    assert_eq!(code, 0);
    assert!(
        stderr.contains("warning") && stderr.contains("exclude"),
        "{stderr}"
    );
}

#[test]
fn human_output_is_sanitised_while_json_and_disk_keep_the_raw_text() {
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);
    let body = "plain\x1b[31m red \x1b[0m\rover";
    let comment = ok_json(&root, &["comment", "add", "-m", body, "--json"]);
    let id = comment["id"].as_str().expect("id").to_string();
    assert_eq!(comment["body"], body, "JSON is raw");
    let disk = std::fs::read_to_string(root.join(".ambidiff.json")).expect("read");
    assert!(
        disk.contains("\\u001b[31m"),
        "disk keeps the raw body (escaped by JSON)"
    );

    for args in [
        vec!["comment", "show", id.as_str()],
        vec!["comment", "list"],
        vec!["comment", "addressed", id.as_str(), "-m", "done\x1b[2J"],
    ] {
        let (code, stdout) = stdout_of(&root, &args);
        assert_eq!(code, 0, "{args:?}");
        let text = String::from_utf8_lossy(&stdout);
        assert!(
            !text.contains('\x1b') && !text.contains('\r'),
            "{args:?}: {text:?}"
        );
        assert!(text.contains("plain"), "{args:?}: {text:?}");
    }
    // Snippets and paths go through the same boundary.
    write(&root, "src/login.ts", "x\x1b[5mblink\n");
    let added = ok_json(
        &root,
        &[
            "comment",
            "add",
            "-p",
            "src/login.ts",
            "-l",
            "1",
            "-m",
            "b",
            "--json",
        ],
    );
    assert_eq!(added["snippet"], "x\x1b[5mblink");
    let (_, stdout) = stdout_of(
        &root,
        &["comment", "show", added["id"].as_str().expect("id")],
    );
    assert!(!String::from_utf8_lossy(&stdout).contains('\x1b'));
}

#[test]
fn the_error_channel_is_sanitised() {
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);
    let (code, stderr) = err_of(&root, &["comment", "resolve", "c-\x1b[2Jnope"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("no comment"), "{stderr:?}");
    assert!(!stderr.contains('\x1b'), "{stderr:?}");
}

#[test]
fn edit_and_delete_round_trip() {
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);
    let comment = ok_json(&root, &["comment", "add", "-m", "first", "--json"]);
    let id = comment["id"].as_str().expect("id").to_string();
    let edited = ok_json(&root, &["comment", "edit", &id, "-m", "second", "--json"]);
    assert_eq!(edited["body"], "second");
    assert_eq!(edited["id"], id.as_str());
    let (code, _) = err_of(&root, &["comment", "edit", &id, "-m", "   "]);
    assert_eq!(code, 1, "blank body refused");
    let shown = ok_json(&root, &["comment", "show", &id, "--json"]);
    assert_eq!(shown["body"], "second");

    let deleted = ok_json(&root, &["comment", "delete", &id, "--json"]);
    assert_eq!(deleted, serde_json::json!({"deleted": id}));
    let listed = ok_json(&root, &["comment", "list", "--json"]);
    assert_eq!(listed["comments"], serde_json::json!([]));
    let (code, stderr) = err_of(&root, &["comment", "delete", &id]);
    assert_eq!(code, 1);
    assert!(stderr.contains("no comment"), "{stderr}");
}

#[test]
fn addressed_without_a_response_stores_none() {
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);
    let a = ok_json(&root, &["comment", "add", "-m", "one", "--json"]);
    let b = ok_json(&root, &["comment", "add", "-m", "two", "--json"]);
    let a_id = a["id"].as_str().expect("id");
    let b_id = b["id"].as_str().expect("id");
    let addressed = ok_json(&root, &["comment", "addressed", a_id, "--json"]);
    assert_eq!(addressed["status"], "addressed");
    assert_eq!(addressed.get("response"), None, "{addressed}");
    let addressed = ok_json(&root, &["comment", "addressed", b_id, "-m", "", "--json"]);
    assert_eq!(addressed["status"], "addressed");
    assert_eq!(addressed.get("response"), None, "{addressed}");
}

#[test]
fn a_review_rooted_in_a_subdirectory_captures_snippets_and_lists_root_relative_paths() {
    let (_dir, root) = scratch_repo();
    write(&root, "sub/inner.txt", "one\ntwo\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "sub"]);
    write(&root, "sub/inner.txt", "one\nTWO\n");
    write(&root, "sub/[AB].txt", "bracket\n");
    write(&root, "sub/A.txt", "plain a\n");
    let sub = root.join("sub");
    let init = ok_json(&sub, &["init", "--json"]);
    assert!(
        init["file"]
            .as_str()
            .expect("file")
            .ends_with("sub/.ambidiff.json")
    );

    let comment = ok_json(
        &sub,
        &[
            "comment",
            "add",
            "-p",
            "inner.txt",
            "-l",
            "2",
            "-m",
            "why??",
            "--json",
        ],
    );
    assert_eq!(comment["snippet"], "TWO", "snippet read root-relative");
    let old = ok_json(
        &sub,
        &[
            "comment",
            "add",
            "-p",
            "inner.txt",
            "-l",
            "2",
            "--side",
            "old",
            "-m",
            "was",
            "--json",
        ],
    );
    assert_eq!(
        old["snippet"], "two",
        "old side reads the index, root-relative"
    );

    // The stdio engine lists paths relative to the review root and its
    // per-file diff addresses exactly the requested file.
    let listing = ambidiff_core::git_source::GitSource::open(&sub, None, false)
        .expect("open")
        .listing()
        .expect("listing");
    let paths: Vec<&str> = listing.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, vec!["A.txt", "[AB].txt", "inner.txt"]);
    let bracket = ok_json(
        &sub,
        &[
            "comment", "add", "-p", "[AB].txt", "-l", "1", "-m", "b", "--json",
        ],
    );
    assert_eq!(bracket["snippet"], "bracket");
}

#[test]
fn status_json_reports_the_read_only_reason_and_mutations_leave_bytes_untouched() {
    let (_dir, root) = scratch_repo();
    ok_json(&root, &["init", "--json"]);
    let path = root.join(".ambidiff.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    value["quarantined"] = serde_json::json!({"kept": "by an older tool"});
    let bytes = serde_json::to_vec_pretty(&value).expect("json");
    std::fs::write(&path, &bytes).expect("write");

    let out = run_in(&root, &["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(status["readOnly"], true, "{status}");
    assert!(
        status["readOnlyReason"]
            .as_str()
            .is_some_and(|r| r.contains("quarantined")),
        "{status}"
    );
    let (code, stderr) = err_of(&root, &["comment", "add", "-m", "blocked"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("read-only"), "{stderr}");
    assert_eq!(
        std::fs::read(&path).expect("read"),
        bytes,
        "bytes untouched"
    );
}
