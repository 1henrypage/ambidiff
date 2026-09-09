//! Watch controller integration with real notify events: the signature gate
//! (touch-without-change vs real change), debounce coalescing, review-file
//! classification, and degrade-to-poll on watcher failure.

// Native-only suite: uses filesystem, subprocesses, or proptest's std rng.
#![cfg(feature = "native")]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ambidiff_core::review::REVIEW_FILE_NAME;
use ambidiff_core::util::fnv1a64;
use ambidiff_core::watch::{Refresh, SignatureFn, WatchConfig, WatchController};

fn file_sig(path: &Path) -> u64 {
    std::fs::read(path).map(|b| fnv1a64(&b)).unwrap_or(0)
}

/// Wrap an infallible signature closure as always-`Ok`, matching what the
/// (now removed) `WatchController::start` compat wrapper used to do.
fn checked(f: impl Fn() -> u64 + Send + Sync + 'static) -> SignatureFn {
    Arc::new(move || Ok(f()))
}

fn config(root: &Path) -> WatchConfig {
    let mut config = WatchConfig::new(root.to_path_buf(), root.join(REVIEW_FILE_NAME));
    config.debounce_quiet = Duration::from_millis(100);
    config.debounce_max = Duration::from_millis(500);
    // Keep the safety poll out of the way so assertions isolate the
    // event-driven path.
    config.poll_interval = Duration::from_secs(120);
    config.degraded_poll = Duration::from_millis(200);
    config
}

fn wait_for(controller: &WatchController, want: Refresh, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if controller.recv_timeout(Duration::from_millis(100)) == Some(want) {
            return true;
        }
    }
    false
}

fn assert_quiet(controller: &WatchController, for_dur: Duration) {
    let deadline = Instant::now() + for_dur;
    while Instant::now() < deadline {
        if let Some(refresh) = controller.recv_timeout(Duration::from_millis(50)) {
            panic!("unexpected refresh {refresh:?}");
        }
    }
}

#[test]
fn real_change_triggers_diff_refresh_and_touch_does_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let data = root.join("data.txt");
    std::fs::write(&data, "v1").expect("write");

    let sig_path = data.clone();
    let controller = WatchController::start_checked(
        config(root),
        checked(move || file_sig(&sig_path)),
        checked(|| 0),
    );
    // Give the watcher a beat to arm before generating events.
    std::thread::sleep(Duration::from_millis(300));

    // Touch without change: same content, new mtime.
    std::fs::write(&data, "v1").expect("touch");
    assert_quiet(&controller, Duration::from_millis(900));

    // Real change: signature moves, refresh arrives.
    std::fs::write(&data, "v2").expect("change");
    assert!(
        wait_for(&controller, Refresh::Diff, Duration::from_secs(5)),
        "real change must refresh"
    );
}

#[test]
fn burst_of_events_coalesces_into_one_refresh() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let data = root.join("data.txt");
    std::fs::write(&data, "0").expect("write");

    let sig_path = data.clone();
    // `start` (like `start_checked`) blocks until the watcher thread has
    // actually attempted OS registration, so no arm-up sleep is needed
    // here: the previous flakiness under heavy parallel load came from
    // generating events before that registration had necessarily happened,
    // not from insufficient sleep duration.
    let controller = WatchController::start_checked(
        config(root),
        checked(move || file_sig(&sig_path)),
        checked(|| 0),
    );

    for i in 1..=10 {
        std::fs::write(&data, format!("{i}")).expect("write");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        wait_for(&controller, Refresh::Diff, Duration::from_secs(5)),
        "burst must produce a refresh"
    );
    // Debounce means the burst coalesced; after it settles nothing else
    // arrives (a second refresh may only occur if the burst straddled the
    // max window, so allow the queue to fully drain first).
    while controller
        .recv_timeout(Duration::from_millis(300))
        .is_some()
    {}
    assert_quiet(&controller, Duration::from_millis(600));
}

#[test]
fn review_file_change_classifies_as_review_refresh() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let review = root.join(REVIEW_FILE_NAME);
    std::fs::write(&review, "{}").expect("write");

    let sig_path = review.clone();
    // See `burst_of_events_coalesces_into_one_refresh`: `start` already
    // guarantees the watcher has attempted registration before returning,
    // so no arm-up sleep is needed.
    let controller = WatchController::start_checked(
        config(root),
        checked(|| 0),
        checked(move || file_sig(&sig_path)),
    );

    std::fs::write(&review, "{\"revision\": 2}").expect("change");
    assert!(
        wait_for(&controller, Refresh::Review, Duration::from_secs(5)),
        "review file change must arrive as Review"
    );
}

#[test]
fn degraded_mode_polls_when_watch_root_is_missing() {
    let missing = std::env::temp_dir().join("ambidiff-definitely-missing-watch-root");
    let _ = std::fs::remove_dir_all(&missing);

    let signal = Arc::new(AtomicU64::new(1));
    let sig = Arc::clone(&signal);
    let controller = WatchController::start_checked(
        config(&missing),
        checked(move || sig.load(Ordering::Relaxed)),
        checked(|| 0),
    );
    std::thread::sleep(Duration::from_millis(150));

    // No fs events possible; only the degraded poll can see this move.
    signal.store(2, Ordering::Relaxed);
    assert!(
        wait_for(&controller, Refresh::Diff, Duration::from_secs(3)),
        "degraded poll must detect the change"
    );
}

/// B12 regression (the watch half): the baseline is computed synchronously,
/// on the caller's thread, before `start` returns and before the background
/// thread is guaranteed to be running. A write landing in that narrow
/// window (here: immediately, with no sleep at all) must still be observed
/// as a change against that baseline, not silently absorbed into it.
#[test]
fn write_in_startup_window_is_detected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let data = root.join("data.txt");
    std::fs::write(&data, "v1").expect("write");

    // A short safety poll: the fs event for a write landing in the tiny gap
    // before the OS watch is actually registered can genuinely be missed
    // (that race is inherent to any watcher, not something baselining
    // fixes). What baselining synchronously against the pre-write content
    // guarantees is that the eventual safety poll still sees a mismatch
    // rather than absorbing the write into its baseline.
    let mut cfg = config(root);
    cfg.poll_interval = Duration::from_millis(200);
    cfg.degraded_poll = Duration::from_millis(200);

    let sig_path = data.clone();
    let controller =
        WatchController::start_checked(cfg, checked(move || file_sig(&sig_path)), checked(|| 0));
    // Deliberately no sleep: change the source in the narrowest possible
    // window right after `start` returns.
    std::fs::write(&data, "v2").expect("change immediately after start");

    assert!(
        wait_for(&controller, Refresh::Diff, Duration::from_secs(5)),
        "a write landing right after start() must still be detected"
    );
}

/// B12 regression (the watch half): swapping the diff signature function
/// (e.g. the review file's `source` changed underneath a running watch)
/// rebaselines synchronously against the new source, so the old source's
/// changes stop mattering and only a subsequent change to the new source
/// fires.
#[test]
fn set_diff_signature_swaps_source_and_rebaselines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let a = root.join("a.txt");
    let b = root.join("b.txt");
    std::fs::write(&a, "a1").expect("write a");
    std::fs::write(&b, "b1").expect("write b");

    let sig_a = a.clone();
    let controller = WatchController::start_checked(
        config(root),
        checked(move || file_sig(&sig_a)),
        checked(|| 0),
    );
    std::thread::sleep(Duration::from_millis(300));

    let sig_b = b.clone();
    let new_diff_sig: SignatureFn = Arc::new(move || Ok(file_sig(&sig_b)));
    controller.set_diff_signature(new_diff_sig);

    std::fs::write(&a, "a2").expect("old source changes");
    assert_quiet(&controller, Duration::from_millis(700));

    std::fs::write(&b, "b2").expect("new source changes");
    assert!(
        wait_for(&controller, Refresh::Diff, Duration::from_secs(5)),
        "the new source's change must be detected after the swap"
    );
}

/// B01/watch integration: the watcher must ignore its own sidecar churn
/// (`.lock`, `.guard`, `.tmp.*`) by name, never treating it as a diff
/// change. A signature that changes on every call would immediately expose
/// a false positive if sidecar events were ever (incorrectly) marked dirty.
#[test]
fn sidecar_churn_does_not_trigger_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let counter = Arc::new(AtomicU64::new(0));
    let c = Arc::clone(&counter);
    let controller = WatchController::start_checked(
        config(root),
        checked(move || c.fetch_add(1, Ordering::Relaxed)),
        checked(|| 0),
    );
    std::thread::sleep(Duration::from_millis(300));

    std::fs::write(root.join(".ambidiff.json.lock"), "x").expect("write lock");
    std::fs::write(root.join(".ambidiff.json.guard"), "x").expect("write guard");
    std::fs::write(root.join(".ambidiff.json.tmp.abcdef0123456789"), "x").expect("write tmp");
    std::fs::remove_file(root.join(".ambidiff.json.lock")).expect("remove lock");

    assert_quiet(&controller, Duration::from_millis(900));
}

#[test]
fn stop_joins_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let controller = WatchController::start_checked(config(root), checked(|| 0), checked(|| 0));
    std::thread::sleep(Duration::from_millis(100));
    // Must return promptly (the background thread actually joins) rather
    // than hang; a hang fails this test via the harness timeout.
    controller.stop();
}

/// B12 regression (the watch half): a signature evaluation failure is a
/// distinct state, never folded to 0. Both the failing transition and the
/// recovering transition emit a `Diff` refresh, and `last_diff_error`
/// reflects the latest evaluation.
#[test]
fn signature_failure_surfaces_via_refresh_and_last_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut cfg = config(root);
    // A short safety poll: this test has no filesystem event to react to,
    // so the poll is what picks up the signature transitions.
    cfg.poll_interval = Duration::from_millis(150);
    cfg.degraded_poll = Duration::from_millis(150);

    let failing = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&failing);
    let diff_sig: SignatureFn = Arc::new(move || {
        if f.load(Ordering::Relaxed) {
            Err("git exploded".to_string())
        } else {
            Ok(1)
        }
    });
    let review_sig: SignatureFn = Arc::new(|| Ok(0));

    let controller = WatchController::start_checked(cfg, diff_sig, review_sig);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(controller.last_diff_error(), None);

    failing.store(true, Ordering::Relaxed);
    assert!(
        wait_for(&controller, Refresh::Diff, Duration::from_secs(5)),
        "a Value -> Failed transition must emit a Diff refresh"
    );
    assert_eq!(
        controller.last_diff_error().as_deref(),
        Some("git exploded")
    );

    failing.store(false, Ordering::Relaxed);
    assert!(
        wait_for(&controller, Refresh::Diff, Duration::from_secs(5)),
        "a Failed -> Value transition must also emit a Diff refresh"
    );
    assert_eq!(controller.last_diff_error(), None);
}
