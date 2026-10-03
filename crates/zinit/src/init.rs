//! PID 1: `zinit --init`.
//!
//! Four jobs, no more (`DESIGN.md` §3):
//!
//! 1. Block every signal, so nothing can interrupt a `fork` or be lost.
//! 2. Mount the minimum: `/proc`, `/sys`, `/run`.
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

/// Idle tick of the reaper: the upper bound on how long a crashed supervisor
/// stays down, and the cost of not having SIGCHLD-as-data yet.
// ponytail: a 50 ms poll, not a signalfd. Waking 20×/s is cheaper than the
// spare process a dedicated `SIGCHLD` thread would need; the supervisor's
// event loop is where that belongs.
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
        announce_degradation(&format!("could not block signals ({e}); continuing unblocked"));
    }
    // ponytail: power signals (firmware `SIGTERM`) are queued and ignored, not
    // forwarded. Forwarding them means delivering to the supervisor's control
    // socket, which does not exist until a later phase; until then, being
    // unreachable to shutdown is a smaller failure than an init that
    // half-handles them.

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
            match spawn_supervisor(exe) {
                Ok(pid) => sup = Some(pid),
                Err(e) => announce_once(
                    &mut last_errno,
                    e.raw_os_error().unwrap_or(libc::EINVAL),
                    format!("fork failed ({e}); no supervisor running"),
                ),
            }
        }

        match (sup, zrt::sys::waitpid_nohang(-1)) {
            // The supervisor died. Say how, then let the top of the loop
            // relaunch it. Never fatal: a machine without a supervisor is
            // ungoverned, a machine whose init exited is halted.
            (Some(pid), Ok(Some(w))) if w.pid == pid => {
                let code = w.status.code();
                announce_once(
                    &mut last_exit,
                    code,
                    format!("supervisor (pid {pid}) exited with {code}; relaunching"),
                );
                sup = None;
            }
            // Any other child of ours — an orphan the kernel re-parented to
            // PID 1 when its parent died. It means nothing here beyond the
            // obligation to reap it, which is why this arm does nothing.
            (_, Ok(Some(_))) => {}
            // Nothing has exited yet (`WNOHANG` returned 0) or there is no
            // child at all (`ECHILD`, folded into `None` by `zrt`). Both are
            // the normal idle case.
            (_, Ok(None)) => {
                let _ = zrt::clock::sleep_ms(IDLE_MS);
            }
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
    let flags = (libc::MS_NOSUID | libc::MS_NODEV) as u32;

    if let Err(e) = std::fs::create_dir_all("/run") {
        announce_degradation(&format!("cannot create /run ({e}); the tmpfs below will fail too"));
    }

    // (filesystem type, mount point, tmpfs options). `mount` takes the source
    // as a `Path` and the type as a `str`, and for all three the two are the
    // same word — `mount(2)` wants exactly that on Linux.
    const TABLE: &[(&str, &str, Option<&str>)] = &[
        ("proc", "/proc", None),
        ("sysfs", "/sys", None),
        ("tmpfs", "/run", Some("mode=0755")),
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
    #[test]
    fn own_binary_path_is_resolvable() {
        assert!(
            super::self_path().is_some(),
            "cannot resolve own path: the supervisor could never be relaunched"
        );
    }
}
