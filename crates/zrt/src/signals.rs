//! Signals as first-class descriptors.
//!
//! # Why this is where zinit beats runit and dinit
//!
//! Both of them install a handler for `SIGCHLD` that writes a byte to a pipe
//! — the "self-pipe trick" — and read that pipe from the event loop. It works,
//! and it has a hole.
//!
//! The hole is a **lost wakeup**. Between the moment the kernel decides to
//! deliver a signal and the moment the handler's `write` returns, the handler
//! runs in a context where the process can be in the middle of anything. A
//! write to a full pipe returns `EAGAIN` and the byte is *dropped*. The
//! handler is not re-run, the pipe is not readable, and the event loop blocks
//! in `epoll_wait` for a child death that already happened. The service is
//! dead; the supervisor thinks it is starting, forever.
//!
//! `runit` has lived with this for twenty years because the window is small
//! and the pipe is big. "Small" is not a correctness argument.
//!
//! zinit does not use a handler at all when it can avoid one:
//!
//! * **Linux**: `signalfd(2)`. The *kernel* queues the signal and turns it
//!   into a readable descriptor. There is no user-space window, no handler, no
//!   drop, no race. The signal arrives through the same [`crate::reactor`] as a log
//!   write or a child death, in the same `wait` call, in the order the kernel
//!   decided.
//! * **BSD**: `EVFILT_SIGNAL` on a kqueue. Same property, different spelling:
//!   the kernel posts to a kevent instead of running code.
//! * **Fallback**: the self-pipe, and it is *named* `SelfPipeSource` so that
//!   the degraded path is visible in the capability line rather than hidden
//!   behind a `sigaction`.
//!
//! The precondition for all three is the same and is not optional: **the
//! watched signals must be blocked in the supervisor's thread**. A blocked
//! signal is not discarded, it is *pending*, and both `signalfd` and
//! `EVFILT_SIGNAL` report pending signals. [`block_all`] does that once, at
//! startup, before anything else exists — which also means the supervisor has
//! no asynchronous signal handlers at all and the fork path in
//! [`crate::childproc`] can never be interrupted by a stray `SIGCHLD`.
//!
//! # What the supervisor does with them
//!
//! Signals *arrive* here and are *interpreted* by `zcore`, which owns the
//! meaning of a `SIGTERM` for a service in a given state. `zrt` does the
//! transport and nothing else; it converts a raw `signo` into a [`Signal`]
//! and stops.

use crate::sys;
use std::io;
use std::os::fd::RawFd;

/// A signal zinit can receive.
///
/// A closed enum with an escape hatch, not a raw `i32`: the send sites are
/// where mistakes happen, and `kill(pid, 9)` in a place that meant `9` as a
/// count is a bug that a type system can catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Signal {
    /// Polite stop. `zcore` decides whether it means "stop" or "reload".
    Term,
    /// Unstoppable stop. Only for the shutdown budget.
    Kill,
    /// Resume a stopped job control process.
    Cont,
    /// Reload configuration.
    Hup,
    /// Interactive interrupt.
    Int,
    /// Operator-defined hook; the first half of the control protocol.
    Usr1,
    /// Operator-defined hook; the second half.
    Usr2,
    /// A child changed state. The one the supervisor blocks at startup.
    Chld,
    /// Terminal resized.
    Winch,
    /// A writer died on a pipe whose reader ignored `SIGPIPE`.
    Pipe,
    /// Real-time timer. Present so `alarm(2)`-based code paths are typed.
    Alarm,
    /// A real signal this enum does not name, e.g. a machine check.
    Other(i32),
}

impl Signal {
    /// The `signo` value, from `libc`.
    ///
    /// The numbers are identical on every POSIX platform for the named
    /// signals, which is what lets a log line, a `kill` invocation and a
    /// config file agree without a table per OS.
    pub const fn as_raw(self) -> i32 {
        match self {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
            Signal::Cont => libc::SIGCONT,
            Signal::Hup => libc::SIGHUP,
            Signal::Int => libc::SIGINT,
            Signal::Usr1 => libc::SIGUSR1,
            Signal::Usr2 => libc::SIGUSR2,
            Signal::Chld => libc::SIGCHLD,
            Signal::Winch => libc::SIGWINCH,
            Signal::Pipe => libc::SIGPIPE,
            Signal::Alarm => libc::SIGALRM,
            Signal::Other(n) => n,
        }
    }

    /// Reverse mapping, or `None` for a signal that cannot be caught.
    ///
    /// `None` for `SIGKILL` and `SIGSTOP` is not pedantry: asking
    /// `signalfd` for them fails with `EINVAL`, and asking `sigaction` for
    /// them silently does nothing. Returning `None` lets the caller report
    /// the mistake at the point it is made.
    pub fn from_raw(signo: i32) -> Option<Signal> {
        let s = match signo {
            libc::SIGTERM => Signal::Term,
            libc::SIGKILL => Signal::Kill,
            libc::SIGCONT => Signal::Cont,
            libc::SIGHUP => Signal::Hup,
            libc::SIGINT => Signal::Int,
            libc::SIGUSR1 => Signal::Usr1,
            libc::SIGUSR2 => Signal::Usr2,
            libc::SIGCHLD => Signal::Chld,
            libc::SIGWINCH => Signal::Winch,
            libc::SIGPIPE => Signal::Pipe,
            libc::SIGALRM => Signal::Alarm,
            _ => {
                if (1..=64).contains(&signo) {
                    Signal::Other(signo)
                } else {
                    return None;
                }
            }
        };
        if s.is_catchable() { Some(s) } else { None }
    }

    /// Whether the kernel lets a process block, catch or ignore this.
    ///
    /// `SIGKILL` and `SIGSTOP` are the only two that cannot. Everything else
    /// on Linux, and everything except `SIGKILL`/`SIGSTOP` on the BSDs, is.
    pub const fn is_catchable(self) -> bool {
        !matches!(self, Signal::Kill) && self.as_raw() != libc::SIGSTOP
    }

    /// The canonical name, as it appears in logs.
    pub const fn name(self) -> &'static str {
        match self {
            Signal::Term => "SIGTERM",
            Signal::Kill => "SIGKILL",
            Signal::Cont => "SIGCONT",
            Signal::Hup => "SIGHUP",
            Signal::Int => "SIGINT",
            Signal::Usr1 => "SIGUSR1",
            Signal::Usr2 => "SIGUSR2",
            Signal::Chld => "SIGCHLD",
            Signal::Winch => "SIGWINCH",
            Signal::Pipe => "SIGPIPE",
            Signal::Alarm => "SIGALRM",
            Signal::Other(_) => "SIG?",
        }
    }
}

/// A set of signals, as a bitset over signal numbers 1..=64.
///
/// Backed by a `u64` rather than a `sigset_t` on purpose: the set is
/// manipulated, printed and compared long before it is handed to the kernel,
/// and a `sigset_t` cannot be inspected without a syscall.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct SignalSet(u64);

impl SignalSet {
    /// The empty set. Nothing is watched and nothing extra is blocked.
    pub const fn empty() -> Self {
        SignalSet(0)
    }

    /// A set containing every catchable signal the platform has.
    ///
    /// This is what [`block_all`] blocks. Every signal, not a list: a
    /// supervisor that blocks "the ones it knows about" is one `SIGWINCH` away
    /// from an asynchronous handler in the middle of a `fork`.
    pub fn all() -> Self {
        let mut s = SignalSet(u64::MAX);
        // Bits 0 and above 64 are not signals. Clearing bit 0 (signal 0 is
        // the "existence check" argument, never a real signal) and forcing
        // `from_raw` to reject the rest keeps `contains` honest.
        s.0 &= !1u64;
        s.remove(Signal::Kill);
        s.remove(Signal::Other(libc::SIGSTOP));
        s
    }

    /// A set holding one signal.
    pub fn with(sig: Signal) -> Self {
        let mut s = SignalSet::empty();
        s.insert(sig);
        s
    }

    /// Add a signal. Returns `false` for the two that cannot be caught,
    /// rather than pretending the insertion worked.
    pub fn insert(&mut self, sig: Signal) -> bool {
        let n = sig.as_raw();
        if !sig.is_catchable() || !(1..=64).contains(&n) {
            return false;
        }
        self.0 |= 1u64 << (n - 1);
        true
    }

    /// Remove a signal.
    pub fn remove(&mut self, sig: Signal) {
        let n = sig.as_raw();
        if (1..=64).contains(&n) {
            self.0 &= !(1u64 << (n - 1));
        }
    }

    /// Membership test.
    pub fn contains(self, sig: Signal) -> bool {
        let n = sig.as_raw();
        (1..=64).contains(&n) && self.0 & (1u64 << (n - 1)) != 0
    }

    /// How many signals are in the set.
    pub fn len(self) -> u32 {
        self.0.count_ones()
    }

    /// True when nothing is in the set.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The signals in the set, ascending by number.
    pub fn iter(self) -> impl Iterator<Item = Signal> {
        (1..=64i32).filter_map(move |n| {
            if self.0 & (1u64 << (n - 1)) != 0 {
                Signal::from_raw(n)
            } else {
                None
            }
        })
    }

    /// Merge another set into this one.
    pub fn union(&mut self, other: SignalSet) {
        self.0 |= other.0;
    }

    /// The kernel representation, built fresh each time.
    ///
    /// Built by asking the libc's own `sigaddset` rather than by writing bits
    /// into a `sigset_t`, because `sigset_t` is opaque and its layout differs
    /// between platforms and even between architectures.
    ///
    /// Signals the libc refuses are *skipped*, not fatal. glibc rejects 32
    /// and 33 with `EINVAL` because they belong to NPTL; musl accepts them.
    /// A `block_all` that failed because of a signal the supervisor could
    /// never have handled anyway would be a worse outcome than one that
    /// leaves two numbers out, so `EINVAL` is the one errno treated as "not
    /// available" here.
    fn to_sigset(self) -> io::Result<libc::sigset_t> {
        // SAFETY: all zeros is a valid empty signal set.
        let mut set: libc::sigset_t = unsafe { core::mem::zeroed() };
        for sig in self.iter() {
            // SAFETY: `set` is a live, zeroed `sigset_t` and `sigaddset` only
            // sets a bit within it. Passing a number outside 1..=NSIG is
            // undefined, which is why `iter` can only produce 1..=64 and why
            // `contains`/`insert` reject everything else.
            if unsafe { libc::sigaddset(&raw mut set, sig.as_raw()) } != 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINVAL) {
                    continue;
                }
                return Err(e);
            }
        }
        Ok(set)
    }
}

/// Block a set of signals **in the calling thread**.
///
/// The whole design of this module depends on it, and the order matters: the
/// supervisor calls [`block_all`] as the first thing it does, before the
/// reactor exists, before any child is forked. After that call the supervisor
/// has *no* asynchronous signal handlers and cannot be interrupted at an
/// arbitrary instruction, which is what makes
/// [`crate::childproc::fork_tracked`] safe to write.
///
/// Note this is per-thread (`pthread_sigmask`), not per-process: the
/// supervisor is single-threaded by design. It is *not*, however, something a
/// spawned child can ignore — `fork` copies the blocked set and `execve`
/// preserves it, so every process forked from a supervisor that called this
/// runs with all of it blocked unless it clears the mask itself, which
/// `zservice::spawn`'s child path does as its very first step.
pub fn block(set: SignalSet) -> io::Result<()> {
    let raw = set.to_sigset()?;
    // SAFETY: `raw` is a live, fully built `sigset_t` and `pthread_sigmask`
    // reads it; it is not retained. `SIG_BLOCK` is a valid `how`.
    if unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Block every catchable signal in the calling thread.
///
/// The idempotent, one-line way to guarantee the invariant above. Cheap enough
/// to be called defensively at the top of every entry point that spawns.
pub fn block_all() -> io::Result<()> {
    block(SignalSet::all())
}

/// The signals currently blocked in the calling thread.
///
/// Exists for the startup assertion: if this does not contain `SIGCHLD` after
/// [`block_all`], the fork-safety argument in [`crate::childproc`] does not
/// hold and the supervisor should refuse to start rather than run with a
/// window it cannot reason about.
pub fn blocked() -> io::Result<SignalSet> {
    // SAFETY: all zeros is a valid empty signal set; read back by `pthread_sigmask` below.
    let mut cur: libc::sigset_t = unsafe { core::mem::zeroed() };
    // SAFETY: `cur` is a live, zeroed `sigset_t` that `pthread_sigmask` fills
    // with the current mask; it is not retained.
    if unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &raw mut cur) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut set = SignalSet::empty();
    for n in 1..=64i32 {
        // SAFETY: `cur` is a valid, kernel-written `sigset_t`; `sigismember`
        // is a pure query on it.
        if unsafe { libc::sigismember(&raw const cur, n) } == 1 {
            set.insert(Signal::Other(n));
        }
    }
    Ok(set)
}

/// Send a signal to the calling thread.
///
/// Async-signal-safe, and used by the tests below to prove the delivery path
/// without a second process. `pthread_kill(pthread_self(), ...)` rather than
/// `kill(getpid(), ...)`: a process-directed signal is delivered to *any*
/// thread with it unblocked, which in a multi-threaded test binary is a
/// coin flip.
pub fn raise_to_self(sig: Signal) -> io::Result<()> {
    // SAFETY: `pthread_self()` returns a valid thread handle for the calling
    // thread and is always valid; `pthread_kill` takes a signal number and
    // returns 0 or an error number. No pointers are retained.
    let rc = unsafe { libc::pthread_kill(libc::pthread_self(), sig.as_raw()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

/// Which backend a [`SignalSource`] ended up using.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum SignalSourceKind {
    /// Linux `signalfd(2)`. No user-space handler at all.
    Signalfd,
    /// BSD `EVFILT_SIGNAL` through a private kqueue.
    Kqueue,
    /// A pipe written by an async signal handler.
    SelfPipe,
}

impl SignalSourceKind {
    /// The name for the capability line.
    pub const fn name(self) -> &'static str {
        match self {
            SignalSourceKind::Signalfd => "signalfd",
            SignalSourceKind::Kqueue => "kqueue-signal",
            SignalSourceKind::SelfPipe => "self-pipe",
        }
    }

    /// Whether delivery is done by the kernel with no user-space window.
    ///
    /// False means the self-pipe fallback is in use, and a dropped write is
    /// possible. This is the single most important boolean in this module and
    /// it is why the kind is reported rather than hidden.
    pub const fn is_kernel_delivered(self) -> bool {
        matches!(self, SignalSourceKind::Signalfd | SignalSourceKind::Kqueue)
    }
}

/// A source of signals that can be plugged into a [`crate::reactor::Reactor`].
///
/// The contract is the point: a signal source is *just another descriptor*.
/// The supervisor registers [`SignalSource::fds`] in the same reactor it uses
/// for log writes and child deaths, so a signal cannot be observed before or
/// after some other event in a way the loop cannot order. A source with its
/// own epoll or kqueue nested inside the main one is the one exception, and it
/// still delivers through the same `wait`.
pub trait SignalSource {
    /// Descriptors to hand to the reactor. Register them
    /// [`crate::reactor::Interest::Read`].
    fn fds(&self) -> &[RawFd];

    /// Consume everything pending. Returns nothing if there is nothing, which
    /// is not an error.
    fn drain(&mut self) -> io::Result<Vec<Signal>>;

    /// Which backend this is, for the boot log.
    fn kind(&self) -> SignalSourceKind;
}

/// Build the best signal source available here.
///
/// Tries the kernel-delivered backends first and falls back to the self-pipe
/// with an explicit announcement. The fallback is *not* an error: a system
/// that cannot do `signalfd` can still be supervised, just with the hole this
/// module's introduction is about.
pub fn new_signal_source(watch: SignalSet) -> io::Result<Box<dyn SignalSource>> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        match SignalfdSource::new(watch) {
            Ok(s) => return Ok(Box::new(s)),
            Err(e) => crate::report::announce_degradation(&format!(
                "signalfd unavailable ({e}); degrading to self-pipe"
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
        match KqueueSignalSource::new(watch) {
            Ok(s) => return Ok(Box::new(s)),
            Err(e) => crate::report::announce_degradation(&format!(
                "kqueue EVFILT_SIGNAL unavailable ({e}); degrading to self-pipe"
            )),
        }
    }
    let s = SelfPipeSource::new(watch)?;
    Ok(Box::new(s))
}

// ─────────────────────────────────────────────────────────────────────────────
// signalfd (Linux)
// ─────────────────────────────────────────────────────────────────────────────

/// Linux `signalfd(2)`.
///
/// The signal is queued by the kernel against the *set* given at creation
/// time, and becomes readable on the returned descriptor. Three properties
/// fall out of that and are the entire reason this backend exists:
///
/// * no user-space handler runs, so nothing can be lost between "the kernel
///   decided" and "somebody looked";
/// * signals are reported *coalesced* per pending signal number, exactly as
///   the kernel would deliver them, so a burst of `SIGWINCH` is one read and
///   not a thousand;
/// * the fd is pollable, so the signal goes through [`crate::reactor::Reactor`]
///   with no special case anywhere in the loop.
///
/// The signals in the set **must already be blocked** in the thread that
/// creates the source; otherwise they are handled or ignored by whatever
/// disposition is installed and never reach the signalfd. Use
/// [`SignalfdSource::new_blocked`] to get that right by construction.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Debug)]
pub struct SignalfdSource {
    fd: RawFd,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl SignalfdSource {
    /// Create a signalfd for signals that are *already blocked* in this
    /// thread.
    pub fn new(watch: SignalSet) -> io::Result<Self> {
        let raw = watch.to_sigset()?;
        // SAFETY: `raw` is a live, fully populated `sigset_t`; `signalfd`
        // copies it into the kernel and does not retain the pointer. The
        // flags request close-on-exec and non-blocking, both of which the
        // descriptor needs to be usable from a level-triggered reactor.
        let fd = unsafe { libc::signalfd(-1, &raw, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    /// Block `watch` in the calling thread and then create the source.
    ///
    /// The constructor the supervisor should use. Getting the order wrong is
    /// the one way to build a signalfd that silently never fires.
    pub fn new_blocked(watch: SignalSet) -> io::Result<Self> {
        block(watch)?;
        Self::new(watch)
    }

    /// The descriptor to register in the reactor.
    pub fn fd(&self) -> RawFd {
        self.fd
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl SignalSource for SignalfdSource {
    fn fds(&self) -> &[RawFd] {
        // Safe as a slice of one: `self.fd` is a field of a live value.
        std::slice::from_ref(&self.fd)
    }

    fn drain(&mut self) -> io::Result<Vec<Signal>> {
        // `signalfd_siginfo` is 128 bytes; the kernel writes whole records and
        // the read returns a whole number of them, so the buffer size is
        // checked for alignment rather than parsed.
        let mut buf = vec![0u8; 128 * 8];
        let mut out = Vec::new();
        loop {
            // SAFETY: `buf` is a live heap buffer and the kernel is told its
            // exact size; `signalfd` writes whole `signalfd_siginfo` records
            // and never writes past the end. EAGAIN means "drained", which is
            // the normal end of this loop.
            let n =
                unsafe { libc::read(self.fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EAGAIN) {
                    return Ok(out);
                }
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                // A signalfd never reports EOF; if it does, the descriptor is
                // broken and looping would spin forever.
                return Ok(out);
            }
            let n = n as usize;
            for chunk in buf[..n].chunks_exact(128) {
                // The record starts with two `u32`s, `ssi_signo` and
                // `ssi_errno`, in native endianness. Reading the first four
                // bytes is the documented, portable way to get the number
                // without depending on the full struct layout.
                let signo = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as i32;
                if let Some(sig) = Signal::from_raw(signo) {
                    out.push(sig);
                }
            }
        }
    }

    fn kind(&self) -> SignalSourceKind {
        SignalSourceKind::Signalfd
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl Drop for SignalfdSource {
    fn drop(&mut self) {
        // Dropping the signalfd releases the kernel's queue, which also drops
        // any signal that was pending for it. That is correct at shutdown and
        // worth knowing at any other time.
        let _ = sys::close_quietly(self.fd);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// kqueue EVFILT_SIGNAL (BSD)
// ─────────────────────────────────────────────────────────────────────────────

/// BSD `EVFILT_SIGNAL`: the kernel's own signal source, through a private
/// kqueue.
///
/// The same property as `signalfd` — no handler, no lost wakeup — with
/// kqueue's spelling. `EV_CLEAR` is used **here** even though the I/O reactor
/// does without it, and the reason is the difference between the two filters:
///
/// * `EVFILT_READ` fires when a condition becomes true. `EV_CLEAR` would make
///   it fire once per transition, which is edge semantics and buys nothing
///   for a supervisor that drains on every wakeup.
/// * `EVFILT_SIGNAL` is *already* a change-notification filter: it has no
///   "condition" to be level-stable on. Without `EV_CLEAR` the knote stays
///   active and every subsequent `kevent` re-reports a signal that has already
///   been handled, forever. With it, each delivery arms the knote once.
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
pub struct KqueueSignalSource {
    kq: RawFd,
    watched: usize,
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
impl KqueueSignalSource {
    /// One knote per watched signal, all on one private kqueue.
    pub fn new(watch: SignalSet) -> io::Result<Self> {
        // SAFETY: `kqueue()` takes no arguments; the descriptor is owned by
        // this struct and closed in `Drop`.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        sys::set_cloexec(kq, true)?;
        let mut s = Self { kq, watched: 0 };
        for sig in watch.iter() {
            s.add_knote(sig)?;
        }
        Ok(s)
    }

    fn add_knote(&mut self, sig: Signal) -> io::Result<()> {
        let k = crate::reactor::kevent_new(
            sig.as_raw() as libc::uintptr_t,
            libc::EVFILT_SIGNAL as i32,
            libc::EV_ADD | libc::EV_CLEAR,
            sig.as_raw() as u32,
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
        self.watched += 1;
        Ok(())
    }

    /// The kqueue descriptor to register in the main reactor.
    pub fn fd(&self) -> RawFd {
        self.kq
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
impl SignalSource for KqueueSignalSource {
    fn fds(&self) -> &[RawFd] {
        std::slice::from_ref(&self.kq)
    }

    fn drain(&mut self) -> io::Result<Vec<Signal>> {
        // SAFETY: all zeros is a valid empty event array; the kernel fills it.
        let mut buf: [libc::kevent; 16] = unsafe { core::mem::zeroed() };
        // A zero timeout: this is called *because* the reactor said the kqueue
        // is readable, so there is nothing to wait for.
        let ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `buf` is a live array of 16 `kevent`s and that is the
        // capacity passed. A null changelist with length 0 is "no changes".
        let n = unsafe {
            libc::kevent(
                self.kq,
                std::ptr::null(),
                0,
                buf.as_mut_ptr(),
                16 as _,
                &raw const ts,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out = Vec::with_capacity(n as usize);
        for ev in buf.iter().take(n as usize) {
            // `fflags` holds the signal number for a triggered EVFILT_SIGNAL
            // (and NOTE_TRIGGERED is set alongside it).
            if let Some(sig) = Signal::from_raw(Into::<u64>::into(ev.fflags) as u32 as i32) {
                out.push(sig);
            }
        }
        Ok(out)
    }

    fn kind(&self) -> SignalSourceKind {
        SignalSourceKind::Kqueue
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
impl Drop for KqueueSignalSource {
    fn drop(&mut self) {
        let _ = sys::close_quietly(self.kq);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Self-pipe (universal fallback)
// ─────────────────────────────────────────────────────────────────────────────

/// The write end of the process-wide self-pipe, set by the first
/// [`SelfPipeSource`] and never cleared.
///
/// A signal handler cannot take arguments, cannot allocate and cannot lock, so
/// it can only reach a global. That is the whole reason the self-pipe design
/// is fragile, and it is why this is a *fallback*: one global pipe, one
/// handler, no way to have two of them in one process.
static PIPE_WRITE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// The universal fallback: a pipe written by a `sigaction` handler.
///
/// # What is wrong with it, stated plainly
///
/// * A full pipe drops the byte. The kernel buffers 64 KiB by default and a
///   supervisor drains on every wakeup, so the window is small — but it is not
///   zero, and there is no way to close it from user space.
/// * The handler is **process-wide**. Constructing a second `SelfPipeSource`
///   reuses the first pipe and re-arms the handler, so two sources are not two
///   independent sources. Enforced by returning an error, not by hoping.
/// * The handler clobbers any disposition the program had installed. The old
///   one is saved and restored on `Drop`, which is the best that can be done.
///
/// Use it because the alternative on that platform is *no signals at all*, and
/// [`SignalSourceKind::is_kernel_delivered`] exists so the boot log says so.
#[derive(Debug)]
pub struct SelfPipeSource {
    read_fd: RawFd,
    write_fd: RawFd,
    /// The dispositions this source replaced, restored in `Drop`.
    saved: Vec<(Signal, libc::sigaction)>,
}

impl SelfPipeSource {
    /// Install the handler and create the pipe.
    ///
    /// Fails if a `SelfPipeSource` already exists in this process: the handler
    /// is a global, and pretending two sources are independent would be a lie
    /// that shows up as lost signals much later.
    pub fn new(watch: SignalSet) -> io::Result<Self> {
        let (_r, w) = sys::pipe2(true)?;
        sys::set_nonblocking(_r, true)?;
        sys::set_nonblocking(w, true)?;
        // Claim the global before doing anything else, so a second caller
        // fails fast instead of half-installing.
        if PIPE_WRITE
            .compare_exchange(
                -1,
                w,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            let _ = sys::close(_r);
            let _ = sys::close(w);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a SelfPipeSource already exists in this process; \
                 the sigaction handler is process-wide",
            ));
        }
        let mut saved = Vec::new();
        for sig in watch.iter() {
            match install_handler(sig) {
                Ok(old) => saved.push((sig, old)),
                Err(e) => {
                    for (s, old) in saved {
                        let _ = restore_handler(s, &old);
                    }
                    PIPE_WRITE.store(-1, std::sync::atomic::Ordering::SeqCst);
                    let _ = sys::close(_r);
                    let _ = sys::close(w);
                    return Err(e);
                }
            }
        }
        Ok(Self {
            read_fd: _r,
            write_fd: w,
            saved,
        })
    }

    /// The descriptor to register in the reactor.
    pub fn fd(&self) -> RawFd {
        self.read_fd
    }
}

impl SignalSource for SelfPipeSource {
    fn fds(&self) -> &[RawFd] {
        std::slice::from_ref(&self.read_fd)
    }

    fn drain(&mut self) -> io::Result<Vec<Signal>> {
        let mut buf = [0u8; 256];
        let mut out = Vec::new();
        loop {
            // SAFETY: `buf` is a live stack array and the kernel is told its
            // exact size. EAGAIN is the normal end of the drain.
            let n = unsafe {
                libc::read(
                    self.read_fd,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EAGAIN) {
                    return Ok(out);
                }
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                return Ok(out);
            }
            for b in &buf[..n as usize] {
                let signo = *b as i32;
                if let Some(sig) = Signal::from_raw(signo) {
                    out.push(sig);
                }
            }
        }
    }

    fn kind(&self) -> SignalSourceKind {
        SignalSourceKind::SelfPipe
    }
}

impl Drop for SelfPipeSource {
    fn drop(&mut self) {
        for (sig, old) in &self.saved {
            let _ = restore_handler(*sig, old);
        }
        // Restore the global only if nobody else has claimed it in the
        // meantime; a later source would own a different pipe.
        let _ = PIPE_WRITE.compare_exchange(
            self.write_fd,
            -1,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        );
        let _ = sys::close_quietly(self.read_fd);
        let _ = sys::close_quietly(self.write_fd);
    }
}

/// The signal handler: write the signal number to the global pipe, and
/// nothing else.
///
/// It is async-signal-safe by construction — no allocation, no locks, no
/// syscalls other than `write` — and it does exactly one thing so that it can
/// be read in thirty seconds by someone debugging at 3am.
extern "C" fn selfpipe_handler(signo: libc::c_int) {
    let w = PIPE_WRITE.load(std::sync::atomic::Ordering::SeqCst);
    if w < 0 {
        return;
    }
    let byte = signo as u8;
    // SAFETY: `write` on a non-blocking pipe is async-signal-safe, the
    // pointer is a single local byte whose address cannot escape, and a
    // failure (EAGAIN on a full pipe) is deliberately ignored — that is the
    // documented weakness of this backend, not a bug to fix here. There is
    // nothing an async handler can usefully do about it.
    unsafe {
        libc::write(w, (&raw const byte).cast::<libc::c_void>(), 1);
    }
}

fn install_handler(sig: Signal) -> io::Result<libc::sigaction> {
    // SAFETY: all zeros is a valid empty action; fields are set before `sigaction` below.
    let mut new: libc::sigaction = unsafe { core::mem::zeroed() };
    new.sa_sigaction = selfpipe_handler as *const () as usize;
    new.sa_flags = libc::SA_RESTART;
    // SAFETY: `new` is a live, fully initialised `sigaction` with a valid
    // handler and an empty mask; `sigaction` copies it. `old` is a live local.
    let mut old: libc::sigaction = unsafe { core::mem::zeroed() };
    if unsafe { libc::sigaction(sig.as_raw(), &raw mut new, &raw mut old) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(old)
}

fn restore_handler(sig: Signal, old: &libc::sigaction) -> io::Result<()> {
    // SAFETY: `old` is a `sigaction` that came back from `sigaction(2)` and
    // has not been modified, which is exactly the precondition for restoring
    // it.
    let old_ptr = std::ptr::from_ref(old);
    if unsafe { libc::sigaction(sig.as_raw(), old_ptr, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — a real signal, delivered to this thread, through the reactor
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reactor::Interest;

    #[test]
    fn set_operations_are_correct() {
        let mut s = SignalSet::empty();
        assert!(s.is_empty());
        assert!(s.insert(Signal::Usr1));
        assert!(s.contains(Signal::Usr1));
        assert!(!s.contains(Signal::Usr2));
        assert!(!s.insert(Signal::Kill), "SIGKILL is not catchable");
        assert!(!s.contains(Signal::Kill));
        s.insert(Signal::Usr2);
        assert_eq!(s.len(), 2);
        let names: Vec<_> = s.iter().map(|x| x.name()).collect();
        assert_eq!(names, vec!["SIGUSR1", "SIGUSR2"]);
        s.remove(Signal::Usr1);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn all_excludes_the_two_uncatchable() {
        let all = SignalSet::all();
        assert!(all.contains(Signal::Chld));
        assert!(all.contains(Signal::Term));
        assert!(!all.contains(Signal::Kill));
        assert!(!all.contains(Signal::Other(libc::SIGSTOP)));
    }

    #[test]
    fn block_all_actually_blocks_and_is_idempotent() {
        // Per-thread, so this test does not disturb the rest of the binary.
        block_all().expect("pthread_sigmask");
        let b = blocked().expect("query mask");
        assert!(b.contains(Signal::Chld), "SIGCHLD must be blocked");
        assert!(b.contains(Signal::Term), "SIGTERM must be blocked");
        block_all().expect("second call is a no-op");
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn signalfd_delivers_through_the_reactor() {
        // The whole point of the module, end to end: block a signal, raise it
        // on this thread, and have it come out of `wait` like any other event.
        block(SignalSet::with(Signal::Usr1)).expect("block");
        let mut src = SignalfdSource::new(SignalSet::with(Signal::Usr1)).expect("signalfd");
        let mut reactor = crate::reactor::new_reactor().expect("reactor");
        for fd in src.fds() {
            reactor.add(*fd, Interest::Read).expect("register signalfd");
        }

        raise_to_self(Signal::Usr1).expect("pthread_kill");

        let evs = reactor
            .wait(Some(2000))
            .expect("signals must not arrive as an error");
        assert!(
            evs.iter().any(|e| e.fd == src.fd() && e.readable),
            "the signalfd must be reported readable, got {evs:?}"
        );
        let got = src.drain().expect("drain");
        assert_eq!(got, vec![Signal::Usr1]);
        assert_eq!(src.kind(), SignalSourceKind::Signalfd);
        assert!(src.kind().is_kernel_delivered());

        // Draining twice must not invent a second signal.
        assert!(src.drain().expect("drain again").is_empty());
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn a_burst_of_signals_is_coalesced_not_lost() {
        // Three `SIGUSR1` in a row must produce at least one delivery, and
        // the loop must not spin. This is the property the self-pipe can only
        // approximate.
        block(SignalSet::with(Signal::Usr1)).expect("block");
        let mut src = SignalfdSource::new(SignalSet::with(Signal::Usr1)).expect("signalfd");
        for _ in 0..3 {
            raise_to_self(Signal::Usr1).expect("raise");
        }
        let got = src.drain().expect("drain");
        assert!(!got.is_empty(), "at least one delivery must be visible");
        assert!(got.iter().all(|s| *s == Signal::Usr1));
    }

    #[test]
    fn new_signal_source_picks_a_kernel_backed_one() {
        block(SignalSet::with(Signal::Usr2)).expect("block");
        let src = new_signal_source(SignalSet::with(Signal::Usr2)).expect("source");
        assert_eq!(src.fds().len(), 1, "a signal source owns one descriptor");
        assert_ne!(src.kind(), SignalSourceKind::SelfPipe);
    }

    #[test]
    fn selfpipe_delivers_through_the_reactor_too() {
        // The fallback has to actually work, not merely exist. It installs a
        // process-wide handler, so it uses SIGUSR2 and refuses to coexist with
        // another `SelfPipeSource` — both of which the test also checks.
        //
        // Note what is *not* here: `block()`. The two designs are mutually
        // exclusive on the same signal. `signalfd` needs the signal blocked
        // so the kernel queues it; the self-pipe needs it *unblocked* so the
        // handler runs at all. Blocking a signal and then installing a
        // handler for it means the signal is queued forever and the pipe
        // stays empty — a silent, permanent failure that looks exactly like
        // "no signals are arriving".
        let mut src = SelfPipeSource::new(SignalSet::with(Signal::Usr2)).expect("self-pipe");
        assert!(
            SelfPipeSource::new(SignalSet::with(Signal::Usr2)).is_err(),
            "a second SelfPipeSource must be refused, not silently aliased"
        );

        let mut reactor = crate::reactor::new_reactor().expect("reactor");
        reactor
            .add(src.fd(), Interest::Read)
            .expect("register pipe");
        raise_to_self(Signal::Usr2).expect("raise");

        let evs = reactor.wait(Some(2000)).expect("wait");
        assert!(evs.iter().any(|e| e.fd == src.fd() && e.readable));
        assert_eq!(src.drain().expect("drain"), vec![Signal::Usr2]);
        assert!(!src.kind().is_kernel_delivered());
    }

    #[test]
    fn signal_round_trips_through_raw_and_name() {
        for sig in [
            Signal::Term,
            Signal::Cont,
            Signal::Hup,
            Signal::Int,
            Signal::Usr1,
            Signal::Usr2,
            Signal::Chld,
            Signal::Winch,
            Signal::Pipe,
            Signal::Alarm,
        ] {
            assert_eq!(Signal::from_raw(sig.as_raw()), Some(sig), "{}", sig.name());
        }
        // `as_raw` round-trips even for the uncatchable ones; `from_raw`
        // deliberately does not, because asking for them is always a bug at
        // the call site.
        assert_eq!(Signal::Kill.as_raw(), libc::SIGKILL);
        assert_eq!(Signal::from_raw(libc::SIGKILL), None);
        assert_eq!(Signal::from_raw(libc::SIGSTOP), None);
        assert_eq!(Signal::from_raw(0), None);
        assert_eq!(Signal::from_raw(9999), None);
    }
}
