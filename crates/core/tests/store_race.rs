//! Review store integration on the real filesystem: cross-process lock
//! contention, atomic-write leftovers, and stale-lock breaking with real
//! processes.
//!
//! The cross-process test re-invokes this test binary as a worker (guarded
//! by an env var) so two genuine OS processes race read-modify-write on one
//! review file.

// Native-only suite: uses filesystem, subprocesses, or proptest's std rng.
#![cfg(feature = "native")]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use ambidiff_core::review::{NewComment, ReviewFile, Side, Source};
use ambidiff_core::store::{Lock, Store};

const WORKER_ENV: &str = "AMBIDIFF_TEST_LOCK_WORKER_DIR";
const ROLE_ENV: &str = "AMBIDIFF_TEST_LOCK_ROLE";
const WORKER_ROUNDS: usize = 20;

fn add_round(store: &Store, author: &str, i: usize) {
    let (_, _warnings) = store
        .mutate(|review| {
            let ids: Vec<&str> = review.comments.iter().map(|c| c.id.as_str()).collect();
            let id = format!("c-{author}-{i}-{}", ids.len());
            let comment = NewComment {
                path: Some("f.txt".to_string()),
                side: Some(Side::New),
                line: Some(1),
                end_line: None,
                snippet: None,
                body: format!("{author} round {i}"),
                author: author.to_string(),
            };
            review.try_add_comment(comment, id, "2026-01-01T00:00:00Z")?;
            Ok(())
        })
        .expect("mutate");
}

/// Worker half of the cross-process race. Does nothing unless spawned by
/// `two_processes_racing_under_the_lock_lose_no_updates`.
#[test]
fn lock_worker() {
    let Ok(dir) = std::env::var(WORKER_ENV) else {
        return;
    };
    let store = Store::new(Path::new(&dir));
    for i in 0..WORKER_ROUNDS {
        add_round(&store, "worker", i);
    }
}

#[test]
fn two_processes_racing_under_the_lock_lose_no_updates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::new(dir.path());
    store
        .init(&ReviewFile::new(
            "race".into(),
            Source::git(None),
            "2026-01-01T00:00:00Z",
        ))
        .expect("init");

    let exe = std::env::current_exe().expect("current exe");
    let mut child = Command::new(&exe)
        .args(["lock_worker", "--exact", "--nocapture", "--test-threads=1"])
        .env(WORKER_ENV, dir.path())
        .spawn()
        .expect("spawn worker process");

    for i in 0..WORKER_ROUNDS {
        add_round(&store, "parent", i);
    }

    let status = child.wait().expect("worker exit");
    assert!(status.success(), "worker process failed");

    let outcome = store.load().expect("load");
    assert_eq!(
        outcome.review.comments.len(),
        WORKER_ROUNDS * 2,
        "every read-modify-write from both processes survived"
    );
    assert!(outcome.review.quarantined.is_empty());
    assert!(!store.lock_path().exists(), "no lock left behind");
}

#[test]
fn reader_never_sees_a_torn_file_during_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::new(dir.path());
    store
        .init(&ReviewFile::new("atomic".into(), Source::git(None), "t0"))
        .expect("init");

    let path = store.review_path();
    let reader = std::thread::spawn(move || {
        let mut reads = 0;
        for _ in 0..400 {
            let content = std::fs::read_to_string(&path).expect("read");
            // Atomic rename means every observed content is complete JSON.
            serde_json::from_str::<serde_json::Value>(&content).expect("parse mid-write read");
            reads += 1;
        }
        reads
    });

    for i in 0..40 {
        add_round(&store, "writer", i);
    }
    let reads = reader.join().expect("reader thread");
    assert_eq!(reads, 400);
}

/// Worker half of the lock-holding regressions below: acquires the lock,
/// holds it for a duration determined by `AMBIDIFF_TEST_LOCK_ROLE`, then
/// releases normally (so `Lock::drop` actually runs). Does nothing unless
/// that variable is set to a role this function recognises.
#[test]
fn lock_worker_hold() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    let hold = match role.as_str() {
        "hold-11s" => Duration::from_secs(11),
        "hold-short" => Duration::from_millis(700),
        _ => return,
    };
    let dir = std::env::var(WORKER_ENV).expect("worker dir");
    let store = Store::new(Path::new(&dir));
    let lock = Lock::acquire(
        &store.guard_path(),
        &store.lock_path(),
        Duration::from_secs(5),
    )
    .expect("worker must acquire the lock");
    std::thread::sleep(hold);
    drop(lock);
}

/// B01 regression: a holder older than the old (removed) ten-second
/// staleness window must never be displaced while it is genuinely alive.
/// Reproduces the finding as two real OS processes: the child holds the
/// lock for eleven real seconds while the parent repeatedly fails to
/// acquire it, then succeeds only after the child actually releases.
#[test]
fn holder_exceeding_ten_seconds_is_not_displaced() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::new(dir.path());
    store
        .init(&ReviewFile::new(
            "long-hold".into(),
            Source::git(None),
            "t0",
        ))
        .expect("init");

    let exe = std::env::current_exe().expect("current exe");
    let mut child = Command::new(&exe)
        .args([
            "lock_worker_hold",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(WORKER_ENV, dir.path())
        .env(ROLE_ENV, "hold-11s")
        .spawn()
        .expect("spawn holder process");

    // Give the child time to actually acquire before probing.
    std::thread::sleep(Duration::from_millis(500));

    // Still well within the child's 11s hold, and long past the old 10s
    // staleness window: must not be displaced.
    let still_held = Lock::acquire(
        &store.guard_path(),
        &store.lock_path(),
        Duration::from_secs(2),
    );
    assert!(
        still_held.is_err(),
        "an 11s-old but genuinely live holder must not be displaced by age"
    );

    let status = child.wait().expect("worker exit");
    assert!(status.success(), "holder process failed");

    // Now that the child released cleanly, acquisition succeeds promptly.
    let lock = Lock::acquire(
        &store.guard_path(),
        &store.lock_path(),
        Duration::from_secs(2),
    )
    .expect("acquire after a clean release");
    drop(lock);
}

/// B01 regression: a legacy (no-nonce) sidecar left behind by a process that
/// has since died must be recoverable, and concurrent reclaimers racing to
/// recover it must never corrupt the guard/sidecar state (each acquire is
/// exclusive via the guard's real OS lock, so "competing" reclaimers simply
/// take turns rather than double-freeing or double-creating).
#[test]
fn dead_legacy_holder_is_recovered_once_by_competing_reclaimers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::new(dir.path());
    store
        .init(&ReviewFile::new(
            "legacy-race".into(),
            Source::git(None),
            "t0",
        ))
        .expect("init");

    // A real, soon-to-be-dead pid: spawn a trivial child and reap it.
    let mut child = Command::new("true").spawn().expect("spawn `true`");
    let dead_pid = child.id();
    child.wait().expect("reap it");

    // A legacy (pre-repair, no-nonce) sidecar, as an old binary would leave
    // it, from that now-dead process.
    let legacy = serde_json::json!({"pid": dead_pid, "acquiredAt": 0});
    std::fs::write(store.lock_path(), legacy.to_string()).expect("write legacy sidecar");

    let guard_path = store.guard_path();
    let sidecar_path = store.lock_path();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let guard_path = guard_path.clone();
            let sidecar_path = sidecar_path.clone();
            std::thread::spawn(move || {
                let lock = Lock::acquire(&guard_path, &sidecar_path, Duration::from_secs(3))
                    .expect("a dead legacy sidecar must be recoverable by any contender");
                std::thread::sleep(Duration::from_millis(20));
                drop(lock);
            })
        })
        .collect();
    for h in handles {
        h.join().expect("reclaimer thread panicked");
    }
    assert!(
        !store.lock_path().exists(),
        "no sidecar left behind once every reclaimer has finished"
    );
}

/// B01 regression: `Lock::drop` must remove only a sidecar that still
/// carries its own pid+nonce. Simulated across two real processes: the
/// child acquires the lock and lingers while the parent, from outside any
/// lock, overwrites the sidecar with another (fabricated) holder's record;
/// the child's release must leave that record untouched.
#[test]
fn superseded_holder_cannot_delete_anothers_sidecar() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::new(dir.path());
    store
        .init(&ReviewFile::new(
            "supersede".into(),
            Source::git(None),
            "t0",
        ))
        .expect("init");

    let exe = std::env::current_exe().expect("current exe");
    let mut child = Command::new(&exe)
        .args([
            "lock_worker_hold",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(WORKER_ENV, dir.path())
        .env(ROLE_ENV, "hold-short")
        .spawn()
        .expect("spawn holder process");

    // Let the child actually acquire, then clobber its sidecar with a
    // fabricated holder's record from outside any lock (this itself is
    // unsafe, which is exactly why it must not fool the real owner's
    // release path).
    std::thread::sleep(Duration::from_millis(200));
    let other = serde_json::json!({
        "pid": 999_999_u32,
        "acquiredAt": 0,
        "nonce": "deadbeefdeadbeef",
    });
    std::fs::write(store.lock_path(), other.to_string())
        .expect("overwrite with a fabricated holder's record");

    // Wait for the child's natural exit: its `hold-short` role finishes its
    // sleep and then runs `Lock::drop` normally.
    let status = child.wait().expect("worker exit");
    assert!(status.success(), "holder process failed");

    assert_eq!(
        std::fs::read_to_string(store.lock_path()).expect("read sidecar"),
        other.to_string(),
        "a holder's Drop must never remove a sidecar it does not own"
    );
}
