//! Native system helpers: wall clock and comment id generation.
//!
//! Kept out of the pure domain modules so domain logic stays clock- and
//! id-injected (tests pass fixed values; wasm never links this).

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::util::{fnv1a64, format_rfc3339_utc};

/// Current time as an RFC 3339 UTC string.
pub fn now_rfc3339() -> String {
    format_rfc3339_utc(unix_seconds())
}

/// Current Unix time in seconds.
pub fn unix_seconds() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// Generate a short comment id (`c-` + 4 hex bytes) unique among `existing`.
/// Seeded from time, pid, and a process-local counter; collisions retry.
pub fn generate_comment_id(existing: &[&str]) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    loop {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let seed = format!("{nanos}:{}:{count}", std::process::id());
        let id = format!("c-{:08x}", (fnv1a64(seed.as_bytes()) & 0xffff_ffff) as u32);
        if !existing.contains(&id.as_str()) {
            return id;
        }
    }
}

/// A fresh pseudo-random 64-bit value, used to give a lock sidecar an
/// ownership nonce distinct from every other process/creation. Seeded from
/// `std::collections::hash_map::RandomState` (process-random since 1.36),
/// combined with the wall clock and a process-local counter so repeated
/// calls in the same process never collide.
pub fn fresh_nonce() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(nanos);
    hasher.write_u64(count);
    hasher.write_u32(std::process::id());
    hasher.finish()
}

/// Whether a process is provably alive, provably gone, or unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Dead,
    Indeterminate,
}

/// Probe process liveness with a signal-0 `kill`: it delivers no signal but
/// still validates that the target could be signalled. `ESRCH` means the pid
/// is provably gone; `0`/`EPERM` mean it exists (we may just lack permission
/// to signal it); anything else is unknown rather than a licence to reclaim.
#[cfg(unix)]
pub fn process_liveness(pid: u32) -> Liveness {
    if pid == 0 || pid > i32::MAX as u32 {
        return Liveness::Indeterminate;
    }
    // Safety: signal 0 sends no signal; it only validates that `pid` could
    // be signalled by this process, which is exactly the liveness probe we
    // want (no side effects on the target).
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return Liveness::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(e) if e == libc::ESRCH => Liveness::Dead,
        Some(e) if e == libc::EPERM => Liveness::Alive,
        _ => Liveness::Indeterminate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_expected_shape_and_avoid_collisions() {
        let a = generate_comment_id(&[]);
        assert!(a.starts_with("c-") && a.len() == 10, "{a}");
        let b = generate_comment_id(&[&a]);
        assert_ne!(a, b);
    }

    #[test]
    fn fresh_nonce_avoids_immediate_collisions() {
        let a = fresh_nonce();
        let b = fresh_nonce();
        assert_ne!(a, b);
    }

    #[cfg(unix)]
    #[test]
    fn own_process_is_alive() {
        assert_eq!(process_liveness(std::process::id()), Liveness::Alive);
    }

    #[cfg(unix)]
    #[test]
    fn pid_zero_is_indeterminate() {
        assert_eq!(process_liveness(0), Liveness::Indeterminate);
    }

    #[cfg(unix)]
    #[test]
    fn pid_beyond_i32_max_is_indeterminate() {
        assert_eq!(process_liveness(u32::MAX), Liveness::Indeterminate);
    }

    #[cfg(unix)]
    #[test]
    fn exited_child_is_dead() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("wait");
        assert_eq!(process_liveness(pid), Liveness::Dead);
    }

    #[test]
    fn now_is_rfc3339_shaped() {
        let now = now_rfc3339();
        assert_eq!(now.len(), 20);
        assert!(now.ends_with('Z'));
        assert_eq!(&now[4..5], "-");
        assert_eq!(&now[10..11], "T");
    }
}
