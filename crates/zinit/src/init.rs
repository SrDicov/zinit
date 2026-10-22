//! PID 1: `zinit --init`.
//!
//! Four jobs, no more (`DESIGN.md` §3):
//!
//! 1. Block every signal, so nothing can interrupt a `fork` or be lost.
//! 2. Mount the minimum: `/proc`, `/sys`, `/run`, and the cgroup2
//!    hierarchy (without it every `cgroup =` service fails obscurely,
//!    and nothing else mounts it on a systemd-less boot).
//! 3. Fork+exec the supervisor, `zinit --sup`.
//! 4. Reap, unconditionally, forever.
//!
//! There is no plan, no socket, no service and no getty here. That is the
//! design, not an omission: the supervisor is a *child*, so a panic, an OOM or
//! a protocol bug in it costs one process instead of the machine's governor,
//! and PID 1 stays small enough to be trivially auditable.

use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::ffi::OsStrExt;

use zrt::report::announce_degradation;

/// Idle sleep when there is nothing to wait for. Reachable only with no
/// children at all — a bare reaper, or every fork failing — where a
/// blocking wait would answer `ECHILD` instantly, forever. It also paces
/// supervisor relaunch attempts while forks fail.
// ponytail: the steady state below does not poll. A blocking `waitpid` is
// the wakeup source — a dead child returns it at once — so the reaper
// burns nothing and a crashed supervisor is relaunched immediately, with
// no signalfd, no thread and no timer. The 50 ms loop this replaces cost
// a tick a minute and delayed every relaunch by up to a tick.
const IDLE_MS: u64 = 50;

/// Become PID 1. Diverges, and never panics.
///
/// `main` calls this and nothing else. The `!` is load-bearing: a PID 1 that
/// returned would take the machine's only init down with it.
pub fn run() -> ! {
    // ── 1. Signals ──────────────────────────────────────────────────────
    //
    // A child that exits between the kernel's decision to notify and the
    // parent's `waitpid` is a lost notification unless SIGCHLD is blocked and
    // something reaps unconditionally. That "something" is job 4, and it is
    // only correct if this succeeds.
    if let Err(e) = zrt::signals::block_all() {
        // Announced, not fatal. An init with signals unblocked is a degraded
        // init; an init that exits here is a machine with no init.
        announce_degradation(&format!(
            "could not block signals ({e}); continuing unblocked"
        ));
    }
    // The power signals are the exception: `SIGTERM` and `SIGINT` to PID 1
    // mean "shut the machine down", which is what the kernel's Ctrl-Alt-Del
    // turns into and what an operator at the console expects from
    // `kill -TERM 1`. The handler only sets a flag (signal context allows
    // nothing else); the loop below does the work.
    if let Err(e) = zrt::signals::install_power_handler() {
        announce_degradation(&format!(
            "cannot install the power-signal handler ({e}); the machine will not shut \
             down from a signal"
        ));
    }

    // ── 2. The minimum ──────────────────────────────────────────────────
    //
    // Before the fork, and `/proc` first: `self_path()` below reads
    // `/proc/self/exe`.
    mount_minimum();

    // ── 3 + 4. Start the supervisor, then reap forever ───────────────────
    //
    // Our own path is resolved once, here, because past `fork` this process
    // allocates nothing.
    let exe = self_path();
    let mut sup: Option<i32> = None;
    // Set when another supervisor owns the control socket: this process is
    // then a bare reaper, which is exactly what a machine whose supervisor
    // lives elsewhere needs.
    let mut other_supervisor = false;
    // Two "say each distinct failure once" gates. An init that repeats the
    // same line twenty times a second turns stderr into the bottleneck and
    // buries the boot that actually needed reading.
    let mut last_exit: Option<i32> = None;
    let mut last_errno: Option<i32> = None;

    loop {
        // A missing supervisor is retried on every tick: the two ways to get
        // here (fork failed, supervisor died) are both transient or fixed by
        // the next tick, and neither is a reason to stop reaping.
        if let (None, Some(exe)) = (sup, exe.as_deref()) {
            // The socket belongs to somebody else: spawning another
            // supervisor would only make them fight for it. The loop
            // still falls through to the reaper below — an init that stops
            // reaping is worse than one that supervises nothing.
            if !other_supervisor {
                match spawn_supervisor(exe) {
                    Ok(pid) => sup = Some(pid),
                    Err(e) => announce_once(
                        &mut last_errno,
                        e.raw_os_error().unwrap_or(libc::EINVAL),
                        format!("fork failed ({e}); no supervisor running"),
                    ),
                }
            }
        }

        // A power signal, if one arrived, is acted on before the wait: the
        // machine must not sit in `waitpid` waiting for a child to die.
        if zrt::signals::power_signalled() {
            request_poweroff();
        }

        match (sup, zrt::sys::waitpid_interruptible(-1)) {
            // The supervisor died. Say how, then let the top of the loop
            // relaunch it at once: the wait above already slept until the
            // death, so there is no tick to wait out. Never fatal: a machine
            // without a supervisor is ungoverned, a machine whose init
            // exited is halted.
            (Some(pid), Ok(w)) if w.pid == pid => {
                let code = w.status.code();
                if code == crate::sup::OTHER_SUPERVISOR_EXIT {
                    // Another supervisor owns the control socket: the
                    // machine is governed, and a second one fighting for
                    // the socket would be worse than none. Announce it once
                    // and stop relaunching.
                    announce_once(
                        &mut last_exit,
                        code,
                        format!(
                            "another supervisor already owns the control socket \
                             (pid {pid} stepped aside); not relaunching"
                        ),
                    );
                    sup = None;
                    // Keep reaping, but do not spawn another supervisor.
                    // The flag below says "there is a supervisor, not mine".
                    other_supervisor = true;
                } else {
                    announce_once(
                        &mut last_exit,
                        code,
                        format!("supervisor (pid {pid}) exited with {code}; relaunching"),
                    );
                    sup = None;
                }
            }
            // Any other child of ours — an orphan the kernel re-parented to
            // PID 1 when its parent died. It means nothing here beyond the
            // obligation to reap it, which is why this arm does nothing.
            (_, Ok(_)) => {}
            // Interrupted by a signal: the loop's turn again, and the
            // power check at the top of it sees the flag. Not a failure.
            (_, Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // No children at all (`ECHILD`): bare reaper, or every fork
            // failing. Sleep instead of spinning; the top of the loop
            // retries the spawn past transient fork failures.
            (_, Err(e)) => {
                announce_once(
                    &mut last_errno,
                    e.raw_os_error().unwrap_or(libc::EINVAL),
                    format!("waitpid failed ({e}); reaping is degraded"),
                );
                let _ = zrt::clock::sleep_ms(IDLE_MS);
            }
        }
    }
}

/// Announce `msg` the first time this `value` is seen, and not again until it
/// changes. Both failure paths in [`run`] are reachable on a timer, so
/// "announce every occurrence" is a stderr flood, not a log.
fn announce_once(last: &mut Option<i32>, value: i32, msg: String) {
    if *last != Some(value) {
        announce_degradation(&msg);
        *last = Some(value);
    }
}

/// Mount `/proc`, `/sys` and a tmpfs `/run`.
///
/// Every failure is announced and ignored. An init that dies because a
/// filesystem is already mounted has taken down a machine that was fine; a
/// missing `/proc` instead shows up as one specific, fixable complaint from
/// whatever needed it.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn mount_minimum() {
    use std::path::Path;

    // `nosuid,nodev`: nothing an init puts on a fresh `/run` needs either
    // bit and both cost nothing. `noexec` is deliberately not forced on
    // `/proc` and `/sys` — the kernel mounts those with its own opinions and
    // overriding them buys nothing an init needs.
    // `c_ulong` is 32-bit on i686 and 64-bit on x86_64, so the cast is redundant
    // on the former and load-bearing on the latter. Clippy only sees one side.
    #[allow(clippy::unnecessary_cast)]
    let flags = (libc::MS_NOSUID | libc::MS_NODEV) as u32;

    if let Err(e) = std::fs::create_dir_all("/run") {
        announce_degradation(&format!(
            "cannot create /run ({e}); the tmpfs below will fail too"
        ));
    }

    // (filesystem type, mount point, tmpfs options). `mount` takes the source
    // as a `Path` and the type as a `str`, and for all three the two are the
    // same word — `mount(2)` wants exactly that on Linux.
    const TABLE: &[(&str, &str, Option<&str>)] = &[
        ("proc", "/proc", None),
        ("sysfs", "/sys", None),
        ("tmpfs", "/run", Some("mode=0755")),
        // No systemd means no cgroup2 mount, and without one every
        // `cgroup =` spawn dies with a bare ENOENT deep in the child
        // path. The source word is ignored for cgroup2; the type is not.
        ("cgroup2", "/sys/fs/cgroup", None),
    ];
    for (fstype, target, data) in TABLE.iter().copied() {
        match zrt::sys::mount(
            Some(Path::new(fstype)),
            Path::new(target),
            Some(fstype),
            flags,
            data,
        ) {
            Ok(()) => {}
            // Already mounted is exactly the state we wanted. Asking
            // `/proc/mounts` first would be a second parser to keep correct,
            // for a question the kernel has already answered with an errno.
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {}
            Err(e) => announce_degradation(&format!("cannot mount {target} ({e}); continuing")),
        }
    }

    // `/dev` needs the kernel's device nodes. Without them there is no
    // `/dev/null`, no `/dev/console` and no `/dev/tty1`, so every console
    // service dies at `open` and the machine has no terminal at all.
    //
    // Mounted only when nothing else provided them: an initramfs `/dev`
    // usually carries extra directories (`/dev/pts`, `/dev/shm`,
    // `/dev/mqueue`) that a fresh devtmpfs would hide, and hiding them is
    // worse than the missing nodes.
    if !Path::new("/dev/null").exists() {
        match zrt::sys::mount(
            Some(Path::new("devtmpfs")),
            Path::new("/dev"),
            Some("devtmpfs"),
            flags,
            None,
        ) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {}
            Err(e) => {
                announce_degradation(&format!("cannot mount devtmpfs on /dev ({e}); continuing"))
            }
        }
    }
}

/// Mount `/proc`, `/sys` and a tmpfs `/run`.
///
/// Nothing to do on a platform whose `mount(2)` `zrt` does not wrap: it would
/// only return `Unsupported`, and hiding that behind a silent skip would fail
/// later, far from the cause.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn mount_minimum() {
    announce_degradation("mount(2) is unavailable here; /proc, /sys and /run are left as they are");
}

/// Our own executable path, as a C string, resolved once and before the first
/// fork.
///
/// `None` means this process cannot be relaunched, which leaves `run` as a
/// bare reaper: still PID 1, still reaping, just with nothing to govern.
fn self_path() -> Option<CString> {
    let path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            announce_degradation(&format!(
                "cannot locate my own binary ({e}); running as a bare reaper"
            ));
            return None;
        }
    };
    match CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => Some(c),
        Err(_) => {
            announce_degradation("my own path contains a NUL byte; running as a bare reaper");
            None
        }
    }
}

/// Ask the supervisor to power the machine off.
///
/// The power lives in the supervisor: it is the one that stops every service,
/// writes the breadcrumb and makes the syscall. PID 1 only *asks*, through
/// the same control socket an operator uses, so there is exactly one power
/// path in the machine.
///
/// The socket may not be there yet — the supervisor binds it a moment after
/// it starts — so the request is retried for a while before it is given up
/// on. Giving up is loud: a machine that was told to power off and did not
/// is a machine whose operator is waiting for a screen that never goes dark.
fn request_poweroff() {
    use std::path::PathBuf;
    let path = std::env::var_os(crate::ctl::SOCKET_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(crate::ctl::DEFAULT_SOCKET_PATH));
    request_poweroff_at(&path);
}

/// Ask the supervisor at `path` to power the machine off.
///
/// The path is a parameter so the test can point it at a fake server
/// instead of the real one: the socket path comes from the environment, and
/// a test that changes the environment changes it for the whole process.
fn request_poweroff_at(path: &std::path::Path) {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    // Ten tries, 200 ms apart: two seconds is plenty for a supervisor that
    // is starting, and short enough that the operator is not left waiting.
    for attempt in 0..10 {
        match UnixStream::connect(path) {
            Ok(mut stream) => {
                if stream.write_all(b"C 1 poweroff\n").is_err() {
                    announce_degradation("power signal: cannot write to the control socket");
                    return;
                }
                let mut reply = String::new();
                let _ = BufReader::new(&stream).read_line(&mut reply);
                announce_degradation(&format!(
                    "power signal: the supervisor answered {:?}",
                    reply.trim()
                ));
                return;
            }
            Err(e) => {
                if attempt == 9 {
                    announce_degradation(&format!(
                        "power signal: no control socket at {} ({e}); the machine stays up. \
                         Use `zctl poweroff` once the supervisor is up.",
                        path.display()
                    ));
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}

/// Fork and exec `zinit --sup`. The child's pid, or the fork's error.
///
/// Everything the child touches is built here, in the parent: the binary path
/// and the whole `argv` array. Past `fork` the child runs one syscall and one
/// exit, which is the entire mitigation `DESIGN.md` §9.1 asks for.
fn spawn_supervisor(exe: &CStr) -> io::Result<i32> {
    let argv = zrt::sys::exec_argv(&[exe, c"--sup"]);
    match zrt::sys::fork() {
        // `execvp_or_exit` is the sanctioned single-exit child path: it execs
        // and, on any failure, `_exit(127)`. An absolute `file` means `execvp`
        // performs no `PATH` search, so this is `execv`. It diverges, so this
        // arm can never fall through into the parent's return — the bug
        // `DESIGN.md` §9.1 exists to prevent.
        Ok(0) => zrt::sys::execvp_or_exit(exe, &argv),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    /// The only part of an init that can be tested without becoming one:
    /// `current_exe` must resolve and survive the `CString` conversion,
    /// because every supervisor relaunch goes through it.
    /// The power signal becomes a power verb on the control socket.
    ///
    /// PID 1 does not power the machine off itself: the supervisor owns
    /// that path (stop every service, breadcrumb, sync, syscall), and one
    /// path is easier to reason about than two. The test is a fake server
    /// that reads the frame the init sends.
    #[test]
    fn a_power_signal_becomes_a_power_verb() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("zinit-powertest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("ctl");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind the fake socket");

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut frame = String::new();
            reader.read_line(&mut frame).expect("read the frame");
            frame
        });

        super::request_poweroff_at(&path);
        let frame = server.join().expect("the server thread finished");
        assert_eq!(
            frame, "C 1 poweroff\n",
            "the power signal did not become a power verb"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn own_binary_path_is_resolvable() {
        assert!(
            super::self_path().is_some(),
            "cannot resolve own path: the supervisor could never be relaunched"
        );
    }
}
