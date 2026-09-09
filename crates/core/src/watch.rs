//! Watch controller: hunk's design on the `notify` crate.
//!
//! Filesystem events only set dirty hints; the authoritative check re-runs
//! the same signature computation the consumer would use and refreshes only
//! when it moves (or when it starts/stops failing), so touch-without-change
//! noise (editor swap files, git index churn from our own reads) never
//! causes a repaint. Debounce is 200ms of quiet with a 1s cap; a 10s safety
//! poll catches missed events; watcher startup failure degrades to a 2s
//! poll instead of failing. Review and diff each get their own debounce and
//! safety-poll deadline: a burst of one kind's events must never postpone
//! the other's safety poll (see [`Scheduler`]).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

/// A signature computation a caller supplies: re-derive an authoritative
/// hash of either the diff source or the review file. `Err` means the
/// computation itself failed (e.g. a git command errored); that failure is
/// itself a distinct, never-folded-to-zero state so a watcher never
/// confuses "the signature is 0" with "we could not compute it".
pub type SignatureFn = Arc<dyn Fn() -> Result<u64, String> + Send + Sync>;

/// What changed, as delivered to the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// The review file's content changed: reload review state.
    Review,
    /// The diff signature moved (or started/stopped failing): re-derive views.
    Diff,
}

/// Tuning knobs; the defaults are the plan's numbers.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    pub root: PathBuf,
    pub review_file: PathBuf,
    pub debounce_quiet: Duration,
    pub debounce_max: Duration,
    pub poll_interval: Duration,
    pub degraded_poll: Duration,
}

impl WatchConfig {
    pub fn new(root: PathBuf, review_file: PathBuf) -> Self {
        WatchConfig {
            root,
            review_file,
            debounce_quiet: Duration::from_millis(200),
            debounce_max: Duration::from_secs(1),
            poll_interval: Duration::from_secs(10),
            degraded_poll: Duration::from_secs(2),
        }
    }
}

// --- Scheduler: pure, Instant-injected debounce/safety-poll timing --------

/// Per-refresh-kind debounce and safety-poll timing, decided from an
/// injected clock so it is exercised in unit tests with no real sleeps.
///
/// `review` and `diff` are tracked completely independently: `checked(kind)`
/// clears only `kind`'s dirty marks and advances only `kind`'s own poll
/// deadline. Earlier code shared one poll deadline across both, so a run of
/// diff-only checks (each resetting the shared deadline) could indefinitely
/// postpone a missed review-file change; that bug cannot recur here because
/// there is no shared deadline to reset.
#[derive(Debug, Clone)]
pub struct Scheduler {
    review: PollTrack,
    diff: PollTrack,
    debounce_quiet: Duration,
    debounce_max: Duration,
}

#[derive(Debug, Clone, Copy)]
struct PollTrack {
    dirty_first: Option<Instant>,
    dirty_last: Option<Instant>,
    next_poll: Instant,
}

impl Scheduler {
    pub fn new(
        now: Instant,
        debounce_quiet: Duration,
        debounce_max: Duration,
        poll_interval: Duration,
    ) -> Self {
        let track = PollTrack {
            dirty_first: None,
            dirty_last: None,
            next_poll: now + poll_interval,
        };
        Scheduler {
            review: track,
            diff: track,
            debounce_quiet,
            debounce_max,
        }
    }

    fn track(&self, kind: Refresh) -> &PollTrack {
        match kind {
            Refresh::Review => &self.review,
            Refresh::Diff => &self.diff,
        }
    }

    fn track_mut(&mut self, kind: Refresh) -> &mut PollTrack {
        match kind {
            Refresh::Review => &mut self.review,
            Refresh::Diff => &mut self.diff,
        }
    }

    /// Record a filesystem event of kind `kind` at `now`.
    pub fn mark(&mut self, kind: Refresh, now: Instant) {
        let t = self.track_mut(kind);
        t.dirty_first.get_or_insert(now);
        t.dirty_last = Some(now);
    }

    /// Whether `kind`'s check should run now: either its debounce window
    /// elapsed (quiet since the last mark, or the max cap since the first
    /// mark of the current burst) or its own safety-poll deadline arrived.
    pub fn due(&self, kind: Refresh, now: Instant) -> bool {
        let t = self.track(kind);
        let debounce_due = matches!(
            (t.dirty_first, t.dirty_last),
            (Some(first), Some(last))
                if now.duration_since(last) >= self.debounce_quiet
                    || now.duration_since(first) >= self.debounce_max
        );
        debounce_due || now >= t.next_poll
    }

    /// Record that `kind`'s check ran at `now`: clears only `kind`'s dirty
    /// marks and advances only `kind`'s poll deadline by `poll_interval`
    /// (the caller's current interval, normal or degraded).
    pub fn checked(&mut self, kind: Refresh, now: Instant, poll_interval: Duration) {
        let t = self.track_mut(kind);
        t.dirty_first = None;
        t.dirty_last = None;
        t.next_poll = now + poll_interval;
    }

    /// How long until some track next becomes due, for a caller that wants
    /// to sleep rather than poll at a fixed interval. Never negative.
    pub fn next_wakeup(&self, now: Instant) -> Duration {
        let deadline = |t: &PollTrack| -> Instant {
            let mut d = t.next_poll;
            if let (Some(first), Some(last)) = (t.dirty_first, t.dirty_last) {
                d = d
                    .min(last + self.debounce_quiet)
                    .min(first + self.debounce_max);
            }
            d
        };
        deadline(&self.review)
            .min(deadline(&self.diff))
            .saturating_duration_since(now)
    }
}

// --- Signature tracking with a distinct, never-zeroed failure state ------

#[derive(Debug, Clone, PartialEq)]
enum SigState {
    Value(u64),
    Failed,
}

fn eval(sig: &SignatureFn) -> (SigState, Option<String>) {
    match sig() {
        Ok(v) => (SigState::Value(v), None),
        Err(e) => (SigState::Failed, Some(e)),
    }
}

struct SigTrack {
    sig: SignatureFn,
    state: SigState,
}

#[derive(Default)]
struct Pending {
    review: bool,
    diff: bool,
}

impl Pending {
    fn take(&mut self) -> Option<Refresh> {
        if self.review {
            self.review = false;
            return Some(Refresh::Review);
        }
        if self.diff {
            self.diff = false;
            return Some(Refresh::Diff);
        }
        None
    }
}

struct Shared {
    pending: Mutex<Pending>,
    cv: Condvar,
    diff_track: Mutex<SigTrack>,
    review_track: Mutex<SigTrack>,
    last_diff_error: Mutex<Option<String>>,
    /// `None` until the background thread has attempted OS watch
    /// registration; then `Some(true)` (armed) or `Some(false)` (degraded).
    /// `start_checked` blocks on this before returning: spawning a thread
    /// gives no guarantee about when (or whether, under contention) it has
    /// actually run its setup, so without this handshake a caller that
    /// starts generating events immediately after `start_checked` returns
    /// is racing an unsynchronized thread start. That race is exactly what
    /// made event-driven tests occasionally miss the startup window under
    /// heavy parallel load; widening a sleep would only have narrowed the
    /// window, not closed it.
    armed: Mutex<Option<bool>>,
    armed_cv: Condvar,
}

/// Running watch; dropping it stops the thread (without joining: use
/// [`WatchController::stop`] to join).
pub struct WatchController {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WatchController {
    /// Start watching with checked signature functions. `diff_sig` must
    /// re-run the source's authoritative signature (e.g.
    /// [`crate::git_source::GitSource::signature`]); `review_sig` hashes the
    /// review file. Both are evaluated once, synchronously, right here
    /// before the background thread spawns: a write that lands after this
    /// call returns but before the thread is actually running must still be
    /// detected, not absorbed into the baseline. Callers therefore arm the
    /// watch BEFORE reading the state they hold.
    ///
    /// This call also blocks (briefly, and bounded) until the background
    /// thread has attempted its OS watch registration, so it returns only
    /// once the watch is either armed or has fallen back to degraded
    /// polling. A caller that starts generating filesystem events the
    /// moment this returns is not racing thread-spawn scheduling to do so.
    pub fn start_checked(
        config: WatchConfig,
        diff_sig: SignatureFn,
        review_sig: SignatureFn,
    ) -> WatchController {
        let (diff_state, diff_err) = eval(&diff_sig);
        let (review_state, _review_err) = eval(&review_sig);
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending::default()),
            cv: Condvar::new(),
            diff_track: Mutex::new(SigTrack {
                sig: diff_sig,
                state: diff_state,
            }),
            review_track: Mutex::new(SigTrack {
                sig: review_sig,
                state: review_state,
            }),
            last_diff_error: Mutex::new(diff_err),
            armed: Mutex::new(None),
            armed_cv: Condvar::new(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || run(config, thread_shared, thread_stop));

        // Wait for the background thread's setup handshake. Bounded so a
        // stuck/never-scheduled thread cannot hang the caller forever; in
        // that pathological case we proceed anyway (the thread will still
        // catch up and mark itself armed/degraded once it does run).
        {
            let mut armed = shared.armed.lock().expect("armed lock");
            let deadline = Instant::now() + Duration::from_secs(2);
            while armed.is_none() {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let (guard, _timeout) = shared
                    .armed_cv
                    .wait_timeout(armed, deadline - now)
                    .expect("armed condvar wait");
                armed = guard;
            }
        }

        WatchController {
            shared,
            stop,
            handle: Some(handle),
        }
    }

    /// Swap the diff signature function (e.g. the review file's `source`
    /// changed underneath a running watch). Rebaselines synchronously, on
    /// the caller's thread, against the new closure right here; the
    /// background thread picks up the new closure and baseline on its next
    /// poll, so the swap itself never spuriously fires a refresh.
    pub fn set_diff_signature(&self, diff_sig: SignatureFn) {
        let (state, err) = eval(&diff_sig);
        {
            let mut track = self.shared.diff_track.lock().expect("diff track lock");
            track.sig = diff_sig;
            track.state = state;
        }
        *self.shared.last_diff_error.lock().expect("diff error lock") = err;
    }

    /// The most recent diff-signature failure, if the latest evaluation
    /// failed; `None` once it succeeds again.
    pub fn last_diff_error(&self) -> Option<String> {
        self.shared
            .last_diff_error
            .lock()
            .expect("diff error lock")
            .clone()
    }

    /// Non-blocking poll for a pending refresh (frontend event loops). At
    /// most one pending `Review` and one pending `Diff` ever exist: repeated
    /// changes of the same kind before a consumer drains coalesce into a
    /// single flag rather than queuing unboundedly.
    pub fn try_recv(&self) -> Option<Refresh> {
        self.shared.pending.lock().expect("pending lock").take()
    }

    /// Blocking wait with timeout (engine notification loop).
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Refresh> {
        let mut pending = self.shared.pending.lock().expect("pending lock");
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(r) = pending.take() {
                return Some(r);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, result) = self
                .shared
                .cv
                .wait_timeout(pending, deadline - now)
                .expect("condvar wait");
            pending = guard;
            if result.timed_out() && pending.take().is_none() {
                return None;
            }
            // A spurious wake with nothing pending loops back to re-check
            // the deadline; a real notification is handled by the `if let`
            // above on the next iteration (or was just consumed).
            if !pending.review && !pending.diff && Instant::now() >= deadline {
                return None;
            }
        }
    }

    /// Signal the background thread to stop and join it.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.shared.cv.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for WatchController {
    fn drop(&mut self) {
        // Signals without joining: dropping must never block. Callers that
        // want a clean join use `stop()`.
        self.stop.store(true, Ordering::Relaxed);
        self.shared.cv.notify_all();
    }
}

fn run(config: WatchConfig, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    let (event_tx, event_rx) = channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = event_tx.send(res);
    })
    .ok();
    let mut degraded = match &mut watcher {
        Some(w) => w.watch(&config.root, RecursiveMode::Recursive).is_err(),
        None => true,
    };

    // Handshake with `start_checked`: OS watch registration has now been
    // attempted (armed or degraded), so a caller blocked there can proceed.
    {
        let mut armed = shared.armed.lock().expect("armed lock");
        *armed = Some(!degraded);
    }
    shared.armed_cv.notify_all();

    let review_name = config
        .review_file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(crate::review::REVIEW_FILE_NAME)
        .to_string();

    // Seed with whichever interval already applies: `degraded` is already
    // known (the watcher either armed or it didn't) by the time the
    // scheduler is built, and a degraded start must poll at the degraded
    // cadence from the very first iteration, not wait out a full normal
    // poll_interval first.
    let initial_interval = if degraded {
        config.degraded_poll
    } else {
        config.poll_interval
    };
    let mut scheduler = Scheduler::new(
        Instant::now(),
        config.debounce_quiet,
        config.debounce_max,
        initial_interval,
    );

    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match event_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(event)) => {
                let now = Instant::now();
                for path in &event.paths {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    // Match the review file by name, not full path: macOS
                    // FSEvents reports canonicalized paths (/private/var vs
                    // /var), so exact-path comparison silently misses.
                    if name == review_name {
                        scheduler.mark(Refresh::Review, now);
                        continue;
                    }
                    if crate::review::is_review_sidecar(name) {
                        continue; // our own lock/guard/temp churn
                    }
                    scheduler.mark(Refresh::Diff, now);
                }
            }
            Ok(Err(_)) => {
                // Watcher error mid-flight: degrade to polling.
                degraded = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                degraded = true;
                // Drain path: without a watcher only polling remains; block
                // briefly to avoid a hot loop.
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        let interval = if degraded {
            config.degraded_poll
        } else {
            config.poll_interval
        };
        let now = Instant::now();

        if scheduler.due(Refresh::Review, now) {
            check_review(&shared);
            scheduler.checked(Refresh::Review, now, interval);
        }
        if scheduler.due(Refresh::Diff, now) {
            check_diff(&shared);
            scheduler.checked(Refresh::Diff, now, interval);
        }
    }
}

fn signal(shared: &Shared, refresh: Refresh) {
    {
        let mut pending = shared.pending.lock().expect("pending lock");
        match refresh {
            Refresh::Review => pending.review = true,
            Refresh::Diff => pending.diff = true,
        }
    }
    shared.cv.notify_all();
}

fn check_review(shared: &Shared) {
    let mut track = shared.review_track.lock().expect("review track lock");
    let (new_state, _err) = eval(&track.sig);
    if new_state != track.state {
        track.state = new_state;
        drop(track);
        signal(shared, Refresh::Review);
    }
}

fn check_diff(shared: &Shared) {
    let mut track = shared.diff_track.lock().expect("diff track lock");
    let (new_state, err) = eval(&track.sig);
    let changed = new_state != track.state;
    if changed {
        track.state = new_state;
    }
    drop(track);
    // The latest evaluation's error (or lack of one) is always surfaced,
    // whether or not the state category changed, so a still-failing signal
    // keeps its most recent message visible.
    *shared.last_diff_error.lock().expect("diff error lock") = err;
    if changed {
        signal(shared, Refresh::Diff);
    }
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn quiet_debounce_fires_after_the_quiet_window() {
        let quiet = Duration::from_millis(200);
        let max = Duration::from_secs(1);
        let poll = Duration::from_secs(10);
        let now = t0();
        let mut s = Scheduler::new(now, quiet, max, poll);
        s.mark(Refresh::Diff, now);
        assert!(!s.due(Refresh::Diff, now));
        assert!(s.due(Refresh::Diff, now + quiet));
    }

    #[test]
    fn max_debounce_fires_even_under_continuous_dirtying() {
        let quiet = Duration::from_millis(200);
        let max = Duration::from_millis(500);
        let poll = Duration::from_secs(10);
        let now = t0();
        let mut s = Scheduler::new(now, quiet, max, poll);
        s.mark(Refresh::Diff, now);
        let mut t = now;
        for _ in 0..4 {
            t += Duration::from_millis(100);
            s.mark(Refresh::Diff, t);
            assert!(
                !s.due(Refresh::Diff, t),
                "quiet window keeps resetting, still under the max cap"
            );
        }
        t += Duration::from_millis(100);
        s.mark(Refresh::Diff, t);
        assert!(
            s.due(Refresh::Diff, t),
            "the max window fires despite continuous dirtying"
        );
    }

    #[test]
    fn safety_poll_fires_with_no_dirty_marks() {
        let now = t0();
        let poll = Duration::from_secs(10);
        let s = Scheduler::new(
            now,
            Duration::from_millis(200),
            Duration::from_secs(1),
            poll,
        );
        assert!(!s.due(Refresh::Review, now + Duration::from_secs(9)));
        assert!(s.due(Refresh::Review, now + poll));
    }

    #[test]
    fn checked_advances_only_its_own_track_deadline() {
        let now = t0();
        let poll = Duration::from_secs(10);
        let mut s = Scheduler::new(
            now,
            Duration::from_millis(200),
            Duration::from_secs(1),
            poll,
        );
        s.checked(Refresh::Diff, now + Duration::from_secs(1), poll);
        assert!(
            s.due(Refresh::Review, now + poll),
            "review's own deadline is untouched by diff's checked()"
        );
        assert!(!s.due(
            Refresh::Diff,
            now + Duration::from_secs(1) + poll - Duration::from_millis(1)
        ));
    }

    #[test]
    fn diff_bursts_do_not_postpone_review_safety_poll() {
        // B13 regression: repeated diff-only checks must never push out
        // review's own safety-poll deadline.
        let now = t0();
        let poll = Duration::from_secs(10);
        let mut s = Scheduler::new(
            now,
            Duration::from_millis(50),
            Duration::from_millis(200),
            poll,
        );
        let mut t = now;
        for _ in 0..50 {
            t += Duration::from_millis(150);
            s.mark(Refresh::Diff, t);
            if s.due(Refresh::Diff, t) {
                s.checked(Refresh::Diff, t, poll);
            }
        }
        assert!(
            s.due(Refresh::Review, now + poll),
            "review safety poll must not be starved by diff-only activity"
        );
    }

    #[test]
    fn review_bursts_do_not_postpone_diff_safety_poll() {
        let now = t0();
        let poll = Duration::from_secs(10);
        let mut s = Scheduler::new(
            now,
            Duration::from_millis(50),
            Duration::from_millis(200),
            poll,
        );
        let mut t = now;
        for _ in 0..50 {
            t += Duration::from_millis(150);
            s.mark(Refresh::Review, t);
            if s.due(Refresh::Review, t) {
                s.checked(Refresh::Review, t, poll);
            }
        }
        assert!(
            s.due(Refresh::Diff, now + poll),
            "diff safety poll must not be starved by review-only activity"
        );
    }

    #[test]
    fn checked_uses_the_supplied_interval_for_degraded_mode() {
        let now = t0();
        let normal = Duration::from_secs(10);
        let degraded = Duration::from_millis(500);
        let mut s = Scheduler::new(
            now,
            Duration::from_millis(50),
            Duration::from_millis(200),
            normal,
        );
        s.checked(Refresh::Diff, now, degraded);
        assert!(!s.due(Refresh::Diff, now + Duration::from_millis(100)));
        assert!(s.due(Refresh::Diff, now + degraded));
    }

    #[test]
    fn next_wakeup_is_zero_once_due() {
        let now = t0();
        let poll = Duration::from_secs(10);
        let s = Scheduler::new(
            now,
            Duration::from_millis(200),
            Duration::from_secs(1),
            poll,
        );
        assert_eq!(s.next_wakeup(now + poll), Duration::ZERO);
        assert!(s.next_wakeup(now) > Duration::ZERO);
    }
}
