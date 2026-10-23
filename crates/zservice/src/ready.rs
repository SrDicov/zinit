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
            // One-shot classification without cross-call memory: a throwaway
            // buffer reads what is readable right now (see `drain_notify` for
            // the rules). The supervisor's own path keeps the buffer across
            // polls so a line split in two still parses as one line; this
            // shape exists for probes and tests, where only readiness —
            // never the watchdog bit — is observable.
            ReadyWait::Notify { fd } => Ok(drain_notify(*fd, &mut Vec::new())?.0),
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

/// Past this much buffered-but-unparsed notify input, stop parsing and report
/// readiness. A service spewing megabytes at fd 3 is alive by any definition;
/// hanging the boot (or the heap) on its output would be worse than believing
/// it. The buffer is cleared when this fires, so the next round starts clean.
const NOTIFY_BUFFER_MAX: usize = 4096;

/// Read one round from a notify pipe into `pending`, classifying complete
/// lines and returning `(ready, watchdog)`.
///
/// `pending` keeps the trailing partial line across calls, so a `WATCHDOG=1`
/// split across two polls is still recognised as one line. Reads until
/// `EAGAIN`; `EOF` with no content is `(false, false)` — a closed pipe
/// without a word is the reaper's business, not the probe's.
pub(crate) fn drain_notify(fd: RawFd, pending: &mut Vec<u8>) -> io::Result<(bool, bool)> {
    loop {
        let mut chunk = [0u8; 512];
        match zrt::sys::read(fd, &mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                pending.extend_from_slice(&chunk[..n]);
                if pending.len() > NOTIFY_BUFFER_MAX {
                    // Fail open (see above), and start clean.
                    pending.clear();
                    return Ok((true, false));
                }
                if n < chunk.len() {
                    break;
                }
            }
            // equivalent (cargo-mutants): this Interrupted guard can never
            // fire — `zrt::sys::read` loops on `Interrupted` inside its own
            // retry loop (crates/zrt/src/sys.rs, `pub fn read`), so `Err` can
            // never carry that kind here: no test can observe this arm taken
            // and its mutants have no observable effect.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    let mut ready = false;
    let mut watchdog = false;
    // Complete lines only; the trailing partial stays buffered — unless it
    // is the *only* thing there, in which case the legacy rule applies.
    let mut start = 0;
    let mut saw_newline = false;
    while let Some(rel) = pending[start..].iter().position(|&b| b == b'\n') {
        saw_newline = true;
        let mut line = &pending[start..start + rel];
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if line == b"READY=1" {
            ready = true;
        } else if line == b"WATCHDOG=1" {
            watchdog = true;
        } else if !line.iter().all(|b| b.is_ascii_whitespace()) {
            ready = true;
        }
        start += rel + 1;
    }
    pending.drain(..start);
    if !saw_newline && !pending.is_empty() && pending.iter().any(|b| !b.is_ascii_whitespace()) {
        // No complete line yet, but bytes: the legacy any-byte rule. The
        // bytes stay buffered — if they grow into a framed line, the line
        // rule above will classify it then.
        ready = true;
    }
    Ok((ready, watchdog))
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
    let envp = EnvBlock::capture()?;
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
        // equivalent (cargo-mutants): ^ would be the same value as | here;
        // O_RDWR and O_CLOEXEC are disjoint bits.
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if null >= 0 {
            libc::dup2(null, 0);
            libc::dup2(null, 1);
            libc::dup2(null, 2);
            // equivalent (cargo-mutants): this close conveys nothing beyond
            // `O_CLOEXEC`: fd `null` always carries the close-at-exec bit,
            // and no replacement of `>` moves the close to anywhere earlier
            // than the `execve` — which performs the same close on every
            // such mutant too. The `dup2`s already copied fd 2 before the
            // guard runs, and copies never carry CLOEXEC, so the executed
            // child's fd table is identical in all branches; the sub-
            // microsecond close-to-exec gap is not observable to any test.
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

/// Wait up to `timeout_ms` for `fd` to become readable.
///
/// Returns `true` on readability (or hangup — for the wakeup-pipe use, a dead
/// child is also news), `false` on timeout. `EINTR` retries internally: a
/// supervisor is woken by signals constantly, and surfacing that as an error
/// would turn every probe into a lie under load.
fn poll_readable(fd: RawFd, timeout_ms: u64) -> io::Result<bool> {
    // equivalent (cargo-mutants): ^ would be the same value as | here;
    // POLLIN and POLLHUP are disjoint bits.
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
    // equivalent (cargo-mutants): ^ is the same value as | on every union
    // below; the SOCK_* flags are disjoint bits.
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

// equivalent (cargo-mutants): every mutant inside this target-out branch
// compiles away on Linux and can never fail the suite here — the `cfg`
// gate below is what proves it; BSD/Apple builds exercise this version.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn new_probe_socket() -> io::Result<RawFd> {
    // SAFETY: `socket` takes three ints; no flags exist here, so the fd is
    // flagged below before it can leak anywhere.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    if let Err(e) = zrt::sys::set_cloexec(fd, true).and(zrt::sys::set_nonblocking(fd, true)) {
        // SAFETY: `fd` came from the `socket` above and is still owned here.
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    fn tcp_closed_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        drop(l);
        port
    }

    /// A port that is genuinely closed *right now*: bind-and-drop hands back a
    /// port the OS may immediately reassign (ephemeral collision on a busy
    /// shared runner), so keep the ones the probe itself confirms dark.
    fn confirmed_closed_port() -> u16 {
        for _ in 0..20 {
            let port = tcp_closed_port();
            if !probe_tcp(port).expect("probe runs") {
                return port;
            }
        }
        panic!("no genuinely closed port in 20 tries; the runner is out of ports");
    }

    #[test]
    fn none_is_ready_at_once_and_needs_nothing() {
        let mut w = ReadyWait::None;
        assert!(w.check(0).expect("none is ready"));
        assert!(w.poll_need(0).is_idle());
        assert_eq!(w.name(), "none");
    }

    #[test]
    fn is_idle_is_true_only_when_there_is_nothing_to_wait_for() {
        assert!(PollNeed::idle().is_idle());
        // One reason to wait is enough to stop being idle. With `||` instead of
        // `&&` the middle two would claim there is nothing to do and the
        // reactor would never poll the fd or the timeout.
        assert!(
            !PollNeed {
                fd: Some(3),
                timeout_ms: None
            }
            .is_idle()
        );
        assert!(
            !PollNeed {
                fd: None,
                timeout_ms: Some(10)
            }
            .is_idle()
        );
        assert!(
            !PollNeed {
                fd: Some(3),
                timeout_ms: Some(10)
            }
            .is_idle()
        );
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

    /// A lone `WATCHDOG=1` line feeds the watchdog and readies nothing; a
    /// `READY=1` line readies; unknown lines keep the legacy any-byte rule.
    #[test]
    fn notify_lines_multiplex_ready_and_watchdog() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        zrt::sys::set_nonblocking(r, true).expect("nonblock");
        let mut pending = Vec::new();
        zrt::sys::write(w, b"WATCHDOG=1\n").expect("ping");
        assert_eq!(
            drain_notify(r, &mut pending).expect("drain"),
            (false, true),
            "a check-in is not readiness"
        );
        zrt::sys::write(w, b"READY=1\n").expect("ready");
        assert_eq!(drain_notify(r, &mut pending).expect("drain"), (true, false));
        zrt::sys::write(w, b"anything-at-all\n").expect("legacy");
        assert!(
            drain_notify(r, &mut pending).expect("drain").0,
            "unframed output keeps the old any-byte rule"
        );
        let _ = zrt::sys::close(r);
        let _ = zrt::sys::close(w);
    }

    /// A CRLF line is a framed line, not an unknown one: stripping the `\r` is
    /// what keeps `WATCHDOG=1\r\n` a check-in instead of a readiness byte.
    #[test]
    fn notify_crlf_watchdog_is_a_check_in_not_readiness() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        zrt::sys::set_nonblocking(r, true).expect("nonblock");
        let mut pending = Vec::new();
        zrt::sys::write(w, b"WATCHDOG=1\r\n").expect("ping");
        assert_eq!(
            drain_notify(r, &mut pending).expect("drain"),
            (false, true),
            "CRLF must be stripped before the line is classified"
        );
        let _ = zrt::sys::close(r);
        let _ = zrt::sys::close(w);
    }

    /// A read that fills the chunk must not be taken for the end of the data:
    /// the ready line sits behind a full buffer of spaces.
    #[test]
    fn notify_full_chunk_keeps_reading_for_the_line_behind_it() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        zrt::sys::set_nonblocking(r, true).expect("nonblock");
        let mut pending = Vec::new();
        let mut blob = vec![b' '; 512];
        blob.extend_from_slice(b"READY=1\n");
        zrt::sys::write(w, &blob).expect("write");
        assert_eq!(drain_notify(r, &mut pending).expect("drain"), (true, false));
        let _ = zrt::sys::close(r);
        let _ = zrt::sys::close(w);
    }

    /// Over the cap the buffer fails open *and* starts clean: a chatty service
    /// must not pin memory, and half a line must not be replayed forever.
    #[test]
    fn notify_over_the_cap_fails_open_and_clears_the_buffer() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        zrt::sys::set_nonblocking(r, true).expect("nonblock");
        let mut pending = Vec::new();
        zrt::sys::write(w, &vec![b'a'; NOTIFY_BUFFER_MAX + 1]).expect("write");
        assert_eq!(drain_notify(r, &mut pending).expect("drain"), (true, false));
        assert!(pending.is_empty(), "the over-cap path starts clean");
        let _ = zrt::sys::close(r);
        let _ = zrt::sys::close(w);
    }

    /// Two framed lines in one read: both are consumed and the buffer is left
    /// empty, so the next call does not replay them.
    #[test]
    fn notify_two_lines_in_one_read_are_both_consumed() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        zrt::sys::set_nonblocking(r, true).expect("nonblock");
        let mut pending = Vec::new();
        zrt::sys::write(w, b"READY=1\nWATCHDOG=1\n").expect("write");
        assert_eq!(drain_notify(r, &mut pending).expect("drain"), (true, true));
        assert!(
            pending.is_empty(),
            "consumed lines are drained, not replayed"
        );
        let _ = zrt::sys::close(r);
        let _ = zrt::sys::close(w);
    }

    /// Exactly at the cap is still buffered: the limit is a ceiling, not a
    /// threshold, and `>=` would drop a full buffer that fits.
    #[test]
    fn notify_exactly_at_the_cap_is_kept() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        zrt::sys::set_nonblocking(r, true).expect("nonblock");
        let mut pending = Vec::new();
        zrt::sys::write(w, &vec![b'a'; NOTIFY_BUFFER_MAX]).expect("write");
        assert_eq!(drain_notify(r, &mut pending).expect("drain"), (true, false));
        assert_eq!(
            pending.len(),
            NOTIFY_BUFFER_MAX,
            "a buffer at the cap is kept, not cleared"
        );
        let _ = zrt::sys::close(r);
        let _ = zrt::sys::close(w);
    }

    /// A whitespace-only line is not a readiness byte.
    #[test]
    fn notify_whitespace_only_line_is_not_readiness() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        zrt::sys::set_nonblocking(r, true).expect("nonblock");
        let mut pending = Vec::new();
        zrt::sys::write(w, b"   \n").expect("write");
        assert_eq!(
            drain_notify(r, &mut pending).expect("drain"),
            (false, false)
        );
        let _ = zrt::sys::close(r);
        let _ = zrt::sys::close(w);
    }

    #[test]
    fn poll_readable_sees_data_and_hangup() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        // Nothing yet: the timeout is the answer, not an error.
        assert!(!poll_readable(r, 5).expect("poll runs"));
        // Data on the read end is readable. With `&` instead of `|` the event
        // mask is empty and a pipe with data looks idle.
        zrt::sys::write(w, b"x").expect("write");
        assert!(poll_readable(r, 5).expect("poll runs"));
        // A closed writer must wake the reader so it can see EOF.
        let _ = zrt::sys::close(w);
        assert!(poll_readable(r, 5).expect("poll runs"));
        let _ = zrt::sys::close(r);
    }

    #[test]
    fn probe_socket_is_nonblocking_and_close_on_exec() {
        let fd = new_probe_socket().expect("socket");
        // SAFETY: `fd` is ours and `fcntl` only reads flags.
        unsafe {
            let fl = libc::fcntl(fd, libc::F_GETFL);
            let fdfl = libc::fcntl(fd, libc::F_GETFD);
            libc::close(fd);
            assert!(
                fl >= 0 && fl & libc::O_NONBLOCK != 0,
                "the probe socket must be nonblocking: a blocking connect would hang the supervisor"
            );
            assert!(
                fdfl >= 0 && fdfl & libc::FD_CLOEXEC != 0,
                "the probe socket must be close-on-exec, or a child inherits it"
            );
        }
    }

    /// The `envp` array must end in a null pointer and carry real entries: a
    /// leaked dummy slice would hand `execve` a pointer to nothing.
    #[test]
    fn env_block_ends_with_a_null_pointer() {
        let block = EnvBlock::capture().expect("environment is representable");
        let ptrs = block.as_ptrs();
        assert!(!ptrs.is_empty(), "an empty envp has no terminator");
        assert_eq!(
            *ptrs.last().expect("non-empty"),
            core::ptr::null(),
            "envp must be null-terminated"
        );
        assert!(
            ptrs.len() > 1,
            "PATH and friends are inherited: only the terminator means capture dropped everything"
        );
        for p in &ptrs[..ptrs.len() - 1] {
            assert!(
                !p.is_null(),
                "every entry before the terminator is a real string"
            );
        }
    }

    /// The deadline is inclusive and the check must run once `now` is past it:
    /// with `now_ms < next_due` inverted, a probe that is already due is
    /// skipped and the service never becomes ready.
    /// The ping half forks, so it runs in a probe child (see
    /// `run_check_child_execs_...` for why no fork stays in this binary).
    #[test]
    fn ping_past_the_deadline_still_runs() {
        assert!(probe_child("dueping").success(), "past the deadline is due");
        let mut t = ReadyWait::Tcp {
            port: 1,
            next_due_ms: 1000,
        };
        // Port 1 on loopback is refused: the point is that the probe runs at
        // all, which the inverted comparison would skip.
        assert!(!t.check(1001).expect("probe runs"));
    }

    /// The child half really reaches `execve` and its exit status comes back.
    /// Both directions run in probe children: under a flipped fork branch the
    /// `run_check` *caller* itself execs away (exit 7), and an in-binary call
    /// would make that replacement the whole suite's exit status.
    #[test]
    fn run_check_child_execs_and_the_status_comes_back() {
        assert!(
            probe_child("exit7").success(),
            "the child's nonzero status must come back as not-ready"
        );
        assert!(
            probe_child("exit0").success(),
            "the child's zero status must come back as ready"
        );
    }

    #[test]
    fn notify_partial_line_survives_across_polls() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        zrt::sys::set_nonblocking(r, true).expect("nonblock");
        let mut pending = Vec::new();
        zrt::sys::write(w, b"WATCHDOG=").expect("first half");
        // A bare partial with no newline yet reads as legacy-ready (documented
        // limitation: only newline-terminated lines are framed), but the bytes
        // stay buffered for the second half...
        let _ = drain_notify(r, &mut pending).expect("drain");
        zrt::sys::write(w, b"1\n").expect("second half");
        // ...and now the framed line classifies, with nothing left over.
        assert_eq!(drain_notify(r, &mut pending).expect("drain"), (false, true));
        assert!(pending.is_empty());
        let _ = zrt::sys::close(r);
        let _ = zrt::sys::close(w);
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
        let port = confirmed_closed_port();
        assert!(!probe_tcp(port).expect("probe runs"));
        let mut w = ReadyWait::Tcp {
            port,
            next_due_ms: 0,
        };
        assert!(!w.check(0).expect("no listener"));
    }

    #[test]
    fn ping_true_is_ready_and_false_is_not() {
        assert!(
            probe_child("truefalse").success(),
            "exit 0 is ready, any nonzero exit is not"
        );
    }

    #[test]
    fn ping_fails_until_the_service_is_really_up() {
        assert!(
            probe_child("growsup").success(),
            "two failing runs, then the third succeeds"
        );
    }

    #[test]
    fn ping_before_it_is_due_does_not_execute() {
        assert!(probe_child("nudge").success(), "not due: no fork, no probe");
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

    /// At the 5s deadline, crash instead of hanging: the mutants that turn an
    /// error branch back into a retry loop (`Interrupted`/`WouldBlock` asked,
    /// any error given) never return, so only an explicit crash fails their
    /// run inside the mutator's budget. A correct drain returns in
    /// microseconds; these tests wait at most ~1.5s on their own timers.
    fn hang_sentinel() -> (&'static AtomicBool, std::thread::JoinHandle<()>) {
        let done: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let guard = std::thread::spawn(move || {
            for _ in 0..50 {
                if done.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // crashes the whole test binary: that is the point — a hung mutant
            // must fail fast, and the sentinel only fires on a hang.
            std::process::abort()
        });
        (done, guard)
    }

    /// An empty-but-open nonblocking pipe answers `WouldBlock`, which is
    /// "nothing yet", not an error: `(false, false)` and the caller keeps its
    /// state. Without the sentinel's crash a mutated error branch would spin
    /// on `EAGAIN` forever.
    #[test]
    fn drain_notify_returns_wouldblock() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        zrt::sys::set_nonblocking(r, true).expect("nonblock");
        let mut pending = Vec::new();
        let (done, guard) = hang_sentinel();
        let drained = drain_notify(r, &mut pending);
        done.store(true, Ordering::Relaxed);
        guard.join().expect("sentinel finishes");
        assert!(
            matches!(drained, Ok((false, false))),
            "WouldBlock is 'nothing yet'"
        );
        let _ = zrt::sys::close(r);
        let _ = zrt::sys::close(w);
    }

    /// `EINTR` mid-read must retry, not surface. The signal is delivered to
    /// this thread while the read is parked on a *blocking* pipe, and the
    /// payload `"\nREADY=1\n"` (leading empty line: `rel = 0` in the parse
    /// loop) is written only afterwards, so both the `Interrupted =>
    /// continue` retry and the next read are exercised deterministically —
    /// a fixed 100 ms handoff instead of a racy one-second alarm.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn drain_notify_retries_on_eintr() {
        arm_sigalrm_waker();
        let (r, w) = zrt::sys::pipe2(true).expect("pipe"); // blocking
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            zrt::sys::write(w, b"\nREADY=1\n").expect("notify");
        });
        let mut pending = Vec::new();
        let (done, guard) = hang_sentinel();
        let alarm = sigalrm_here_after(100);
        let drained = drain_notify(r, &mut pending);
        done.store(true, Ordering::Relaxed);
        guard.join().expect("sentinel finishes");
        alarm.join().expect("alarm thread finishes");
        writer.join().expect("writer finishes");
        assert!(
            matches!(drained, Ok((true, false))),
            "EINTR must retry the read, not surface"
        );
        let _ = zrt::sys::close(r);
    }

    /// A real error is an `Err`, never "nothing yet": a `WouldBlock` guard
    /// turned unconditional (true) would swallow `EBADF` as `(false, false)`
    /// and the caller would keep polling a dead descriptor forever.
    #[test]
    fn drain_notify_propagates_a_dead_descriptor() {
        let mut pending = Vec::new();
        assert!(
            drain_notify(-1, &mut pending).is_err(),
            "a closed descriptor is an error, not silence"
        );
    }

    /// Bytes without a newline obey the any-byte rule only when they are not
    /// whitespace: `&&`→`||` in the lone-bytes branch would call spaces a
    /// handshake.
    #[test]
    fn notify_whitespace_without_a_newline_is_not_readiness() {
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        zrt::sys::set_nonblocking(r, true).expect("nonblock");
        let mut pending = Vec::new();
        zrt::sys::write(w, b"   ").expect("write");
        assert_eq!(
            drain_notify(r, &mut pending).expect("drain"),
            (false, false),
            "spaces with no newline are not a handshake"
        );
        let _ = zrt::sys::close(r);
        let _ = zrt::sys::close(w);
    }

    /// A `Tcp` wait is due strictly past its instant: `<`→`>` would answer
    /// "wrong time" exactly when the service is up.
    #[test]
    fn tcp_check_strictly_past_its_due_instant_probes() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        let mut w = ReadyWait::Tcp {
            port,
            next_due_ms: 0,
        };
        assert!(
            w.check(1).expect("probe runs"),
            "past due with a live listener is readiness now"
        );
    }

    /// Probe children that exercise the fork paths run isolated in their own
    /// process group: a mutated child half may signal or kill its whole
    /// group, and that must never reach the test binary or the driver behind
    /// it. The modes do the actual work inside `run_check_probe_entry`. The
    /// cap keeps a mutated in-child path (a spin or a blocked connect) from
    /// hanging the *suite*: the watchdog reaps the child as a failure and the
    /// parent test reports it long before the 60s cargo-mutants budget.
    fn probe_child(mode: &str) -> std::process::ExitStatus {
        use std::os::unix::process::CommandExt;
        let mut watching =
            std::process::Command::new(std::env::current_exe().expect("self executable"))
                .arg("run_check_probe_entry")
                .arg("--nocapture")
                .env("ZREADY_PROBE_MODE", mode)
                .process_group(0)
                .spawn()
                .expect("spawn probe child");
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            match watching.try_wait().expect("poll probe child") {
                Some(status) => return status,
                None => {
                    if std::time::Instant::now() >= deadline {
                        let _ = watching.kill();
                        return watching.wait().expect("reap the timed-out probe child");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    /// One process-wide SIGALRM no-op handler, installed exactly once: the
    /// signal action table is process-wide, so concurrent arm/restore pairs
    /// would race; every test here only needs "SIGALRM wakes a parked
    /// syscall with EINTR", which one common no-op handler provides. Senders
    /// are always targeted `tgkill`s at a single requesting thread.
    fn arm_sigalrm_waker() {
        static ARM: std::sync::Once = std::sync::Once::new();
        ARM.call_once(|| {
            unsafe extern "C" fn on_sigalrm(_: libc::c_int) {}
            let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
            action.sa_sigaction = on_sigalrm as *const () as usize;
            // No SA_RESTART: parked read/poll must wake with EINTR.
            action.sa_flags = 0;
            let mut old: libc::sigaction = unsafe { core::mem::zeroed() };
            // SAFETY: registering the suite-wide no-op alarm handler once;
            // the previous action is captured and left unmourned — nothing
            // else in this binary puts SIGALRM to term.
            let rc = unsafe { libc::sigaction(libc::SIGALRM, &raw const action, &raw mut old) };
            assert_eq!(rc, 0, "sigaction(SIGALRM) runs");
        });
    }

    /// Deliver SIGALRM to the *calling* thread `delay_ms` from now; the
    /// caller must be (about to be) parked inside the syscall whose EINTR
    /// behaviour is under test.
    fn sigalrm_here_after(delay_ms: u64) -> std::thread::JoinHandle<()> {
        // The thread-targeted delivery is Linux's: `gettid` and
        // `SYS_tgkill` do not exist on the BSDs, where the signal tests
        // below are not compiled either. The fallback keeps the helper (and
        // the `"alarm"` probe arm) compiling there; it is never reached.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let (pid, tid) = unsafe { (libc::getpid(), libc::gettid()) };
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay_ms));
            // SAFETY: tgkill is the documented thread-targeted delivery. The
            // musl bindings of the `libc` crate do not declare `tgkill` (the
            // symbol exists in musl; the declaration is missing), so the raw
            // syscall is used instead of a cfg fork: same call, one code path.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            unsafe {
                libc::syscall(libc::SYS_tgkill, pid, tid, libc::SIGALRM)
            };
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            unsafe {
                libc::kill(libc::getpid(), libc::SIGALRM)
            };
        })
    }

    /// The sacrifice fork branch is the one with caller privileges: flipping
    /// it makes the run_check *caller* itself exec away and drop the
    /// process's exit status on the floor (here 7). The probe child, a
    /// process the suite can afford to lose, catches that.
    #[test]
    fn run_check_reports_the_child_status_not_the_callers() {
        assert!(
            probe_child("exit7").success(),
            "run_check must return false, not exec the caller away"
        );
    }

    /// The child half seals stdio onto /dev/null. Layout A frees 0, 1 and 2,
    /// so the seal `open` lands at 0 and the check must see fds 0 and 2
    /// readable *and* writable; layout B leaves only 2 free, so the seal
    /// open lands exactly at 2 and that descriptor must survive the whole
    /// dup/close dance (it does in mutants that close fd 2 "early"). fd 1 is
    /// untested in A: the probe pipe legitimately re-uses it and the real
    /// `close(w)` closes it again.
    #[test]
    fn run_check_child_seals_stdio() {
        assert!(
            probe_child("writeprobe").success(),
            "a plain check can still write through the sealed stdout"
        );
        assert!(
            probe_child("sealA").success(),
            "with fd 0/1 freed, the seal must rewire fd 1"
        );
    }

    /// The five `|`s annotated `// equivalent (cargo-mutants)` pair flags
    /// that are disjoint bitsets, and only disjoint bitsets make `a | b`
    /// identical to `a ^ b` for every input. These asserts are the machine
    /// check behind those annotations: shrink one pair and this test fails
    /// exactly when a claimed equivalence stops holding.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn annotated_or_flags_are_disjoint_bitsets() {
        assert_eq!(libc::O_RDWR & libc::O_CLOEXEC, 0, "open(/dev/null) flags");
        assert_eq!(libc::POLLIN & libc::POLLHUP, 0, "poll events");
        assert_eq!(
            libc::SOCK_STREAM & libc::SOCK_CLOEXEC,
            0,
            "socket type vs run-time flag"
        );
        assert_eq!(
            libc::SOCK_STREAM & libc::SOCK_NONBLOCK,
            0,
            "socket type vs io flag"
        );
        assert_eq!(
            libc::SOCK_CLOEXEC & libc::SOCK_NONBLOCK,
            0,
            "socket run-time flags"
        );
    }

    /// With slot 0 freed before the probe, the probe socket must still win a
    /// usable low descriptor; `fd < 0` mutants that call fd 0 a failure make
    /// this probe error out.
    #[test]
    fn probe_socket_wins_the_lowest_free_descriptor() {
        assert!(probe_child("kprobe").success());
    }

    #[test]
    fn run_check_probe_entry() {
        let Ok(mode) = std::env::var("ZREADY_PROBE_MODE") else {
            return; // ordinary suite run: nothing to do
        };
        let ok = match mode.as_str() {
            "exit7" => !run_check("exit 7", 2_000).unwrap_or(true),
            "exit0" => run_check("exit 0", 2_000).unwrap_or(false),
            "writeprobe" => run_check("echo hi", 2_000).unwrap_or(false),
            "truefalse" => {
                run_check(crate::testutil::true_bin(), 5_000).unwrap_or(false)
                    && { !run_check(crate::testutil::false_bin(), 5_000).unwrap_or(true) }
                    && { !run_check("exit 3", 5_000).unwrap_or(true) }
            }
            "dueping" => {
                let mut w = ReadyWait::Ping {
                    command: crate::testutil::true_bin().to_string(),
                    next_due_ms: 1000,
                };
                w.check(1001).unwrap_or(false)
            }
            "growsup" => {
                let dir = crate::testutil::scratch("ping");
                let counter = dir.join("n");
                std::fs::write(&counter, "0").expect("seed");
                // Succeeds only from the third run on: the first two are
                // "starting".
                let script = format!(
                    "n=$(cat {0}); n=$((n+1)); echo $n > {0}; test $n -ge 3",
                    counter.display()
                );
                let mut w = ReadyWait::Ping {
                    command: script,
                    next_due_ms: 0,
                };
                let mut now = 0u64;
                let ok = !w.check(now).unwrap_or(true);
                now += POLL_PING_INTERVAL_MS;
                let ok = ok && !w.check(now).unwrap_or(true);
                now += POLL_PING_INTERVAL_MS;
                let ok = ok && w.check(now).unwrap_or(false);
                let _ = std::fs::remove_dir_all(&dir);
                ok
            }
            "nudge" => {
                let dir = crate::testutil::scratch("norun");
                let flag = dir.join("ran");
                let mut w = ReadyWait::Ping {
                    command: format!("touch {}", flag.display()),
                    next_due_ms: 10_000,
                };
                let not_due = !w.check(0).unwrap_or(true) && !flag.exists();
                let due = w.check(10_000).unwrap_or(false) && flag.exists();
                let _ = std::fs::remove_dir_all(&dir);
                not_due && due
            }
            "stuff" => {
                let (_l, fillers, port) = stuffed_listener();
                drop(fillers);
                // Ok(false) is the verdict; an Err or a hang is neither.
                !probe_tcp(port).unwrap_or(true)
            }
            "alarm" => {
                arm_sigalrm_waker();
                let (_l, fillers, port) = stuffed_listener();
                drop(fillers);
                let alarm = sigalrm_here_after(200);
                let not_ready = !probe_tcp(port).unwrap_or(true);
                alarm.join().expect("alarm thread finishes");
                not_ready
            }
            "sealA" => {
                // Free slots 0 and 2 so the probe pipe lands at (2, 3): the
                // child half's own close(w) can then never eat a dup'd stdio.
                // Slot 0 gets a read-only placeholder, duplicated onto fd 1,
                // so a dup2 block that is skipped wholesale leaves the check
                // writing through a read-only fd 1 — and failing.
                // SAFETY: raw descriptors are closed/redirected only in this
                // fresh probe child, whose stdio we re-seal right after.
                unsafe {
                    close_fds(&[0, 2]);
                    let ro = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
                    assert_eq!(ro, 0, "/dev/null must reopen at slot 0");
                    libc::dup2(0, 1);
                }
                run_check("echo hi", 2_000).unwrap_or(false)
            }
            "kprobe" => {
                let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
                let port = l.local_addr().expect("addr").port();
                // SAFETY: slot 0 is a normally reusable low descriptor; the
                // probe socket acquires it (lowest free) and stays valid.
                unsafe { libc::close(0) };
                probe_tcp(port).unwrap_or(false)
            }
            other => panic!("unknown probe mode {other}"),
        };
        std::process::exit(if ok { 0 } else { 1 });
    }

    /// Close descriptor numbers before a stdio-sensitive probe.
    ///
    /// # SAFETY
    /// Callers run in a fresh probe child state where 0/1/2 belong to them.
    unsafe fn close_fds(fds: &[RawFd]) {
        // SAFETY: as documented; each fd is owned or free in the caller.
        for &fd in fds {
            unsafe { libc::close(fd) };
        }
    }

    /// A listener whose accept queue is capped at one slot and stuffed with
    /// two unaccepted connections: the kernel then drops new SYNs, which is
    /// the only way a loopback connect stays in `EINPROGRESS` long enough
    /// for the `poll(POLLOUT)` after it to time out or eat a signal.
    fn stuffed_listener() -> (std::net::TcpListener, [std::net::TcpStream; 2], u16) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&l);
        // SAFETY: `fd` is the listener's own descriptor; re-listening only
        // tightens queueing policy, it evicts nothing.
        unsafe { libc::listen(fd, 1) };
        let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
        let one = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2))
            .expect("first fits");
        let two = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(200));
        (l, [one, two.expect("second queued")], port)
    }

    /// A full accept queue is the only way a loopback connect parks in
    /// `EINPROGRESS`: the poll after it runs to its timeout, and `n < 0`
    /// mutants pretending `0` is an error verdict turn that silent delay
    /// into a hard failure (or, on a stale `EINTR`, into a spin — which is
    /// why this runs in a watchdogged probe child).
    ///
    /// Linux only: `listen(fd, 1)` queues `backlog + 1`, so two connects fit
    /// and the third parks. Darwin queues exactly `backlog`, so the second
    /// connect itself blocks until `connect_timeout` and the premise (a full
    /// queue with a non-blocking probe after it) never holds.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn probe_tcp_reports_a_full_backlog_as_not_ready() {
        assert!(
            probe_child("stuff").success(),
            "the queue is full: the probe must wait out its poll, silently"
        );
    }

    /// A signal must not break the in-progress probe any more than a read:
    /// the poll restarts after EINTR and still reports the timeout calmly.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn probe_tcp_poll_survives_a_signal() {
        assert!(
            probe_child("alarm").success(),
            "backlogged probe still waits, re-polling through EINTR"
        );
    }

    /// `interrupted => continue` in `poll_readable`: a signal mid-wait is a
    /// retry, not an error, and the data that arrives after it is still seen.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn poll_readable_retries_when_interrupted() {
        arm_sigalrm_waker();
        let (r, w) = zrt::sys::pipe2(true).expect("pipe");
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            zrt::sys::write(w, b"x").expect("write");
        });
        let alarm = sigalrm_here_after(200);
        let readable = poll_readable(r, 1_500).expect("a signal is not an error");
        alarm.join().expect("alarm thread finishes");
        writer.join().expect("writer thread finishes");
        assert!(readable, "data written after the EINTR is still seen");
        let _ = zrt::sys::close(r);
    }
}
