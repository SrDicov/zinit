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
mod tests {
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
    #[test]
    fn datagram_carries_boundary_line() {
        let dir = tmpdir("dgram-boundary");
        let path = dir.join("dev-log.sock");
        let listener = bind_dgram(&path);
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
        use std::os::fd::AsRawFd;
        let null = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .expect("open /dev/null");
        // SAFETY: dup of fd 0; we restore fd 0 before the test returns.
        let saved = unsafe { libc::dup(0) };
        assert!(saved >= 0, "dup: {}", io::Error::last_os_error());
        // SAFETY: dup2 into fd 0; /dev/null accepts any write.
        assert_eq!(unsafe { libc::dup2(null.as_raw_fd(), 0) }, 0, "dup2");
        let mut h = LogHandle {
            fd: 0,
            path: None,
            max_bytes: u64::MAX,
            backups: 0,
            datagram: false,
        };
        write_line(&mut h, b"into fd zero").expect("write on fd 0");
        h.close(); // closes fd 0 (our /dev/null copy); must not leak or panic
        // SAFETY: restore the runner's original fd 0 from the saved dup.
        assert_eq!(unsafe { libc::dup2(saved, 0) }, 0, "restore fd 0");
        // SAFETY: the dup is no longer needed once fd 0 is restored.
        unsafe { libc::close(saved) };
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
        let dir = tmpdir("fd0-open");
        let path = dir.join("fresh.log");
        // SAFETY: free fd 0 for the test, restore from the dup afterwards.
        let saved = unsafe { libc::dup(0) };
        assert!(saved >= 0, "dup: {}", io::Error::last_os_error());
        unsafe { libc::close(0) };
        let fd = open_append(&path).expect("open(2) may use fd 0");
        assert_eq!(fd, 0, "fresh file claimed the freed low fd");
        write_all(fd, b"hi").expect("write through fd 0");
        assert_eq!(std::fs::read(&path).expect("read"), b"hi");
        // SAFETY: fd 0 currently points at our own file; restore the saved.
        assert_eq!(unsafe { libc::dup2(saved, 0) }, 0, "restore fd 0");
        // SAFETY: the dup is no longer needed.
        unsafe { libc::close(saved) };
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
}
