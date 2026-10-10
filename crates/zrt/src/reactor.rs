//! The event loop's wait primitive: one trait, three backends.
//!
//! # Why a trait and not just `epoll`
//!
//! `epoll` is a Linux invention. `kqueue` is a BSD one, with a different
//! model: filters instead of an interest mask, `EV_EOF` instead of
//! `POLLHUP`, and a semantics that has to be pinned down by hand. `poll(2)` is
//! the one that is everywhere, and slow, and perfectly adequate as a floor.
//! Writing the supervisor against `epoll` directly and pretending otherwise is
//! how "runs on any BSD" turns out to be false at the first `ENOENT`.
//!
//! # The two rules every backend obeys
//!
//! **1. Readiness is LEVEL-triggered, everywhere.**
//!
//! `epoll` without `EPOLLET`, and kqueue *without* `EV_CLEAR` for I/O
//! filters. A descriptor stays reported as readable until someone actually
//! reads it. This is the safe choice: an edge-triggered reactor that forgets
//! to read to `EAGAIN` wedges a connection forever, and "forgets" is a
//! one-line mistake with a two-day outage. The price is that level-triggered
//! fds must be non-blocking, because "read until EAGAIN" cannot work on a
//! blocking descriptor. Every descriptor registered here must therefore have
//! [`crate::sys::set_nonblocking`] applied; the backends cannot enforce it, so it is
//! a documented precondition rather than a checked one.
//!
//! (`EV_CLEAR` is still used, but only where it is *required* rather than
//! optional — see `KqueueReactor`.)
//!
//! **2. A hangup is a read.**
//!
//! `POLLHUP`, `EPOLLHUP` and `EV_EOF` all mean "your peer is gone", and every
//! one of them must be reported as **readable**, never as an error and never
//! as "nothing". The reason is the classic init bug: a service closes its
//! notify pipe on shutdown, and a supervisor that maps that to "not readable"
//! sits in a blocking `wait` forever, convinced the service is healthy. The
//! reader then does a `read` that returns 0 and understands it. Hiding the
//! hangup means the reader never gets to find out.
//!
//! Both rules are tested against real pipes and real socket pairs at the
//! bottom of this file.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::RawFd;

/// What a caller wants to hear about a descriptor.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Interest {
    /// Call me when a read would not block.
    Read,
    /// Call me when a write would not block.
    Write,
    /// Both.
    ReadWrite,
}

impl Interest {
    /// Whether read readiness is wanted.
    pub const fn wants_read(self) -> bool {
        matches!(self, Interest::Read | Interest::ReadWrite)
    }

    /// Whether write readiness is wanted.
    pub const fn wants_write(self) -> bool {
        matches!(self, Interest::Write | Interest::ReadWrite)
    }
}

/// One descriptor reported ready.
///
/// Note the shape: a *pair* of booleans, not an enum. A socket that is both
/// readable and writable is both, and collapsing that into one variant loses
/// the write side and produces a protocol handler that cannot drain its own
/// output.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct ReadyEvent {
    /// The descriptor that is ready.
    pub fd: RawFd,
    /// A read would not block, **or the peer hung up**. See rule 2 above.
    pub readable: bool,
    /// A write would not block.
    pub writable: bool,
}

impl ReadyEvent {
    /// True when either direction is ready.
    pub fn is_ready(&self) -> bool {
        self.readable || self.writable
    }
}

/// Which backend a reactor is, for the boot log and for `zctl version`.
///
/// This is reported, never branched on. A supervisor that changes behaviour
/// silently depending on the backend is the failure mode `DESIGN.md` §8.1
/// exists to prevent.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ReactorKind {
    /// Linux `epoll`.
    Epoll,
    /// BSD `kqueue`.
    Kqueue,
    /// POSIX `poll`. The floor: always available, never fast.
    Poll,
}

impl ReactorKind {
    /// The name printed in the capability line.
    pub const fn name(self) -> &'static str {
        match self {
            ReactorKind::Epoll => "epoll",
            ReactorKind::Kqueue => "kqueue",
            ReactorKind::Poll => "poll",
        }
    }
}

/// One event loop's worth of readiness.
///
/// Implementations are single-threaded and own their fds: `add`, `modify` and
/// `remove` mutate kernel state, and `wait` blocks. There is no `Send`, and
/// that is deliberate — the supervisor's event loop has exactly one thread
/// because signal ordering is much easier to reason about when there is
/// nothing to race with.
pub trait Reactor {
    /// Register `fd`, replacing any previous interest.
    ///
    /// Fd numbers are the key, so registering a descriptor twice is a
    /// replacement, not a duplicate — the same discipline as `epoll_ctl` and
    /// `kevent` themselves, kept so the three backends are interchangeable.
    fn add(&mut self, fd: RawFd, interest: Interest) -> io::Result<()>;

    /// Change the interest of an already-registered descriptor.
    fn modify(&mut self, fd: RawFd, interest: Interest) -> io::Result<()>;

    /// Stop watching `fd`. The descriptor is **not** closed: it belongs to
    /// whoever opened it.
    fn remove(&mut self, fd: RawFd) -> io::Result<()>;

    /// Block for at most `timeout_ms` and return whatever is ready.
    ///
    /// `None` blocks indefinitely. An empty vector means "the timeout
    /// expired", which is how the supervisor implements its own tick without
    /// a timer fd.
    ///
    /// Returning an empty vector is a *legal outcome*, never a panic: a
    /// supervisor that treats it as an error will spin or, worse, treat it as
    /// a fault and restart its services.
    fn wait(&mut self, timeout_ms: Option<u64>) -> io::Result<Vec<ReadyEvent>>;

    /// Which backend this is, for the boot log.
    fn kind(&self) -> ReactorKind;
}

/// Build the best reactor this machine can give us.
///
/// Falls back to [`PollReactor`] if the native backend refuses to be created —
/// an `epoll_create1` that fails means something is very wrong with the kernel
/// or the sandbox, and a slow event loop is a much better outcome than no
/// event loop. The fallback is announced through the same mechanism as every
/// other degradation: the caller prints [`ReactorKind`].
pub fn new_reactor() -> io::Result<Box<dyn Reactor>> {
    match native_reactor() {
        Ok(r) => Ok(Box::new(r)),
        Err(e) if cfg!(any(target_os = "linux", target_os = "android")) => {
            crate::report::announce_degradation(&format!(
                "epoll unavailable ({e}); degrading to poll(2)"
            ));
            Ok(Box::new(PollReactor::new()?))
        }
        Err(e) if kqueue_supported() => {
            crate::report::announce_degradation(&format!(
                "kqueue unavailable ({e}); degrading to poll(2)"
            ));
            Ok(Box::new(PollReactor::new()?))
        }
        Err(e) => Err(e),
    }
}

/// The platform's native reactor, or an error explaining why there is none.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn native_reactor() -> io::Result<EpollReactor> {
    EpollReactor::new()
}

/// The platform's native reactor, or an error explaining why there is none.
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
pub fn native_reactor() -> io::Result<KqueueReactor> {
    KqueueReactor::new()
}

/// No native reactor exists for this target; `poll` is the floor.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
)))]
pub fn native_reactor() -> io::Result<PollReactor> {
    PollReactor::new()
}

/// The concrete reactor type this platform uses, for code that wants to avoid
/// a `Box` on the hot path.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub type DefaultReactor = EpollReactor;

/// The concrete reactor type this platform uses.
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
pub type DefaultReactor = KqueueReactor;

/// The concrete reactor type this platform uses.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
)))]
pub type DefaultReactor = PollReactor;

const fn kqueue_supported() -> bool {
    cfg!(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
    ))
}

/// How many events one `wait` can return.
///
/// Not a tuning knob, a correctness one: the buffer is on the stack of the
/// loop, and a supervisor that watches 4096 sockets still only needs one
/// batch at a time because the loop goes back for more immediately.
const MAX_EVENTS: usize = 128;

// ─────────────────────────────────────────────────────────────────────────────
// epoll
// ─────────────────────────────────────────────────────────────────────────────

/// Linux `epoll`.
///
/// Level-triggered by construction: no `EPOLLET` is ever set. See rule 1.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Debug)]
pub struct EpollReactor {
    epfd: RawFd,
    /// Interest per fd, kept because `epoll_ctl` has no "tell me what you
    /// registered" call, and `modify`/`remove` have to know what is there.
    interest: BTreeMap<RawFd, Interest>,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl EpollReactor {
    /// Create an epoll set. `EPOLL_CLOEXEC` always: it will outlive the
    /// forks that start services, and no service may inherit the supervisor's
    /// private event set.
    pub fn new() -> io::Result<Self> {
        // SAFETY: `epoll_create1` takes a flag set; no pointers, and the
        // returned fd is owned by this struct, which closes it in `Drop`.
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            epfd,
            interest: BTreeMap::new(),
        })
    }

    /// The underlying descriptor, for registration in *another* reactor (a
    /// nested set, used by [`crate::childproc::PidfdTracker`]).
    pub fn as_raw_fd(&self) -> RawFd {
        self.epfd
    }

    fn event_mask(interest: Interest) -> u32 {
        let mut mask = 0u32;
        if interest.wants_read() {
            mask |= libc::EPOLLIN as u32;
        }
        if interest.wants_write() {
            mask |= libc::EPOLLOUT as u32;
        }
        // EPOLLERR and EPOLLHUP are always reported by epoll whether or not
        // they are asked for; spelling them out documents that a hangup is
        // expected here, not an accident. EPOLLRDHUP is the half-close
        // signal (a peer that shut down its write side but may still read),
        // which a supervisor needs to tell "done writing" from "gone".
        mask |= libc::EPOLLERR as u32 | libc::EPOLLHUP as u32 | libc::EPOLLRDHUP as u32;
        mask
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl Reactor for EpollReactor {
    fn add(&mut self, fd: RawFd, interest: Interest) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events: Self::event_mask(interest),
            u64: fd as u64,
        };
        // SAFETY: `ev` is a live, initialised `epoll_event` on this frame and
        // `epoll_ctl` copies it. The `u64` field is opaque storage, not a
        // pointer. `epfd` is a descriptor this struct owns.
        if unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_ADD, fd, &raw mut ev) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.interest.insert(fd, interest);
        Ok(())
    }

    fn modify(&mut self, fd: RawFd, interest: Interest) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events: Self::event_mask(interest),
            u64: fd as u64,
        };
        // SAFETY: as `add`, with EPOLL_CTL_MOD. `EPERM` here means the fd is
        // not in this set, which is a caller bug and is reported as such.
        if unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_MOD, fd, &raw mut ev) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.interest.insert(fd, interest);
        Ok(())
    }

    fn remove(&mut self, fd: RawFd) -> io::Result<()> {
        // SAFETY: a null event pointer is explicitly allowed for
        // EPOLL_CTL_DEL, which only needs the fd.
        if unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut()) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        self.interest.remove(&fd);
        Ok(())
    }

    fn wait(&mut self, timeout_ms: Option<u64>) -> io::Result<Vec<ReadyEvent>> {
        // SAFETY: all zeros is a valid empty event array; the kernel fills it.
        let mut buf: [libc::epoll_event; MAX_EVENTS] = unsafe { core::mem::zeroed() };
        // `epoll_wait` takes an `int` timeout; clamping rather than wrapping is
        // the difference between "wait ~24 days" and "return immediately".
        let timeout: libc::c_int = match timeout_ms {
            None => -1,
            Some(ms) => ms.min(i32::MAX as u64) as libc::c_int,
        };
        // SAFETY: `buf` is a live array of exactly MAX_EVENTS `epoll_event`s,
        // which is the length passed; epoll writes at most that many.
        let n = unsafe { libc::epoll_wait(self.epfd, buf.as_mut_ptr(), MAX_EVENTS as _, timeout) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out = Vec::with_capacity(n as usize);
        for ev in buf.iter().take(n as usize) {
            let flags = ev.events;
            // Rule 2: HUP, ERR and RDHUP all mean the peer is gone or the
            // descriptor is in error, and all three are things you learn by
            // *reading* (a 0-byte read, an error from read). Reporting them
            // as readable is what lets a draining loop find out.
            let readable = flags
                & (libc::EPOLLIN as u32
                    | libc::EPOLLHUP as u32
                    | libc::EPOLLERR as u32
                    | libc::EPOLLRDHUP as u32)
                != 0;
            let writable = flags & (libc::EPOLLOUT as u32 | libc::EPOLLERR as u32) != 0;
            out.push(ReadyEvent {
                fd: ev.u64 as RawFd,
                readable,
                writable,
            });
        }
        Ok(out)
    }

    fn kind(&self) -> ReactorKind {
        ReactorKind::Epoll
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl Drop for EpollReactor {
    fn drop(&mut self) {
        // The kernel tears down the whole set when the fd closes, so nothing
        // has to be de-registered. Losing this fd is impossible: it is closed
        // exactly once, here, and never handed out.
        let _ = sys_close(self.epfd);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// kqueue
// ─────────────────────────────────────────────────────────────────────────────

/// BSD `kqueue`.
///
/// # `EV_CLEAR`: here and not there
///
/// kqueue's default is level semantics, which is what rule 1 wants, so
/// **I/O filters are registered with `EV_ADD` and no `EV_CLEAR`**. Adding
/// `EV_CLEAR` to `EVFILT_READ` would make the knote fire only on transitions,
/// turning this into an edge-triggered reactor with all the starvation risk
/// that implies and none of the performance on a workload this small.
///
/// `EV_CLEAR` *is* used elsewhere in this crate, where it is not a
/// performance knob but a requirement: [`crate::signals::KqueueSignalSource`]
/// (so one knote per delivery, not an event that repeats forever) and
/// [`crate::childproc::KqueueProcTracker`] (so `NOTE_EXIT` is delivered once
/// and the knote does not stay permanently active for a dead pid).
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
pub struct KqueueReactor {
    kq: RawFd,
    interest: BTreeMap<RawFd, Interest>,
}

/// Build a `struct kevent` with portable field assignment.
///
/// This function exists because `struct kevent` is the least uniform struct in
/// any syscall API:
///
/// * **FreeBSD 12+** added a trailing `ext: [u64; 4]`. A struct literal that
///   omits it does not compile, and a literal that includes it does not
///   compile on every other BSD.
/// * **NetBSD** types `filter`, `flags` and `fflags` as `u32`; the others use
///   `i16`/`u16`/`u32`.
/// * **Darwin** declares the struct `#[repr(packed(4)]`, so *assigning* to a
///   field — `k.ident = x` — is a compile error ("reference to packed field"),
///   even though reading one is fine.
///
/// The only construction that survives all three is: zero the whole struct,
/// then write each field through a raw pointer. `addr_of_mut!` produces a
/// `*mut Field` without ever forming a reference to the field, which is what
/// makes it legal on a packed struct; and a zeroed `ext` array is exactly
/// what "no extension data" means. The `as _` casts resolve against the
/// field's own declared type, so no platform is named here.
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
///
/// `flags` and `fflags` are generic over `Into<u64>` rather than being `u32`,
/// and that is the whole trick for the *call sites*. The `EV_*` and `NOTE_*`
/// constants are `u16` on Apple/FreeBSD/OpenBSD and `u32` on NetBSD, so
/// spelling the argument as `u32` forces a cast — redundant on one target and
/// load-bearing on another, which means clippy is always right on one target
/// and always wrong on the other. Taking `impl Into<u64>` lets a call site
/// write `libc::EV_CLEAR` and nothing else, and the single conversion lives
/// here where it is cast exactly once.
pub(crate) fn kevent_new<F, G>(
    ident: libc::uintptr_t,
    filter: i32,
    flags: F,
    fflags: G,
) -> libc::kevent
where
    F: Into<u64>,
    G: Into<u64>,
{
    // SAFETY: an all-zero bit pattern is a valid `kevent`: every scalar field
    // is an integer, `udata` is a null pointer, and FreeBSD's `ext` is a
    // zero array meaning "no extension data". Every field is then
    // overwritten below except `udata` and `ext`, which are meant to be zero.
    let mut k: libc::kevent = unsafe { core::mem::zeroed() };
    let p = core::ptr::addr_of_mut!(k);
    // SAFETY: `p` points at a live, initialised `kevent`; each `addr_of_mut!`
    // yields a correctly typed pointer to one of its own fields, and
    // `ptr::write` copies a value of exactly that type into it. No reference
    // to a packed field is ever formed, which is what makes this legal on
    // Darwin. The writes do not overlap: they are five distinct fields.
    unsafe {
        core::ptr::addr_of_mut!((*p).ident).write(ident as _);
        core::ptr::addr_of_mut!((*p).filter).write(filter as _);
        core::ptr::addr_of_mut!((*p).flags).write((flags.into() as u32) as _);
        core::ptr::addr_of_mut!((*p).fflags).write((fflags.into() as u32) as _);
    }
    k
}

/// Read-side counterparts to [`kevent_new`], for the same reason.
///
/// `struct kevent`'s field types differ per platform (`flags` is `u16`
/// everywhere except NetBSD, where it is `u32`), so reading one back and
/// casting produces the same "redundant here, load-bearing there" problem
/// that `kevent_new` exists to avoid. `Into::<u64>::into` normalises it
/// without naming a platform, and the single `as u32` is a `u64`-to-`u32`
/// narrowing that is never a same-type cast.
///
/// These copy the field out by value, which is legal even on Darwin's
/// `#[repr(packed(4)]` struct; forming a *reference* to a packed field is
/// what the compiler rejects, and nothing here does that.
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
pub(crate) mod kevent_read {
    // Each of these is `as u32`/`as i32` for the same reason `flags()` is
    // written the way it is: the libc constant is `u16` on Apple, FreeBSD and
    // OpenBSD and `u32` on NetBSD, so the cast is redundant on one target and
    // load-bearing on the other. The lint therefore cannot be satisfied on all
    // of them at once, and is silenced here with the reason rather than by
    // deleting a cast that another target needs.
    /// The `EV_EOF` flag, widened for use against [`flags`].
    #[allow(clippy::unnecessary_cast)]
    pub(crate) const EV_EOF_MASK: u32 = libc::EV_EOF as u32;

    /// The `EV_ERROR` flag, widened for use against [`flags`].
    #[allow(clippy::unnecessary_cast)]
    pub(crate) const EV_ERROR_MASK: u32 = libc::EV_ERROR as u32;

    /// The `EVFILT_READ` filter number, as an `i32`.
    #[allow(clippy::unnecessary_cast)]
    pub(crate) const FILTER_READ: i32 = libc::EVFILT_READ as i32;

    /// The `EVFILT_WRITE` filter number, as an `i32`.
    #[allow(clippy::unnecessary_cast)]
    pub(crate) const FILTER_WRITE: i32 = libc::EVFILT_WRITE as i32;

    /// The descriptor or pid the event is about.
    pub(crate) fn ident(ev: &libc::kevent) -> libc::uintptr_t {
        ev.ident
    }

    /// The filter that fired (`EVFILT_READ`, `EVFILT_PROC`, ...).
    pub(crate) fn filter(ev: &libc::kevent) -> i32 {
        ev.filter as i32
    }

    /// The flags delivered with the event, including `EV_EOF` and `EV_ERROR`.
    pub(crate) fn flags(ev: &libc::kevent) -> u32 {
        Into::<u64>::into(ev.flags) as u32
    }

    /// Filter-specific data: bytes available, an exit code, a signal number.
    ///
    /// `i64` because that is the widest `data` is on any of these platforms;
    /// the narrowing on the platforms that declare it `intptr_t` or `i32` is
    /// exact for every value kqueue produces.
    #[allow(clippy::unnecessary_cast)]
    pub(crate) fn data(ev: &libc::kevent) -> i64 {
        ev.data as i64
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
impl KqueueReactor {
    /// Create a kqueue. Always `EV_CLOEXEC`.
    pub fn new() -> io::Result<Self> {
        // SAFETY: `kqueue()` takes no arguments and has no preconditions; the
        // returned descriptor is owned by this struct and closed in `Drop`.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        set_close_on_exec(kq)?;
        Ok(Self {
            kq,
            interest: BTreeMap::new(),
        })
    }

    /// The underlying descriptor, for nesting inside another reactor.
    pub fn as_raw_fd(&self) -> RawFd {
        self.kq
    }

    fn change(&mut self, fd: RawFd, filter: i32, add: bool, fflags: u32) -> io::Result<()> {
        let flags = if add { libc::EV_ADD } else { libc::EV_DELETE };
        let k = kevent_new(fd as libc::uintptr_t, filter, flags, fflags);
        // SAFETY: `k` is a live, fully initialised `kevent`; `kevent` copies
        // the change list. A zero-length event list is expressed with a null
        // pointer and 0, which is the documented way to ask for "no changes".
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
impl Reactor for KqueueReactor {
    fn add(&mut self, fd: RawFd, interest: Interest) -> io::Result<()> {
        if interest.wants_read() {
            self.change(fd, libc::EVFILT_READ as i32, true, 0)?;
        }
        if interest.wants_write() {
            self.change(fd, libc::EVFILT_WRITE as i32, true, 0)?;
        }
        self.interest.insert(fd, interest);
        Ok(())
    }

    fn modify(&mut self, fd: RawFd, interest: Interest) -> io::Result<()> {
        // kqueue has no "modify": a filter is added, deleted or left alone.
        // So a change of interest is a delete of what is no longer wanted
        // followed by an add of what is, and a filter that is unchanged is
        // simply not touched — re-adding it would reset its internal state
        // for no reason.
        let previous = self.interest.get(&fd).copied();
        if let Some(prev) = previous {
            if prev.wants_read() && !interest.wants_read() {
                self.change(fd, libc::EVFILT_READ as i32, false, 0)?;
            }
            if prev.wants_write() && !interest.wants_write() {
                self.change(fd, libc::EVFILT_WRITE as i32, false, 0)?;
            }
        }
        if previous != Some(interest) {
            if interest.wants_read() {
                self.change(fd, libc::EVFILT_READ as i32, true, 0)?;
            }
            if interest.wants_write() {
                self.change(fd, libc::EVFILT_WRITE as i32, true, 0)?;
            }
        }
        self.interest.insert(fd, interest);
        Ok(())
    }

    fn remove(&mut self, fd: RawFd) -> io::Result<()> {
        if let Some(prev) = self.interest.get(&fd).copied() {
            if prev.wants_read() {
                self.change(fd, libc::EVFILT_READ as i32, false, 0)?;
            }
            if prev.wants_write() {
                self.change(fd, libc::EVFILT_WRITE as i32, false, 0)?;
            }
        }
        self.interest.remove(&fd);
        Ok(())
    }

    fn wait(&mut self, timeout_ms: Option<u64>) -> io::Result<Vec<ReadyEvent>> {
        // SAFETY: all zeros is a valid empty event array; the kernel fills it.
        let mut buf: [libc::kevent; MAX_EVENTS] = unsafe { core::mem::zeroed() };
        let ts = timeout_ms.map(|ms| {
            // `tv_nsec` follows the platform `long`: 64 bits on macOS/aarch64,
            // 32 on 32-bit BSDs. Computed wide, narrowed with `as _` at the
            // field (immune to `unnecessary_cast`, like the ioctl request
            // numbers in `sys.rs`). (`tv_sec` needs no such treatment:
            // `time_t` is deprecated only on musl, which never compiles this
            // BSD-only block.)
            let nsecs = ((ms % 1000) * 1_000_000) as i64;
            libc::timespec {
                tv_sec: (ms / 1000) as libc::time_t,
                tv_nsec: nsecs as _,
            }
        });
        let ts_ptr = ts.as_ref().map_or(std::ptr::null(), |t| t as *const _);
        // SAFETY: `buf` is a live array of exactly MAX_EVENTS `kevent`s, and
        // that is the capacity passed. A null timeout pointer means "block
        // forever", which is what kevent(2) documents. A null changelist with
        // a zero length is "no changes".
        let n = unsafe {
            libc::kevent(
                self.kq,
                std::ptr::null(),
                0,
                buf.as_mut_ptr(),
                MAX_EVENTS as _,
                ts_ptr,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out = Vec::with_capacity(n as usize);
        for ev in buf.iter().take(n as usize) {
            // Read through the accessors rather than touching the fields
            // directly: `struct kevent` is `#[repr(packed)]` on Darwin and its
            // field types differ per BSD. See `kevent_read`.
            let ident = kevent_read::ident(ev) as RawFd;
            let filter = kevent_read::filter(ev);
            let flags = kevent_read::flags(ev);
            if flags & kevent_read::EV_ERROR_MASK != 0 {
                // The kernel reports per-knote errors in-band rather than
                // through errno, so they must be unpacked here or a broken
                // registration looks like an event.
                return Err(io::Error::from_raw_os_error(ev.data as i32));
            }
            let is_read = filter == kevent_read::FILTER_READ;
            let is_write = filter == kevent_read::FILTER_WRITE;
            // Rule 2 again, in kqueue's spelling: EV_EOF is not an error, it
            // is a read condition. A kqueue-based supervisor that checks
            // EV_EOF before EVFILT_READ will hang forever on a closed pipe.
            let eof = flags & kevent_read::EV_EOF_MASK != 0;
            out.push(ReadyEvent {
                fd: ident,
                readable: is_read || eof,
                writable: is_write,
            });
        }
        // A single descriptor can be reported twice (once per filter). Merge,
        // so callers can rely on one event per descriptor per wait.
        //
        // The merge must *or* the directions, never keep the first event: the
        // read and write knotes are separate entries and arrive in whatever
        // order the kernel walks them, so keeping one silently drops half the
        // readiness. A socket that comes back "writable only" cannot drain its
        // own output, which is the exact loss `ReadyEvent`'s pair of booleans
        // exists to prevent.
        out.sort_by_key(|e| e.fd);
        let mut merged: Vec<ReadyEvent> = Vec::with_capacity(out.len());
        for ev in out {
            match merged.last_mut() {
                Some(prev) if prev.fd == ev.fd => {
                    prev.readable |= ev.readable;
                    prev.writable |= ev.writable;
                }
                _ => merged.push(ev),
            }
        }
        Ok(merged)
    }

    fn kind(&self) -> ReactorKind {
        ReactorKind::Kqueue
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
impl Drop for KqueueReactor {
    fn drop(&mut self) {
        let _ = sys_close(self.kq);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// poll
// ─────────────────────────────────────────────────────────────────────────────

/// POSIX `poll(2)`: the universal floor.
///
/// O(n) per wait, so it is the backend of last resort, not a modern one. It
/// exists because "works everywhere" beats "fast somewhere", and because it is
/// the reference the other two are checked against in the tests below: if all
/// three agree on what a closed pipe looks like, the mapping is right.
#[derive(Debug, Default)]
pub struct PollReactor {
    interest: BTreeMap<RawFd, Interest>,
}

impl PollReactor {
    /// A reactor with nothing registered yet. Cannot fail.
    pub fn new() -> io::Result<Self> {
        Ok(Self::default())
    }
}

impl Reactor for PollReactor {
    fn add(&mut self, fd: RawFd, interest: Interest) -> io::Result<()> {
        // A descriptor that is not open is not an error here: `poll` reports
        // it later as POLLNVAL. Validating up front would need an `fcntl` per
        // registration, and the answer would be a race anyway.
        self.interest.insert(fd, interest);
        Ok(())
    }

    fn modify(&mut self, fd: RawFd, interest: Interest) -> io::Result<()> {
        self.interest.insert(fd, interest);
        Ok(())
    }

    fn remove(&mut self, fd: RawFd) -> io::Result<()> {
        self.interest.remove(&fd);
        Ok(())
    }

    fn wait(&mut self, timeout_ms: Option<u64>) -> io::Result<Vec<ReadyEvent>> {
        let mut fds: Vec<libc::pollfd> = self
            .interest
            .iter()
            .map(|(&fd, &i)| {
                // Exactly the requested interest, no more. `poll` always
                // reports POLLERR/POLLHUP/POLLNVAL regardless of `events`,
                // so there is no way to *suppress* those, but POLLIN must not
                // be left in by default or a write-only registration would
                // keep reporting readable descriptors.
                let mut events = 0;
                if i.wants_read() {
                    events |= libc::POLLIN;
                }
                if i.wants_write() {
                    events |= libc::POLLOUT;
                }
                libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                }
            })
            .collect();
        if fds.is_empty() {
            // `poll(NULL, 0, timeout)` is a sleep, which is what an empty
            // registration list means. Doing it through std would pull
            // `Instant` into the event loop, which this crate refuses.
            crate::clock::sleep_ms(timeout_ms.unwrap_or(0))?;
            return Ok(Vec::new());
        }
        let timeout: libc::c_int = match timeout_ms {
            None => -1,
            Some(ms) => ms.min(i32::MAX as u64) as libc::c_int,
        };
        // SAFETY: `fds` is a live vector of `pollfd` and its exact length is
        // passed; `poll` writes only `revents` in each element. The pointer
        // is not retained past the call.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out = Vec::new();
        for p in fds.iter() {
            if p.revents == 0 {
                continue;
            }
            // Rule 2, third spelling: POLLHUP and POLLNVAL are both things the
            // reader must find out about by reading. POLLNVAL means the
            // descriptor was closed behind the reactor's back; mapping it to
            // "readable" makes the caller's read fail with EBADF, which is a
            // diagnosable error, instead of leaving the descriptor silently
            // unreported forever.
            let readable =
                p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
            let writable = p.revents & (libc::POLLOUT | libc::POLLERR) != 0;
            out.push(ReadyEvent {
                fd: p.fd,
                readable,
                writable,
            });
        }
        Ok(out)
    }

    fn kind(&self) -> ReactorKind {
        ReactorKind::Poll
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Small helpers
// ─────────────────────────────────────────────────────────────────────────────

fn sys_close(fd: RawFd) -> io::Result<()> {
    crate::sys::close_quietly(fd)
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
)))]
fn set_close_on_exec(fd: RawFd) -> io::Result<()> {
    crate::sys::set_cloexec(fd, true)
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
fn set_close_on_exec(fd: RawFd) -> io::Result<()> {
    // `kqueue` has no `CLOEXEC` flag, unlike `epoll_create1`; the flag has to
    // be set by hand, and forgotten here it would leak the supervisor's event
    // set into every service.
    crate::sys::set_cloexec(fd, true)
}

/// The events a descriptor can report, for callers that need the three-bit
/// truth rather than a `ReadyEvent` pair.
pub const EVENT_READ: u8 = 1;
/// See [`EVENT_READ`].
pub const EVENT_WRITE: u8 = 2;

// ─────────────────────────────────────────────────────────────────────────────
// Tests — real pipes, real sockets, real hangs
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys;

    /// Every backend, so the same assertions run on all of them.
    fn reactors() -> Vec<(&'static str, Box<dyn Reactor>)> {
        let mut v: Vec<(&'static str, Box<dyn Reactor>)> = Vec::new();
        match native_reactor() {
            Ok(r) => v.push((r.kind().name(), Box::new(r))),
            Err(e) => panic!("native reactor unavailable: {e}"),
        }
        v.push(("poll", Box::new(PollReactor::new().expect("poll reactor"))));
        v
    }

    fn nonblock(fd: RawFd) {
        sys::set_nonblocking(fd, true).expect("O_NONBLOCK");
    }

    fn find(evs: &[ReadyEvent], fd: RawFd) -> Option<&ReadyEvent> {
        evs.iter().find(|e| e.fd == fd)
    }

    #[test]
    fn pipe_write_end_is_reported_readable() {
        for (name, mut r) in reactors() {
            let (rd, wr) = sys::pipe2(true).expect("pipe2");
            nonblock(rd);
            r.add(rd, Interest::Read)
                .unwrap_or_else(|e| panic!("{name}: add: {e}"));

            // Nothing yet: the pipe is empty.
            let evs = r
                .wait(Some(0))
                .unwrap_or_else(|e| panic!("{name}: wait: {e}"));
            assert!(
                find(&evs, rd).is_none(),
                "{name}: an empty pipe must not be reported"
            );

            let b = [7u8];
            sys::write(wr, &b).expect("write");
            let evs = r
                .wait(Some(2000))
                .unwrap_or_else(|e| panic!("{name}: wait: {e}"));
            let ev = find(&evs, rd).unwrap_or_else(|| panic!("{name}: pipe not reported"));
            assert!(ev.readable, "{name}: pipe with data must be readable");
            assert!(!ev.writable, "{name}: registered read-only");

            let mut got = [0u8; 1];
            let n = sys::read(rd, &mut got).expect("read");
            assert_eq!((n, got[0]), (1, 7));

            // Level-triggered, rule 1: once drained, it is no longer ready.
            let evs = r
                .wait(Some(0))
                .unwrap_or_else(|e| panic!("{name}: wait: {e}"));
            assert!(
                find(&evs, rd).is_none(),
                "{name}: a drained pipe must stop being reported"
            );

            let _ = sys::close(rd);
            let _ = sys::close(wr);
        }
    }

    #[test]
    fn closed_pipe_end_is_readable_because_a_hangup_is_a_read() {
        // Rule 2. This is the regression test for the classic init bug: a
        // supervisor that ignores POLLHUP/EV_EOF blocks forever in wait()
        // believing a dead service is still talking.
        for (name, mut r) in reactors() {
            let (rd, wr) = sys::pipe2(true).expect("pipe2");
            nonblock(rd);
            r.add(rd, Interest::Read).expect("add");
            let _ = sys::close(wr);

            let evs = r
                .wait(Some(2000))
                .unwrap_or_else(|e| panic!("{name}: wait: {e}"));
            let ev = find(&evs, rd)
                .unwrap_or_else(|| panic!("{name}: a pipe whose writer closed must be reported"));
            assert!(
                ev.readable,
                "{name}: HUP must map to readable, not to nothing"
            );

            let mut buf = [0u8; 8];
            let n = sys::read(rd, &mut buf).expect("read must not fail on EOF");
            assert_eq!(n, 0, "{name}: the read must observe the EOF");

            let _ = sys::close(rd);
        }
    }

    #[test]
    fn socketpair_reports_readable_then_writable() {
        for (name, mut r) in reactors() {
            let (a, b) = sys::socketpair().expect("socketpair");
            r.add(a, Interest::ReadWrite).expect("add a");
            r.add(b, Interest::ReadWrite).expect("add b");

            // A fresh unix socket is immediately writable and not readable.
            let evs = r
                .wait(Some(0))
                .unwrap_or_else(|e| panic!("{name}: wait: {e}"));
            let ev = find(&evs, a).unwrap_or_else(|| panic!("{name}: a must be ready"));
            assert!(ev.writable, "{name}: idle socket is writable");
            assert!(!ev.readable, "{name}: idle socket is not readable");

            sys::write(a, &[42]).expect("write to socket");

            let evs = r
                .wait(Some(2000))
                .unwrap_or_else(|e| panic!("{name}: wait: {e}"));
            let ev = find(&evs, b).unwrap_or_else(|| panic!("{name}: b not ready"));
            assert!(ev.readable, "{name}: socket with data is readable");
            let mut got = [0u8; 1];
            let n = sys::read(b, &mut got).expect("read");
            assert_eq!((n, got[0]), (1, 42));

            let _ = sys::close(a);
            let _ = sys::close(b);
        }
    }

    #[test]
    fn socketpair_read_is_delivered() {
        for (name, mut r) in reactors() {
            let (a, b) = sys::socketpair().expect("socketpair");
            r.add(b, Interest::Read).expect("add b");

            sys::write(a, &[42]).expect("write to socket");

            let evs = r
                .wait(Some(2000))
                .unwrap_or_else(|e| panic!("{name}: wait: {e}"));
            let ev = find(&evs, b).unwrap_or_else(|| panic!("{name}: b not ready"));
            assert!(ev.readable, "{name}: socket with data is readable");

            let mut got = [0u8; 1];
            let n = sys::read(b, &mut got).expect("read");
            assert_eq!((n, got[0]), (1, 42));

            let _ = sys::close(a);
            let _ = sys::close(b);
        }
    }

    #[test]
    fn modify_and_remove_change_what_is_reported() {
        for (name, mut r) in reactors() {
            let (rd, wr) = sys::pipe2(true).expect("pipe2");
            nonblock(rd);
            r.add(rd, Interest::Read).expect("add");
            // Switch to write-only interest: a readable pipe must stop being
            // reported. This is the part where kqueue has to delete the
            // EVFILT_READ knote, because it has no "modify".
            r.modify(rd, Interest::Write).expect("modify");
            let b = [1u8];
            sys::write(wr, &b).expect("write");
            let evs = r
                .wait(Some(50))
                .unwrap_or_else(|e| panic!("{name}: wait: {e}"));
            let ev = find(&evs, rd);
            assert!(
                ev.is_none_or(|e| !e.readable),
                "{name}: read interest must be gone after modify"
            );

            r.remove(rd).expect("remove");
            let evs = r
                .wait(Some(50))
                .unwrap_or_else(|e| panic!("{name}: wait: {e}"));
            assert!(
                find(&evs, rd).is_none(),
                "{name}: a removed fd must never be reported"
            );

            let _ = sys::close(rd);
            let _ = sys::close(wr);
        }
    }

    #[test]
    fn timeout_expires_and_returns_nothing() {
        for (name, mut r) in reactors() {
            let before = crate::clock::now_ms();
            let evs = r
                .wait(Some(60))
                .unwrap_or_else(|e| panic!("{name}: wait: {e}"));
            let after = crate::clock::now_ms();
            assert!(evs.is_empty(), "{name}: nothing registered, nothing ready");
            assert!(
                after.saturating_sub(before) >= 40,
                "{name}: wait(60) returned after only {}ms",
                after.saturating_sub(before)
            );
        }
    }

    #[test]
    fn an_event_is_returned_exactly_once_per_descriptor() {
        // A descriptor readable *and* writable must arrive as one event, not
        // two, or a handler that clears flags after the first one will miss
        // the second.
        for (name, mut r) in reactors() {
            // Write on one end, watch the other: a unix socket pair gives
            // each end its own receive queue, so data written to `b` is what
            // makes `a` readable. `a` is also writable, being an idle socket.
            let (a, b) = sys::socketpair().expect("socketpair");
            r.add(a, Interest::ReadWrite).expect("add");
            sys::write(b, &[1]).expect("write");

            let evs = r
                .wait(Some(2000))
                .unwrap_or_else(|e| panic!("{name}: wait: {e}"));
            let n = evs.iter().filter(|e| e.fd == a).count();
            assert_eq!(n, 1, "{name}: descriptor reported {n} times");
            let ev = evs.iter().find(|e| e.fd == a).expect("event");
            assert!(ev.readable && ev.writable, "{name}: both directions");

            let _ = sys::close(a);
        }
    }
}
