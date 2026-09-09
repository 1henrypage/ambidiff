//! Git adapter integration tests against per-test scratch repositories:
//! the revdiff/hunk edge-case catalog exercised on real git.

// Native-only suite: uses filesystem, subprocesses, or proptest's std rng.
#![cfg(feature = "native")]

use std::path::{Path, PathBuf};
use std::process::Command;

use ambidiff_core::git_source::GitSource;
use ambidiff_core::model::{FileDiffKind, FileStatus, LineKind};
use ambidiff_core::review::Side;
use ambidiff_core::source::{DiffSource, Endpoint, FileDiffRequest, SkipReason, SourceError};

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
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

/// Scratch repo with an initial commit of the given files.
fn repo(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    // Pin eol handling so a developer's global autocrlf cannot skew content
    // assertions (CRLF must survive to the diff verbatim).
    git(&root, &["config", "core.autocrlf", "false"]);
    for (path, content) in files {
        write(&root, path, content);
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    (dir, root)
}

fn entry_for<'a>(
    entries: &'a [ambidiff_core::model::FileEntry],
    path: &str,
) -> &'a ambidiff_core::model::FileEntry {
    entries
        .iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("no entry for {path} in {entries:?}"))
}

#[test]
fn working_tree_modification_is_listed_and_diffed() {
    let (_dir, root) = repo(&[("src/a.txt", "one\ntwo\nthree\n")]);
    write(&root, "src/a.txt", "one\nTWO\nthree\n");

    let source = GitSource::open(&root, None, false).expect("open");
    let entries = source.listing().expect("changed files").entries;
    let entry = entry_for(&entries, "src/a.txt");
    assert_eq!(entry.status, FileStatus::Modified);
    assert_eq!((entry.adds, entry.dels), (Some(1), Some(1)));

    let diff = source
        .file_diff(&FileDiffRequest::for_entry(entry, 3))
        .expect("diff");
    assert_eq!(diff.kind, FileDiffKind::Text);
    assert_eq!(diff.count_changes(), (1, 1));
    assert_eq!(diff.old_total_lines, Some(3));
    let kinds: Vec<LineKind> = diff.hunks[0].lines.iter().map(|l| l.kind).collect();
    assert_eq!(
        kinds,
        vec![
            LineKind::Context,
            LineKind::Remove,
            LineKind::Add,
            LineKind::Context
        ]
    );
}

#[test]
fn staged_and_unstaged_are_distinguished() {
    let (_dir, root) = repo(&[("f.txt", "a\n")]);
    write(&root, "f.txt", "staged\n");
    git(&root, &["add", "f.txt"]);
    write(&root, "f.txt", "unstaged\n");

    let staged = GitSource::open(&root, None, true).expect("open");
    let entries = staged.listing().expect("staged files").entries;
    assert_eq!(entries.len(), 1);
    let diff = staged
        .file_diff(&FileDiffRequest::for_entry(&entries[0], 3))
        .expect("diff");
    let adds: Vec<&str> = diff.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Add)
        .map(|l| l.content.as_str())
        .collect();
    assert_eq!(adds, vec!["staged"]);

    let unstaged = GitSource::open(&root, None, false).expect("open");
    let entries = unstaged.listing().expect("unstaged files").entries;
    let diff = unstaged
        .file_diff(&FileDiffRequest::for_entry(&entries[0], 3))
        .expect("diff");
    let adds: Vec<&str> = diff.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Add)
        .map(|l| l.content.as_str())
        .collect();
    assert_eq!(adds, vec!["unstaged"]);
}

#[test]
fn base_ref_diff_includes_committed_and_working_tree_changes() {
    let (_dir, root) = repo(&[("f.txt", "base\n")]);
    write(&root, "f.txt", "committed\n");
    git(&root, &["commit", "-qam", "change"]);
    write(&root, "f.txt", "working\n");

    let source = GitSource::open(&root, Some("main~1".into()), false).expect("open");
    let entries = source.listing().expect("files").entries;
    let diff = source
        .file_diff(&FileDiffRequest::for_entry(&entries[0], 3))
        .expect("diff");
    let adds: Vec<&str> = diff.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Add)
        .map(|l| l.content.as_str())
        .collect();
    assert_eq!(adds, vec!["working"], "working tree is the new side");
}

#[test]
fn range_diff_ignores_working_tree() {
    let (_dir, root) = repo(&[("f.txt", "v1\n")]);
    write(&root, "f.txt", "v2\n");
    git(&root, &["commit", "-qam", "v2"]);
    write(&root, "f.txt", "dirty\n");

    let source = GitSource::open(&root, Some("main~1..main".into()), false).expect("open");
    let entries = source.listing().expect("files").entries;
    let diff = source
        .file_diff(&FileDiffRequest::for_entry(&entries[0], 3))
        .expect("diff");
    let adds: Vec<&str> = diff.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Add)
        .map(|l| l.content.as_str())
        .collect();
    assert_eq!(adds, vec!["v2"], "range diff pins the new side to the ref");
    assert_eq!(
        source
            .read_side(Side::New, "f.txt")
            .expect("read side")
            .as_deref(),
        Some("v2\n"),
        "new-side reads resolve the right ref, not the working tree"
    );
}

#[test]
fn git_mv_rename_populates_old_path_and_minimal_diff() {
    let body = "fn main() {\n    println!(\"hello\");\n}\nline4\nline5\nline6\n";
    let (_dir, root) = repo(&[("old_name.rs", body)]);
    git(&root, &["mv", "old_name.rs", "new_name.rs"]);
    // Small edit so the diff is non-empty but still a rename.
    write(&root, "new_name.rs", &body.replace("hello", "hi"));
    git(&root, &["add", "-A"]);

    let source = GitSource::open(&root, Some("HEAD".into()), false).expect("open");
    let entries = source.listing().expect("files").entries;
    let entry = entry_for(&entries, "new_name.rs");
    assert_eq!(entry.status, FileStatus::Renamed);
    assert_eq!(entry.old_path.as_deref(), Some("old_name.rs"));

    let diff = source
        .file_diff(&FileDiffRequest::for_entry(entry, 3))
        .expect("diff");
    let (adds, dels) = diff.count_changes();
    assert_eq!((adds, dels), (1, 1), "rename pairs into a minimal diff");
    assert_eq!(
        diff.old_total_lines,
        Some(6),
        "old side read from origin path"
    );
}

#[test]
fn rename_with_heavy_edit_past_threshold_reports_delete_and_add() {
    let (_dir, root) = repo(&[("original.txt", "a\nb\nc\nd\ne\n")]);
    git(&root, &["rm", "-q", "original.txt"]);
    write(
        &root,
        "renamed.txt",
        "completely\ndifferent\ncontent\nnow\nhere\n",
    );
    git(&root, &["add", "-A"]);

    let source = GitSource::open(&root, Some("HEAD".into()), false).expect("open");
    let entries = source.listing().expect("files").entries;
    // Similarity too low for -M: git reports delete + add; the review-side
    // anchor resolver is what turns this into an "unattached" comment group.
    assert_eq!(
        entry_for(&entries, "original.txt").status,
        FileStatus::Deleted
    );
    let added = entry_for(&entries, "renamed.txt");
    assert!(matches!(added.status, FileStatus::Added));
    assert_eq!(added.old_path, None);
}

#[test]
fn untracked_file_is_listed_and_diffs_as_all_adds() {
    let (_dir, root) = repo(&[("tracked.txt", "x\n")]);
    write(&root, "brand_new.txt", "alpha\nbeta\n");

    let source = GitSource::open(&root, None, false).expect("open");
    let entries = source.listing().expect("files").entries;
    let entry = entry_for(&entries, "brand_new.txt");
    assert_eq!(entry.status, FileStatus::Untracked);
    assert_eq!(entry.adds, Some(2));

    let diff = source
        .file_diff(&FileDiffRequest::for_entry(entry, 3))
        .expect("diff");
    assert_eq!(diff.count_changes(), (2, 0));
    assert!(diff.hunks[0].lines.iter().all(|l| l.kind == LineKind::Add));
}

#[test]
fn untracked_files_excluded_from_range_diffs() {
    let (_dir, root) = repo(&[("f.txt", "v1\n")]);
    write(&root, "f.txt", "v2\n");
    git(&root, &["commit", "-qam", "v2"]);
    write(&root, "loose.txt", "untracked\n");

    let source = GitSource::open(&root, Some("main~1..main".into()), false).expect("open");
    let entries = source.listing().expect("files").entries;
    assert!(
        !entries.iter().any(|e| e.path == "loose.txt"),
        "range diffs have no working-tree side"
    );
}

#[test]
fn empty_repo_unborn_head_lists_nothing_tracked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    write(&root, "first.txt", "hello\n");

    let source = GitSource::open(&root, None, false).expect("open");
    let entries = source.listing().expect("files on unborn HEAD").entries;
    let entry = entry_for(&entries, "first.txt");
    assert_eq!(entry.status, FileStatus::Untracked);
    let diff = source
        .file_diff(&FileDiffRequest::for_entry(entry, 3))
        .expect("diff");
    assert_eq!(diff.count_changes(), (1, 0));
}

#[test]
fn non_ascii_paths_survive_the_z_pipeline() {
    let (_dir, root) = repo(&[("d\u{e9}p\u{f4}t/\u{65e5}\u{672c}.txt", "one\n")]);
    write(&root, "d\u{e9}p\u{f4}t/\u{65e5}\u{672c}.txt", "two\n");

    let source = GitSource::open(&root, None, false).expect("open");
    let entries = source.listing().expect("files").entries;
    let entry = entry_for(&entries, "d\u{e9}p\u{f4}t/\u{65e5}\u{672c}.txt");
    assert_eq!(entry.status, FileStatus::Modified);
    let diff = source
        .file_diff(&FileDiffRequest::for_entry(entry, 3))
        .expect("diff");
    assert_eq!(diff.count_changes(), (1, 1));
}

#[test]
fn binary_file_yields_placeholder() {
    let (_dir, root) = repo(&[("data.bin", "text\n")]);
    std::fs::write(root.join("data.bin"), [0u8, 159, 146, 150, 0, 1, 2]).expect("write binary");

    let source = GitSource::open(&root, None, false).expect("open");
    let entries = source.listing().expect("files").entries;
    let entry = entry_for(&entries, "data.bin");
    assert_eq!(
        (entry.adds, entry.dels),
        (None, None),
        "numstat marks binary"
    );
    let diff = source
        .file_diff(&FileDiffRequest::for_entry(entry, 3))
        .expect("diff");
    assert!(matches!(diff.kind, FileDiffKind::Binary { .. }));
}

#[test]
fn crlf_content_diffs_without_mangling() {
    let (_dir, root) = repo(&[("win.txt", "a\r\nb\r\n")]);
    write(&root, "win.txt", "a\r\nB\r\n");

    let source = GitSource::open(&root, None, false).expect("open");
    let entries = source.listing().expect("files").entries;
    let diff = source
        .file_diff(&FileDiffRequest::for_entry(&entries[0], 3))
        .expect("diff");
    let add = diff.hunks[0]
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Add)
        .expect("add line");
    assert_eq!(add.content, "B\r");
}

#[test]
fn worktree_checkout_diffs_against_its_own_tree() {
    let (_dir, root) = repo(&[("f.txt", "main content\n")]);
    let wt_parent = tempfile::tempdir().expect("tempdir");
    let wt = wt_parent.path().join("wt");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().expect("utf8 path"),
        ],
    );
    write(&wt, "f.txt", "feature content\n");

    let source = GitSource::open(&wt, Some("main".into()), false).expect("open");
    let entries = source.listing().expect("worktree files").entries;
    assert_eq!(entries.len(), 1);
    let diff = source
        .file_diff(&FileDiffRequest::for_entry(&entries[0], 3))
        .expect("diff");
    assert_eq!(diff.count_changes(), (1, 1));
    assert!(GitSource::is_repo(&wt));
}

#[test]
fn huge_change_hits_numstat_preflight() {
    let big: String = (0..12_000).map(|i| format!("line {i}\n")).collect();
    let (_dir, root) = repo(&[("big.txt", "small\n")]);
    write(&root, "big.txt", &big);

    let source = GitSource::open(&root, None, false).expect("open");
    let entries = source.listing().expect("files").entries;
    let diff = source
        .file_diff(&FileDiffRequest::for_entry(&entries[0], 3))
        .expect("diff");
    match diff.kind {
        FileDiffKind::TooLarge { adds, dels } => {
            assert_eq!(adds, 12_000);
            assert_eq!(dels, 1);
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[test]
fn deleted_file_diffs_as_all_removes() {
    let (_dir, root) = repo(&[("gone.txt", "a\nb\n")]);
    std::fs::remove_file(root.join("gone.txt")).expect("rm");

    let source = GitSource::open(&root, None, false).expect("open");
    let entries = source.listing().expect("files").entries;
    let entry = entry_for(&entries, "gone.txt");
    assert_eq!(entry.status, FileStatus::Deleted);
    let diff = source
        .file_diff(&FileDiffRequest::for_entry(entry, 3))
        .expect("diff");
    assert_eq!(diff.count_changes(), (0, 2));
}

#[test]
fn snippet_reads_resolve_old_and_new_sides() {
    let (_dir, root) = repo(&[("f.txt", "old line\n")]);
    write(&root, "f.txt", "new line\n");

    let source = GitSource::open(&root, None, false).expect("open");
    assert_eq!(
        source
            .read_side(Side::New, "f.txt")
            .expect("read side")
            .as_deref(),
        Some("new line\n")
    );
    assert_eq!(
        source
            .read_side(Side::Old, "f.txt")
            .expect("read side")
            .as_deref(),
        Some("old line\n")
    );
    assert_eq!(
        source
            .read_side(Side::Old, "missing.txt")
            .expect("read side"),
        None
    );
}

#[test]
fn signature_moves_on_change_and_holds_on_touch() {
    let (_dir, root) = repo(&[("f.txt", "content\n")]);
    write(&root, "f.txt", "changed\n");
    let source = GitSource::open(&root, None, false).expect("open");

    let sig1 = source.try_signature().expect("signature");
    let sig2 = source.try_signature().expect("signature");
    assert_eq!(sig1, sig2, "signature is stable across runs");

    write(&root, "f.txt", "changed again\n");
    assert_ne!(
        source.try_signature().expect("signature"),
        sig1,
        "content change moves the signature"
    );
}

#[test]
fn ensure_git_exclude_is_idempotent_and_worktree_aware() {
    let (_dir, root) = repo(&[("f.txt", "x\n")]);
    GitSource::ensure_git_exclude(&root).expect("exclude");
    GitSource::ensure_git_exclude(&root).expect("exclude again");
    let exclude = std::fs::read_to_string(root.join(".git/info/exclude")).expect("read");
    assert_eq!(
        exclude.matches(".ambidiff.json\n").count(),
        1,
        "exactly one entry after two runs"
    );
    assert!(exclude.contains(".ambidiff.json.lock"));

    // In a linked worktree the exclude file lives in the common git dir.
    let wt_parent = tempfile::tempdir().expect("tempdir");
    let wt = wt_parent.path().join("wt");
    git(
        &root,
        &["worktree", "add", "-q", wt.to_str().expect("utf8")],
    );
    GitSource::ensure_git_exclude(&wt).expect("worktree exclude");
    let exclude = std::fs::read_to_string(root.join(".git/info/exclude")).expect("read");
    assert_eq!(exclude.matches(".ambidiff.json\n").count(), 1);
}

#[test]
fn ensure_git_exclude_preserves_bytes_and_adds_guard() {
    let (_dir, root) = repo(&[("f.txt", "x\n")]);
    let exclude_path = root.join(".git/info/exclude");
    std::fs::write(&exclude_path, "# a custom comment\ncustom-rule\n").expect("seed exclude");

    GitSource::ensure_git_exclude(&root).expect("exclude");
    let content = std::fs::read_to_string(&exclude_path).expect("read");
    assert!(
        content.contains("# a custom comment"),
        "existing bytes must survive: {content}"
    );
    assert!(content.contains("custom-rule"));
    assert!(content.contains(".ambidiff.json.guard"), "{content}");
    assert!(content.contains(".ambidiff.json.tmp.*"), "{content}");
    assert_eq!(
        content.matches(".ambidiff.json\n").count(),
        1,
        "exactly one entry, not appended again"
    );
}

#[test]
fn ensure_git_exclude_refuses_a_symlinked_exclude_file() {
    let (_dir, root) = repo(&[("f.txt", "x\n")]);
    let exclude_path = root.join(".git/info/exclude");
    std::fs::remove_file(&exclude_path).expect("rm real exclude");
    let target = root.join("elsewhere-exclude");
    std::fs::write(&target, "").expect("write symlink target");
    std::os::unix::fs::symlink(&target, &exclude_path).expect("symlink exclude");

    let err = GitSource::ensure_git_exclude(&root).expect_err("must refuse a symlinked exclude");
    assert!(err.to_string().to_lowercase().contains("symlink"), "{err}");
}

#[test]
fn staged_new_side_reads_index_not_worktree() {
    let (_dir, root) = repo(&[("f.txt", "base\n")]);
    write(&root, "f.txt", "staged\n");
    git(&root, &["add", "f.txt"]);
    write(&root, "f.txt", "further unstaged edit\n");

    let source = GitSource::open(&root, None, true).expect("open staged");
    assert_eq!(
        source
            .read_side(Side::New, "f.txt")
            .expect("read side")
            .as_deref(),
        Some("staged\n"),
        "the staged new side is the index, not the dirtier working tree"
    );
}

#[test]
fn three_dot_uses_merge_base_not_left_tip() {
    let (_dir, root) = repo(&[("f.txt", "base\n")]);
    git(&root, &["checkout", "-qb", "feature"]);
    write(&root, "f.txt", "feature change\n");
    git(&root, &["commit", "-qam", "feature"]);
    git(&root, &["checkout", "-q", "main"]);
    write(
        &root,
        "f.txt",
        "main change, irrelevant to the merge base\n",
    );
    git(&root, &["commit", "-qam", "main"]);

    let source =
        GitSource::open(&root, Some("main...feature".into()), false).expect("open three-dot");
    let entries = source.listing().expect("files").entries;
    let diff = source
        .file_diff(&FileDiffRequest::for_entry(&entries[0], 3))
        .expect("diff");
    let adds: Vec<&str> = diff.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Add)
        .map(|l| l.content.as_str())
        .collect();
    assert_eq!(
        adds,
        vec!["feature change"],
        "the old side is the merge base, not main's tip"
    );
}

#[test]
fn unborn_head_staged_diffs_against_empty_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    write(&root, "first.txt", "hello\n");
    git(&root, &["add", "first.txt"]);

    let source = GitSource::open(&root, None, true).expect("open staged on an unborn HEAD");
    let entries = source.listing().expect("files on unborn HEAD").entries;
    let entry = entry_for(&entries, "first.txt");
    assert_eq!(entry.status, FileStatus::Added);
    let diff = source
        .file_diff(&FileDiffRequest::for_entry(entry, 3))
        .expect("diff");
    assert_eq!(diff.count_changes(), (1, 0));
}

#[test]
fn bracket_pathspec_addresses_exactly_one_file() {
    let (_dir, root) = repo(&[("A.txt", "orig-a\n"), ("[AB].txt", "orig-bracket\n")]);
    write(&root, "A.txt", "changed-a\n");
    write(&root, "[AB].txt", "changed-bracket\n");

    let source = GitSource::open(&root, None, false).expect("open");
    let entries = source.listing().expect("files").entries;
    let bracket_entry = entry_for(&entries, "[AB].txt");
    let diff = source
        .file_diff(&FileDiffRequest::for_entry(bracket_entry, 3))
        .expect("diff");
    let adds: Vec<&str> = diff.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Add)
        .map(|l| l.content.as_str())
        .collect();
    assert_eq!(
        adds,
        vec!["changed-bracket"],
        "a literal pathspec must not glob-match A.txt"
    );
}

#[test]
fn special_characters_in_paths_round_trip() {
    let name = "weird 'file' (caf\u{e9}) #1.txt";
    let (_dir, root) = repo(&[(name, "one\n")]);
    write(&root, name, "two\n");

    let source = GitSource::open(&root, None, false).expect("open");
    let entries = source.listing().expect("files").entries;
    let entry = entry_for(&entries, name);
    let diff = source
        .file_diff(&FileDiffRequest::for_entry(entry, 3))
        .expect("diff");
    assert_eq!(diff.count_changes(), (1, 1));
}

#[test]
fn subdirectory_root_lists_root_relative_and_scoped() {
    let (_dir, root) = repo(&[("top.txt", "a\n"), ("sub/inner.txt", "b\n")]);
    write(&root, "top.txt", "a-changed\n");
    write(&root, "sub/inner.txt", "b-changed\n");

    let sub_root = root.join("sub");
    let source = GitSource::open(&sub_root, None, false).expect("open");
    let entries = source.listing().expect("files").entries;
    assert_eq!(
        entries.len(),
        1,
        "top.txt is outside the review root: {entries:?}"
    );
    assert_eq!(
        entries[0].path, "inner.txt",
        "path is root-relative: no sub/ prefix"
    );
}

#[test]
fn rename_across_review_root_boundary_appears_as_add_or_delete() {
    let body = "shared content long enough to stay above the -M similarity floor\nline2\nline3\nline4\nline5\n";
    let (_dir, root) = repo(&[("sub/a.txt", body)]);
    git(&root, &["mv", "sub/a.txt", "moved_out.txt"]);
    git(&root, &["commit", "-qam", "move out of the review root"]);

    let sub_root = root.join("sub");
    let source = GitSource::open(&sub_root, Some("HEAD~1".into()), false).expect("open");
    let entries = source.listing().expect("files").entries;
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].path, "a.txt");
    assert_eq!(
        entries[0].status,
        FileStatus::Deleted,
        "the rename target lives outside the root, so inside it this reads as a delete"
    );
}

#[test]
fn symlink_entry_reads_as_link_text() {
    let (_dir, root) = repo(&[("target.txt", "target content\n")]);
    std::os::unix::fs::symlink("target.txt", root.join("link.txt")).expect("symlink");

    let source = GitSource::open(&root, None, false).expect("open");
    assert_eq!(
        source
            .read_side(Side::New, "link.txt")
            .expect("read side")
            .as_deref(),
        Some("target.txt"),
        "a symlink entry reads as its link text, matching Git"
    );
}

#[test]
fn traversal_is_denied() {
    let (_dir, root) = repo(&[("f.txt", "x\n")]);
    let source = GitSource::open(&root, None, false).expect("open");
    let err = source
        .read_side(Side::New, "../outside.txt")
        .expect_err("must deny a path escaping the review root");
    assert!(err.is_invalid_path(), "{err}");
}

#[test]
fn non_utf8_path_is_reported_not_guessed() {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStrExt;

    let (_dir, root) = repo(&[("tracked.txt", "x\n")]);

    // APFS itself refuses to create a file whose name is not valid UTF-8, so
    // a non-UTF-8 path has to be injected straight into the index via git
    // plumbing (which stores paths as raw bytes) rather than the filesystem.
    let hash_out = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(&root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(b"content")?;
            child.wait_with_output()
        })
        .expect("hash-object -w");
    assert!(hash_out.status.success());
    let oid = String::from_utf8_lossy(&hash_out.stdout).trim().to_string();

    let mut bad_path = OsString::new();
    bad_path.push(OsStr::from_bytes(b"bad_"));
    bad_path.push(OsStr::from_bytes(&[0xFF]));
    bad_path.push(OsStr::from_bytes(b"_name.txt"));
    let status = Command::new("git")
        .arg("update-index")
        .arg("--add")
        .arg("--cacheinfo")
        .arg("100644")
        .arg(&oid)
        .arg(&bad_path)
        .current_dir(&root)
        .status()
        .expect("update-index --cacheinfo");
    assert!(status.success());
    git(&root, &["commit", "-qm", "add a non-utf8 path"]);

    let source = GitSource::open(&root, Some("HEAD~1..HEAD".into()), false).expect("open");
    let listing = source.listing().expect("listing");
    assert_eq!(listing.skipped.len(), 1, "{:?}", listing.skipped);
    assert_eq!(listing.skipped[0].reason, SkipReason::NonUtf8);
    assert!(
        listing.skipped[0].display.contains("bad_"),
        "{}",
        listing.skipped[0].display
    );
    assert!(
        listing.skipped[0].display.contains("\\xFF"),
        "a non-UTF-8 byte is escaped, not guessed at: {}",
        listing.skipped[0].display
    );
}

/// Object reads (index and commit trees) from a subdirectory review root
/// address the root-relative path exactly as the listing reports it.
#[test]
fn subdirectory_root_reads_old_side_from_index_and_commit() {
    let (_dir, root) = repo(&[("sub/inner.txt", "one\ntwo\n"), ("top.txt", "t\n")]);
    write(&root, "sub/inner.txt", "one\nTWO\n");
    let sub = root.join("sub");

    // Default comparison: the old side is the index.
    let source = GitSource::open(&sub, None, false).expect("open");
    assert_eq!(
        source
            .read_side_lines(Side::Old, "inner.txt", 2, 2)
            .expect("read"),
        Some(vec!["two".to_string()])
    );
    assert_eq!(
        source
            .read_side(Side::Old, "inner.txt")
            .expect("read")
            .as_deref(),
        Some("one\ntwo\n")
    );
    assert_eq!(
        source.read_side(Side::Old, "sub/inner.txt").expect("read"),
        None,
        "repository-relative paths are not a second coordinate system"
    );

    // A commit base: the old side is that commit's tree.
    let source = GitSource::open(&sub, Some("HEAD".into()), false).expect("open");
    assert_eq!(
        source
            .read_side_lines(Side::Old, "inner.txt", 1, 2)
            .expect("read"),
        Some(vec!["one".to_string(), "two".to_string()])
    );
    let diff = source
        .file_diff(&FileDiffRequest {
            path: "inner.txt".into(),
            old_path: None,
            context: 3,
        })
        .expect("diff");
    assert_eq!(
        diff.old_total_lines,
        Some(2),
        "old line count from the commit tree"
    );
}

/// `--staged` with a single base ref compares that ref with the index
/// (`git diff --cached <ref>`); only a range is refused, because a range
/// has no index side.
#[test]
fn staged_with_a_base_ref_compares_the_ref_with_the_index() {
    let (_dir, root) = repo(&[("f.txt", "one\ntwo\n")]);
    git(&root, &["tag", "base-tag"]);
    write(&root, "f.txt", "one\nTWO\n");
    git(&root, &["add", "f.txt"]);
    git(&root, &["commit", "-qm", "staged then committed"]);
    // Now stage a third version and leave a fourth in the worktree.
    write(&root, "f.txt", "one\nTHREE\n");
    git(&root, &["add", "f.txt"]);
    write(&root, "f.txt", "one\nFOUR\n");

    let source = GitSource::open(&root, Some("base-tag".into()), true).expect("open");
    let cmp = source.comparison();
    assert!(matches!(cmp.old, Endpoint::Commit { .. }), "{cmp}");
    assert_eq!(cmp.new, Endpoint::Index);
    assert_eq!(
        source
            .read_side(Side::Old, "f.txt")
            .expect("read")
            .as_deref(),
        Some("one\ntwo\n"),
        "old side is the tag"
    );
    assert_eq!(
        source
            .read_side(Side::New, "f.txt")
            .expect("read")
            .as_deref(),
        Some("one\nTHREE\n"),
        "new side is the index, not the worktree"
    );
    let diff = source
        .file_diff(&FileDiffRequest {
            path: "f.txt".into(),
            old_path: None,
            context: 3,
        })
        .expect("diff");
    let texts: Vec<&str> = diff.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind != LineKind::Context)
        .map(|l| l.content.as_str())
        .collect();
    assert_eq!(texts, vec!["two", "THREE"]);

    let err = GitSource::open(&root, Some("base-tag..HEAD".into()), true).expect_err("range");
    assert!(
        matches!(err, SourceError::UnsupportedComparison { .. }),
        "staged + range is refused: {err}"
    );
}
