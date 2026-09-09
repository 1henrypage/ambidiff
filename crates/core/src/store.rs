//! Filesystem-backed review store: the native layer around the pure
//! `review` module.
//!
//! Concurrency contract (agents, CLI verbs, and three frontends all write
//! the same file): every read-modify-write takes an OS advisory lock on a
//! persistent, never-unlinked `.ambidiff.json.guard` file for the complete
//! transaction (see [`Lock`] for why the guard exists alongside the
//! documented `.ambidiff.json.lock` sidecar), and every write goes through a
//! same-directory temp file plus rename so readers never observe a torn
//! file. Direct whole-file edits by agents bypass the lock; salvage-mode
//! reads plus write atomicity make the rare last-write-wins survivable.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::review::{
    GUARD_FILE_NAME, LOCK_FILE_NAME, LoadOutcome, REVIEW_FILE_NAME, ReviewError, ReviewFile,
    TMP_FILE_PREFIX, parse_review, to_json,
};
use crate::sys::{Liveness, fresh_nonce, unix_seconds};

/// Total time to wait for the guard's OS lock (and, within it, for a
/// legacy pre-repair sidecar to clear) before giving up.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// Initial retry interval while waiting; doubles up to [`BACKOFF_MAX`].
const BACKOFF_INITIAL: Duration = Duration::from_millis(10);
const BACKOFF_MAX: Duration = Duration::from_millis(200);

/// Age after which crash-leftover temp files are swept during a save.
const TMP_SWEEP_SECS: u64 = 600;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no review here: {path} not found (run `ambidiff init`)")]
    NotInitialized { path: String },
    #[error("a review already exists at {path}")]
    AlreadyInitialized { path: String },
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("review file is locked by {holder}; timed out after {waited:?}")]
    Locked { holder: String, waited: Duration },
    #[error("review file is read-only: {reason}")]
    ReadOnly { reason: String },
    #[error("lock sidecar at {path} could not be read as a valid ownership record: {detail}")]
    LockUnreadable { path: String, detail: String },
    #[error("refusing to use {path}: {reason}")]
    UnsafePath { path: String, reason: String },
    #[error(transparent)]
    Review(#[from] ReviewError),
}

fn io_err(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> StoreError {
    let context = context.into();
    move |source| StoreError::Io { context, source }
}

/// Refuse a symlink at `path`: review/ownership files are never followed or
/// replaced through a link, accidentally or otherwise.
fn reject_symlink(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(StoreError::UnsafePath {
            path: path.display().to_string(),
            reason: "refusing to follow a symlink".to_string(),
        }),
        _ => Ok(()),
    }
}

/// Walk up from `start` looking for a directory containing `.ambidiff.json`.
pub fn find_review_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    loop {
        if dir.join(REVIEW_FILE_NAME).is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Handle to the review file at one review root.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Store { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn review_path(&self) -> PathBuf {
        self.root.join(REVIEW_FILE_NAME)
    }

    /// The create-exclusive ownership sidecar (documented, legacy-compatible).
    pub fn lock_path(&self) -> PathBuf {
        self.root.join(LOCK_FILE_NAME)
    }

    /// The persistent, never-unlinked guard whose OS advisory lock is held
    /// for the complete transaction. See [`Lock`].
    pub fn guard_path(&self) -> PathBuf {
        self.root.join(GUARD_FILE_NAME)
    }

    pub fn exists(&self) -> bool {
        self.review_path().is_file()
    }

    /// Salvage-mode load. Errors only when the file is missing or beyond
    /// salvage (invalid JSON / non-object root).
    pub fn load(&self) -> Result<LoadOutcome, StoreError> {
        let path = self.review_path();
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NotInitialized {
                    path: path.display().to_string(),
                });
            }
            Err(e) => return Err(io_err(format!("read {}", path.display()))(e)),
        };
        Ok(parse_review(&content)?)
    }

    /// Create the review file; fails if one already exists.
    pub fn init(&self, review: &ReviewFile) -> Result<(), StoreError> {
        if self.exists() {
            return Err(StoreError::AlreadyInitialized {
                path: self.review_path().display().to_string(),
            });
        }
        let _lock = Lock::acquire(&self.guard_path(), &self.lock_path(), LOCK_WAIT)?;
        if self.exists() {
            return Err(StoreError::AlreadyInitialized {
                path: self.review_path().display().to_string(),
            });
        }
        self.write_atomic(review)
    }

    /// Locked read-modify-write. Loads in salvage mode, refuses read-only
    /// outcomes, applies `f`, validates the result, and writes atomically.
    /// Returns `f`'s value and any salvage warnings from the load.
    pub fn mutate<T>(
        &self,
        f: impl FnOnce(&mut ReviewFile) -> Result<T, ReviewError>,
    ) -> Result<(T, Vec<String>), StoreError> {
        let _lock = Lock::acquire(&self.guard_path(), &self.lock_path(), LOCK_WAIT)?;
        let outcome = self.load()?;
        if outcome.read_only {
            return Err(StoreError::ReadOnly {
                reason: outcome
                    .read_only_reason
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            });
        }
        let mut review = outcome.review;
        let value = f(&mut review)?;
        self.write_atomic(&review)?;
        Ok((value, outcome.warnings))
    }

    /// Atomic write: validate, serialize, same-directory randomly-named temp
    /// file, fsync, rename over the review file (refusing a symlink
    /// destination), best-effort directory sync. Preserves the existing
    /// file's permissions. Also sweeps old crash-leftover temp files.
    /// Failure unlinks only our own temp file (via `NamedTempFile`'s drop).
    fn write_atomic(&self, review: &ReviewFile) -> Result<(), StoreError> {
        review.validate()?;
        let content = to_json(review)?;
        self.sweep_stale_tmp_files();

        let review_path = self.review_path();
        reject_symlink(&review_path)?;

        let existing_perms = fs::metadata(&review_path).ok().map(|m| m.permissions());

        let mut tmp = tempfile::Builder::new()
            .prefix(TMP_FILE_PREFIX)
            .rand_bytes(16)
            .tempfile_in(&self.root)
            .map_err(io_err(format!(
                "create temp file in {}",
                self.root.display()
            )))?;
        tmp.write_all(content.as_bytes())
            .map_err(io_err(format!("write {}", tmp.path().display())))?;
        tmp.as_file()
            .sync_all()
            .map_err(io_err(format!("sync {}", tmp.path().display())))?;
        if let Some(perms) = existing_perms {
            fs::set_permissions(tmp.path(), perms)
                .map_err(io_err(format!("chmod {}", tmp.path().display())))?;
        }

        if let Err(e) = tmp.persist(&review_path) {
            // `e.file` (the still-open NamedTempFile) unlinks itself on
            // drop, so only the destination and any pre-existing content
            // survive a failed rename.
            return Err(io_err(format!("rename into {}", review_path.display()))(
                e.error,
            ));
        }

        // Directory sync makes the rename durable; failure is non-fatal.
        if let Ok(dir) = fs::File::open(&self.root) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    /// Remove `.ambidiff.json.tmp.*` files older than the sweep threshold
    /// (crash leftovers). Never fails.
    fn sweep_stale_tmp_files(&self) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with(TMP_FILE_PREFIX) {
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_none_or(|age| age.as_secs() > TMP_SWEEP_SECS);
            if stale {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// Ownership record written into the sidecar. Unknown/added fields are
/// ignored by an older reader, so an old (pre-repair) binary's `.lock`
/// parsing still works: it just never recognises `nonce` and falls back to
/// its own (unsafe, age-based) staleness rule for whatever it finds.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct LockInfo {
    pid: u32,
    #[serde(rename = "acquiredAt")]
    acquired_at: i64,
    /// Present only on sidecars created under the guard protocol
    /// (repaired binaries). Its presence, not its value, is what
    /// `Lock::acquire` uses to classify a leftover sidecar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nonce: Option<String>,
}

/// How an existing sidecar found under `create_new` is classified, once we
/// hold the guard's OS lock exclusively.
enum Classify {
    /// Has a nonce: it was created by a guard-protocol binary. Since the
    /// guard's OS lock is held for a holder's *entire* transaction and we
    /// now hold that very lock ourselves, that holder cannot possibly still
    /// be alive and mid-transaction. Safe to remove unconditionally.
    Guarded,
    /// No nonce (a pre-repair sidecar, or one from a process that somehow
    /// lost its nonce): its owning pid is provably dead.
    LegacyDead,
    /// No nonce and the owning pid is alive or its liveness is unknown: not
    /// provably abandoned, so it is never removed on our say-so.
    LegacyLive,
    /// The sidecar vanished between `create_new` failing and our read (a
    /// racing reclaimer got there first): retry immediately.
    Vanished,
    /// Present but not parseable as [`LockInfo`] at all: unknown/corrupt
    /// ownership is a recoverable lock error, never permission to delete.
    Unreadable,
}

fn classify_sidecar(path: &Path) -> Classify {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Classify::Vanished,
        Err(_) => return Classify::Unreadable,
    };
    match serde_json::from_str::<LockInfo>(&content) {
        Ok(info) if info.nonce.is_some() => Classify::Guarded,
        Ok(info) => {
            #[cfg(unix)]
            let liveness = crate::sys::process_liveness(info.pid);
            #[cfg(not(unix))]
            let liveness = Liveness::Indeterminate;
            match liveness {
                Liveness::Dead => Classify::LegacyDead,
                Liveness::Alive | Liveness::Indeterminate => Classify::LegacyLive,
            }
        }
        Err(_) => Classify::Unreadable,
    }
}

fn describe_holder(sidecar_path: &Path) -> String {
    fs::read_to_string(sidecar_path)
        .ok()
        .and_then(|c| serde_json::from_str::<LockInfo>(&c).ok())
        .map(|i| format!("pid {}", i.pid))
        .unwrap_or_else(|| "unknown process".to_string())
}

fn open_guard(path: &Path) -> Result<File, StoreError> {
    reject_symlink(path)?;
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true);
    // Never truncate, never unlink: the guard's whole value is a stable
    // inode that every process opens and locks, so it must survive exactly
    // as-is across every acquire.
    #[cfg(unix)]
    {
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)
        .map_err(io_err(format!("open guard {}", path.display())))
}

/// Held lock on the review file. Released on drop: the sidecar is removed
/// first (only if it still carries this holder's pid+nonce), then the
/// guard's OS lock is released.
///
/// Why not just `flock` the sidecar directly: every pre-repair binary (and
/// every peer using the documented `.ambidiff.json.lock` contract) unlinks
/// and recreates that file on acquire/release, so a second process taking
/// an OS lock on "the sidecar path" actually locks a *different inode* than
/// the first the moment either side recreates it. A persistent, never
/// unlinked `.ambidiff.json.guard` file gives one stable inode on which
/// sidecar creation, stale recovery, and owner-checked release are all
/// serialised; the OS lock on that guard is held for the complete
/// transaction, which is the actual safety property. The documented
/// `.ambidiff.json.lock` sidecar is still written/removed alongside it,
/// purely so an unmodified pre-repair binary can still see "someone is
/// editing" (its own age-based staleness logic remains unsafe, which is why
/// closing older ambidiff instances before upgrading is documented).
#[derive(Debug)]
pub struct Lock {
    guard: File,
    sidecar: PathBuf,
    owner: LockInfo,
}

impl Lock {
    /// Acquire the lock, waiting up to `timeout` in total across both the
    /// initial guard-lock wait and any legacy-sidecar wait.
    pub fn acquire(
        guard_path: &Path,
        sidecar_path: &Path,
        timeout: Duration,
    ) -> Result<Lock, StoreError> {
        reject_symlink(sidecar_path)?;
        let guard = open_guard(guard_path)?;
        let start = Instant::now();

        // Phase 1: take the guard's OS advisory lock. This alone serialises
        // every guard-protocol writer; a legacy sidecar cannot block it.
        let mut backoff = BACKOFF_INITIAL;
        loop {
            match guard.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    let elapsed = start.elapsed();
                    if elapsed >= timeout {
                        return Err(StoreError::Locked {
                            holder: describe_holder(sidecar_path),
                            waited: elapsed,
                        });
                    }
                    std::thread::sleep(backoff.min(timeout - elapsed));
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
                Err(TryLockError::Error(e)) => {
                    return Err(io_err(format!("lock guard {}", guard_path.display()))(e));
                }
            }
        }

        // Inode check: the guard we locked must still be the file at
        // `guard_path` (we never unlink it ourselves, but this guards
        // against external interference replacing it out from under us).
        #[cfg(unix)]
        {
            if let (Ok(by_fd), Ok(by_path)) = (guard.metadata(), fs::metadata(guard_path))
                && by_fd.ino() != by_path.ino()
            {
                let _ = guard.unlock();
                return Err(StoreError::UnsafePath {
                    path: guard_path.display().to_string(),
                    reason: "guard file was replaced while acquiring its lock".to_string(),
                });
            }
        }

        // Phase 2: create the documented sidecar (legacy compatibility) now
        // that we hold the guard exclusively for the whole transaction.
        loop {
            let owner = LockInfo {
                pid: std::process::id(),
                acquired_at: unix_seconds(),
                nonce: Some(format!("{:016x}", fresh_nonce())),
            };
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(sidecar_path)
            {
                Ok(mut file) => {
                    let body = serde_json::to_string(&owner).unwrap_or_default();
                    let _ = file.write_all(body.as_bytes());
                    let _ = file.sync_all();
                    return Ok(Lock {
                        guard,
                        sidecar: sidecar_path.to_path_buf(),
                        owner,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    match classify_sidecar(sidecar_path) {
                        Classify::Guarded | Classify::LegacyDead | Classify::Vanished => {
                            let _ = fs::remove_file(sidecar_path);
                            continue;
                        }
                        Classify::LegacyLive => {
                            let elapsed = start.elapsed();
                            if elapsed >= timeout {
                                let _ = guard.unlock();
                                return Err(StoreError::Locked {
                                    holder: describe_holder(sidecar_path),
                                    waited: elapsed,
                                });
                            }
                            std::thread::sleep(Duration::from_millis(50).min(timeout - elapsed));
                        }
                        Classify::Unreadable => {
                            let elapsed = start.elapsed();
                            if elapsed >= timeout {
                                let _ = guard.unlock();
                                return Err(StoreError::LockUnreadable {
                                    path: sidecar_path.display().to_string(),
                                    detail: "sidecar contents are not a valid ownership record"
                                        .to_string(),
                                });
                            }
                            std::thread::sleep(Duration::from_millis(50).min(timeout - elapsed));
                        }
                    }
                }
                Err(e) => {
                    let _ = guard.unlock();
                    return Err(io_err(format!("create sidecar {}", sidecar_path.display()))(e));
                }
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Remove the sidecar only if it still carries our pid+nonce: a
        // sidecar that was reclaimed out from under us (or already
        // replaced by a new owner) must survive.
        if let Ok(content) = fs::read_to_string(&self.sidecar)
            && let Ok(current) = serde_json::from_str::<LockInfo>(&content)
            && current.pid == self.owner.pid
            && current.nonce == self.owner.nonce
        {
            let _ = fs::remove_file(&self.sidecar);
        }
        // Sidecar removal precedes guard release.
        let _ = self.guard.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::Source;

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn new_review() -> ReviewFile {
        ReviewFile::new("r".into(), Source::git(Some("main".into())), "t0")
    }

    #[test]
    fn init_load_round_trip() {
        let dir = scratch();
        let store = Store::new(dir.path());
        assert!(!store.exists());
        store.init(&new_review()).expect("init");
        let outcome = store.load().expect("load");
        assert_eq!(outcome.review.review, "r");
        assert!(outcome.warnings.is_empty());
    }

    #[test]
    fn init_twice_fails() {
        let dir = scratch();
        let store = Store::new(dir.path());
        store.init(&new_review()).expect("init");
        assert!(matches!(
            store.init(&new_review()),
            Err(StoreError::AlreadyInitialized { .. })
        ));
    }

    #[test]
    fn load_without_init_is_not_initialized() {
        let dir = scratch();
        let store = Store::new(dir.path());
        assert!(matches!(
            store.load(),
            Err(StoreError::NotInitialized { .. })
        ));
    }

    #[test]
    fn mutate_applies_and_persists() {
        let dir = scratch();
        let store = Store::new(dir.path());
        store.init(&new_review()).expect("init");
        let (rev, _) = store.mutate(|r| r.try_rev_bump("t1")).expect("mutate");
        assert_eq!(rev, 2);
        assert_eq!(store.load().expect("load").review.revision, 2);
        assert!(!store.lock_path().exists(), "sidecar released");
        assert!(store.guard_path().is_file(), "guard is never unlinked");
    }

    #[test]
    fn mutate_refuses_newer_schema() {
        let dir = scratch();
        let store = Store::new(dir.path());
        fs::write(
            store.review_path(),
            r#"{"ambidiff": 99, "review": "r", "revision": 1,
                "source": {"kind": "git"}, "createdAt": "t", "updatedAt": "t",
                "comments": []}"#,
        )
        .expect("write");
        let err = store.mutate(|_| Ok(())).expect_err("must refuse");
        assert!(matches!(err, StoreError::ReadOnly { .. }));
        // File untouched.
        assert!(
            fs::read_to_string(store.review_path())
                .expect("read")
                .contains("\"ambidiff\": 99")
        );
    }

    #[test]
    fn held_lock_blocks_and_times_out() {
        let dir = scratch();
        let store = Store::new(dir.path());
        store.init(&new_review()).expect("init");
        let _held =
            Lock::acquire(&store.guard_path(), &store.lock_path(), LOCK_WAIT).expect("acquire");
        let err = Lock::acquire(
            &store.guard_path(),
            &store.lock_path(),
            Duration::from_millis(150),
        )
        .expect_err("second acquire must time out");
        assert!(matches!(err, StoreError::Locked { .. }));
    }

    #[test]
    fn legacy_lock_from_dead_process_is_recovered() {
        let dir = scratch();
        let store = Store::new(dir.path());
        store.init(&new_review()).expect("init");

        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a short-lived child");
        let dead_pid = child.id();
        child.wait().expect("wait for it to exit");

        let legacy = serde_json::json!({"pid": dead_pid, "acquiredAt": unix_seconds()});
        fs::write(store.lock_path(), legacy.to_string()).expect("write legacy sidecar");

        let lock = Lock::acquire(
            &store.guard_path(),
            &store.lock_path(),
            Duration::from_secs(2),
        )
        .expect("a legacy sidecar from a provably dead process must be recovered");
        drop(lock);
    }

    #[test]
    fn legacy_lock_from_live_process_is_not_displaced_by_age() {
        let dir = scratch();
        let store = Store::new(dir.path());
        store.init(&new_review()).expect("init");

        // Our own process is alive; an hour old is not a licence to break it.
        let legacy = serde_json::json!({
            "pid": std::process::id(),
            "acquiredAt": unix_seconds() - 3600,
        });
        fs::write(store.lock_path(), legacy.to_string()).expect("write legacy sidecar");

        let err = Lock::acquire(
            &store.guard_path(),
            &store.lock_path(),
            Duration::from_millis(200),
        )
        .expect_err("a live legacy holder must never be displaced by age alone");
        assert!(matches!(err, StoreError::Locked { .. }));
        assert!(store.lock_path().exists(), "untouched");
    }

    #[test]
    fn unreadable_sidecar_is_recoverable_and_never_removed() {
        let dir = scratch();
        let store = Store::new(dir.path());
        store.init(&new_review()).expect("init");
        fs::write(store.lock_path(), "not json").expect("write garbage sidecar");

        let err = Lock::acquire(
            &store.guard_path(),
            &store.lock_path(),
            Duration::from_millis(200),
        )
        .expect_err("unreadable ownership cannot be safely claimed dead");
        assert!(matches!(err, StoreError::LockUnreadable { .. }));
        assert!(store.lock_path().exists(), "never deleted");
    }

    #[test]
    fn release_removes_only_own_sidecar() {
        let dir = scratch();
        let store = Store::new(dir.path());
        store.init(&new_review()).expect("init");

        let lock =
            Lock::acquire(&store.guard_path(), &store.lock_path(), LOCK_WAIT).expect("acquire");
        // Simulate the sidecar having been superseded by another holder's
        // record while we still think we hold it.
        let other = serde_json::json!({
            "pid": 999_999_u32,
            "acquiredAt": unix_seconds(),
            "nonce": "deadbeefdeadbeef",
        });
        fs::write(store.lock_path(), other.to_string()).expect("overwrite sidecar");
        drop(lock);
        assert!(
            store.lock_path().exists(),
            "must not delete a sidecar it does not own"
        );

        fs::remove_file(store.lock_path()).expect("cleanup");
        let lock2 = Lock::acquire(&store.guard_path(), &store.lock_path(), LOCK_WAIT)
            .expect("acquire again");
        drop(lock2);
        assert!(
            !store.lock_path().exists(),
            "must delete its own untouched sidecar"
        );
    }

    #[cfg(unix)]
    #[test]
    fn guard_symlink_is_refused() {
        let dir = scratch();
        let store = Store::new(dir.path());
        let target = dir.path().join("elsewhere");
        fs::write(&target, "x").expect("write");
        std::os::unix::fs::symlink(&target, store.guard_path()).expect("symlink");

        let err = Lock::acquire(
            &store.guard_path(),
            &store.lock_path(),
            Duration::from_millis(200),
        )
        .expect_err("a symlinked guard must be refused");
        assert!(matches!(err, StoreError::UnsafePath { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_symlink_is_refused() {
        let dir = scratch();
        let store = Store::new(dir.path());
        let target = dir.path().join("elsewhere");
        fs::write(&target, "x").expect("write");
        std::os::unix::fs::symlink(&target, store.lock_path()).expect("symlink");

        let err = Lock::acquire(
            &store.guard_path(),
            &store.lock_path(),
            Duration::from_millis(200),
        )
        .expect_err("a symlinked sidecar must be refused");
        assert!(matches!(err, StoreError::UnsafePath { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn review_file_symlink_destination_is_refused() {
        let dir = scratch();
        let store = Store::new(dir.path());
        let target = dir.path().join("elsewhere.json");
        std::os::unix::fs::symlink(&target, store.review_path()).expect("symlink");

        let err = store
            .init(&new_review())
            .expect_err("a symlinked review destination must be refused");
        assert!(matches!(err, StoreError::UnsafePath { .. }));
        assert!(!target.exists(), "must never write through the symlink");
    }

    #[test]
    fn rename_failure_leaves_destination_and_no_temp() {
        let dir = scratch();
        let store = Store::new(dir.path());
        // A directory at the review path makes the final rename fail
        // (EISDIR/ENOTEMPTY) without needing special permissions.
        fs::create_dir(store.review_path()).expect("mkdir");

        let err = store
            .init(&new_review())
            .expect_err("renaming onto a directory must fail");
        assert!(matches!(err, StoreError::Io { .. }));
        assert!(store.review_path().is_dir(), "destination untouched");

        let leftover: Vec<_> = fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(TMP_FILE_PREFIX))
            .collect();
        assert!(
            leftover.is_empty(),
            "temp file must be cleaned up on failure"
        );
    }

    #[cfg(unix)]
    #[test]
    fn permissions_are_preserved_on_rewrite() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch();
        let store = Store::new(dir.path());
        store.init(&new_review()).expect("init");
        fs::set_permissions(store.review_path(), fs::Permissions::from_mode(0o640)).expect("chmod");

        store.mutate(|r| r.try_rev_bump("t1")).expect("mutate");

        let mode = fs::metadata(store.review_path())
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o640, "rewrite must preserve existing permissions");
    }

    #[test]
    fn crash_leftover_tmp_files_are_swept_on_save() {
        let dir = scratch();
        let store = Store::new(dir.path());
        let leftover = dir.path().join(format!("{TMP_FILE_PREFIX}1234dead"));
        fs::write(&leftover, "torn write").expect("write");
        store
            .init(&new_review())
            .expect("init with leftover present");
        assert!(store.load().is_ok());
        assert!(leftover.exists(), "fresh tmp not swept");
    }

    #[test]
    fn find_review_root_walks_up() {
        let dir = scratch();
        let store = Store::new(dir.path());
        store.init(&new_review()).expect("init");
        let nested = dir.path().join("a/b/c");
        fs::create_dir_all(&nested).expect("mkdir");
        let found = find_review_root(&nested).expect("found");
        // Canonicalize both sides: macOS tempdirs live behind /private symlinks.
        assert_eq!(
            found.canonicalize().expect("canon"),
            dir.path().canonicalize().expect("canon")
        );
        let outside = scratch();
        assert!(find_review_root(outside.path()).is_none());
    }
}
