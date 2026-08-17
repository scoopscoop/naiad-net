//! Daily-salted, memory-only distinct-user counter (#235, Piece C).
//!
//! `blake3(salt || ip_octets)` per observation; hashes go into per-hour and
//! per-day `HashSet`s; raw IPs are never stored. Salt rotates at UTC midnight.
//! `UserCounter` is `Send + Sync` for `Arc<StatsHandle>` sharing.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Mutex;

// ─── Compile-time Send+Sync guarantee ────────────────────────────────────────

// Body is always type-checked even though the fn is never called.
fn _assert_user_counter_send_sync() {
    fn _check<T: Send + Sync>() {}
    _check::<UserCounter>();
}

// ─── Hash primitive ───────────────────────────────────────────────────────────

/// `blake3(salt || ip_octets)` — 4 bytes for IPv4, 16 for IPv6.
/// Free function so tests can verify decorrelation without a `UserCounter`.
pub(crate) fn hash_ip(salt: &[u8; 32], ip: IpAddr) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(salt);
    match ip {
        IpAddr::V4(a) => {
            h.update(&a.octets());
        }
        IpAddr::V6(a) => {
            h.update(&a.octets());
        }
    }
    *h.finalize().as_bytes()
}

// ─── Inner state (lock-guarded) ───────────────────────────────────────────────

struct Inner {
    salt: [u8; 32],
    hour_set: HashSet<[u8; 32]>,
    day_set: HashSet<[u8; 32]>,
    /// Current hour index: `unix_secs / 3600`.
    cur_hour: i64,
    /// Current UTC-day index: `unix_secs / 86400`.
    cur_day: i64,
}

// ─── Public counter ───────────────────────────────────────────────────────────

/// Memory-only distinct-user counter. `Send + Sync` for `Arc<StatsHandle>`.
///
/// `salt_src` returns `Option<[u8; 32]>`: `Some` on success, `None` on RNG
/// failure. On `None` at rotation the previous salt is reused (slight
/// cross-day correlation, acceptable) and a `warn` is emitted. `clock` is
/// not stored; `roll()` takes `now` explicitly.
pub(crate) struct UserCounter {
    inner: Mutex<Inner>,
    /// `Send + Sync + 'static`; returns `None` to signal RNG unavailability.
    salt_src: Box<dyn Fn() -> Option<[u8; 32]> + Send + Sync + 'static>,
}

impl UserCounter {
    /// Injectable constructor. `clock` is called once (seeds window indices;
    /// not stored). `salt_src` must be `Send + Sync + 'static`.
    pub(crate) fn new(
        clock: impl Fn() -> i64,
        salt_src: impl Fn() -> Option<[u8; 32]> + Send + Sync + 'static,
    ) -> Self {
        let now = clock();
        let initial_salt = salt_src().unwrap_or_else(|| {
            tracing::warn!(
                target: "stats",
                "OS RNG unavailable at UserCounter init; using zero salt"
            );
            [0u8; 32]
        });
        Self {
            inner: Mutex::new(Inner {
                salt: initial_salt,
                hour_set: HashSet::new(),
                day_set: HashSet::new(),
                cur_hour: now / 3600,
                cur_day: now / 86400,
            }),
            salt_src: Box::new(salt_src),
        }
    }

    /// Production constructor: OS-RNG salt (graceful on failure), system clock.
    pub(crate) fn production() -> Self {
        Self::new(
            || {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            },
            || {
                let mut b = [0u8; 32];
                match getrandom::getrandom(&mut b) {
                    Ok(()) => Some(b),
                    Err(e) => {
                        tracing::warn!(
                            target: "stats",
                            "getrandom failed for daily salt rotation: {e}"
                        );
                        None
                    }
                }
            },
        )
    }

    /// Hash the IP and insert into the current-hour and current-day sets.
    /// The raw IP is dropped immediately after hashing.
    pub(crate) fn observe(&self, ip: IpAddr) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let h = hash_ip(&g.salt, ip);
        g.hour_set.insert(h);
        g.day_set.insert(h);
    }

    /// Emit completed-window counts and reset crossed windows.
    ///
    /// Returns `(metric, ts, count)` rows to persist:
    /// - `("users_hour", hour_boundary_unix, distinct_count)` on hour roll
    /// - `("users_day",  day_boundary_unix,  distinct_count)`  on day roll
    ///
    /// On a day boundary the salt is rotated; if the RNG is unavailable the
    /// previous salt is reused (one `warn` emitted) rather than panicking.
    pub(crate) fn roll(&self, now: i64) -> Vec<(&'static str, i64, u64)> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();

        let hour = now / 3600;
        if hour > g.cur_hour {
            out.push(("users_hour", g.cur_hour * 3600, g.hour_set.len() as u64));
            g.hour_set.clear();
            g.cur_hour = hour;
        }

        let day = now / 86400;
        if day > g.cur_day {
            out.push(("users_day", g.cur_day * 86400, g.day_set.len() as u64));
            // Rotate salt; on RNG failure reuse the existing salt rather than
            // panicking — slight cross-day correlation is better than dead stats.
            match (self.salt_src)() {
                Some(new_salt) => g.salt = new_salt,
                None => tracing::warn!(
                    target: "stats",
                    "OS RNG unavailable at day boundary; reusing previous daily salt \
                     (slight cross-day correlation acceptable)"
                ),
            }
            g.day_set.clear();
            g.cur_day = day;
        }

        out
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, Ordering};

    // `clock` is never stored → tests may use Rc<Cell<i64>> freely.
    // `salt_src` IS stored and must be Send+Sync → tests use Arc<AtomicU8>.

    #[test]
    fn same_ip_hashes_differently_across_salt_rotation() {
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        let h_a = hash_ip(&[1u8; 32], ip);
        let h_b = hash_ip(&[2u8; 32], ip);
        assert_ne!(h_a, h_b, "a rotated salt must decorrelate the same IP");
    }

    #[test]
    fn ipv6_hashes_differently_from_ipv4() {
        // Exercises the IPv6 arm of hash_ip and confirms 4-byte vs 16-byte
        // octets produce distinct digests even with the same salt.
        let salt = [0u8; 32];
        let v4 = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let v6 = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)); // ::1
        assert_ne!(
            hash_ip(&salt, v4),
            hash_ip(&salt, v6),
            "IPv4 and IPv6 must produce different hashes"
        );
    }

    #[test]
    fn per_hour_count_equals_distinct_cardinality() {
        let now = Rc::new(Cell::new(0i64));
        let n2 = now.clone();
        let uc = UserCounter::new(move || n2.get(), || Some([9u8; 32]));
        for last in [1u8, 1, 2, 3, 3] {
            uc.observe(IpAddr::V4(Ipv4Addr::new(10, 0, 0, last)));
        }
        now.set(3600);
        let rows = uc.roll(now.get());
        let hour = rows
            .iter()
            .find(|(m, _, _)| *m == "users_hour")
            .expect("users_hour row present");
        assert_eq!(hour.2, 3);
    }

    #[test]
    fn sets_are_cleared_after_roll() {
        let now = Rc::new(Cell::new(0i64));
        let n2 = now.clone();
        let uc = UserCounter::new(move || n2.get(), || Some([1u8; 32]));
        // First window: 2 distinct IPs.
        uc.observe(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        uc.observe(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)));
        now.set(3600);
        let r1 = uc.roll(3600);
        let h1 = r1
            .iter()
            .find(|(m, _, _)| *m == "users_hour")
            .expect("users_hour row present");
        assert_eq!(h1.2, 2, "first window: 2 distinct IPs");
        // Second window: only one of the same IPs re-observed.
        uc.observe(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        now.set(7200);
        let r2 = uc.roll(7200);
        let h2 = r2
            .iter()
            .find(|(m, _, _)| *m == "users_hour")
            .expect("users_hour row present");
        assert_eq!(h2.2, 1, "hour set must have been cleared after first roll");
    }

    #[test]
    fn simultaneous_hour_and_day_boundary_emits_both_rows() {
        let now = Rc::new(Cell::new(0i64));
        let n2 = now.clone();
        let uc = UserCounter::new(move || n2.get(), || Some([7u8; 32]));
        uc.observe(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        // 86400 s = exactly one UTC day = also an hour boundary.
        now.set(86400);
        let rows = uc.roll(now.get());
        assert!(
            rows.iter().any(|(m, _, _)| *m == "users_hour"),
            "must emit users_hour on simultaneous boundary"
        );
        assert!(
            rows.iter().any(|(m, _, _)| *m == "users_day"),
            "must emit users_day on simultaneous boundary"
        );
    }

    #[test]
    fn day_boundary_rotates_salt_and_persists_day_count() {
        let now = Rc::new(Cell::new(0i64));
        let n2 = now.clone();
        let salt_calls = Arc::new(AtomicU8::new(0));
        let sc = salt_calls.clone();
        let uc = UserCounter::new(
            move || n2.get(),
            move || {
                let v = sc.fetch_add(1, Ordering::Relaxed) + 1;
                Some([v; 32])
            },
        );
        uc.observe(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        now.set(86400);
        let rows = uc.roll(now.get());
        assert!(
            rows.iter().any(|(m, _, c)| *m == "users_day" && *c == 1),
            "must emit users_day with count 1"
        );
        assert!(
            salt_calls.load(Ordering::Relaxed) >= 2,
            "salt must be regenerated at the day boundary"
        );
    }
}
