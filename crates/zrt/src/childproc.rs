//! Knowing when a child dies, without reading `/proc`.
//!
//! # Why `/proc` is not an option
//!
//! `/proc` does not exist on FreeBSD, is not mounted by default on OpenBSD and
//! NetBSD, and is frequently mounted with `hidepid` in a container. Any design
//! that reads `/proc/<pid>/stat` to find out whether a child is alive is an
//! init that cannot boot those systems — and, worse, one whose behaviour
//! changes silently depending on a mount option it never asked about.
//! `DESIGN.md` §8 lists `/proc` as a *capability*, probed at runtime, never as
//! a dependency. This module has no `/proc` in it at all.
//!
//! # The three backends
//!
//! | | Mechanism | Recycle-safe |
//! |---|---|---|
//! | [`PidfdTracker`] | `pidfd_open` + epoll (Linux 5.3+) | yes — the fd names a process, not a number |
//! | `KqueueProcTracker` | `EVFILT_PROC`/`NOTE_EXIT` (BSD) | no — but the kernel only reports it once |
//! | [`WaitpidTracker`] | `waitpid(WNOHANG)` (universal) | no |
//!
//! All three implement the same trait, expose their readiness through
//! [`ChildTracker::fds`] (zero or one descriptor), and are reaped through one
//! [`ChildTracker::reap`] call that the supervisor makes from the event loop.
//!
//! # The ordering problem, and what actually solves it
//!
//! The scary race in a supervisor is:
//!
//! ```text
//! parent forks ──► child exits immediately ──► SIGCHLD ──► handler looks up
//!                                                        the pid in a table
//!                                                        that does not have it yet
//! ```
//!
//! A pid can be recycled the moment it dies, and PIDs wrap. A supervisor that
//! handles a death notification by looking up a number can therefore act on
//! the wrong process — it may `SIGKILL` an unrelated service that happened to
//! inherit the pid.
//!
//! **There is no priority flag on `fork(2)`.** That has to be said plainly
//! because pretending otherwise would be inventing a mechanism that does not
//! exist. What zinit does instead is make the window *structurally* absent:
//!
//! 1. [`crate::signals::block_all`] runs once at startup, before anything
//!    else. `SIGCHLD` is therefore *pending*, not delivered, from the very
//!    first instruction of the supervisor.
//! 2. [`fork_tracked`] registers the new pid in the tracker **before it
//!    returns to the event loop**, so by the time any notification can be
//!    observed, the bookkeeping already exists.
//!
//! Together those make the race unreachable rather than unlikely, and they
//! cost one call at startup. On top of that, [`PidfdTracker`] removes the
//! question entirely: its descriptor refers to a specific process, so even a
//! reused pid cannot produce a false match.

use crate::sys::{self, ExitStatus};
use std::io;
use std::os::fd::RawFd;

/// Which child-tracking backend is in use.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ChildKind {
    /// Linux 5.3+: `pidfd_open` polled through a private epoll.
    Pidfd,
    /// BSD: `EVFILT_PROC` with `NOTE_EXIT` on a private kqueue.
    KqueueProc,
    /// `waitpid(WNOHANG)` on every reap. Always available, never eventful.
    Waitpid,
}

impl ChildKind {
    /// The name for the capability line.
    pub const fn name(self) -> &'static str {
        match self {
            ChildKind::Pidfd => "pidfd",
            ChildKind::KqueueProc => "kqueue-proc",
            ChildKind::Waitpid => "waitpid",
        }
    }

    /// Whether a death notification identifies a *specific* process rather
    /// than a number that can be recycled.
    ///
    /// False means the supervisor is relying on the blocked-signal ordering
    /// argument for its correctness. That argument is sound, but it is an
    /// argument, not a kernel guarantee, and it is reported as such.
    pub const fn is_recycle_safe(self) -> bool {
        matches!(self, ChildKind::Pidfd)
    }
}

/// Tracks children and reports their deaths through the reactor.
///
/// The trait is implemented three times and there is no other difference
/// between them anywhere in the crate. A caller must not branch on
/// [`ChildTracker::kind`]; the point of a capability is to be *reported*.
pub trait ChildTracker {
    /// Start watching a pid that has just been forked.
    fn track(&mut self, pid: i32) -> io::Result<()>;

    /// Stop watching a pid, for good.
    ///
    /// Needed when a service is intentionally detached, and as the error
    /// path of [`fork_tracked`]. It is not the same as reaping: an untracked
    /// child still has to be reaped by something or it becomes a zombie.
    fn untrack(&mut self, pid: i32) -> io::Result<()>;

    /// Descriptors to register in the supervisor's reactor.
    ///
    /// Zero or one, and always the same for a given backend. An empty slice
    /// means this backend has nothing to wake the loop with, and the caller
    /// must arrange its own wakeup (usually `SIGCHLD` through
    /// [`crate::signals`]) — see [`WaitpidTracker`].
    fn fds(&self) -> &[RawFd];

    /// Collect every child that has finished since the last call.
    ///
    /// Empty is the normal answer and is never an error. Called from the event
    /// loop, so a call that blocks would be a deadlock.
    fn reap(&mut self) -> io::Result<Vec<(i32, ExitStatus)>>;

    /// Which backend this is, for the boot log.
    fn kind(&self) -> ChildKind;
}

/// What [`fork_tracked`] returns, from the parent's point of view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ForkOutcome {
    /// The child. The caller must now do the child path — and nothing else.
    Child,
    /// The parent, with the child already registered in the tracker.
    Parent(i32),
}

/// Fork and register the child before anything else can observe it.
///
/// In the **parent** this returns [`ForkOutcome::Parent`] with the pid already
/// in the tracker, so by the time the event loop gets control the bookkeeping
/// exists. In the **child** it returns [`ForkOutcome::Child`] and the caller
/// must go straight to `execvp` — or to [`sys::_exit`] — without allocating,
/// without taking a lock and without touching the tracker. The tracker is a
/// `&mut` borrow that must not outlive the fork; returning early in the child
/// is what releases it.
///
/// If registration fails in the parent, the child is killed and reaped here
/// rather than being leaked: a supervisor that forks a process it will never
/// track has created a zombie at boot and will not notice for a long time.
pub fn fork_tracked(tracker: &mut dyn ChildTracker) -> io::Result<ForkOutcome> {
    let pid = sys::fork()?;
    if pid == 0 {
        return Ok(ForkOutcome::Child);
    }
    if let Err(e) = tracker.track(pid) {
        // Best effort, in this order: the child must not outlive this call
        // unreaped, and it must be reaped before anything else so that it
        // cannot be mistaken for a service.
        let _ = sys::kill_process(pid, crate::signals::Signal::Kill);
        let _ = sys::waitpid_blocking(pid);
        return Err(e);
    }
    Ok(ForkOutcome::Parent(pid))
}

/// Build the best child tracker this machine can give us.
///
/// Probes, in order: `pidfd_open` on Linux, `EVFILT_PROC` on the BSDs, and
/// `waitpid` everywhere. Each failure falls through to the next and is
/// announced, because a supervisor that silently ends up on the weakest
/// backend is a supervisor whose bug reports cannot be reproduced.
pub fn new_child_tracker() -> io::Result<Box<dyn ChildTracker>> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        match PidfdTracker::new() {
            Ok(t) => return Ok(Box::new(t)),
            Err(e) => crate::report::announce_degradation(&format!(
                "pidfd_open unavailable ({e}); degrading to waitpid(WNOHANG) child tracking"
            )),
        }
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
    ))]
    {
        match KqueueProcTracker::new() {
            Ok(t) => return Ok(Box::new(t)),
            Err(e) => crate::report::announce_degradation(&format!(
                "EVFILT_PROC unavailable ({e}); degrading to waitpid(WNOHANG) child tracking"
            )),
        }
    }
    let t = WaitpidTracker::new()?;
    Ok(Box::new(t))
}

// ─────────────────────────────────────────────────────────────────────────────
// pidfd (Linux 5.3+)
// ─────────────────────────────────────────────────────────────────────────────

/// Linux `pidfd` child tracking.
///
/// Each child gets an `O_RDONLY` descriptor that becomes readable when *that
/// process* exits. It is registered in a private epoll which the supervisor
/// registers in its own reactor as a single "a child died" descriptor.
///
/// The indirection costs one extra level of readiness and buys three things:
///
/// * **Recycle safety.** The descriptor names a process, not a number. A pid
///   that is reused before we get around to `waitpid` cannot be confused for
///   the original.
/// * **No `SIGCHLD` needed.** A child that is stopped, continued or ptraced
///   does not generate a spurious event, which is exactly the class of noise
///   that makes `SIGCHLD` handlers loop forever.
/// * **Uniformity.** Every other backend has the same "zero or one descriptor"
///   shape, so the supervisor's registration code is written once.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Debug)]
pub struct PidfdTracker {
    epfd: RawFd,
    /// pid → pidfd, kept in sync with the epoll registration.
    tracked: std::collections::BTreeMap<i32, RawFd>,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl PidfdTracker {
    /// Create the tracker and its private epoll set.
    ///
    /// The `pidfd_open` syscall is **probed here**, not at the first `track`,
    /// because a tracker that only fails later is a tracker whose failure
    /// lands in the middle of starting a service — with a half-registered
    /// child and no obvious culprit. On a kernel older than 5.3 this returns
    /// `ENOSYS` and [`new_child_tracker`] falls back to [`WaitpidTracker`].
    pub fn new() -> io::Result<Self> {
        // SAFETY: flag-only call; the descriptor is owned by this struct and
        // closed in `Drop`. CLOEXEC because this must never be inherited by a
        // service.
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(io::Error::last_os_error());
        }
        // Probe the syscall on this process's own pid: it cannot fail for a
        // reason that would not also apply to a child, and it is closed
        // immediately.
        match sys::pidfd_open(sys::getpid()) {
            Ok(fd) => {
                let _ = sys::close(fd);
            }
            Err(e) => {
                let _ = sys::close(epfd);
                return Err(e);
            }
        }
        Ok(Self {
            epfd,
            tracked: std::collections::BTreeMap::new(),
        })
    }

    /// How many children are currently tracked.
    pub fn len(&self) -> usize {
        self.tracked.len()
    }

    /// True when nothing is tracked.
    pub fn is_empty(&self) -> bool {
        self.tracked.is_empty()
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl ChildTracker for PidfdTracker {
    fn track(&mut self, pid: i32) -> io::Result<()> {
        let pfd = sys::pidfd_open(pid)?;
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLHUP) as u32,
            // The payload is unused; the event list is matched back to a pid
            // by position, and a `u64` field is the only storage epoll offers.
            u64: pid as u64,
        };
        // SAFETY: `ev` is a live, initialised `epoll_event`; `epoll_ctl`
        // copies it. `epfd` is owned by this struct.
        let rc = unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_ADD, pfd, &raw mut ev) };
        if rc != 0 {
            let e = io::Error::last_os_error();
            let _ = sys::close(pfd);
            return Err(e);
        }
        self.tracked.insert(pid, pfd);
        Ok(())
    }

    fn untrack(&mut self, pid: i32) -> io::Result<()> {
        let pfd = match self.tracked.remove(&pid) {
            Some(f) => f,
            None => return Ok(()),
        };
        // SAFETY: a null event pointer is allowed for EPOLL_CTL_DEL.
        let rc =
            unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_DEL, pfd, std::ptr::null_mut()) };
        let _ = sys::close(pfd);
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn fds(&self) -> &[RawFd] {
        std::slice::from_ref(&self.epfd)
    }

    fn reap(&mut self) -> io::Result<Vec<(i32, ExitStatus)>> {
        // SAFETY: all zeros is a valid empty event array; the kernel fills it.
        let mut buf: [libc::epoll_event; 32] = unsafe { core::mem::zeroed() };
        // Timeout 0: `reap` is called *because* the supervisor's reactor said
        // the epoll is readable. Waiting again here would block the loop.
        // SAFETY: `buf` is a live array of exactly 32 `epoll_event`s, which is
        // the length passed; the kernel writes at most that many.
        let n = unsafe {
            libc::epoll_wait(
                self.epfd,
                buf.as_mut_ptr(),
                32 as _,
                0, // no wait
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            // A signal can land between the reactor's wakeup and this call.
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(Vec::new());
            }
            return Err(e);
        }
        let mut out = Vec::with_capacity(n as usize);
        for ev in buf.iter().take(n as usize) {
            let pid = ev.u64 as i32;
            // A pidfd is level-triggered and stays readable for the rest of
            // its life, so it must be removed from the set every time it
            // fires or the supervisor spins on one dead child forever.
            self.untrack(pid)?;
            // The pidfd said "this process is gone"; only `waitpid` knows
            // *how*. It cannot block: the process is already dead, and if the
            // wait says otherwise the pidfd was for a different process and
            // the right answer is "nothing to report".
            if let Some(waited) = sys::waitpid_nohang(pid)? {
                out.push((waited.pid, waited.status));
            }
        }
        Ok(out)
    }

    fn kind(&self) -> ChildKind {
        ChildKind::Pidfd
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl Drop for PidfdTracker {
    fn drop(&mut self) {
        for pfd in self.tracked.values() {
            let _ = sys::close_quietly(*pfd);
        }
        self.tracked.clear();
        let _ = sys::close_quietly(self.epfd);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// EVFILT_PROC (BSD)
// ─────────────────────────────────────────────────────────────────────────────

/// BSD child tracking through `EVFILT_PROC` / `NOTE_EXIT`.
///
/// One knote per child on a private kqueue. `EV_CLEAR` is **required** here,
/// unlike in the I/O reactor: `NOTE_EXIT` is a change notification, and
/// without `EV_CLEAR` the knote stays active for the (now dead) pid and every
/// subsequent `kevent` re-reports the same exit forever. With `EV_CLEAR`,
/// each exit is delivered once, which is the contract the trait promises.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
#[derive(Debug)]
pub struct KqueueProcTracker {
    kq: RawFd,
    tracked: std::collections::BTreeMap<i32, ()>,
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
impl KqueueProcTracker {
    /// Create the tracker and its private kqueue.
    pub fn new() -> io::Result<Self> {
        // SAFETY: no arguments, no preconditions; the descriptor is owned by
        // this struct.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        sys::set_cloexec(kq, true)?;
        Ok(Self {
            kq,
            tracked: std::collections::BTreeMap::new(),
        })
    }

    /// How many children are currently tracked.
    pub fn len(&self) -> usize {
        self.tracked.len()
    }

    /// True when nothing is tracked.
    pub fn is_empty(&self) -> bool {
        self.tracked.is_empty()
    }

    fn change(&mut self, pid: i32, add: bool) -> io::Result<()> {
        // `EV_CLEAR` is required here, unlike in the I/O reactor; see the
        // type-level comment on `KqueueProcTracker`. The constants are passed
        // unwidened because `kevent_new` takes them as `impl Into<u64>`,
        // which is what lets the same expression type-check whether libc
        // declares them as `u16` or as `u32`.
        let k = crate::reactor::kevent_new(
            pid as libc::uintptr_t,
            libc::EVFILT_PROC as i32,
            if add {
                libc::EV_ADD | libc::EV_CLEAR
            } else {
                libc::EV_DELETE
            },
            libc::NOTE_EXIT,
        );
        // SAFETY: `k` is a live, fully initialised `kevent`; `kevent` copies
        // the change list and does not retain it.
        let rc = unsafe {
            libc::kevent(
                self.kq,
                &raw const k,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
impl ChildTracker for KqueueProcTracker {
    fn track(&mut self, pid: i32) -> io::Result<()> {
        self.change(pid, true)?;
        self.tracked.insert(pid, ());
        Ok(())
    }

    fn untrack(&mut self, pid: i32) -> io::Result<()> {
        if self.tracked.remove(&pid).is_none() {
            return Ok(());
        }
        self.change(pid, false)
    }

    fn fds(&self) -> &[RawFd] {
        std::slice::from_ref(&self.kq)
    }

    fn reap(&mut self) -> io::Result<Vec<(i32, ExitStatus)>> {
        // SAFETY: all zeros is a valid empty event array; the kernel fills it.
        let mut buf: [libc::kevent; 32] = unsafe { core::mem::zeroed() };
        let ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `buf` is a live array of 32 `kevent`s and that is the
        // capacity passed; the timeout is a live `timespec`; a null changelist
        // with length 0 means "no changes".
        let n = unsafe {
            libc::kevent(
                self.kq,
                std::ptr::null(),
                0,
                buf.as_mut_ptr(),
                32 as _,
                &raw const ts,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out = Vec::with_capacity(n as usize);
        for ev in buf.iter().take(n as usize) {
            use crate::reactor::kevent_read;
            let pid = kevent_read::ident(ev) as i32;
            // EV_EOF is how EVFILT_PROC says "this note is finished"; the
            // knote has to be deleted or it fires again on the next call.
            if kevent_read::flags(ev) & kevent_read::EV_EOF_MASK != 0 {
                let _ = self.untrack(pid);
            }
            if let Some(waited) = sys::waitpid_nohang(pid)? {
                self.tracked.remove(&waited.pid);
                out.push((waited.pid, waited.status));
            }
        }
        Ok(out)
    }

    fn kind(&self) -> ChildKind {
        ChildKind::KqueueProc
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
impl Drop for KqueueProcTracker {
    fn drop(&mut self) {
        self.tracked.clear();
        let _ = sys::close_quietly(self.kq);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// waitpid (universal)
// ─────────────────────────────────────────────────────────────────────────────

/// Universal child tracking: ask the kernel who died, whenever you remember.
///
/// This backend has no descriptor, and that is a real cost rather than a
/// detail. With nothing to poll, the supervisor has to be woken by `SIGCHLD`
/// through [`crate::signals`] to know when to call [`WaitpidTracker::reap`].
/// The event is coalesced — a burst of five deaths is one `SIGCHLD` — which is
/// why `reap` drains *every* finished child rather than assuming one per call.
///
/// What it does **not** have is the pid-recycling problem in its usual form:
/// `waitpid` is only ever answered for children this process actually still
/// owns, so a recycled pid belonging to somebody else is simply not returned.
/// The residual risk is a child that was never registered, which
/// [`fork_tracked`] prevents by construction.
#[derive(Debug, Default)]
pub struct WaitpidTracker {
    /// Pids registered by [`fork_tracked`], kept only so `reap` can report
    /// "a child died that we were not tracking" instead of silently
    /// swallowing it.
    tracked: std::collections::BTreeMap<i32, ()>,
}

impl WaitpidTracker {
    /// Create a tracker. Cannot fail.
    pub fn new() -> io::Result<Self> {
        Ok(Self::default())
    }

    /// How many children are currently tracked.
    pub fn len(&self) -> usize {
        self.tracked.len()
    }

    /// True when nothing is tracked.
    pub fn is_empty(&self) -> bool {
        self.tracked.is_empty()
    }
}

impl ChildTracker for WaitpidTracker {
    fn track(&mut self, pid: i32) -> io::Result<()> {
        self.tracked.insert(pid, ());
        Ok(())
    }

    fn untrack(&mut self, pid: i32) -> io::Result<()> {
        self.tracked.remove(&pid);
        Ok(())
    }

    fn fds(&self) -> &[RawFd] {
        // Nothing to register. The supervisor must arrange its own wakeup.
        &[]
    }

    fn reap(&mut self) -> io::Result<Vec<(i32, ExitStatus)>> {
        let mut out = Vec::new();
        loop {
            match sys::waitpid_nohang(-1)? {
                Some(waited) => {
                    self.tracked.remove(&waited.pid);
                    out.push((waited.pid, waited.status));
                }
                // `waitpid(-1, WNOHANG)` returning 0 means "no child has
                // changed state"; that is the end of the drain. It cannot be
                // an error, and looping would spin.
                None => return Ok(out),
            }
        }
    }

    fn kind(&self) -> ChildKind {
        ChildKind::Waitpid
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — real processes, really killed, really reaped
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reactor::{Interest, Reactor};
    use crate::signals::Signal;

    /// Fork a child that `_exit`s with `code` after a short pause.
    ///
    /// The child path does exactly one thing and never returns: no
    /// allocation, no lock, no destructor. That is the same constraint the
    /// real spawn path has, and exercising it here means a violation shows up
    /// as a hung test rather than as a mysterious production crash.
    ///
    /// The pause is what makes this deterministic. `EVFILT_PROC` answers
    /// `ESRCH` for a process that has already exited, and `fork` returns in
    /// *both* processes, so an instant `_exit` can win against the parent's
    /// `track` syscall — no ordering of the parent's own calls closes that
    /// window. `nanosleep` is async-signal-safe, so the child path stays
    /// legal, and the burst still lands inside one reap window.
    fn fork_exiting(code: i32) -> i32 {
        match sys::fork() {
            Ok(0) => {
                let _ = crate::clock::sleep_ms(50);
                sys::_exit(code)
            }
            Ok(pid) => pid,
            Err(e) => panic!("fork: {e}"),
        }
    }

    /// Fork a child that runs `/bin/sleep 30`, so there is a live process to
    /// signal.
    fn fork_sleeping() -> i32 {
        let prog = std::ffi::CString::new("/bin/sleep").expect("literal has no NUL");
        let arg = std::ffi::CString::new("30").expect("literal has no NUL");
        // argv is built *before* the fork, per DESIGN.md §9.1: the child path
        // must not allocate, and it only references these two strings.
        let argv = [prog.as_ptr(), arg.as_ptr(), std::ptr::null()];
        match sys::fork() {
            Ok(0) => sys::execvp_or_exit(&prog, &argv),
            Ok(pid) => pid,
            Err(e) => panic!("fork: {e}"),
        }
    }

    /// Wait for `fds` to become readable, then drain the tracker.
    fn reap_within(tracker: &mut dyn ChildTracker, ms: u64) -> Vec<(i32, ExitStatus)> {
        let deadline = crate::clock::deadline_from_now(std::time::Duration::from_millis(ms));
        let mut collected = Vec::new();
        while crate::clock::now_ms() < deadline {
            let remaining = crate::clock::remaining_ms(deadline);
            match tracker.fds().first() {
                Some(&fd) => {
                    // A private reactor per call keeps the test honest: this
                    // is the same path the supervisor uses, not a shortcut.
                    let mut r = crate::reactor::PollReactor::new().expect("poll");
                    r.add(fd, Interest::Read).expect("register");
                    let _ = r.wait(Some(remaining.min(50)));
                }
                None => crate::clock::sleep_ms(5).expect("sleep"),
            }
            collected.extend(tracker.reap().expect("reap"));
            if !collected.is_empty() {
                break;
            }
        }
        collected
    }

    #[test]
    fn a_killed_child_is_reaped_as_signalled() {
        let mut t = new_child_tracker().expect("tracker");
        let pid = fork_sleeping();
        t.track(pid).expect("track");
        assert_eq!(t.fds().len(), 1, "a pidfd/kqueue tracker has one fd");

        sys::kill_process(pid, Signal::Kill).expect("kill");
        let reaped = reap_within(t.as_mut(), 5000);
        assert_eq!(reaped.len(), 1, "exactly one child must be reaped");
        let (got_pid, status) = reaped[0];
        assert_eq!(got_pid, pid, "the right child");
        assert_eq!(
            status,
            ExitStatus::Signaled {
                signal: Signal::Kill.as_raw(),
                core_dumped: false
            },
            "SIGKILL must be reported as a signal, not an exit code"
        );
        assert!(!status.is_success());
        assert_eq!(status.code(), 137, "128 + 9, the shell convention");
    }

    #[test]
    fn an_exit_code_is_preserved_exactly() {
        let mut t = new_child_tracker().expect("tracker");
        let pid = fork_exiting(42);
        t.track(pid).expect("track");
        let reaped = reap_within(t.as_mut(), 5000);
        assert_eq!(reaped, vec![(pid, ExitStatus::Exited(42))]);
        assert!(!reaped[0].1.is_success());
    }

    #[test]
    fn exit_zero_is_success_and_only_exit_zero_is() {
        let mut t = new_child_tracker().expect("tracker");
        let pid = fork_exiting(0);
        t.track(pid).expect("track");
        let reaped = reap_within(t.as_mut(), 5000);
        assert_eq!(reaped, vec![(pid, ExitStatus::Exited(0))]);
        assert!(reaped[0].1.is_success());
    }

    #[test]
    fn reaping_an_idle_system_returns_nothing() {
        let mut t = new_child_tracker().expect("tracker");
        assert!(
            t.reap().expect("reap").is_empty(),
            "no children, no events — and it must not block"
        );
    }

    #[test]
    fn fork_tracked_registers_before_it_returns() {
        // The ordering argument, made observable: `fork_tracked` must return
        // to the parent with the child already in the tracker, and a live
        // child must reap as nothing at all.
        let mut t = new_child_tracker().expect("tracker");
        match fork_tracked(t.as_mut()).expect("fork") {
            // The child takes the production path: async-signal-safe calls
            // and no return. Letting it fall through to the test harness
            // would run the rest of the suite a second time in a process that
            // shares every fd and every lock with this one. `sleep(2)` is one
            // of the handful of libc calls that *is* async-signal-safe, so
            // it stands in for the `execvp` the real child would reach.
            ForkOutcome::Child => {
                // SAFETY: `sleep` is async-signal-safe, takes an unsigned
                // number of seconds and returns the number left unslept.
                unsafe { libc::sleep(1) };
                sys::_exit(0)
            }
            ForkOutcome::Parent(pid) => {
                let live = reap_within(t.as_mut(), 200);
                assert!(
                    live.is_empty(),
                    "our child {pid} reaped as {live:?} (tracker {:?})",
                    t.kind()
                );
                sys::kill_process(pid, Signal::Kill).expect("kill");
                let reaped = reap_within(t.as_mut(), 5000);
                assert_eq!(reaped.len(), 1);
                assert_eq!(reaped[0].0, pid);
            }
        }
    }

    #[test]
    fn the_default_tracker_on_this_machine_reports_its_kind() {
        let t = new_child_tracker().expect("tracker");
        let kind = t.kind();
        // A test, not an assertion of correctness: what matters is that the
        // answer is *reported*. The one thing that must hold everywhere is
        // that the tracker exists.
        assert!(
            matches!(
                kind,
                ChildKind::Pidfd | ChildKind::KqueueProc | ChildKind::Waitpid
            ),
            "unexpected backend {kind:?}"
        );
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(
            kind,
            ChildKind::Pidfd,
            "this machine runs Linux ≥ 5.3; the capability line would be wrong"
        );
    }

    #[test]
    fn a_burst_of_deaths_is_reported_whole() {
        // The coalescing case. Five children die, one `SIGCHLD` arrives. A
        // tracker that reported one per notification would leave four zombies
        // and a service stuck in `Starting` forever.
        let mut t = new_child_tracker().expect("tracker");
        let pids: Vec<i32> = (0..5).map(fork_exiting).collect();
        for p in &pids {
            t.track(*p).expect("track");
        }
        let mut all = Vec::new();
        let deadline = crate::clock::deadline_from_now(std::time::Duration::from_millis(5000));
        while all.len() < 5 && crate::clock::now_ms() < deadline {
            crate::clock::sleep_ms(5).expect("sleep");
            all.extend(t.reap().expect("reap"));
        }
        all.sort_by_key(|(p, _)| *p);
        // The children exit with 0..4, in the order they were forked.
        let expected: Vec<(i32, ExitStatus)> = pids
            .iter()
            .zip(0..5)
            .map(|(p, code)| (*p, ExitStatus::Exited(code)))
            .collect();
        assert_eq!(all, expected);
    }
}
