//! Monotonic and realtime clocks.
//!
//! # Why the clock is a parameter and not `std::time::Instant`
//!
//! Every deadline in zinit is a monotonic millisecond stamp handed to
//! [`zcore`](https://docs.rs/zcore) as `now_ms`. That has two consequences:
//!
//! 1. `zcore` stays pure — it never reads a clock, so a test can feed it
//!    10<sup>6</sup> arbitrary timestamps and the reconciler is still correct.
//! 2. This module must expose a clock that is *controllable in tests*. A test
//!    that wants "a service that has been starting for 31 seconds" must be able
//!    to say so without sleeping for 31 seconds.
//!
//! That is why there is no `std::time::Instant` anywhere in this crate: an
//! `Instant` cannot be constructed from a raw value, so every test would have
//! to be a slow, flaky, wall-clock test. The kernel's `CLOCK_MONOTONIC` is read
//! directly through [`sys::try_clock_gettime_ms`](crate::sys::try_clock_gettime_ms) instead,
//! and [`ManualClock`] provides the injectable half.
//!
//! # Monotonic is not realtime
//!
//! * [`now_ms`] — `CLOCK_MONOTONIC`. Never jumps, unaffected by `settimeofday`,
//!   unaffected by NTP steps. This is what every deadline, budget and
//!   restart-backoff uses. It is meaningless to a human.
//! * [`now_realtime_ms`] — `CLOCK_REALTIME`. Wall clock. It *does* jump, and a
//!   supervisor that used it for deadlines would either restart services after
//!   an NTP correction or hang for an hour. It is used for exactly two things:
//!   `utmp`/`wtmp` login records and human-readable log lines.
//!
//! The two are never mixed. A function that takes a deadline wants
//! [`now_ms`]; a function that writes a timestamp wants [`now_realtime_ms`].

use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use std::io;
use std::sync::OnceLock;

/// A source of time.
///
/// The supervisor owns one [`SystemClock`]. Tests own a [`ManualClock`] and
/// hand it to the code under test, so timeouts and budgets are decided by
/// arithmetic instead of by how fast the machine is.
pub trait Clock {
    /// Milliseconds from an arbitrary, monotonically increasing origin.
    ///
    /// The origin is boot time on a real system and 0 on a [`ManualClock`];
    /// only differences are ever meaningful.
    fn now_ms(&self) -> u64;

    /// Milliseconds since the Unix epoch.
    fn now_realtime_ms(&self) -> u64;
}

/// The real clock: `clock_gettime(CLOCK_MONOTONIC)` and `CLOCK_REALTIME`.
///
/// Cheap enough to call in a loop (it is a `vDSO` call on Linux, a real
/// syscall on some BSDs) and side-effect free, so it is not worth caching.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        // The fallback inside `monotonic_ns` covers kernels or clock sources
        // without CLOCK_MONOTONIC; see `monotonic_source` for what happens
        // there, because it is a degradation, not an error.
        crate::sys::clock_gettime_ms(crate::sys::ClockId::Monotonic)
    }

    fn now_realtime_ms(&self) -> u64 {
        crate::sys::clock_gettime_ms(crate::sys::ClockId::Realtime)
    }
}

/// A clock the test drives by hand.
///
/// Monotonic and realtime advance together, because a fake clock where the two
/// disagree would hide exactly the class of bug (a deadline computed on one,
/// compared against the other) that this crate exists to prevent.
#[derive(Debug)]
pub struct ManualClock {
    /// Milliseconds since boot, i.e. the monotonic value.
    monotonic_ms: AtomicU64,
    /// Milliseconds since the epoch, i.e. the realtime value.
    realtime_ms: AtomicU64,
}

impl ManualClock {
    /// A clock reading 0 on both timelines, with the monotonic origin at 0.
    pub const fn new() -> Self {
        Self {
            monotonic_ms: AtomicU64::new(0),
            realtime_ms: AtomicU64::new(0),
        }
    }

    /// Move both timelines forward. The only way a test makes time pass.
    pub fn advance(&self, by: Duration) {
        let ms = duration_ms_u64(by);
        self.monotonic_ms.fetch_add(ms, Ordering::SeqCst);
        self.realtime_ms.fetch_add(ms, Ordering::SeqCst);
    }

    /// Jump the monotonic timeline to an absolute value.
    ///
    /// Only for tests that need a specific instant (`now_ms == 1234`), never
    /// for ordinary advancement — a backwards jump is not a thing real time
    /// does, and code that survives one is not necessarily correct.
    pub fn set_monotonic_ms(&self, ms: u64) {
        self.monotonic_ms.store(ms, Ordering::SeqCst);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.monotonic_ms.load(Ordering::SeqCst)
    }

    fn now_realtime_ms(&self) -> u64 {
        self.realtime_ms.load(Ordering::SeqCst)
    }
}

/// Milliseconds since an arbitrary monotonic origin. The supervisor's `now_ms`.
///
/// Prefer the [`Clock`] trait when a clock can be passed in; these free
/// functions are the `std`-less convenience used by code that is already
/// talking to the real system (the supervisor itself, tests of syscalls).
pub fn now_ms() -> u64 {
    SystemClock.now_ms()
}

/// Milliseconds since the Unix epoch, for `utmp` and for log lines.
pub fn now_realtime_ms() -> u64 {
    SystemClock.now_realtime_ms()
}

/// The absolute monotonic timestamp at which `d` from now expires.
///
/// Deadlines are *absolute* everywhere in zinit, never "remaining milliseconds":
/// a relative deadline that is recomputed on every loop iteration drifts, and
/// a `ready_timeout` that silently restarts whenever the loop spins is a bug
/// that only shows up under load.
pub fn deadline_from_now(d: Duration) -> u64 {
    now_ms().saturating_add(duration_ms_u64(d))
}

/// Convert a duration to whole milliseconds, rounding **up**.
///
/// Rounding up is the safe direction for a timeout: rounding down can produce
/// a zero-length wait, which turns "wait up to 500ms" into "spin".
pub fn duration_ms_u64(d: Duration) -> u64 {
    // Ceiling division on nanoseconds, not truncation on milliseconds: a
    // 999µs timeout truncated to 0 becomes a spin loop, and the whole reason
    // this function rounds up is that a timeout must never degenerate into
    // "do not wait".
    let ns = d.as_nanos();
    let rounded = ns.div_ceil(1_000_000);
    if rounded > u64::MAX as u128 {
        u64::MAX
    } else {
        rounded as u64
    }
}

/// Milliseconds remaining until an absolute monotonic `deadline`, floored at 0.
///
/// Used to convert an absolute deadline into the relative timeout the
/// reactor wants, which is the only thing [`crate::reactor::Reactor::wait`] accepts.
pub fn remaining_ms(deadline: u64) -> u64 {
    deadline.saturating_sub(now_ms())
}

/// Sleep the calling thread, retrying across interruptions.
///
/// `nanosleep` returning `EINTR` is routine — a supervisor is woken by signals
/// constantly — and propagating that would make every sleep in the system a
/// lie. Retrying is the only correct behaviour here.
pub fn sleep_ms(ms: u64) -> io::Result<()> {
    // `libc::time_t` is deprecated upstream (libc#1848: width changing with
    // musl 1.2.0). The seconds are still computed through it and coerced to
    // whatever `timespec::tv_sec` is declared as; `tv_sec` is at least 32 bits
    // on every platform in scope and 64 bits on every 64-bit one, so this
    // cannot truncate a value that matters.
    #[allow(deprecated)]
    let secs = (ms / 1000) as libc::time_t;
    // `tv_nsec` is `c_long`: 64 bits on every 64-bit target, 32 bits on
    // 32-bit ones. An unadorned `as i64` compiles on x86_64 and breaks on
    // i686 — this exact failure is what the CI's 32-bit jobs exist to catch.
    // The value is bounded by construction (sub-second nanos), so the
    // narrowing coercion cannot truncate.
    let nsecs = ((ms % 1000) * 1_000_000) as libc::c_long;
    let req = libc::timespec {
        tv_sec: secs,
        tv_nsec: nsecs,
    };
    let mut rem = req;
    loop {
        // SAFETY: `rem` is a live, correctly aligned `timespec`, and
        // `nanosleep` only writes to it through the out-pointer. It is not
        // allowed to retain the pointer past the call.
        let rc = unsafe { libc::nanosleep(&req, &raw mut rem) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

pub use crate::sys::ClockId;

/// How [`now_ms`] is actually being computed on this machine.
///
/// It is a public value because the answer must be printed at boot. A silently
/// degraded clock is exactly the kind of difference between two machines that
/// turns into a bug report nobody can reproduce.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClockSource {
    /// `CLOCK_MONOTONIC` works. Everything is as designed.
    Monotonic,
    /// `CLOCK_MONOTONIC` is unavailable; [`now_ms`] reads `CLOCK_REALTIME`.
    ///
    /// Deadlines still *work* — they are still compared against each other —
    /// but they inherit the wall clock's discontinuities. A backwards step
    /// shortens every pending timeout, including `stop_timeout`; a forward step
    /// can make the supervisor believe a service has been starting for a year.
    RealtimeFallback,
}

/// Decide, once per process, whether `CLOCK_MONOTONIC` is usable.
///
/// The probe is a real `clock_gettime`, not a compile-time assumption, because
/// a container or an emulated kernel can disagree with the headers. The result
/// is cached in a [`OnceLock`]: the answer cannot change while the process
/// runs, and the fallback path prints a warning exactly once.
pub fn clock_source() -> ClockSource {
    static SOURCE: OnceLock<ClockSource> = OnceLock::new();
    *SOURCE.get_or_init(|| {
        match crate::sys::try_clock_gettime_ms(crate::sys::ClockId::Monotonic) {
            Ok(_) => ClockSource::Monotonic,
            Err(_) => {
                // Deliberately not a logging call: `zrt` owns no log sink, and
                // an init must not lose the one line that explains why its
                // timing behaves differently. Stderr of PID 1 is the log, and
                // `report` is the single module allowed to write to it.
                crate::report::announce_degradation(
                    "CLOCK_MONOTONIC unavailable; falling back to CLOCK_REALTIME. \
                     Deadlines inherit wall-clock discontinuities.",
                );
                ClockSource::RealtimeFallback
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_never_goes_backwards() {
        let a = now_ms();
        // Busy-wait: this test is asserting on the kernel's guarantees, so it
        // must not depend on a sleep being long enough.
        while now_ms() == a {}
        let b = now_ms();
        assert!(b >= a);
    }

    #[test]
    fn realtime_is_plausible() {
        // 2020-01-01T00:00:00Z in ms. If this is wildly off, CLOCK_REALTIME
        // is not what we think it is and every log line would be garbage.
        let y2020: u64 = 1_577_836_800_000;
        let now = now_realtime_ms();
        assert!(
            now > y2020,
            "realtime clock looks wrong: {now} ms since epoch"
        );
    }

    #[test]
    fn clock_source_is_monotonic_on_this_machine() {
        assert_eq!(clock_source(), ClockSource::Monotonic);
    }

    #[test]
    fn manual_clock_only_moves_when_told() {
        let c = ManualClock::new();
        assert_eq!(c.now_ms(), 0);
        assert_eq!(c.now_realtime_ms(), 0);
        c.advance(Duration::from_millis(1500));
        assert_eq!(c.now_ms(), 1500);
        assert_eq!(c.now_realtime_ms(), 1500);
    }

    #[test]
    fn duration_rounds_up_so_a_timeout_never_becomes_a_spin() {
        assert_eq!(duration_ms_u64(Duration::from_micros(1)), 1);
        assert_eq!(duration_ms_u64(Duration::from_micros(1999)), 2);
        assert_eq!(duration_ms_u64(Duration::from_millis(7)), 7);
        assert_eq!(
            duration_ms_u64(Duration::from_millis(1999) + Duration::from_micros(1)),
            2000
        );
        assert_eq!(duration_ms_u64(Duration::ZERO), 0);
    }

    #[test]
    fn deadlines_are_absolute() {
        let before = now_ms();
        let deadline = deadline_from_now(Duration::from_millis(5_000));
        let after = now_ms();
        // Exactly one of the two readings must bracket the deadline: a
        // relative deadline recomputed per iteration would drift, an absolute
        // one cannot.
        assert!(deadline >= before + 5_000 && deadline <= after + 5_000);
    }

    #[test]
    fn remaining_never_goes_negative() {
        assert_eq!(remaining_ms(now_ms() - 10_000), 0);
    }

    #[test]
    fn sleep_actually_sleeps() {
        let before = now_ms();
        sleep_ms(25).expect("nanosleep is available everywhere we build");
        assert!(now_ms().saturating_sub(before) >= 20, "slept too little");
    }
}
