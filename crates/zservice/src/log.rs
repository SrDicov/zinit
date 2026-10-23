//! Where a service's bytes go: one `write(2)` per line, rotation by size.
//!
//! # Deliberately the 5% of `svlogd`, not the 100%
//!
//! `DESIGN.md` §10 makes the call: rotation is the only feature that matters,
//! and it fits in twenty lines with no state machine. What is kept:
//!
//! * **Default sink**: `/var/log/zinit/<svc>.log`, resolved by `zconfig`'s
//!   plan builder long before this crate sees it.
//! * **Rotation**: past `max_bytes`, the file is renamed to `.1` (then `.2`,
//!   … up to `backups`), the oldest generation is discarded, and a fresh file
//!   is opened. No daemon, no timestamp rewriting, no pattern matching.
//! * **One `write(2)` per line.** No `BufWriter`, no userspace buffer that a
//!   `SIGKILL` between the write and the flush would silently eat. A partial
//!   write is looped in this function, so the caller never sees half a line.
//!
//! What is *not* here: rotation for anything but files, and timestamps.
//! Timestamps belong to the writer (a service that wants them prints them);
//! rewriting other programs' lines is how log pipelines start disagreeing
//! about what was actually said.
//!
//! `log = syslog` is a connected `AF_UNIX` datagram socket to `/dev/log`,
//! handed to the child as stdout/stderr like any other sink: one `write(2)`
//! is one datagram, which is the whole syslog transport contract. There is
//! deliberately no `PRI`/`TAG` framing in this crate — attribution travels
//! in the kernel's `SCM_CREDENTIALS`, which is how journald names the
//! sender, and inventing a line format here would make a future journald
//! consumer disagree with every log already written. Supervisor-side lines
//! share the handle's connected socket; a failed write reconnects once
//! (a restarted daemon rebinds `/dev/log`, orphaning the old connection)
//! before failing loudly. No daemon at `/dev/log` is announced degradation
//! with the service running dark, never a refused spawn: a machine whose
//! logger is down still needs its services.

use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// An open log destination, owned by the supervisor.
///
/// The supervisor keeps one of these per service and hands the raw `fd` to
/// the child as stdout/stderr. `fd` is always `O_APPEND`: concurrent writers
/// (a service that forks helpers sharing stdout) can interleave lines but
/// never overwrite each other, and rotation-by-rename stays coherent because
/// no offset is ever cached.
#[derive(Debug)]
pub struct LogHandle {
    /// The open file (or `/dev/null`, or a connected syslog socket) the
    /// child writes to.
    fd: RawFd,
    /// Filesystem path, when this is a real file. `None` for `/dev/null`
    /// and for syslog (rotation is a file concept; datagrams are not
    /// renamed).
    path: Option<PathBuf>,
    /// Rotate once the file reaches this size.
    max_bytes: u64,
    /// Rotated generations kept (`.1` … `.N`).
    backups: u8,
    /// True for a syslog socket: supervisor-side lines share this connected
    /// socket (reconnecting once on failure), instead of a log file.
    datagram: bool,
}

impl LogHandle {
    /// The descriptor the child inherits as stdout/stderr.
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Close the sink. Best-effort by design: this runs on teardown paths
    /// where there is nothing useful to do with an `EBADF`.
    pub fn close(&mut self) {
        if self.fd >= 0 {
            let _ = zrt::sys::close_quietly(self.fd);
            self.fd = -1;
        }
    }
}

impl Drop for LogHandle {
    fn drop(&mut self) {
        self.close();
    }
}

/// Open the sink described by a frozen [`zcore::LogSink`].
///
/// The parent directory is created (`mkdir -p`) because `/var/log/zinit` may
/// not exist on first boot; a component that exists but is not a directory is
/// a hard error, never silently stepped over. Files open `O_APPEND | O_CREAT
/// | O_WRONLY | O_CLOEXEC` with mode `0644` — `CLOEXEC` because the
/// supervisor must never leak its log fds into a *different* service's
/// child, `O_APPEND` for the multi-writer argument in [`LogHandle`].
///
/// `O_NOFOLLOW` is deliberately **not** set: the default log path may itself
/// be a symlink (tmpfs `/var/log`, syslog shims), and refusing to follow it
/// would break exactly the setups the default is meant to support.
pub fn open_sink(sink: &zcore::LogSink) -> io::Result<LogHandle> {
    match sink {
        zcore::LogSink::None => Ok(LogHandle {
            fd: open_devnull()?,
            path: None,
            max_bytes: u64::MAX,
            backups: 0,
            datagram: false,
        }),
        zcore::LogSink::File {
            path,
            max_bytes,
            backups,
        } => {
            let file = PathBuf::from(path);
            if let Some(dir) = file.parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir).map_err(|e| {
                        io::Error::new(
                            e.kind(),
                            format!("cannot create log directory {}: {e}", dir.display()),
                        )
                    })?;
                }
            }
            let fd = open_append(&file)?;
            Ok(LogHandle {
                fd,
                path: Some(file),
                max_bytes: *max_bytes,
                backups: *backups,
                datagram: false,
            })
        }
        zcore::LogSink::Syslog => open_syslog_at(Path::new(SYSLOG_PATH)),
    }
}

/// Where syslog daemons listen, on every system that has one.
const SYSLOG_PATH: &str = "/dev/log";

/// Open the syslog sink against `path` (normally [`SYSLOG_PATH`]).
///
/// The socket is connected and blocking: it is inherited as the service's
/// stdout/stderr, and stdio expects blocking fds. No daemon there is an
/// error naming the path — the supervisor announces it and runs the service
/// dark (see `open_sink`'s contract), which is degradation, not refusal.
///
/// Factored by path (rather than inlining `/dev/log`) so tests can bind
/// their own daemon: the host's `/dev/log` may or may not exist, and a test
/// must not depend on which.
fn open_syslog_at(path: &Path) -> io::Result<LogHandle> {
    match zrt::sys::unix_dgram_connect(path) {
        Ok(fd) => Ok(LogHandle {
            fd,
            path: None,
            max_bytes: u64::MAX,
            backups: 0,
            datagram: true,
        }),
        Err(e) => Err(io::Error::new(
            e.kind(),
            format!("no syslog daemon at {} ({e})", path.display()),
        )),
    }
}

/// Append one line with a single `write(2)`.
///
/// Rotation is checked first via `fstat` on the open fd (never the path:
/// the path may have been renamed out from under us, and stating it would
/// answer about a different file). Short lines go out as one syscall —
/// callers must not include the trailing newline; it is added here, inside
/// the same write, so a reader never sees a line without its terminator.
/// Lines longer than the 4 KiB stack buffer take two writes (line, then
/// newline); `O_APPEND` keeps them contiguous on regular files, and the
/// fallback is documented rather than hidden.
///
/// An empty `line` still writes the newline: an empty line from the child is
/// data, not a reason to emit nothing.
pub fn write_line(h: &mut LogHandle, line: &[u8]) -> io::Result<()> {
    if h.fd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "log sink is closed",
        ));
    }
    // Datagrams leave through the handle's own connected socket (see
    // `write_datagram`): the supervisor shares the child's connection the
    // way it shares a log file.
    if h.datagram {
        return write_datagram(h, line);
    }
    rotate(h)?;
    // Room for the newline without splitting the line across two writes.
    if line.len() < 4096 {
        let mut buf = [0u8; 4096];
        buf[..line.len()].copy_from_slice(line);
        buf[line.len()] = b'\n';
        write_all(h.fd, &buf[..line.len() + 1])?;
    } else {
        // Long line: two writes. `O_APPEND` on a regular file makes each
        // write land atomically at the end, so no interleave can split them;
        // on anything else (a pipe, a socket) the caller was told this sink
        // is file-oriented. Bounded by honesty, not by silence.
        write_all(h.fd, line)?;
        write_all(h.fd, b"\n")?;
    }
    Ok(())
}

/// One supervisor-side line to the syslog daemon, through the handle.
///
/// Normally a single write on the connected socket opened at `open_sink.
/// If the write fails the daemon likely restarted and rebound `/dev/log`
/// under us — a connected datagram socket points at the old binding
/// forever — so reconnect once and retry once before admitting failure.
/// A missing daemon fails here, loudly, instead of parking the loop in a
/// blocking write to nowhere. (The child's own copy of the fd is not
/// refreshed: like a rotated log file, post-`exec` writes go wherever the
/// descriptor pointed at spawn. Same shape, same acceptance.)
fn write_datagram(h: &mut LogHandle, line: &[u8]) -> io::Result<()> {
    // Same framing as `write_line`: line plus terminator in one write when
    // it fits (one datagram), split across two when it does not — the same
    // documented fallback pipes already live with.
    let send = |fd: RawFd| {
        if line.len() < 4096 {
            let mut buf = [0u8; 4096];
            buf[..line.len()].copy_from_slice(line);
            buf[line.len()] = b'\n';
            write_all(fd, &buf[..line.len() + 1])
        } else {
            write_all(fd, line).and(write_all(fd, b"\n"))
        }
    };
    if send(h.fd).is_ok() {
        return Ok(());
    }
    let fd = zrt::sys::unix_dgram_connect(Path::new(SYSLOG_PATH))?;
    let _ = zrt::sys::close(h.fd);
    h.fd = fd;
    send(h.fd)
}

/// Rotate when the file has reached `max_bytes`. No-op for `/dev/null`.
///
/// Generations shift up (`.2` → `.3`, …, `.1` → `.2`), the live file becomes
/// `.1`, and a fresh file is opened at the original path. Missing generations
/// are skipped (`ENOENT` is expected, not an error); any other failure aborts
/// with the fd left open on the *original* path so the service keeps logging
/// somewhere. With `backups == 0` the file is truncated in place instead —
/// the size bound is still honoured, just without history.
fn rotate(h: &mut LogHandle) -> io::Result<()> {
    let path = match &h.path {
        Some(p) => p.clone(),
        None => return Ok(()),
    };
    if fstat_size(h.fd)? < h.max_bytes {
        return Ok(());
    }
    if h.backups == 0 {
        // No history: truncate in place. `ftruncate` on the open fd keeps the
        // child's descriptor valid — no reopen, no window without a log.
        // SAFETY: `ftruncate` takes an fd and a length; both are valid here.
        if unsafe { libc::ftruncate(h.fd, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
    // Close first: on Linux a rename-while-open is legal, but reopening after
    // the rename is what guarantees the new file — not a stale handle to the
    // rotated one — receives the next line.
    let _ = zrt::sys::close(h.fd);
    h.fd = -1;
    let result = shift_generations(&path, h.backups).and_then(|()| {
        let fd = open_append(&path)?;
        h.fd = fd;
        Ok(())
    });
    if result.is_err() {
        // Best effort recovery: the service must keep logging *somewhere*.
        // Reopen the live path even if the rotation half-finished; the error
        // still propagates so the supervisor can report it.
        if h.fd < 0 {
            // equivalent (cargo-mutants): with `result.is_err()` the fd is
            // still -1 here (`close` set it above; `h.fd` is only assigned
            // once the reopen `open` already succeeded, i.e. `result` was
            // `Ok`), so `<` and `<=` cannot differ;
            // `rotation_failure_reopens_the_live_fd` pins that state.
            match open_append(&path) {
                Ok(fd) => h.fd = fd,
                Err(_) => return result,
            }
        }
    }
    result
}

/// Shift `.N-1` → `.N` down to `.1` → `.2`, then the live file → `.1`.
fn shift_generations(path: &Path, backups: u8) -> io::Result<()> {
    let mut i = backups;
    while i > 1 {
        let from = sibling(path, i - 1);
        let to = sibling(path, i);
        match zrt::sys::rename(&from, &to) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
            Err(e) => return Err(e),
        }
        i -= 1;
    }
    match zrt::sys::rename(path, &sibling(path, 1)) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
        Err(e) => Err(e),
    }
}

/// `path` with `.N` appended: `sshd.log` → `sshd.log.1`.
fn sibling(path: &Path, n: u8) -> PathBuf {
    let mut s = path.as_os_str().as_bytes().to_vec();
    s.push(b'.');
    s.extend_from_slice(n.to_string().as_bytes());
    PathBuf::from(std::ffi::OsStr::from_bytes(&s))
}

/// Current size of an open fd, via `fstat` — never via the path.
fn fstat_size(fd: RawFd) -> io::Result<u64> {
    // SAFETY: all zeros is a valid empty `stat`; `fstat` fills it.
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: `st` is a live, aligned `stat`; `fstat` writes exactly one and
    // retains nothing.
    if unsafe { libc::fstat(fd, &raw mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.st_size.max(0) as u64)
}

/// Open (creating) for append, `O_CLOEXEC`, mode `0644`.
fn open_append(path: &Path) -> io::Result<RawFd> {
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("log path contains a NUL byte: {}", path.display()),
        )
    })?;
    // SAFETY: `c` is NUL-terminated and alive for the call; `mode` is only
    // consulted because `O_CREAT` is set.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            // equivalent (cargo-mutants): the flags are disjoint bitsets
            // (see the `annotated_or_flags_are_disjoint_bitsets` test), so
            // `|` cannot differ from `^` here.
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND | libc::O_CLOEXEC,
            0o644 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Open `/dev/null` for writing.
fn open_devnull() -> io::Result<RawFd> {
    // SAFETY: static literal, NUL-terminated by construction; the fd is fresh.
    // equivalent (cargo-mutants): the flags are disjoint bitsets (see the
    // `annotated_or_flags_are_disjoint_bitsets` test), so `|` cannot differ
    // from `^` here.
    let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Write the whole buffer, looping over partial writes and `EINTR`.
///
/// `zrt::sys::write` reports a short write as-is (it cannot know whether a
/// half-written frame is retryable); here the answer is known — a log line is
/// always retryable — so the loop lives in this function and nowhere else.
fn write_all(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match zrt::sys::write(fd, buf) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "log write returned zero",
                ));
            }
            Ok(n) => buf = &buf[n..],
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    /// Runs `body` in a forked child. The runner's threads share one fd table,
    /// so freeing fd 0 in the parent races every other test (and the runner's
    /// stdin) — a mutex does not help, because unrelated tests keep opening
    /// files and can steal the low slot. `body` must only touch paths computed
    /// before the fork: it runs in a child of a multi-threaded process.
    pub(crate) fn in_isolated_fd_table(body: impl FnOnce() -> bool) -> bool {
        // SAFETY: plain fork; the child only opens/closes/dups stdio and _exits,
        // and never allocates.
        match unsafe { libc::fork() } {
            -1 => panic!("fork: {}", io::Error::last_os_error()),
            0 => {
                let ok = body();
                // SAFETY: leaving the child without running parent destructors.
                unsafe { libc::_exit(if ok { 0 } else { 1 }) }
            }
            pid => {
                let mut status = 0;
                // SAFETY: waiting on the child pid we just created.
                unsafe { libc::waitpid(pid, &mut status, 0) };
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
            }
        }
    }

    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        crate::testutil::scratch(&format!("log-{name}"))
    }

    fn file_sink(path: &Path, max_bytes: u64, backups: u8) -> zcore::LogSink {
        zcore::LogSink::File {
            path: path.to_string_lossy().into_owned(),
            max_bytes,
            backups,
        }
    }

    #[test]
    fn open_creates_missing_directories() {
        let dir = tmpdir("mkdir");
        let log = dir.join("sub").join("svc.log");
        let mut h = open_sink(&file_sink(&log, 1024, 1)).expect("open");
        assert!(log.exists());
        write_line(&mut h, b"hello").expect("write");
        h.close();
        assert_eq!(std::fs::read(&log).expect("read"), b"hello\n");
    }

    #[test]
    fn each_line_is_one_write_with_terminator() {
        let dir = tmpdir("lines");
        let log = dir.join("svc.log");
        let mut h = open_sink(&file_sink(&log, 1 << 20, 1)).expect("open");
        write_line(&mut h, b"one").expect("w1");
        write_line(&mut h, b"").expect("empty line is data");
        write_line(&mut h, b"three").expect("w3");
        h.close();
        assert_eq!(std::fs::read(&log).expect("read"), b"one\n\nthree\n");
    }

    #[test]
    fn rotation_renames_to_dot1_and_keeps_writing() {
        let dir = tmpdir("rotate");
        let log = dir.join("svc.log");
        let mut h = open_sink(&file_sink(&log, 10, 2)).expect("open");
        write_line(&mut h, b"0123456789ABCDEF").expect("overflow");
        // The overflow line rotated the (empty) file, then landed in the fresh one.
        write_line(&mut h, b"second-overflow-line").expect("overflow again");
        h.close();
        let gen1 = PathBuf::from(format!("{}.1", log.display()));
        let gen2 = PathBuf::from(format!("{}.2", log.display()));
        assert!(gen1.exists() || gen2.exists(), "a generation must exist");
        let live = std::fs::read(&log).expect("live");
        assert!(live.ends_with(b"\n"), "live file keeps its terminator");
    }

    #[test]
    fn rotation_discards_beyond_backups() {
        let dir = tmpdir("discard");
        let log = dir.join("svc.log");
        let mut h = open_sink(&file_sink(&log, 4, 1)).expect("open");
        for i in 0..5 {
            write_line(&mut h, format!("line-{i}-overflow").as_bytes()).expect("w");
        }
        h.close();
        let gen2 = PathBuf::from(format!("{}.2", log.display()));
        assert!(!gen2.exists(), "only .1 may exist with backups=1");
    }

    #[test]
    fn zero_backups_truncates_in_place() {
        let dir = tmpdir("trunc");
        let log = dir.join("svc.log");
        let mut h = open_sink(&file_sink(&log, 8, 0)).expect("open");
        write_line(&mut h, b"0123456789ABCDEF").expect("overflow");
        write_line(&mut h, b"tiny").expect("after truncate");
        let fd = h.fd();
        h.close();
        let _ = fd;
        let content = std::fs::read(&log).expect("read");
        assert!(content.len() < 30, "must have truncated: {content:?}");
        assert!(content.ends_with(b"tiny\n"));
    }

    #[test]
    fn none_sink_discards_without_a_path() {
        let mut h = open_sink(&zcore::LogSink::None).expect("open");
        write_line(&mut h, b"into the void").expect("write to /dev/null");
        h.close();
    }

    #[test]
    fn syslog_without_daemon_is_a_loud_error() {
        // No listener at the path: the error names it, and the supervisor
        // announces that error and runs the service dark. What must never
        // happen is silent success.
        let missing = tmpdir("syslog-missing").join("dev-log.sock");
        let e = open_syslog_at(&missing).expect_err("no daemon must fail");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("no syslog daemon"), "{e}");
    }

    #[test]
    fn syslog_round_trip_through_a_bound_peer() {
        // A daemon of our own (a bound datagram socket): one `write_line`
        // arrives as one datagram with its terminator.
        let dir = tmpdir("syslog-roundtrip");
        let path = dir.join("dev-log.sock");
        let listener = bind_dgram(&path);
        bound_recv(listener);
        let mut h = open_syslog_at(&path).expect("daemon present");
        write_line(&mut h, b"hello").expect("write");
        h.close();
        let mut buf = [0u8; 32];
        // SAFETY: `listener` is an owned bound datagram socket; `recvfrom`
        // with null address arguments writes at most `buf.len()` bytes and
        // retains nothing.
        let n = unsafe {
            libc::recvfrom(
                listener,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        assert!(n > 0, "recvfrom: {}", io::Error::last_os_error());
        assert_eq!(&buf[..n as usize], b"hello\n");
        let _ = zrt::sys::close(listener);
        let _ = std::fs::remove_file(&path);
    }

    /// Bind a `SOCK_DGRAM` socket at `path`: the test daemon.
    ///
    /// Test-only because production never binds (it only dials `/dev/log`).
    /// `path` comes from `tmpdir`, so it fits `sun_path` with room.
    fn bind_dgram(path: &Path) -> RawFd {
        let bytes = path.as_os_str().as_bytes();
        assert!(bytes.len() + 1 < 108, "test path must fit sun_path");
        // SAFETY: `socket` takes ints; the fd is owned on success.
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) };
        assert!(fd >= 0, "socket: {}", io::Error::last_os_error());
        let mut addr: libc::sockaddr_un = unsafe { core::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (i, b) in bytes.iter().enumerate() {
            addr.sun_path[i] = *b as libc::c_char;
        }
        // SAFETY: `addr` carries exactly the path bytes plus the zeroed
        // terminator; `bind` copies `len` bytes and retains nothing.
        let len = (core::mem::size_of::<libc::sockaddr_un>() - addr.sun_path.len()
            + bytes.len()
            + 1) as libc::socklen_t;
        let rc = unsafe { libc::bind(fd, &raw const addr as *const libc::sockaddr, len) };
        assert_eq!(rc, 0, "bind: {}", io::Error::last_os_error());
        zrt::sys::set_cloexec(fd, true).expect("cloexec");
        fd
    }

    /// The receive-side watchdog: a mutant that turns a write into `Ok(())`
    /// without `write(2)` must make the datagram tests *fail* (~3 s later,
    /// `EAGAIN` on the timed socket), not hang the whole test binary on a
    /// silent blocking socket — a hung run is a `TIMEOUT`; a failed one is a
    /// caught mutant.
    fn bound_recv(fd: RawFd) {
        let tv = libc::timeval {
            tv_sec: 3,
            tv_usec: 0,
        };
        // SAFETY: `fd` is an owned test socket; `SO_RCVTIMEO` reads exactly
        // one `timeval` and retains nothing.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    &raw const tv as *const libc::c_void,
                    core::mem::size_of::<libc::timeval>() as libc::socklen_t,
                )
            },
            0,
            "setsockopt SO_RCVTIMEO: {}",
            io::Error::last_os_error()
        );
    }

    #[test]
    fn long_lines_still_terminate() {
        let dir = tmpdir("long");
        let log = dir.join("svc.log");
        let mut h = open_sink(&file_sink(&log, 1 << 20, 1)).expect("open");
        let big = vec![b'x'; 9000];
        write_line(&mut h, &big).expect("long write");
        h.close();
        let content = std::fs::read(&log).expect("read");
        assert_eq!(content.len(), 9001);
        assert_eq!(&content[..9000], big.as_slice());
        assert_eq!(content[9000], b'\n');
    }

    #[test]
    fn sibling_appends_the_generation() {
        let p = Path::new("/var/log/zinit/sshd.log");
        assert_eq!(
            sibling(p, 1).as_os_str().as_bytes(),
            b"/var/log/zinit/sshd.log.1"
        );
    }

    /// Logging to a fixed 4 KiB threshold (`4096`) differs by one byte from
    /// "up to 4095 in one write, longer in two": both sides of the boundary
    /// must land identically on disk, and the boundary line itself must not
    /// overflow the stack buffer (`4096th byte is reserved for the newline`).
    #[test]
    fn boundary_line_sizes_round_trip() {
        let dir = tmpdir("boundary");
        let log = dir.join("svc.log");
        let mut h = open_sink(&file_sink(&log, 1 << 20, 1)).expect("open");
        let mid = vec![b'x'; 4095];
        let edge = vec![b'y'; 4096];
        write_line(&mut h, &mid).expect("4095-byte line");
        write_line(&mut h, &edge).expect("4096-byte line");
        h.close();
        let content = std::fs::read(&log).expect("read");
        assert_eq!(content.len(), 4095 + 1 + 4096 + 1);
        assert_eq!(&content[..4095], mid.as_slice());
        assert_eq!(content[4095], b'\n');
        assert_eq!(&content[4096..4096 + 4096], edge.as_slice());
        assert_eq!(content[4096 + 4096], b'\n');
    }

    /// The datagram path shares the same 4096 framing; the boundary line must
    /// still go out complete (and must not index past the stack buffer).
    ///
    /// Linux only: Darwin's `net.local.dgram.maxdgram` defaults to 2048, so a
    /// 4096-byte `send` fails with `EMSGSIZE` before any of this framing is
    /// observable. The fallback then redials `/dev/log`, which is not what the
    /// test bound, and the failure surfaces as a misleading `ENOENT`.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn datagram_carries_boundary_line() {
        let dir = tmpdir("dgram-boundary");
        let path = dir.join("dev-log.sock");
        let listener = bind_dgram(&path);
        bound_recv(listener);
        let mut h = open_syslog_at(&path).expect("daemon present");
        let edge = vec![b'z'; 4096];
        write_line(&mut h, &edge).expect("boundary datagram");
        h.close();
        let mut buf = vec![0u8; 5000];
        // SAFETY: `listener` is an owned bound datagram socket; recvfrom with
        // null address arguments writes at most buf.len() and retains nothing.
        let n = unsafe {
            libc::recvfrom(
                listener,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        assert_eq!(n as usize, 4096, "boundary line is its own datagram");
        assert_eq!(&buf[..4096], edge.as_slice());
        // SAFETY: same owned socket; the terminator follows as its own write.
        let n = unsafe {
            libc::recvfrom(
                listener,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        assert_eq!(n, 1, "the newline datagram");
        assert_eq!(buf[0], b'\n');
        let _ = zrt::sys::close(listener);
        let _ = std::fs::remove_file(&path);
    }

    /// `close` is idempotent and handoff-safe: after it, the sink refuses
    /// writes with `NotConnected` (not a leaked-EBADF from a raw write).
    #[test]
    fn closed_sink_refuses_writes_with_notconnected() {
        let dir = tmpdir("closed");
        let log = dir.join("svc.log");
        let mut h = open_sink(&file_sink(&log, 1 << 20, 1)).expect("open");
        write_line(&mut h, b"before").expect("write before close");
        h.close();
        h.close(); // double close is intentionally tolerated
        let e = write_line(&mut h, b"after").expect_err("closed sink must refuse");
        assert_eq!(e.kind(), io::ErrorKind::NotConnected, "{e}");
        assert_eq!(std::fs::read(&log).expect("read"), b"before\n");
    }

    /// fd 0 is a live descriptor like any other: only a *negative* fd means
    /// closed. (`fd <= 0` mutants would refuse fd 0 outright.)
    #[test]
    fn fd_zero_writes_normally() {
        assert!(
            in_isolated_fd_table(|| {
                // SAFETY: fd 0 is this child's alone.
                let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY) };
                if null < 0 || unsafe { libc::dup2(null, 0) } != 0 {
                    return false;
                }
                unsafe { libc::close(null) };
                // SAFETY: fd 0 now is /dev/null in this child.
                let mut h = LogHandle {
                    fd: 0,
                    path: None,
                    max_bytes: u64::MAX,
                    backups: 0,
                    datagram: false,
                };
                let ok = write_line(&mut h, b"into fd zero").is_ok();
                h.close();
                ok
            }),
            "writing through fd 0 must behave like any other descriptor"
        );
    }

    /// A file already at exactly `max_bytes` must rotate on the next line:
    /// "past max_bytes" means `size < max` keeps, `size == max` rotates.
    #[test]
    fn rotate_triggers_at_exact_max_bytes() {
        let dir = tmpdir("exact-max");
        let log = dir.join("svc.log");
        std::fs::write(&log, b"0123456789").expect("seed file at exactly max");
        let mut h = open_sink(&file_sink(&log, 10, 1)).expect("open");
        write_line(&mut h, b"ab").expect("write past max");
        h.close();
        let gen1 = log.with_extension("log.1");
        assert_eq!(
            std::fs::read(&gen1).expect("generation 1"),
            b"0123456789",
            "the full previous file is the rotated generation"
        );
        assert_eq!(std::fs::read(&log).expect("live"), b"ab\n");
    }

    /// When rotation fails (unwritable directory) the handle must come back
    /// usable through the recovery reopen — and never keep writing through a
    /// dangling fd (e.g. borrowed stdout).
    #[test]
    fn rotation_failure_recovers_without_dangling_fd() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("rot-fail");
        let log = dir.join("svc.log");
        let mut h = open_sink(&file_sink(&log, 4, 1)).expect("open");
        write_line(&mut h, b"aaaa").expect("first");
        write_line(&mut h, b"bbbb").expect("second");
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555));
        write_line(&mut h, b"cc").expect_err("rotation must fail on read-only dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
        write_line(&mut h, b"dd").expect("recovery must have left a usable fd");
        h.close();
        assert_eq!(
            std::fs::read(&log).expect("read"),
            b"dd\n",
            "nothing may go to a dangling fd; live log restarted at dd"
        );
        assert_eq!(
            std::fs::read(log.with_extension("log.1")).expect("gen1"),
            b"bbbb\n"
        );
    }

    /// The failure path reopens the live file so the fd stays valid (nameable
    /// through `fd()`): a skipped reopen leaves the killed `-1` handle.
    #[test]
    fn rotation_failure_reopens_the_live_fd() {
        let dir = tmpdir("rot-reopen");
        let log = dir.join("svc.log");
        let mut h = open_sink(&file_sink(&log, 4, 1)).expect("open");
        write_line(&mut h, b"aaaa").expect("fill");
        // Target of the final rename is a directory: the rename fails, but
        // the live path itself is still perfectly openable.
        std::fs::create_dir(dir.join("svc.log.1")).expect("block .1");
        write_line(&mut h, b"bb").expect_err("rename must fail");
        assert_ne!(
            h.fd(),
            -1,
            "recovery must reopen the live path, keeping the fd valid"
        );
        h.close();
        let _ = std::fs::remove_dir(dir.join("svc.log.1"));
    }

    /// Three backups shift oldest→newest in order: `.3` is the oldest line's
    /// generation, `.1` the newest, the live file the current one.
    #[test]
    fn shift_generations_keeps_history_in_order() {
        let dir = tmpdir("shift-table");
        let log = dir.join("svc.log");
        let mut h = open_sink(&file_sink(&log, 8, 3)).expect("open");
        for name in ["first!!", "second!", "third!!", "fourth!"] {
            write_line(&mut h, name.as_bytes()).expect(name);
        }
        h.close();
        assert_eq!(
            std::fs::read(log.with_extension("log.1")).expect("gen1"),
            b"third!!\n"
        );
        assert_eq!(
            std::fs::read(log.with_extension("log.2")).expect("gen2"),
            b"second!\n"
        );
        assert_eq!(
            std::fs::read(log.with_extension("log.3")).expect("gen3"),
            b"first!!\n"
        );
        assert_eq!(std::fs::read(&log).expect("live"), b"fourth!\n");
    }

    /// `.0` is not a generation: shifting may never touch a pre-existing
    /// `path + ".0"` file. (Indexes start at 1; a mandate from `log.0` data
    /// would clobber unrelated state.)
    #[test]
    fn generation_zero_is_never_touched() {
        let dir = tmpdir("gen-zero");
        let log = dir.join("svc.log");
        let zero = log.with_extension("log.0");
        std::fs::write(&zero, b"unrelated").expect("seed .0");
        let mut h = open_sink(&file_sink(&log, 8, 3)).expect("open");
        for _ in 0..3 {
            write_line(&mut h, b"abcdefg").expect("write");
        }
        h.close();
        assert_eq!(
            std::fs::read(&zero).expect(".0 must survive every rotation"),
            b"unrelated"
        );
    }

    /// Opening a directory is a hard error (EISDIR), not a success with a
    /// dead fd (`fd <= 0` mutants report failure; `==` lets `-1` through).
    #[test]
    fn open_append_refuses_a_directory() {
        let dir = tmpdir("open-dir");
        let e = open_append(&dir).expect_err("cannot open(2) a directory");
        assert_ne!(e.kind(), io::ErrorKind::Other, "real errno, {e}");
    }

    /// open(2) can legitimately return fd 0 (after the runner frees it);
    /// that is a success, not `fd > 0` territory. Restores fd 0 on exit.
    #[test]
    fn open_append_accepts_fd_zero() {
        let path = tmpdir("fd0-open").join("fresh.log");
        assert!(
            in_isolated_fd_table(|| {
                // SAFETY: fd 0 is this child's alone.
                unsafe { libc::close(0) };
                let Ok(fd) = open_append(&path) else {
                    return false;
                };
                fd == 0
                    && write_all(fd, b"hi").is_ok()
                    && std::fs::read(&path).is_ok_and(|b| b == b"hi")
            }),
            "a fresh file must claim the freed low fd 0"
        );
    }

    /// Reopening an existing log must succeed, and two open handles must
    /// interleave at the end (O_APPEND), never clobber from offset 0 — a
    /// bit-mangled flags expression loses exactly that guarantee.
    #[test]
    fn reopened_and_parallel_handles_append() {
        let dir = tmpdir("append-reopen");
        let log = dir.join("svc.log");
        let mut h1 = open_sink(&file_sink(&log, 1 << 20, 1)).expect("first");
        write_line(&mut h1, b"aaaaaaaaaaaa").expect("write h1");
        h1.close();
        let mut h2 = open_sink(&file_sink(&log, 1 << 20, 1)).expect("reopen");
        write_line(&mut h2, b"bbbbbbbbbbbb").expect("write h2");
        h2.close();
        let content = std::fs::read(&log).expect("read");
        assert!(
            content.starts_with(b"aaaaaaaaaaaa\n"),
            "a second handle must append, not clobber offset 0: {content:?}"
        );
        assert!(content.ends_with(b"bbbbbbbbbbbb\n"));
    }

    /// The `|`s in `open_append` and `open_devnull` annotated
    /// `// equivalent (cargo-mutants)` pair flags that are disjoint
    /// bitsets, and only disjoint bitsets make `a | b` identical to
    /// `a ^ b` for every input. These asserts are the machine check behind
    /// those annotations: shrink one pair and this test fails exactly when
    /// a claimed equivalence stops holding.
    #[test]
    fn annotated_or_flags_are_disjoint_bitsets() {
        assert_eq!(libc::O_WRONLY & libc::O_CREAT, 0, "open_append: 1/2");
        assert_eq!(libc::O_WRONLY & libc::O_APPEND, 0, "open_append: 1/3");
        assert_eq!(libc::O_WRONLY & libc::O_CLOEXEC, 0, "open_append: 1/4");
        assert_eq!(libc::O_CREAT & libc::O_APPEND, 0, "open_append: 2/3");
        assert_eq!(libc::O_CREAT & libc::O_CLOEXEC, 0, "open_append: 2/4");
        assert_eq!(libc::O_APPEND & libc::O_CLOEXEC, 0, "open_append: 3/4");
    }

    /// `Drop` runs `close`: a handle leaving scope must not leak the sink
    /// descriptor into the table — `<impl Drop for LogHandle>::drop with ()`
    /// would quietly burn an fd per service teardown.
    #[test]
    fn drop_closes_the_file_descriptor() {
        let h = open_sink(&zcore::LogSink::None).expect("open");
        let fd = h.fd();
        drop(h);
        // SAFETY: `F_GETFD` only observes one descriptor and retains nothing.
        let rc = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(
            rc,
            -1,
            "dropping the handle must close its fd: {}",
            io::Error::last_os_error()
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
    }

    /// A directory blocking a generation makes the shift's rename fail with
    /// `EISDIR` — a *real* error, not a missing-generation `ENOENT`: the
    /// guard must swallow only `ENOENT`. Its `true`-mutant swallows the
    /// `EISDIR`, finishes the rotation anyway, and rewrites the history the
    /// blocker was protecting.
    #[test]
    fn shift_reports_real_rename_errors_not_just_enoent() {
        let dir = tmpdir("shift-eisdir");
        let log = dir.join("svc.log");
        std::fs::create_dir(log.with_extension("log.2")).expect("block .2");
        let mut h = open_sink(&file_sink(&log, 4, 2)).expect("open");
        write_line(&mut h, b"aaaa").expect("fill to 5 bytes (no rotate yet)");
        write_line(&mut h, b"bbbb").expect("rotate 1: .1 := aaaa, live := bbbb");
        write_line(&mut h, b"cc").expect_err("rename .1 -> .2 onto a directory is EISDIR");
        h.close();
        assert_eq!(
            std::fs::read(&log).expect("live"),
            b"bbbb\n",
            "a failed shift must not move or truncate the live file"
        );
        assert_eq!(
            std::fs::read(log.with_extension("log.1")).expect("gen1"),
            b"aaaa\n",
            "the blocked rename must abort the whole shift"
        );
    }

    /// The live file may vanish between `fstat` (on the still-open fd) and
    /// the final rename — an external `unlink(2)` in between is legal. The
    /// final-rename `ENOENT` is recovery, not failure: the fresh reopen
    /// recreates the path and the line lands. A `false`-mutant turns that
    /// `ENOENT` into `Err`, keeping the service dark for a vanished file.
    #[test]
    fn rotate_survives_a_vanished_live_file() {
        let dir = tmpdir("shift-gone");
        let log = dir.join("svc.log");
        let mut h = open_sink(&file_sink(&log, 4, 1)).expect("open");
        write_line(&mut h, b"aaaa").expect("fill to 5 bytes");
        std::fs::remove_file(&log).expect("unlink between fstat and rename");
        write_line(&mut h, b"bbbb").expect("rotation must recover over a vanished live file");
        h.close();
        assert_eq!(
            std::fs::read(&log).expect("recreated live file"),
            b"bbbb\n",
            "the fresh open(2) recreates the name and takes the line"
        );
        assert!(
            !log.with_extension("log.1").exists(),
            "nothing was available to rotate"
        );
    }

    /// `open(2)` can legitimately hand back fd 0 for `/dev/null` too;
    /// `fd < 0` mutants that read `<= 0` refuse the descriptor (and answer
    /// with a stale errno, worse than the truth). Mirrors
    /// `open_append_accepts_fd_zero`.
    #[test]
    fn open_devnull_accepts_fd_zero() {
        assert!(
            in_isolated_fd_table(|| {
                // SAFETY: fd 0 is this child's alone.
                unsafe { libc::close(0) };
                open_sink(&zcore::LogSink::None).is_ok_and(|h| h.fd() == 0)
            }),
            "/dev/null must keep the freed low fd 0"
        );
    }

    /// One rotation must terminate in microseconds. The sentinel waits 5 s
    /// and then aborts the whole binary.
    ///
    /// WHY THIS IS THE FIRST (alphabetically ordered) TEST: the mutants
    /// that make the generation-shift loop spin (`i -= 1` given `i /= 1`)
    /// hang the *first* rotation with `backups > 1` — whatever test it is —
    /// so the crash must be armed before any rotate runs, and only this
    /// test sorts early enough. Without it those mutants burn the mutator's
    /// full 60 s budget every run instead of failing.
    #[test]
    fn a_rotation_terminates_before_the_suite_spins() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        let done: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let guard = std::thread::spawn(move || {
            for _ in 0..50 {
                if done.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // crashes the whole test binary: that is the point — a hung
            // mutant must fail fast, and the sentinel only fires on a hang.
            std::process::abort()
        });
        let dir = tmpdir("shift-term");
        let log = dir.join("svc.log");
        std::fs::write(&log, b"0123456789").expect("seed at exactly max");
        let mut h = open_sink(&file_sink(&log, 10, 2)).expect("open");
        write_line(&mut h, b"ab").expect("one rotation must terminate");
        h.close();
        done.store(true, Ordering::Relaxed);
        guard.join().expect("sentinel finishes");
        assert_eq!(std::fs::read(&log).expect("live"), b"ab\n");
        assert_eq!(
            std::fs::read(log.with_extension("log.1")).expect("gen1"),
            b"0123456789"
        );
    }
}
