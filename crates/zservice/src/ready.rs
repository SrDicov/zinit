//! Readiness: how the supervisor learns a service is actually usable.
//!
//! # The four adapters (`DESIGN.md` §6)
//!
//! | Adapter | Mechanism |
//! |---|---|
//! | `none` | `Running` as soon as the fork succeeds — no handshake at all. |
//! | `notify` | The service writes a line to fd 3 (`ZINIT_NOTIFY_FD=3`). |
//! | `ping:<cmd>` | The supervisor re-runs `cmd` every 250 ms until it exits 0. |
//! | `tcp:<port>` | `connect()` to `127.0.0.1:<port>` until it is accepted. |
//!
//! # The rule that makes this ergonomic
//!
//! The readiness timeout **never blocks the boot**. When it expires, the
//! service is declared `Running` anyway — with a warning for the lenient
//! variants, with an error for `Strict`. `zcore::due_events` already encodes
//! that mapping; [`ManagedService`](crate::service::ManagedService) only
//! feeds it the clock. A service without a handshake (`none`) needs no
//! deadline at all, and arming one would be a way to make a config file lie.
//!
//! # Reactor integration
//!
//! A readiness wait is either "a descriptor to watch" (notify) or "a moment
//! to retry" (ping/tcp). [`ReadyWait::poll_need`] reports exactly that as a
//! [`PollNeed`], so the supervisor registers the fd or arms the timer without
//! knowing which adapter it is serving.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::RawFd;

use zcore::{Ready, StrictReady};

/// Re-run `ping` commands and retry `tcp` connects this often.
///
/// 250 ms: fast enough that a service ready in 300 ms does not sit a whole
/// second, slow enough that a failing `ping` (a fork+exec each time) does not
/// become a fork bomb of its own. Matches the `ping` row of `DESIGN.md` §6.
pub const POLL_PING_INTERVAL_MS: u64 = 250;

/// How long one `ping` execution may run before it counts as failed.
///
/// A hung check command must not wedge the supervisor's event loop forever:
/// past this deadline the child is `SIGKILL`ed, reaped, and the probe reports
/// "not ready". Generous (5 s) on purpose — a slow check is a correct check,
/// and killing it early would flap a healthy service.
pub const PING_RUN_TIMEOUT_MS: u64 = 5_000;

/// How long a `tcp` connect may take.
///
/// `localhost` connects complete in microseconds or refuse immediately; the
/// deadline exists for the pathological case (a full backlog, a firewall
/// rule that drops instead of refusing) and is deliberately short.
pub const TCP_CONNECT_TIMEOUT_MS: u64 = 2_000;

/// What the reactor must do to serve one readiness wait.
///
/// Either half may be absent: `none` needs nothing, `notify` needs only the
/// fd, `ping`/`tcp` need only the retry instant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PollNeed {
    /// Descriptor to watch for readability (`notify`), if any.
    pub fd: Option<RawFd>,
    /// Monotonic ms at which to retry a `ping`/`tcp` probe, if any.
    pub timeout_ms: Option<u64>,
}

impl PollNeed {
    /// Nothing to watch and nowhere to be: the `none` adapter.
    pub const fn idle() -> PollNeed {
        PollNeed {
            fd: None,
            timeout_ms: None,
        }
    }

    /// True when there is nothing for the reactor to do.
    pub const fn is_idle(self) -> bool {
        self.fd.is_none() && self.timeout_ms.is_none()
    }
}

/// One service's pending readiness handshake.
///
/// Built once per start from the frozen [`Ready`] plus the notify read-end
/// the parent kept (for the notify variants). `None` is a real state, not an
/// absence: a service with no handshake is ready the moment it is polled.
#[derive(Clone, Debug)]
pub enum ReadyWait {
    /// No handshake. [`ReadyWait::check`] answers `true` immediately.
    None,
    /// A line on this (nonblocking, parent-side) read-end means ready. `EOF`
    /// without a line is **not** ready — the child closed the pipe without
    /// notifying, and its death will arrive separately through the reaper.
    Notify {
        /// Parent-side read-end of the notify pipe.
        fd: RawFd,
    },
    /// Re-run `command` through `/bin/sh -c` every [`POLL_PING_INTERVAL_MS`].
    Ping {
        /// The check command, exactly as configured.
        command: String,
        /// Monotonic ms at which the next execution is due.
        next_due_ms: u64,
    },
    /// `connect()` to `127.0.0.1:port` every [`POLL_PING_INTERVAL_MS`].
    Tcp {
        /// Destination port on loopback.
        port: u16,
        /// Monotonic ms at which the next attempt is due.
        next_due_ms: u64,
    },
}

impl ReadyWait {
    /// Build the waiter for one start of service `idx`.
    ///
    /// `notify_fd` is the parent's read-end, required for the notify
    /// variants and ignored otherwise. `now_ms` seeds the first probe so a
    /// `ping`/`tcp` service is checked immediately, not after one interval.
    pub fn from_ready(ready: &Ready, notify_fd: Option<RawFd>, now_ms: u64) -> ReadyWait {
        match ready.protocol() {
            None => ReadyWait::None,
            Some(StrictReady::Notify) => match notify_fd {
                Some(fd) => ReadyWait::Notify { fd },
                // No pipe (the spawn was told otherwise, or the fd was
                // already consumed): without a descriptor there is nothing
                // to wait on, so this degrades to immediate readiness rather
                // than to a wait that can never complete.
                None => ReadyWait::None,
            },
            Some(StrictReady::Ping(cmd)) => ReadyWait::Ping {
                command: cmd,
                next_due_ms: now_ms,
            },
            Some(StrictReady::Tcp(port)) => ReadyWait::Tcp {
                port,
                next_due_ms: now_ms,
            },
        }
    }

    /// What the reactor must do to serve this wait, as of `now_ms`.
    pub fn poll_need(&self, now_ms: u64) -> PollNeed {
        match *self {
            ReadyWait::None => PollNeed::idle(),
            ReadyWait::Notify { fd } => PollNeed {
                fd: Some(fd),
                timeout_ms: None,
            },
            ReadyWait::Ping { next_due_ms, .. } | ReadyWait::Tcp { next_due_ms, .. } => PollNeed {
                fd: None,
                timeout_ms: Some(next_due_ms.max(now_ms)),
            },
        }
    }

    /// The service index this waiter belongs to is tracked by the caller;
    /// this is the per-service probe step.
    ///
    /// Returns `Ok(true)` the moment the handshake completes. `Ok(false)`
    /// means "not yet" — due in the future, or attempted and failed — and the
    /// caller should consult [`ReadyWait::poll_need`] for when to ask again.
    /// `Err` is reserved for the probe itself being broken (the pipe died
    /// under us, the socket could not be created); a *failing check* is
    /// `Ok(false)`, because a service that is not ready is routine, not an
    /// error.
    pub fn check(&mut self, now_ms: u64) -> io::Result<bool> {
        match self {
            ReadyWait::None => Ok(true),
            ReadyWait::Notify { fd } => check_notify(*fd),
            ReadyWait::Ping {
                command,
                next_due_ms,
            } => {
                if now_ms < *next_due_ms {
                    return Ok(false);
                }
                *next_due_ms = now_ms.saturating_add(POLL_PING_INTERVAL_MS);
                run_check(command, PING_RUN_TIMEOUT_MS)
            }
            ReadyWait::Tcp { port, next_due_ms } => {
                if now_ms < *next_due_ms {
                    return Ok(false);
                }
                *next_due_ms = now_ms.saturating_add(POLL_PING_INTERVAL_MS);
                probe_tcp(*port)
            }
        }
    }

    /// A stable adapter name for logs (`none`, `notify`, `ping`, `tcp`).
    pub fn name(&self) -> &'static str {
        match self {
            ReadyWait::None => "none",
            ReadyWait::Notify { .. } => "notify",
            ReadyWait::Ping { .. } => "ping",
            ReadyWait::Tcp { .. } => "tcp",
        }
    }
}

/// Drain a nonblocking notify read-end: any byte is readiness.
///
/// Reads until `EAGAIN`: a half-written line still counts (the writer wrote,
/// which is the handshake — line discipline is the service's business, and
/// waiting for a newline the service never sends would hang the boot on a
/// technicality). `EOF` (closed without writing) is `Ok(false)`: the verdict
/// on that child belongs to the reaper, not to this probe.
fn check_notify(fd: RawFd) -> io::Result<bool> {
    let mut buf = [0u8; 64];
    match zrt::sys::read(fd, &mut buf) {
        Ok(0) => Ok(false),
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(e) => Err(e),
    }
}

/// Run `command` via `/bin/sh -c` and report whether it exits 0.
///
/// The child is a real fork+exec (not `std::process::Command`, which would
/// put a `SIGCHLD`-and-`waitpid` user inside the supervisor's own reaper
/// bookkeeping): the pid is reaped here, on every path, so no zombie can
/// escape into PID 1's table. On timeout the child is `SIGKILL`ed first —
/// a hung check is "not ready", not a leaked process.
///
/// argv, envp and the pipe are all prepared **before** the fork; the child
/// path touches nothing but syscalls and `_exit`, per `DESIGN.md` §9.1.
pub fn run_check(command: &str, timeout_ms: u64) -> io::Result<bool> {
    let sh = CString::new("/bin/sh").expect("static literal has no NUL");
    let dash_c = CString::new("-c").expect("static literal has no NUL");
    let cmd = CString::new(command).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "ping command contains a NUL byte",
        )
    })?;
    let envp = current_envp()?;
    let argv = [
        sh.as_ptr(),
        dash_c.as_ptr(),
        cmd.as_ptr(),
        core::ptr::null(),
    ];
    let (r, w) = zrt::sys::pipe2(true)
        .map_err(|e| io::Error::new(e.kind(), format!("ping probe pipe failed: {e}")))?;
    // SAFETY: `fork` takes no arguments; the child-side obligations (no
    // allocation, no locks, straight to `execve` or `_exit`) are met below.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let _ = zrt::sys::close_quietly(r);
        let _ = zrt::sys::close_quietly(w);
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        run_check_child(r, w, &sh, &argv, envp.as_ptrs());
    }
    // Parent: close the write end so the read-end sees EOF the moment the
    // child (and only the child) goes away.
    let _ = zrt::sys::close(w);
    let deadline = zrt::clock::now_ms().saturating_add(timeout_ms);
    loop {
        let remaining = deadline.saturating_sub(zrt::clock::now_ms());
        // The pipe is only a wakeup: readability (or HUP on child death)
        // means "ask waitpid", it is never actually read.
        let _ = poll_readable(r, remaining.min(50));
        match zrt::sys::waitpid_nohang(pid)? {
            Some(waited) => {
                let _ = zrt::sys::close(r);
                return Ok(matches!(waited.status, zrt::sys::ExitStatus::Exited(0)));
            }
            None => {
                if zrt::clock::now_ms() >= deadline {
                    // Hung check: kill, reap, report not-ready. The blocking
                    // reap cannot hang — the child is either dead already or
                    // about to be, by our own hand.
                    let _ = zrt::sys::kill_process(pid, zrt::signals::Signal::Kill);
                    let _ = zrt::sys::waitpid_blocking(pid);
                    let _ = zrt::sys::close(r);
                    return Ok(false);
                }
            }
        }
    }
}

/// The child half of [`run_check`]: stdio to `/dev/null`, then `exec`.
#[inline(never)]
fn run_check_child(
    r: RawFd,
    w: RawFd,
    sh: &CStr,
    argv: &[*const libc::c_char],
    envp: &[*const libc::c_char],
) -> ! {
    // SAFETY: every call below is a raw syscall on ints and pointers prepared
    // before the fork. No allocation, no locks, no Rust std: this is the
    // async-signal-safe path `DESIGN.md` §9.1 requires.
    unsafe {
        libc::close(r);
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if null >= 0 {
            libc::dup2(null, 0);
            libc::dup2(null, 1);
            libc::dup2(null, 2);
            if null > 2 {
                libc::close(null);
            }
        }
        libc::close(w);
        libc::execve(
            sh.as_ptr(),
            argv.as_ptr().cast_mut(),
            envp.as_ptr().cast_mut(),
        );
        libc::_exit(127);
    }
}

/// The supervisor's own environment as a NUL-terminated `envp` array.
///
/// Owner and pointers travel in one struct because the pointer array borrows
/// from the strings: returning them separately would let the borrow outlive
/// the owner, which past the fork means pointing at freed memory in the
/// child. Prepared before any fork that needs it.
///
/// Entries that cannot be represented (a NUL byte inside a key or value)
/// abort the whole block with `InvalidInput`: an environment that cannot be
/// passed on exactly is not passed on approximately.
#[derive(Debug)]
pub(crate) struct EnvBlock {
    /// Owned strings. Never read directly; they exist to own the bytes.
    #[allow(dead_code)]
    owned: Vec<CString>,
    /// NUL-terminated pointer array into `owned`, for `execve`.
    ptrs: Vec<*const libc::c_char>,
}

impl EnvBlock {
    /// Capture the current process environment.
    pub(crate) fn capture() -> io::Result<EnvBlock> {
        let mut owned: Vec<CString> = Vec::new();
        for (k, v) in std::env::vars_os() {
            let mut bytes = k.as_encoded_bytes().to_vec();
            bytes.push(b'=');
            bytes.extend_from_slice(v.as_encoded_bytes());
            owned.push(CString::new(bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "environment entry contains a NUL byte",
                )
            })?);
        }
        let mut ptrs: Vec<*const libc::c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(core::ptr::null());
        Ok(EnvBlock { owned, ptrs })
    }

    /// The raw `envp` array. Valid as long as `self` is alive — which, past
    /// a fork, means "until the `execve` or `_exit` a few lines below".
    pub(crate) fn as_ptrs(&self) -> &[*const libc::c_char] {
        &self.ptrs
    }
}

/// Capture the current environment as raw parts for a pre-fork `execve`.
fn current_envp() -> io::Result<EnvBlock> {
    EnvBlock::capture()
}

/// Wait up to `timeout_ms` for `fd` to become readable.
///
/// Returns `true` on readability (or hangup — for the wakeup-pipe use, a dead
/// child is also news), `false` on timeout. `EINTR` retries internally: a
/// supervisor is woken by signals constantly, and surfacing that as an error
/// would turn every probe into a lie under load.
fn poll_readable(fd: RawFd, timeout_ms: u64) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    let timeout = timeout_ms.min(i32::MAX as u64) as libc::c_int;
    loop {
        // SAFETY: `pfd` is a live, aligned `pollfd`; `nfds` is 1; the kernel
        // writes only `revents` and retains nothing.
        let n = unsafe { libc::poll(&raw mut pfd, 1, timeout) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        return Ok(n > 0);
    }
}

/// Try one `connect()` to `127.0.0.1:port`, with a bounded wait.
///
/// A refused connection is `Ok(false)` — the normal "not yet" — as is a
/// timeout. Only a socket that cannot even be created is an `Err`: that is a
/// broken supervisor, not an unready service. The socket is nonblocking and
/// driven by `poll(POLLOUT)`, so no thread ever blocks in the kernel's
/// connect path; `SO_ERROR` afterwards is the real verdict, because a
/// writable socket after a nonblocking connect means "finished", not
/// "succeeded".
pub fn probe_tcp(port: u16) -> io::Result<bool> {
    let fd = new_probe_socket()?;
    let ok = probe_tcp_connect(fd, port);
    // SAFETY: `fd` is ours, opened two lines up.
    unsafe {
        libc::close(fd);
    }
    ok
}

/// A nonblocking, close-on-exec loopback probe socket.
///
/// Linux takes `SOCK_CLOEXEC | SOCK_NONBLOCK` atomically; Apple/BSD targets
/// have neither flag, so the plain socket is flagged afterwards with the same
/// `zrt::sys` setters the spawn path uses — same guarantees, two syscalls.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn new_probe_socket() -> io::Result<RawFd> {
    // SAFETY: `socket` takes three ints; `SOCK_STREAM | SOCK_CLOEXEC |
    // SOCK_NONBLOCK` on `AF_INET` is the canonical loopback-probe form.
    let fd = unsafe {
        libc::socket(
            libc::AF_INET,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn new_probe_socket() -> io::Result<RawFd> {
    // SAFETY: `socket` takes three ints; no flags exist here, so the fd is
    // flagged below before it can leak anywhere.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    if let Err(e) = zrt::sys::set_cloexec(fd, true).and(zrt::sys::set_nonblocking(fd, true)) {
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok(fd)
}

fn probe_tcp_connect(fd: RawFd, port: u16) -> io::Result<bool> {
    let ip: u32 = u32::from_be_bytes([127, 0, 0, 1]);
    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr { s_addr: ip.to_be() },
        sin_zero: [0; 8],
    };
    // BSD-derived kernels carry the struct length in its first byte; Linux
    // has no such field. The length is the size of the whole struct, which
    // is also what `connect` is handed below.
    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    let addr = libc::sockaddr_in {
        sin_len: core::mem::size_of::<libc::sockaddr_in>() as u8,
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr { s_addr: ip.to_be() },
        sin_zero: [0; 8],
    };
    // SAFETY: `addr` is a live, fully initialised `sockaddr_in` of exactly
    // the length passed; `connect` copies it and retains nothing.
    let rc = unsafe {
        libc::connect(
            fd,
            (&raw const addr).cast::<libc::sockaddr>(),
            core::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() != Some(libc::EINPROGRESS) {
        // Refused, unreachable, no route — every immediate failure is "not
        // yet". Only EINPROGRESS means "ask again after the wait".
        return Ok(false);
    }
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    let timeout = TCP_CONNECT_TIMEOUT_MS.min(i32::MAX as u64) as libc::c_int;
    // SAFETY: as in `poll_readable`: one live `pollfd`, nothing retained.
    let n = loop {
        let n = unsafe { libc::poll(&raw mut pfd, 1, timeout) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        break n;
    };
    if n == 0 {
        return Ok(false);
    }
    let mut err: libc::c_int = 0;
    let mut len = core::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `err` is a live `c_int` of exactly `len`; `getsockopt`
    // writes at most that and retains nothing.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&raw mut err).cast::<libc::c_void>(),
            &raw mut len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(err == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp_closed_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        drop(l);
        port
    }

    #[test]
    fn none_is_ready_at_once_and_needs_nothing() {
        let mut w = ReadyWait::None;
        assert!(w.check(0).expect("none is ready"));
        assert!(w.poll_need(0).is_idle());
        assert_eq!(w.name(), "none");
    }

    #[test]
    fn from_ready_maps_every_variant() {
        assert!(matches!(
            ReadyWait::from_ready(&Ready::None, None, 0),
            ReadyWait::None
        ));
        let (r, _w) = zrt::sys::pipe2(true).expect("pipe");
        let w = ReadyWait::from_ready(&Ready::Notify, Some(r), 0);
        assert!(matches!(w, ReadyWait::Notify { .. }));
        let _ = zrt::sys::close(r);
        // Notify without a pipe degrades to immediate rather than to a wait
        // that can never complete.
        assert!(matches!(
            ReadyWait::from_ready(&Ready::Notify, None, 0),
            ReadyWait::None
        ));
        assert!(matches!(
            ReadyWait::from_ready(&Ready::Strict(StrictReady::Ping("c".into())), None, 0),
            ReadyWait::Ping { .. }
        ));
        assert!(matches!(
            ReadyWait::from_ready(&Ready::Strict(StrictReady::Tcp(80)), None, 0),
            ReadyWait::Tcp { .. }
        ));
    }

    #[test]
    fn notify_poll_need_carries_the_fd() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        let waiter = ReadyWait::Notify { fd: r };
        let need = waiter.poll_need(0);
        assert_eq!(need.fd, Some(r));
        assert_eq!(need.timeout_ms, None);
        let _ = zrt::sys::close(r);
        let _ = zrt::sys::close(w);
    }

    #[test]
    fn notify_line_means_ready() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        zrt::sys::set_nonblocking(r, true).expect("nonblock");
        let mut waiter = ReadyWait::Notify { fd: r };
        assert!(!waiter.check(0).expect("nothing written yet"));
        zrt::sys::write(w, b"ready\n").expect("notify");
        assert!(waiter.check(0).expect("a line is readiness"));
        let _ = zrt::sys::close(r);
        let _ = zrt::sys::close(w);
    }

    #[test]
    fn notify_eof_without_a_line_is_not_ready() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        zrt::sys::set_nonblocking(r, true).expect("nonblock");
        let _ = zrt::sys::close(w);
        let mut waiter = ReadyWait::Notify { fd: r };
        assert!(!waiter.check(0).expect("eof is not readiness"));
        let _ = zrt::sys::close(r);
    }

    #[test]
    fn tcp_probe_sees_a_real_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        assert!(probe_tcp(port).expect("probe runs"), "listener is there");
        let mut w = ReadyWait::Tcp {
            port,
            next_due_ms: 0,
        };
        assert!(w.check(0).expect("ready via listener"));
        let need = w.poll_need(0);
        assert!(need.fd.is_none() && need.timeout_ms.is_some());
        assert_eq!(w.name(), "tcp");
    }

    #[test]
    fn tcp_probe_reports_a_closed_port_as_not_ready() {
        let port = tcp_closed_port();
        assert!(!probe_tcp(port).expect("probe runs"));
        let mut w = ReadyWait::Tcp {
            port,
            next_due_ms: 0,
        };
        assert!(!w.check(0).expect("no listener"));
    }

    #[test]
    fn ping_true_is_ready_and_false_is_not() {
        assert!(run_check("/bin/true", 5_000).expect("probe runs"));
        assert!(!run_check("/bin/false", 5_000).expect("probe runs"));
        assert!(!run_check("exit 3", 5_000).expect("nonzero is not ready"));
    }

    #[test]
    fn ping_fails_until_the_service_is_really_up() {
        let dir = crate::testutil::scratch("ping");
        let counter = dir.join("n");
        std::fs::write(&counter, "0").expect("seed");
        // Succeeds only from the third run on: the first two are "starting".
        let script = format!(
            "n=$(cat {}); n=$((n+1)); echo $n > {}; test $n -ge 3",
            counter.display(),
            counter.display()
        );
        let mut w = ReadyWait::Ping {
            command: script,
            next_due_ms: 0,
        };
        let mut now = 0u64;
        assert!(!w.check(now).expect("run 1 fails"));
        now += POLL_PING_INTERVAL_MS;
        assert!(!w.check(now).expect("run 2 fails"));
        now += POLL_PING_INTERVAL_MS;
        assert!(w.check(now).expect("run 3 succeeds"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ping_before_it_is_due_does_not_execute() {
        let dir = crate::testutil::scratch("norun");
        let flag = dir.join("ran");
        let mut w = ReadyWait::Ping {
            command: format!("touch {}", flag.display()),
            next_due_ms: 10_000,
        };
        assert!(!w.check(0).expect("not due"));
        assert!(!flag.exists(), "a probe that is not due must not fork");
        assert!(w.check(10_000).expect("due now, touch exits 0"));
        assert!(flag.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ping_with_nul_is_refused() {
        assert_eq!(
            run_check("a\0b", 100).expect_err("NUL").kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn tcp_before_it_is_due_does_not_connect() {
        let mut w = ReadyWait::Tcp {
            port: 1,
            next_due_ms: 5_000,
        };
        assert!(!w.check(0).expect("not due"));
    }
}
