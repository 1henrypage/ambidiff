//! Size and time budget tests for the git source: the raw-byte patch cap,
//! the untracked-file cap, the git process deadline (which bounds the whole
//! process group, not only the direct child), and the content-derived
//! (never mtime-derived) change signature.

#![cfg(feature = "native")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ambidiff_core::git_source::{GitSource, RawDiff, SourceLimits};
use ambidiff_core::source::{DiffSource, FileDiffRequest, SourceError};
use ambidiff_core::sys::{Liveness, process_liveness};

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

fn write(root: &Path, path: &str, content: &[u8]) {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(full, content).expect("write");
}

fn repo(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    for (path, content) in files {
        write(&root, path, content.as_bytes());
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
fn single_huge_line_hits_the_raw_byte_budget_not_the_line_count() {
    let (_dir, root) = repo(&[("f.txt", "short\n")]);
    // One replaced line (numstat: 1 add, 1 del, comfortably under the
    // changed-lines preflight) but 9 MiB of bytes on that one line.
    let huge_line = "x".repeat(9 * 1024 * 1024);
    write(&root, "f.txt", huge_line.as_bytes());

    let source = GitSource::open(&root, None, false)
        .expect("open")
        .with_limits(SourceLimits {
            raw_bytes: 1024 * 1024,
            ..SourceLimits::default()
        });
    let entries = source.listing().expect("files").entries;
    let entry = entry_for(&entries, "f.txt");
    match source
        .file_diff_raw(&FileDiffRequest::for_entry(entry, 3))
        .expect("raw diff")
    {
        RawDiff::TooLarge { .. } => {}
        other => panic!("expected the raw-byte budget to trip, got {other:?}"),
    }
}

#[test]
fn untracked_file_over_budget_is_too_large_without_a_line_count() {
    let (_dir, root) = repo(&[("tracked.txt", "x\n")]);
    let huge = vec![b'a'; 3 * 1024 * 1024];
    write(&root, "huge.txt", &huge);

    let source = GitSource::open(&root, None, false)
        .expect("open")
        .with_limits(SourceLimits {
            untracked_bytes: 2 * 1024 * 1024,
            ..SourceLimits::default()
        });
    let entries = source.listing().expect("files").entries;
    let entry = entry_for(&entries, "huge.txt");
    assert_eq!(
        entry.adds, None,
        "no line count for an untracked file over the byte budget"
    );
    let raw = source
        .file_diff_raw(&FileDiffRequest::for_entry(entry, 3))
        .expect("raw diff");
    assert!(matches!(raw, RawDiff::TooLarge { adds: 0, dels: 0 }));
}

#[test]
fn signature_holds_across_a_touch_without_content_change() {
    let (_dir, root) = repo(&[("f.txt", "content\n")]);
    write(&root, "f.txt", b"changed\n");
    let source = GitSource::open(&root, None, false).expect("open");
    let sig1 = source.try_signature().expect("sig1");

    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(root.join("f.txt"))
        .expect("open for touch");
    file.set_modified(std::time::SystemTime::now() + Duration::from_secs(120))
        .expect("bump mtime");

    let sig2 = source.try_signature().expect("sig2");
    assert_eq!(
        sig1, sig2,
        "a touch with no content change must not move the signature"
    );
}

#[test]
fn signature_is_content_derived_across_a_restored_mtime() {
    let (_dir, root) = repo(&[("tracked.txt", "x\n")]);
    write(&root, "loose.txt", b"same content\n");
    let source = GitSource::open(&root, None, false).expect("open");
    let sig1 = source.try_signature().expect("sig1");

    std::fs::remove_file(root.join("loose.txt")).expect("rm");
    write(&root, "loose.txt", b"same content\n");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(root.join("loose.txt"))
        .expect("open");
    file.set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
        .expect("backdate mtime");

    let sig2 = source.try_signature().expect("sig2");
    assert_eq!(
        sig1, sig2,
        "the signature is content-derived, never mtime-derived"
    );
}

#[test]
fn signature_moves_when_untracked_content_changes() {
    let (_dir, root) = repo(&[("tracked.txt", "x\n")]);
    write(&root, "loose.txt", b"version one\n");
    let source = GitSource::open(&root, None, false).expect("open");
    let sig1 = source.try_signature().expect("sig1");

    write(&root, "loose.txt", b"version two\n");
    let sig2 = source.try_signature().expect("sig2");
    assert_ne!(sig1, sig2, "untracked content is part of the signature");
}

/// Write an executable script into `dir`, run it once with `--warm-up`
/// (every script here exits 0 on that argument), and return its path.
/// macOS spends ~300 ms on the first execution of a freshly written
/// script (the syspolicy check), which would eat a 200 ms deadline before
/// the script's first line runs; the warm-up pays that once, outside the
/// timed call.
fn executable(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write script");
    let mut perms = std::fs::metadata(&path).expect("meta").permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod +x");
    let status = Command::new(&path)
        .arg("--warm-up")
        .status()
        .expect("warm-up exec");
    assert!(status.success(), "warm-up of {name} failed: {status}");
    path
}

const WARM_UP_GUARD: &str = "[ \"$1\" = --warm-up ] && exit 0\n";

/// A wrapper script that stands in for `git`, always sleeping before
/// delegating to the real binary; the wrapper file's `TempDir` is returned
/// alongside so it stays alive for the test's duration.
fn slow_git_wrapper() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = executable(
        dir.path(),
        "slow-git",
        &format!("#!/bin/sh\n{WARM_UP_GUARD}sleep 5\nexec git \"$@\"\n"),
    );
    (dir, path)
}

/// A wrapper that forks a grandchild holding the stdio pipes (its pid is
/// written to `pidfile`), then runs `body` (something that keeps the
/// wrapper alive: `wait`, or a stream of output) so the deadline or the
/// output budget must reach the whole process group to release the reader.
fn grandchild_wrapper(body: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pidfile = dir.path().join("grandchild.pid");
    let script = format!(
        "#!/bin/sh\n{WARM_UP_GUARD}sleep 30 &\necho $! > '{}'\n{body}\n",
        pidfile.display()
    );
    let path = executable(dir.path(), "wrapper", &script);
    (dir, path, pidfile)
}

fn recorded_pid(pidfile: &Path) -> u32 {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(text) = std::fs::read_to_string(pidfile)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            return pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the wrapper never recorded its grandchild's pid"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll `process_liveness` until `pid` is provably dead, or fail after
/// `within`.
fn wait_dead(pid: u32, within: Duration) {
    let deadline = std::time::Instant::now() + within;
    loop {
        if process_liveness(pid) == Liveness::Dead {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "process {pid} is still alive after {within:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn git_deadline_yields_timeout() {
    let (_dir, root) = repo(&[("f.txt", "x\n")]);
    write(&root, "f.txt", b"y\n");
    let (_wrapper_dir, wrapper) = slow_git_wrapper();

    let source = GitSource::open(&root, None, false)
        .expect("open resolves with the real git binary")
        .with_git_program(&wrapper)
        .with_limits(SourceLimits {
            git_deadline: Duration::from_millis(200),
            ..SourceLimits::default()
        });

    let started = std::time::Instant::now();
    let err = source
        .listing()
        .expect_err("the slow wrapper must time out");
    assert!(
        matches!(err, SourceError::Timeout { .. }),
        "expected Timeout, got {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the deadline, not the wrapper's sleep, bounds the call: {:?}",
        started.elapsed()
    );
}

#[test]
fn git_deadline_kills_a_pipe_holding_grandchild() {
    let (_dir, root) = repo(&[("f.txt", "x\n")]);
    let (_wrapper_dir, wrapper, pidfile) = grandchild_wrapper("wait");

    let source = GitSource::open(&root, None, false)
        .expect("open resolves with the real git binary")
        .with_git_program(&wrapper)
        .with_limits(SourceLimits {
            git_deadline: Duration::from_millis(200),
            ..SourceLimits::default()
        });

    let started = std::time::Instant::now();
    let err = source
        .listing()
        .expect_err("the wrapper never produces a listing");
    assert!(
        matches!(err, SourceError::Timeout { .. }),
        "expected Timeout, got {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a grandchild holding stdout must not extend the deadline: {:?}",
        started.elapsed()
    );
    wait_dead(recorded_pid(&pidfile), Duration::from_secs(2));
}

#[test]
fn output_budget_kills_a_streaming_grandchild() {
    let (_dir, root) = repo(&[("f.txt", "x\n")]);
    let (_wrapper_dir, wrapper, pidfile) = grandchild_wrapper("exec yes");

    let source = GitSource::open(&root, None, false)
        .expect("open resolves with the real git binary")
        .with_git_program(&wrapper)
        .with_limits(SourceLimits {
            raw_bytes: 1024,
            ..SourceLimits::default()
        });

    let started = std::time::Instant::now();
    let err = source
        .listing()
        .expect_err("an endless stream trips the byte budget");
    assert!(
        matches!(err, SourceError::TooLarge { .. }),
        "expected TooLarge, got {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the budget kill must not wait for the default deadline: {:?}",
        started.elapsed()
    );
    wait_dead(recorded_pid(&pidfile), Duration::from_secs(2));
}
