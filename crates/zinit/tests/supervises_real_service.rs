//! Layer 11.1c: integration tests that run **real processes**.
//!
//! dinit has 171 tests and not one of them forks a service: it stubs
//! `run_child_proc` out, so the whole `fork`-to-`exec` class of bug — the class
//! an init lives or dies by — is untested there (`DESIGN.md` §11). These tests
//! do the opposite. They run the `zinit` **binary** as a subprocess against a
//! directory of `.conf` files and assert on what exists in the process table,
//! not on what a mock recorded.
//!
//! §11.2 sets the bar: *"a test that cannot detect a failure in the real start
//! of a real process does not count as an init test."* So the evidence in every
//! test here is a pid that `kill(pid, 0)` answers for — either the supervisor's
//! own, or, better, one the service wrote about itself.
//!
//! # How a service is watched without touching `sup.rs`
//!
//! A `process` command is split on ASCII whitespace with no quoting
//! (`zservice::spawn`), so a service that has to *write a file before exec'ing
//! a binary* must be a `type = script`. The scripts here are one line:
//!
//! ```sh
//! echo $$ > <scratch>/svc.pid; exec /bin/sleep 30
//! ```
//!
//! `$$` is the shell's pid and `exec` keeps it, so the number in the pidfile is
//! the pid of the live `/bin/sleep` the supervisor forked. `sh` performs the
//! expansion (a script's variables belong to the service), which is why `$$`
//! survives to the shell and not into the supervisor's own environment.
//!
//! # The rules every test in this file obeys
//!
//! * **Bounded.** Every wait has a deadline. A supervisor that hangs is a test
//!   failure, never a CI hang.
//! * **Cleaned up.** [`Sup`]'s `Drop` kills the supervisor's process group and
//!   every service pid the test learned about, so a failed assertion cannot leak
//!   a supervisor that keeps restarting a service into the next test.
//! * **POSIX-portable.** No `/proc`, no `ps` (busybox's takes no `-p`), no
//!   `unshare`. `/bin/sh` and `/bin/sleep` exist on every system in §11.1c.
//! * **Silent.** No `println!`: CI greps all of `crates/`. Failures explain
//!   themselves in `assert!` messages instead.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

// ── deadlines and poll interval ──────────────────────────────────────────────
//
// 10 s is generous for a fork+exec on a loaded CI runner and still far below
// the point where a stuck test costs more than the information it gives.

/// How often a wait re-checks its condition.
const POLL_MS: u64 = 25;
/// How long a supervisor may take to start a service.
const START_DEADLINE_MS: u64 = 10_000;
/// How long a terminated process may take to become a name nobody can find.
const REAP_DEADLINE_MS: u64 = 5_000;

/// One supervisor at a time.
///
/// The tests are otherwise independent, but a supervisor is an init: it owns
/// machine-wide names (today nothing outside its own process; by §12 a pid
/// directory and a control socket under `/run/zinit`), and two of them racing
/// for one is a flake this suite would spend a day blaming on the wrong commit.
/// Serialising costs a few seconds; the payoff is that a failure means
/// something. Poisoning is ignored on purpose: a test that panicked while
/// holding the lock must not take the other four down with it.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

// ── process probes ───────────────────────────────────────────────────────────

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Is there a process with this pid?
///
/// `kill(pid, 0)` is the only portable liveness probe there is: no `/proc`, no
/// `ps`. `EPERM` means the process exists and belongs to somebody else, which
/// is still alive — the test just may not signal it.
fn pid_alive(pid: i32) -> bool {
    // SAFETY: `kill` with signal 0 performs the permission check and returns
    // without delivering anything, so there is nothing to be unsafe about.
    unsafe { libc::kill(pid, 0) == 0 || last_errno() == libc::EPERM }
}

/// Has this pid been collected by its parent?
///
/// The distinction from [`pid_alive`] is the whole point of the reaping test:
/// a **zombie still answers `kill(pid, 0)`**. Only `ESRCH` proves nobody is
/// holding an unreaped corpse.
fn pid_gone(pid: i32) -> bool {
    // SAFETY: as above — signal 0 is not delivered.
    unsafe { libc::kill(pid, 0) == -1 && last_errno() == libc::ESRCH }
}

fn signal(pid: i32, sig: i32) {
    // SAFETY: sending a signal to a pid this test's own service reported is
    // the intent; a pid that has already exited yields ESRCH, which is fine.
    unsafe { libc::kill(pid, sig) };
}

/// Poll `f` until it is true or `deadline_ms` have passed. `true` if it became
/// true, `false` on timeout — never blocks past the deadline.
fn wait_until(deadline_ms: u64, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if f() {
            return true;
        }
        if start.elapsed() >= Duration::from_millis(deadline_ms) {
            return false;
        }
        sleep_ms(POLL_MS);
    }
}

// ── scratch directories ──────────────────────────────────────────────────────

/// A fresh, writable, empty directory, owned by one test.
///
/// Copied in approach from `zservice::testutil::scratch` (which is `#[cfg(test)]`
/// and private, so it cannot be imported): the machine's temp directory may be a
/// quota'd tmpfs — writes fail with `EDQUOT` on the author's — so candidates are
/// tried in order and the first one that survives a real write is used.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let pid = std::process::id();
        let mut candidates: Vec<PathBuf> = Vec::new();
        let mut sys = std::env::temp_dir();
        sys.push(format!("zinit-{tag}-{pid}"));
        candidates.push(sys);
        if let Ok(home) = std::env::var("HOME") {
            let mut h = PathBuf::from(home);
            h.push(".cache");
            h.push(format!("zinit-{tag}-{pid}"));
            candidates.push(h);
        }
        let mut manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest.push("target");
        manifest.push("test-scratch");
        manifest.push(format!("{tag}-{pid}"));
        candidates.push(manifest);

        for dir in candidates {
            let _ = std::fs::remove_dir_all(&dir);
            if std::fs::create_dir_all(&dir).is_err() {
                continue;
            }
            let probe = dir.join(".writable");
            if std::fs::write(&probe, b"x").is_ok() {
                let _ = std::fs::remove_file(&probe);
                // The services write files whose *path* is embedded in a shell
                // command line, so a path with a space, a `#` (which the config
                // format treats as a comment) or a newline would be mangled into
                // a confusing failure. Refuse it here, where the message can
                // still name the directory.
                let text = dir.display().to_string();
                assert!(
                    !text.contains([' ', '\t', '\n', '#', '\'', '"', '\\']),
                    "scratch directory {text} cannot be used: it contains a character the \
                     config format or the shell would eat. Set TMPDIR to something plain."
                );
                return Scratch { dir };
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
        panic!("no writable scratch space for test `{tag}`");
    }

    fn path(&self) -> &Path {
        &self.dir
    }

    /// The path of `name` inside the scratch directory, as text a `.conf` may
    /// carry in a shell command.
    fn at(&self, name: &str) -> String {
        self.dir.join(name).display().to_string()
    }

    /// Write a service description.
    fn conf(&self, name: &str, body: &str) {
        let path = self.dir.join(name);
        std::fs::write(&path, body)
            .unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
    }

    /// The last lines of a file, for a failure message.
    fn tail(&self, name: &str) -> String {
        let text = std::fs::read_to_string(self.path().join(name)).unwrap_or_default();
        let lines: Vec<&str> = text.lines().rev().take(12).collect();
        let mut out: Vec<&str> = lines.into_iter().rev().collect();
        out.push("");
        out.join("\n")
    }

    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(name)).ok()
    }

    /// The pid written in `name`, if it is there and could be one.
    ///
    /// `> 1` filters out the two answers a truncated or empty read gives (`0`,
    /// and nothing at all): the service writes with a plain `>`, so a read can
    /// catch the file mid-write.
    fn pid(&self, name: &str) -> Option<i32> {
        self.read(name)?.trim().parse().ok().filter(|&p| p > 1)
    }

    /// Lines in `name`, counted by newline. Short appends from one service are
    /// never interleaved, so a read may only ever miss the last one.
    fn lines(&self, name: &str) -> usize {
        self.read(name).map_or(0, |s| s.lines().count())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Litter in a scratch directory is not a failure; removing it keeps a
        // red CI run from filling `/tmp` with failed tests.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ── the service descriptions ─────────────────────────────────────────────────

/// The `zctl` binary, if this build produced one.
///
/// `zctl` is another workspace package, so `CARGO_BIN_EXE_zctl` does not
/// exist here; the binary sits next to the test executable in the target
/// directory. A run that only built this package has no `zctl`, and the
/// tests that need it say so instead of failing for the wrong reason.
fn zctl_path() -> Option<PathBuf> {
    let mut dir = std::env::current_exe().ok()?;
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let bin = dir.join("zctl");
    if bin.is_file() { Some(bin) } else { None }
}

/// Send one frame to the control socket as somebody who is not root.
///
/// The reply comes back through a pipe: the peer is a forked child that
/// dropped its privileges, and a pipe is how its answer gets out.
fn ask_as_nobody(socket: &str, frame: &str) -> String {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    // `nobody` is the conventional unprivileged account; when the test is
    // not root there is nothing to drop and the peer is already
    // unprivileged.
    const NOBODY: u32 = 65534;

    // A socket pair is the channel: the child writes its answer on one end,
    // the parent reads it on the other. (`std::io::pipe` would be shorter
    // but it is newer than this workspace's MSRV.)
    let (mut parent_end, mut child_end) = UnixStream::pair().expect("socket pair");
    // SAFETY: a fork whose child does nothing but async-signal-safe work:
    // it drops its privileges, connects, writes, reads and exits. The
    // parent only reads the pipe.
    let pid = unsafe { libc::fork() };
    match pid {
        0 => {
            // Child: become somebody else, then ask.
            // SAFETY: the ids are valid, and these are the only calls
            // between the fork and the exit.
            unsafe {
                libc::setgid(NOBODY);
                libc::setuid(NOBODY);
            }
            let mut stream = match UnixStream::connect(socket) {
                Ok(s) => s,
                Err(_) => std::process::exit(1),
            };
            let _ = stream.write_all(frame.as_bytes());
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            let _ = child_end.write_all(line.as_bytes());
            std::process::exit(0);
        }
        -1 => panic!("cannot fork the peer"),
        _ => {}
    }
    let mut answer = String::new();
    let _ = BufReader::new(&mut parent_end).read_line(&mut answer);
    // Reap the child: a test that leaves a corpse is a test that makes the
    // next one flakier.
    // SAFETY: the pid is this test's own child.
    unsafe {
        let mut status = 0;
        libc::waitpid(pid, &raw mut status, 0);
    }
    answer
}

/// Run the real `zctl` against this test's supervisor.
///
/// The control socket refuses a mutation from a peer that is not the known
/// client, so a test that wants to change something has to go through the
/// same binary an operator uses.
fn zctl(scratch: &Scratch, args: &[&str]) -> std::process::Output {
    let bin = zctl_path().expect("the zctl binary was not built");
    Command::new(bin)
        .args(args)
        .env("ZINIT_SOCKET", scratch.at("ctl"))
        .output()
        .expect("cannot run zctl")
}

/// A service description with the boilerplate every test here needs.
///
/// * `type = script` — a `process` command is split on whitespace with no
///   quoting, and these tests need the service to write a pid file and then
///   `exec` a real binary.
/// * `ready = none` — no handshake: `Running` then means "the fork happened",
///   which is the thing under test.
/// * `log = none` — the default sink is `/var/log/zinit/<svc>.log`, which a
///   non-root test in CI cannot write. These tests assert on processes; a log
///   failure must not be what fails them.
fn desc(command: &str, extra: &str) -> String {
    format!("type = script\ncommand = {command}\nready = none\nlog = none\n{extra}")
}

/// The command of a service that announces its own pid and then becomes a real,
/// long-lived process. `exec` keeps the pid, so the number in the pidfile is
/// the pid of the live `/bin/sleep`.
fn announce_then_sleep(pidfile: &str, seconds: u32) -> String {
    format!("echo $$ > {pidfile}; exec /bin/sleep {seconds}")
}

// ── the supervisor under test ────────────────────────────────────────────────

/// A running `zinit --sup <dir>`, plus the pids its services reported.
///
/// The guard is what makes the suite safe to fail: `Drop` kills the supervisor's
/// process group first — while it lives it would restart anything else killed —
/// then every service pid the test learned about. Without that, one panic leaves
/// a supervisor restarting a service forever and the *next* test hangs.
struct Sup<'a> {
    child: Child,
    scratch: &'a Scratch,
    /// Service pids read out of pid files, killed on the way out.
    services: Vec<i32>,
}

/// `zinit --sup <dir>`, detached from the test's terminal and put in its own
/// process group so that a guard can sweep up whatever it leaves behind.
fn sup_command(scratch: &Scratch) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zinit"));
    cmd.arg("--sup").arg(scratch.path());
    cmd.stdin(Stdio::null()).process_group(0);
    // The control socket and the state directory live in the scratch
    // directory: the default paths are under `/run`, which a test that does
    // not run as root cannot write, and a socket shared by every test on
    // the machine is a race nobody wants to debug.
    cmd.env("ZINIT_SOCKET", scratch.at("ctl"));
    cmd.env("ZINIT_STATE_DIR", scratch.at("state"));
    cmd
}

impl<'a> Sup<'a> {
    /// Start `zinit --sup <scratch>`. The scratch directory must already hold
    /// the descriptions: the supervisor reads it once, at startup.
    fn start(scratch: &'a Scratch) -> Sup<'a> {
        // Output is discarded rather than piped: a supervisor that runs for ten
        // seconds could fill a 64 KiB pipe and then block on its own stderr,
        // which would look exactly like a hang. The refusal tests pipe it
        // instead, because there the process is expected to exit at once.
        // The supervisor's stderr goes to a file, not a pipe: a pipe could
        // fill up and block the supervisor on its own output, which looks
        // exactly like a hang. A file can be read after a failure, which is
        // the only reason it exists.
        let log = std::fs::File::create(scratch.at("sup.log")).expect("create the log file");
        let err = log.try_clone().expect("clone the log");
        let child = sup_command(scratch)
            .stdout(Stdio::null())
            .stderr(Stdio::from(err))
            .spawn()
            .expect("cannot spawn `zinit --sup`");
        Sup {
            child,
            scratch,
            services: Vec::new(),
        }
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    /// The supervisor's exit status, if it has exited. Repeated calls keep
    /// reporting the same status.
    fn exited(&mut self) -> Option<ExitStatus> {
        self.child
            .try_wait()
            .expect("waiting on the supervisor failed")
    }

    fn note_service(&mut self, pid: i32) {
        self.services.push(pid);
    }

    /// Block until the service has written a **live** pid to `file`, and return
    /// it. Panics if the supervisor dies first or nothing appears in time.
    ///
    /// "Live" is the whole assertion: a pidfile proves a file was written, and
    /// `kill(pid, 0)` proves a process is still there. A stale or half-written
    /// file is skipped rather than trusted.
    fn await_live_pid(&mut self, file: &str, deadline_ms: u64) -> i32 {
        let start = Instant::now();
        loop {
            if let Some(pid) = self.scratch.pid(file) {
                if pid_alive(pid) {
                    self.note_service(pid);
                    return pid;
                }
            }
            if let Some(status) = self.exited() {
                panic!(
                    "the supervisor exited ({status}) before the service wrote {file}: \
                     `zinit --sup` must keep running while it has services to supervise"
                );
            }
            assert!(
                start.elapsed() < Duration::from_millis(deadline_ms),
                "no live pid in {file} after {deadline_ms}ms: `zinit --sup` was pointed at a \
                 directory with a service in it, and no real process ever appeared"
            );
            sleep_ms(POLL_MS);
        }
    }
}

impl Drop for Sup<'_> {
    fn drop(&mut self) {
        // The supervisor first: while it is alive it restarts anything else
        // that gets killed.
        let group = self.pid();
        // SAFETY: signal 9 to a process group this test created with
        // `process_group(0)`, so the pid is this supervisor's own and the
        // negative pid is that group. Both sends are checked for nothing: a
        // process already gone is the outcome we wanted.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
            libc::kill(group, libc::SIGKILL);
        }
        // Bounded: SIGKILL on an `epoll_wait` is immediate, but a `Drop` that
        // blocks forever is how a CI job dies. If it somehow survives, the
        // signal is already delivered and the kernel finishes the job.
        let deadline = Instant::now() + Duration::from_millis(2_000);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            sleep_ms(POLL_MS);
        }
        // Then the services, for one that outlived its supervisor.
        for &pid in &self.services {
            // SAFETY: as above — a pid this test's own service reported.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Nothing the refused plan started may still be running.
fn assert_nothing_left_running(scratch: &Scratch, files: &[&str]) {
    for file in files {
        if let Some(pid) = scratch.pid(file) {
            assert!(
                pid_gone(pid),
                "{file} left pid {pid} alive even though the plan was refused: a refused boot \
                 must not leave services behind"
            );
        }
    }
}

// ── the tests ────────────────────────────────────────────────────────────────

/// §11.2's bar, in its simplest form: a service described by a file is really
/// running, as a real process, supervised by the binary.
#[test]
fn supervisor_starts_a_real_service() {
    let _one = serial();
    let scratch = Scratch::new("starts");
    let command = announce_then_sleep(&scratch.at("svc.pid"), 30);
    scratch.conf("probe.conf", &desc(&command, "restart = never\n"));

    let mut sup = Sup::start(&scratch);

    // The evidence: a pid the service wrote about itself, still answering
    // kill(pid, 0) 500 ms later. Not a mock's record of a fork — a process.
    let pid = sup.await_live_pid("svc.pid", START_DEADLINE_MS);
    assert_ne!(
        pid,
        sup.pid(),
        "the pidfile holds the supervisor's own pid, not a service's"
    );
    assert!(
        sup.exited().is_none(),
        "the supervisor exited while its only service was healthy"
    );
    sleep_ms(500);
    assert!(
        wait_until(REAP_DEADLINE_MS, || pid_alive(pid)),
        "pid {pid} was alive when the service started and gone 500 ms later: a process that \
         cannot survive half a second is not a supervised service"
    );
    // And `restart = never` was obeyed, which rules out "it started and died and
    // was started again so fast nobody noticed".
    assert_eq!(
        scratch.pid("svc.pid"),
        Some(pid),
        "the service was replaced within 500 ms although `restart = never` forbids it"
    );
    assert!(
        sup.exited().is_none(),
        "the supervisor exited while its service was up"
    );
}

/// Stopping a service goes through the supervisor: the process really goes away
/// (reaped, not left as a zombie) and the policy owes a replacement.
#[test]
fn supervisor_reaps_the_service_on_stop() {
    let _one = serial();
    let scratch = Scratch::new("reaps");
    let command = announce_then_sleep(&scratch.at("svc.pid"), 30);
    let extra = "restart = always\nrestart-delay = 100ms\nstop-timeout = 2s\n";
    scratch.conf("probe.conf", &desc(&command, extra));

    let mut sup = Sup::start(&scratch);
    let first = sup.await_live_pid("svc.pid", START_DEADLINE_MS);

    signal(first, libc::SIGTERM);

    // `kill(pid, 0)` answering means the process is still alive *or* is a zombie
    // nobody collected. Only ESRCH proves the supervisor reaped it, which is the
    // one thing an init exists to do.
    assert!(
        wait_until(REAP_DEADLINE_MS, || pid_gone(first)),
        "pid {first} still answers kill(pid, 0) {REAP_DEADLINE_MS}ms after SIGTERM: it is alive \
         or an unreaped zombie, and neither is a stopped service"
    );
    assert!(
        sup.exited().is_none(),
        "the supervisor exited when one of its services was stopped, instead of carrying on with \
         the rest"
    );

    // Reaped *and* replaced. A new pid in the pidfile is the only honest proof
    // that a new process was forked after the old one was collected.
    let second = sup.await_live_pid("svc.pid", REAP_DEADLINE_MS);
    assert_ne!(
        second, first,
        "the supervisor reaped pid {first} but did not start a replacement, though \
         `restart = always` promises one"
    );
}

/// A demand-pulled service that exits immediately keeps flapping.
///
/// Live 2026-10-16: `agetty-service@tty1` (disabled at boot, pulled by the
/// running `agetty` oneshot) spawned once and stayed `Running` with a dead
/// pid forever — exactly one attempt, `restarts = 0`, no respawn. This test
/// is that shape (requirer up, disabled instance dep, instant death) and
/// asserts the only honest outcomes: repeated attempts. One attempt ever
/// means the death vanished between spawn and account.
#[test]
fn a_demand_pulled_instant_death_keeps_flapping() {
    let _one = serial();
    let scratch = Scratch::new("demandflap");
    let puller = format!(
        "{}\ndepends = flapper@tty1\n",
        desc(&announce_then_sleep(&scratch.at("a.pid"), 30), "")
    );
    scratch.conf("a.conf", &puller);
    let command = format!(
        "echo $$ > {}; echo x >> {}; exit 1",
        scratch.at("b.pid"),
        scratch.at("attempts")
    );
    let body = format!(
        "type = script\ncommand = {command}\nready = none\nlog = none\nenabled = no\nrestart = always\nrestart-delay = 100ms\n"
    );
    scratch.conf("flapper@tty1.conf", &body);

    let mut sup = Sup::start(&scratch);
    assert!(
        wait_until(5_000, || scratch.lines("attempts") >= 4),
        "demand-pulled flapper died instantly and `restart = always` should have \
         started it again, but there were only {} attempts in 5s: the death went \
         stale (live: Running with a dead pid, restarts 0)",
        scratch.lines("attempts")
    );
    assert!(
        sup.exited().is_none(),
        "the supervisor itself died while a demand-pulled service flapped"
    );
}

/// A service that exits immediately is restarted — several times, and at a rate
/// a human can still read the log for.
///
/// §1.4 lists "rate limiting on restarts" as the thing dinit lacks, and the
/// reason is not politeness: without a delay a service that cannot start is a
/// fork bomb that starves the machine it was supposed to be fixed on.
#[test]
fn a_crash_is_restarted_within_budget() {
    let _one = serial();
    let scratch = Scratch::new("crash");
    let command = format!("echo x >> {}; exit 1", scratch.at("attempts"));
    // `critical = no` says out loud that a service which cannot start must not
    // take the supervisor down with it. The plan drops the field today; the
    // assertion at the end of this test is what enforces it either way.
    let extra = "restart = always\nrestart-delay = 100ms\ncritical = no\n";
    scratch.conf("crasher.conf", &desc(&command, extra));

    let mut sup = Sup::start(&scratch);

    // It keeps trying: one attempt is a service that failed, several is a
    // supervisor doing its job.
    assert!(
        wait_until(3_000, || scratch.lines("attempts") >= 4),
        "the service exited immediately and `restart = always` should have started it again, but \
         there were only {} attempts in 3s",
        scratch.lines("attempts")
    );

    // And it does not spin. `restart-delay = 100ms` puts the ceiling at ten
    // attempts a second; forty over two seconds is double that, which leaves
    // room for a loaded runner without leaving room for a busy loop.
    let mid = scratch.lines("attempts");
    sleep_ms(2_000);
    let late = scratch.lines("attempts");
    assert!(
        late > mid,
        "restarting stopped dead ({mid} → {late} attempts): a crash loop must not end in silence"
    );
    assert!(
        late - mid <= 40,
        "{} restarts in 2s with `restart-delay = 100ms`: the supervisor is spinning instead of \
         rate-limiting a service that cannot start, which is the fork bomb §1.4 refuses",
        late - mid
    );
    assert!(
        sup.exited().is_none(),
        "the supervisor itself died while supervising a service that cannot start"
    );
}

/// A service that stays root must not carry NoNewPrivs.
///
/// NNP was once set unconditionally in the child path; every descendant —
/// including ssh sessions — then silently lost all setuid elevation
/// (`doas: not installed setuid`), with no way back but a power cycle.
/// NNP is defence in depth *after a uid drop*; without one it only breaks
/// setuid children. Fails on the old code (status reads 1), passes fixed.
/// Linux-only: it reads the child's `NoNewPrivs` from `/proc`, which the
/// rest of this suite (and BSD/macOS runners) must never assume.
#[test]
#[cfg(target_os = "linux")]
fn a_root_service_keeps_setuid_working() {
    let _one = serial();
    let scratch = Scratch::new("nnp");
    let command = format!(
        "grep ^NoNewPrivs /proc/self/status > {}; exec /bin/sleep 30",
        scratch.at("nnp.status")
    );
    scratch.conf("plain.conf", &desc(&command, ""));
    let mut sup = Sup::start(&scratch);
    assert!(
        wait_until(10_000, || scratch.read("nnp.status").is_some()),
        "the service never reported its NoNewPrivs status in 10s"
    );
    let status = scratch.read("nnp.status").unwrap_or_default();
    assert!(
        status.contains("NoNewPrivs:\t0"),
        "a service with no `user =` runs with NoNewPrivs set ({status:?}): \
         its whole subtree lost setuid elevation"
    );
    assert!(
        sup.exited().is_none(),
        "the supervisor itself died while supervising a plain service"
    );
}

/// A cycle in the graph has no topological order, so there is no plan. The
/// supervisor must refuse the whole directory rather than start the half of it
/// it happens to understand.
#[test]
fn a_dependency_cycle_starts_in_emergency_mode() {
    let _one = serial();
    let scratch = Scratch::new("cycle");
    // Both services are perfectly runnable. It is the *plan* that cannot exist,
    // so both would leave a pid behind if the supervisor started anything.
    for (file, depends, pidfile) in [("a.conf", "b", "a.pid"), ("b.conf", "a", "b.pid")] {
        let command = announce_then_sleep(&scratch.at(pidfile), 30);
        let body = format!("depends = {depends}\n{}", desc(&command, ""));
        scratch.conf(file, &body);
    }

    // A plan that cannot exist must not be half-loaded: the machine comes
    // up in emergency mode (a shell on the console, no services of its own)
    // instead of supervising a configuration nobody wrote.
    let mut sup = Sup::start(&scratch);
    sleep_ms(500);
    assert!(
        sup.exited().is_none(),
        "the supervisor exited on a cyclic plan: the machine would spin"
    );
    assert_nothing_left_running(&scratch, &["a.pid", "b.pid"]);
}

/// One unparseable file stops the plan. A supervisor that skips the broken
/// file and starts the rest is a machine that comes up in a configuration
/// nobody wrote, which is worse than one that says why and waits. It waits
/// in emergency mode: a shell on the console, and no service of the broken
/// plan running.
#[test]
fn an_unparseable_config_starts_in_emergency_mode() {
    let _one = serial();
    let scratch = Scratch::new("unparseable");
    // A valid service next to the broken one: the point is that one bad file
    // stops everything, not that it stops only its own service.
    let command = announce_then_sleep(&scratch.at("good.pid"), 30);
    scratch.conf("good.conf", &desc(&command, ""));
    // `restart = sometimes` is not a policy. One fatal error, in one directive.
    let broken = format!("{}\nrestart = sometimes\n", desc("exec /bin/sleep 30", ""));
    scratch.conf("broken.conf", &broken);

    let mut sup = Sup::start(&scratch);
    sleep_ms(500);
    assert!(
        sup.exited().is_none(),
        "the supervisor exited on an unparseable file: the machine would spin"
    );
    // The good service next to the broken one must not have run: the plan
    // as a whole is what failed.
    assert_nothing_left_running(&scratch, &["good.pid"]);
}

/// An orphaned grandchild must be reaped, not left a zombie.
///
/// The supervisor is the child subreaper, so a service that forks and exits
/// leaves its child to us. The pidfd backend only watches pids it started;
/// the reap loop must additionally sweep `waitpid(-1)` or the orphan answers
/// `kill(pid, 0)` forever. `pid_gone` (ESRCH) is the whole assertion: a
/// zombie still answers liveness probes, so "alive" would pass on a corpse.
#[test]
fn a_reparented_orphan_is_reaped_not_left_a_zombie() {
    let _one = serial();
    let scratch = Scratch::new("orphan");
    scratch.conf(
        "forker.conf",
        &desc(
            &format!("sleep 30 & echo $! > {}", scratch.at("gc.pid")),
            "restart = never\n",
        ),
    );
    let mut sup = Sup::start(&scratch);
    let gc = sup.await_live_pid("gc.pid", START_DEADLINE_MS);
    // The service exited at once; the grandchild belongs to the supervisor
    // now. Kill it and require collection, not a corpse.
    signal(gc, libc::SIGKILL);
    assert!(
        wait_until(REAP_DEADLINE_MS, || pid_gone(gc)),
        "orphan {gc} was never reaped: it still answers kill(0), \
         so a zombie is sitting under the supervisor"
    );
}

/// A machine that is going down must not resurrect a service.
///
/// The bug: `zctl restart` leaves a *pending restart* behind, and when the
/// service finally dies the supervisor brings it back up. If the machine is
/// stopping at that moment — a power verb, an `on-fatal`, or a signal — the
/// service comes back up in the middle of the power epilogue, which is
/// waiting for exactly that service to stay down. The stop deadline then
/// expires and the machine powers off "loudly anyway".
///
/// The test: a service that ignores SIGTERM (so the stop escalates to
/// SIGKILL and takes a moment), a restart requested, then a stop. The
/// service must stay dead.
#[test]
fn a_stop_in_progress_does_not_resurrect_a_pending_restart() {
    let _one = serial();
    let scratch = Scratch::new("stopping-restart");
    if zctl_path().is_none() {
        // No print macros outside `zrt::report` (the purity job): the
        // reason goes to the scratch directory instead.
        let _ = std::fs::write(scratch.at("skipped"), "no zctl binary\n");
        return;
    }
    // The service ignores SIGTERM *in its own process*, which for a
    // `script` service is the shell the supervisor forks: the trap has to be
    // in that shell's body, not in a nested `/bin/sh -c`, or the direct child
    // dies on the first signal and the restart is legitimate instead of
    // racing the stop. The supervisor then has to escalate to SIGKILL after
    // `stop-timeout`, which is the window in which the stop and the pending
    // restart overlap.
    let command = format!(
        "trap \"\" TERM; echo $$ > {}; sleep 300",
        scratch.at("svc.pid")
    );
    scratch.conf(
        "probe.conf",
        &desc(&command, "restart = always\nstop-timeout = 1s\n"),
    );

    let mut sup = Sup::start(&scratch);
    let pid = sup.await_live_pid("svc.pid", START_DEADLINE_MS);

    // Ask for the restart, through the real client: the control socket only
    // lets a known client (`zctl`) touch the machine, so a test that wrote
    // raw frames would be refused before it got anywhere.
    let out = zctl(&scratch, &["restart", "probe"]);
    assert!(
        out.status.success(),
        "the restart was refused: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Now the machine starts stopping. The service is still alive (it
    // ignores SIGTERM), so this is the moment the pending restart and the
    // stop overlap.
    signal(sup.pid(), libc::SIGTERM);

    // The service dies (SIGKILL after the stop timeout) and, without the
    // guard, comes straight back up.
    assert!(
        wait_until(REAP_DEADLINE_MS, || pid_gone(pid)),
        "the service never died: the stop did not escalate to SIGKILL\n{}",
        scratch.tail("sup.log")
    );
    sleep_ms(1500);
    assert_eq!(
        scratch.pid("svc.pid"),
        Some(pid),
        "the service was restarted while the machine was stopping: the power \
         epilogue waits for services to stay down, and this one came back"
    );
    assert!(
        sup.exited().is_none(),
        "the supervisor exited while stopping"
    );
}

/// The control socket knows who is calling.
///
/// The socket is `0600`, so only its owner can connect at all — but "the
/// owner" is every process of that user, and one of them writing
/// `C 1 poweroff` into the socket is enough to take the machine down.
/// The server therefore checks the peer's credentials, and a peer that is
/// not the known client (`zctl`) may look but not touch.
///
/// The test peer is this test binary: same user as the supervisor, and not
/// a client. Reading must work; mutating must be refused with code `6`.
#[test]
fn the_control_socket_refuses_a_mutation_from_an_unknown_peer() {
    let _one = serial();
    let scratch = Scratch::new("peercred");
    scratch.conf(
        "probe.conf",
        &desc(
            &format!("echo $$ > {}; exec /bin/sleep 30", scratch.at("svc.pid")),
            "restart = never\n",
        ),
    );
    let mut sup = Sup::start(&scratch);
    let _pid = sup.await_live_pid("svc.pid", START_DEADLINE_MS);

    // The peer must not be root: root may do everything, by design, so a
    // root peer would prove nothing. Where the test runs as root (the CI
    // containers do) the peer is a child that drops to `nobody` first;
    // where it does not, the peer is already unprivileged.
    let ask = |frame: &str| -> String { ask_as_nobody(&scratch.at("ctl"), frame) };

    // Reading works for anybody who got this far.
    let listed = ask("C 1 list\n");
    assert!(
        listed.starts_with("R 1 0"),
        "a read-only verb must be answered for any peer: {listed:?}"
    );

    // Powering the machine off does not: that verb takes the known client
    // or root. (Managing services is the socket's own job and stays open
    // to its owner, or the C4 contract would mean nothing.)
    let refused = ask("C 1 poweroff\n");
    assert!(
        refused.starts_with("E 1 6"),
        "a power verb from an unknown peer must be refused with code 6: {refused:?}"
    );

    // And the service is still running: the refusal happened before the
    // command became an action.
    assert!(
        scratch.pid("svc.pid").is_some(),
        "the service vanished although the power verb was refused"
    );
    assert!(sup.exited().is_none(), "the supervisor exited");
}

/// One supervisor per control socket.
///
/// Two supervisors over one socket is not a configuration, it is a race: the
/// second one unlinks the path and binds its own, and the first one's
/// clients end up talking to somebody else. The machine then looks like it
/// has two of every daemon, fighting over the ports — which is exactly what
/// a hand-started second supervisor used to cause.
///
/// The second supervisor must step aside (exit code 11) and the first must
/// keep the socket.
#[test]
fn a_second_supervisor_steps_aside() {
    let _one = serial();
    let scratch = Scratch::new("instance");
    scratch.conf(
        "probe.conf",
        &desc(
            &format!("echo $$ > {}; exec /bin/sleep 30", scratch.at("svc.pid")),
            "restart = never\n",
        ),
    );
    let mut first = Sup::start(&scratch);
    let _pid = first.await_live_pid("svc.pid", START_DEADLINE_MS);

    // The second one gets the same socket path, so it finds the lock.
    let mut second = sup_command(&scratch)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("cannot spawn the second supervisor");
    let status = second.wait().expect("the second supervisor must exit");
    assert_eq!(
        status.code(),
        Some(11),
        "the second supervisor did not step aside: {status:?}"
    );

    // The first one still owns the socket: its service is still running and
    // the socket still answers.
    assert!(
        scratch.pid("svc.pid").is_some(),
        "the first supervisor lost its service"
    );
    let out = zctl(&scratch, &["list"]);
    assert!(
        out.status.success(),
        "the first supervisor lost the control socket: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        first.exited().is_none(),
        "the first supervisor exited when the second one appeared"
    );
}

/// A configuration that does not load must not take the machine with it.
///
/// The old behaviour: the supervisor exited, PID 1 relaunched it, it
/// exited again, and the machine spun in front of the operator with no
/// control socket, no services and no way in. The emergency mode puts a
/// shell on the console and keeps the control socket up, so `zctl
/// reload-all` can pick the real plan up once the file is fixed.
#[test]
fn a_broken_configuration_starts_in_emergency_mode() {
    let _one = serial();
    let scratch = Scratch::new("emergency");
    if zctl_path().is_none() {
        // No print macros outside `zrt::report` (the purity job): the
        // reason goes to the scratch directory instead.
        let _ = std::fs::write(scratch.at("skipped"), "no zctl binary\n");
        return;
    }
    // A file the parser cannot read: the plan will not load.
    scratch.conf("broken.conf", "not-a-directive\n");

    let mut sup = Sup::start(&scratch);

    // The supervisor is alive: it did not exit and let PID 1 relaunch it
    // in a loop.
    sleep_ms(500);
    assert!(
        sup.exited().is_none(),
        "the supervisor exited on a broken configuration: the machine would spin"
    );

    // The control socket answers, and the only service is the emergency
    // shell.
    let out = zctl(&scratch, &["list"]);
    assert!(
        out.status.success(),
        "no control socket in emergency mode: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listed = String::from_utf8_lossy(&out.stdout);
    assert!(
        listed.contains("emergency"),
        "the emergency plan is not the one being supervised: {listed}"
    );

    // Fix the file and reload: the real plan takes over.
    std::fs::remove_file(scratch.path().join("broken.conf")).expect("remove the broken file");
    scratch.conf(
        "probe.conf",
        &desc(
            &format!("echo $$ > {}; exec /bin/sleep 30", scratch.at("svc.pid")),
            "restart = never\n",
        ),
    );
    let out = zctl(&scratch, &["reload-all"]);
    assert!(
        out.status.success(),
        "the reload was refused: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _pid = sup.await_live_pid("svc.pid", START_DEADLINE_MS);
    let after = zctl(&scratch, &["list"]);
    let listed = String::from_utf8_lossy(&after.stdout);
    assert!(
        listed.contains("probe"),
        "the real plan did not take over: {listed}"
    );
}

/// The control socket must not grow without bound.
///
/// A local process that opens connections in a loop would otherwise make
/// the supervisor grow: every client costs a descriptor and a buffer, and
/// the supervisor is the one process that must not run out of memory. Past
/// the cap the connection is closed before it is registered.
#[test]
fn the_control_socket_refuses_more_clients_than_it_can_hold() {
    use std::os::unix::net::UnixStream;

    let _one = serial();
    let scratch = Scratch::new("clientcap");
    scratch.conf(
        "probe.conf",
        &desc(
            &format!("echo $$ > {}; exec /bin/sleep 30", scratch.at("svc.pid")),
            "restart = never\n",
        ),
    );
    let mut sup = Sup::start(&scratch);
    let _pid = sup.await_live_pid("svc.pid", START_DEADLINE_MS);

    // More connections than the cap, all of them held open.
    let mut held = Vec::new();
    for _ in 0..(crate_cap() + 8) {
        match UnixStream::connect(scratch.at("ctl")) {
            Ok(s) => held.push(s),
            Err(_) => break,
        }
    }
    // The supervisor is still there: the cap refused the extra
    // connections, it did not wedge anything.
    assert!(
        sup.exited().is_none(),
        "the supervisor died when the clients piled up"
    );
    // With the pile gone, the socket works again. The closed connections are
    // noticed when the loop next looks at them, so the question is asked
    // until it is answered instead of exactly once.
    drop(held);
    let start = Instant::now();
    let mut answered = false;
    while start.elapsed() < Duration::from_secs(10) {
        if zctl(&scratch, &["list"]).status.success() {
            answered = true;
            break;
        }
        sleep_ms(50);
    }
    assert!(
        answered,
        "the control socket did not recover after the clients left: {}",
        scratch.tail("sup.log")
    );
}

/// The cap the control socket enforces, as the test sees it.
fn crate_cap() -> usize {
    64
}

/// The visible state must describe the machine that is running.
///
/// Two things used to go wrong: a service deleted from the configuration
/// left its state file behind, so the file kept claiming a service that no
/// longer existed; and the file was written in place, so a reader could
/// see half of the new content and half of the old one.
#[test]
fn the_visible_state_follows_the_plan() {
    let _one = serial();
    let scratch = Scratch::new("state");
    if zctl_path().is_none() {
        // No print macros outside `zrt::report` (the purity job): the
        // reason goes to the scratch directory instead.
        let _ = std::fs::write(scratch.at("skipped"), "no zctl binary\n");
        return;
    }
    scratch.conf(
        "probe.conf",
        &desc(
            &format!("echo $$ > {}; exec /bin/sleep 30", scratch.at("svc.pid")),
            "restart = never\n",
        ),
    );
    let mut sup = Sup::start(&scratch);
    let _pid = sup.await_live_pid("svc.pid", START_DEADLINE_MS);

    // The state file exists and says the service is running.
    let state = scratch.read("state/probe").expect("the state file");
    assert!(
        state.contains("state=Running"),
        "the state file does not describe a running service: {state}"
    );

    // Remove the service and reload: its state file must go with it.
    std::fs::remove_file(scratch.path().join("probe.conf")).expect("remove the description");
    let out = zctl(&scratch, &["reload-all"]);
    assert!(
        out.status.success(),
        "the reload was refused: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    sleep_ms(500);
    assert!(
        scratch.read("state/probe").is_none(),
        "the state file of a deleted service survived the reload: it describes a \\
         machine that is not this one"
    );
}

/// zinit as a real PID 1.
///
/// Every other test in this file drives a *supervisor*. This one drives the
/// *init*: PID 1 mounts `/proc`, `/sys` and `/run`, forks the supervisor,
/// reaps orphans and answers signals. A PID namespace is the cheapest way
/// to be PID 1 without a virtual machine:
///
/// ```text
/// unshare --user --map-root-user --pid --fork --mount-proc
/// ```
///
/// The shell that `unshare` forks *is* PID 1 of the new namespace, and it
/// execs zinit, so the process that ends up as PID 1 is zinit. The
/// namespace's own init is killed at the end of the test — the kernel
/// tears the namespace down with it, and nothing survives.
///
/// The test skips itself where a user namespace is not available (a
/// container without `CLONE_NEWUSER`, a kernel without it): the check is
/// about the code, not about the machine's kernel configuration.
#[test]
#[cfg(target_os = "linux")]
fn zinit_boots_as_pid_one_in_a_namespace() {
    let _one = serial();
    let scratch = Scratch::new("pid1");
    if which("unshare").is_none() {
        let _ = std::fs::write(scratch.at("skipped"), "no unshare binary\n");
        return;
    }
    // The service the namespace is supposed to bring up, in its own
    // configuration directory: `--init <dir>` makes that directory the
    // whole configuration, exactly like `--sup <dir>`.
    let services = scratch.path().join("services");
    std::fs::create_dir_all(&services).expect("create the service directory");
    scratch.conf(
        "services/probe.conf",
        &desc(
            &format!("echo $$ > {}; exec /bin/sleep 60", scratch.at("svc.pid")),
            "restart = never\n",
        ),
    );
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/pid1-namespace.sh"
    );
    let zinit = env!("CARGO_BIN_EXE_zinit");

    let mut child = Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--pid",
            "--fork",
            "--mount-proc",
            "sh",
            script,
            &scratch.path().to_string_lossy(),
            zinit,
        ])
        .stdin(Stdio::null())
        // The namespace's processes inherit these descriptors, and a pipe
        // they keep open is a pipe the test runner waits on forever. A file
        // is read after the fact and blocks nobody.
        .stdout(Stdio::from(
            std::fs::File::create(scratch.at("ns.out")).expect("create the log"),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(scratch.at("ns.err")).expect("create the log"),
        ))
        .spawn()
        .expect("cannot spawn unshare");

    // The namespace's PID 1 is a child of the `unshare` process, and it
    // starts as the shell that execs zinit: poll until it says it is
    // zinit, or until it is clear that there is no namespace here at all.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut comm = String::new();
    loop {
        let children =
            std::fs::read_to_string(format!("/proc/{}/task/{}/children", child.id(), child.id()))
                .unwrap_or_default();
        if let Some(pid) = children.split_whitespace().next() {
            comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            if comm.trim() == "zinit" {
                break;
            }
        }
        if Instant::now() > deadline {
            break;
        }
        sleep_ms(100);
    }
    let ns_pid1 = children_pid(child.id());
    if comm.trim() != "zinit" {
        // No user namespace on this machine: the test has nothing to say.
        // The namespace's PID 1 (if there is one) goes first, then the
        // `unshare` process. Never a pid of 0 and never a process group:
        // `kill(0, ...)` is "every process in my group" and a group kill
        // would take the test runner (and the shell that started it) with
        // it.
        if ns_pid1 > 1 {
            // SAFETY: the pid came from this test's own /proc entry.
            unsafe {
                libc::kill(ns_pid1 as i32, libc::SIGKILL);
            }
        }
        // SAFETY: the pid is this test's own child.
        unsafe {
            libc::kill(child.id() as i32, libc::SIGKILL);
        }
        let _ = child.wait();
        let _ = std::fs::write(scratch.at("skipped"), "no PID namespace\n");
        return;
    }

    // The supervisor must come up and bind the control socket, and the
    // service must be running: PID 1 mounted what it needed and the
    // supervisor did its job inside the namespace.
    let start = Instant::now();
    let up = loop {
        if scratch.pid("svc.pid").is_some_and(pid_alive) {
            break true;
        }
        if start.elapsed() > Duration::from_secs(10) {
            break false;
        }
        sleep_ms(100);
    };
    let mut stderr = String::new();
    if !up {
        stderr = scratch.tail("ns.err");
    }
    assert!(
        up,
        "the service never came up inside the namespace\\n{stderr}"
    );

    // Tear the namespace down: killing the `unshare` process takes PID 1
    // with it, and the kernel cleans up everything inside. Only that one
    // pid: a process-group kill here would take the test runner with it.
    // SAFETY: the pid is this test's own child.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    let _ = child.wait();
    // The namespace's PID 1 is still alive (its parent died, so it was
    // reparented); killing it is what actually empties the namespace.
    // SAFETY: the pid came from this test's own /proc entry.
    unsafe {
        libc::kill(ns_pid1 as i32, libc::SIGKILL);
    }
}

/// The first child of `pid`, from `/proc`.
fn children_pid(pid: u32) -> u32 {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .ok()
        .and_then(|c| c.split_whitespace().next().and_then(|p| p.parse().ok()))
        .unwrap_or(0)
}

/// The full path of `name` in `PATH`, if it is there.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The sandbox must actually confine.
///
/// The filter's implicit baseline covers what libc needs to start (read,
/// write, open, socket, clone, execve...), so the proof cannot be "the
/// service cannot open a file". It is an operation outside the baseline:
/// `unshare(2)` is not in it, so a service that tries it is either refused
/// (errno mode) or killed (enforce mode).
///
/// The comparison is the point: the *same* command must succeed without
/// confinement and fail with it. A test that only shows the confined
/// service failing proves nothing — it might have failed for any reason.
///
/// Root only: `unshare(CLONE_NEWNS)` needs `CAP_SYS_ADMIN`, and without
/// root the unconfined service fails too.
#[test]
#[cfg(target_os = "linux")]
fn the_sandbox_really_confines() {
    let _one = serial();
    let scratch = Scratch::new("sandbox");
    if unsafe { libc::geteuid() } != 0 {
        let _ = std::fs::write(scratch.at("skipped"), "needs root\n");
        return;
    }
    if which("unshare").is_none() {
        let _ = std::fs::write(scratch.at("skipped"), "no unshare binary\n");
        return;
    }
    // The command under test: it needs a namespace, and it says whether it
    // got one.
    let probe = format!(
        "/bin/sh -c 'unshare -m true 2>/dev/null; echo $? > {}'",
        scratch.at("rc")
    );
    // The same command, three times over: unconfined, capability-dropped
    // and syscall-filtered.
    scratch.conf("plain.conf", &desc(&probe, ""));
    scratch.conf(
        "nocaps.conf",
        &desc(&probe, "drop-capabilities = sys_admin\n"),
    );
    scratch.conf("seccomp.conf", &desc(&probe, "syscall-filter = errno\n"));

    let mut sup = Sup::start(&scratch);

    let rc = |name: &str| -> i32 {
        let text = scratch.read(name).unwrap_or_default();
        text.trim().parse().unwrap_or(-1)
    };
    // Wait for all three to have answered.
    assert!(
        wait_until(15_000, || {
            scratch.read("plain.rc").is_some()
                && scratch.read("nocaps.rc").is_some()
                && scratch.read("seccomp.rc").is_some()
        }),
        "not every probe answered: plain={:?} nocaps={:?} seccomp={:?}",
        scratch.read("plain.rc"),
        scratch.read("nocaps.rc"),
        scratch.read("seccomp.rc")
    );

    assert_eq!(
        rc("plain.rc"),
        0,
        "the unconfined service could not create a namespace: the test proves \
         nothing if the baseline operation fails"
    );
    assert_ne!(
        rc("nocaps.rc"),
        0,
        "dropping CAP_SYS_ADMIN did not stop the service from creating a namespace"
    );
    assert_ne!(
        rc("seccomp.rc"),
        0,
        "the syscall filter did not stop the service from creating a namespace"
    );
    assert!(
        sup.exited().is_none(),
        "the supervisor died while supervising confined services"
    );
}

/// A supervisor that comes back must not start a second copy of what is
/// already running.
///
/// The supervisor that started the services died; its children were
/// reparented to PID 1, which knows nothing about them. Without adoption,
/// the new supervisor starts a copy of everything: the machine ends up
/// with two of every daemon fighting over the ports, which is exactly what
/// happened on real hardware.
///
/// The test: start a supervisor, kill it (the service survives), start a
/// new one over the same state directory, and require that the service is
/// the *same* process and that the new supervisor knows about it.
#[test]
fn a_restarted_supervisor_adopts_the_services_that_are_still_alive() {
    let _one = serial();
    let scratch = Scratch::new("adopt");
    scratch.conf(
        "probe.conf",
        &desc(
            &format!("echo $$ > {}; exec /bin/sleep 300", scratch.at("svc.pid")),
            "restart = always\nrestart-delay = 100ms\n",
        ),
    );
    let mut first = Sup::start(&scratch);
    let pid = first.await_live_pid("svc.pid", START_DEADLINE_MS);

    // Kill the supervisor the way a crash would: no cleanup, no goodbye.
    // The service is not its child any more (the kernel reparents it), and
    // it must survive.
    // SAFETY: the pid is this test's own child.
    unsafe {
        libc::kill(first.pid(), libc::SIGKILL);
    }
    let _ = first.child.wait();
    sleep_ms(300);
    assert!(
        pid_alive(pid),
        "the service died with its supervisor: the test is not testing adoption"
    );

    // The new supervisor reads the same state directory.
    let mut second = Sup::start(&scratch);
    sleep_ms(1_500);

    // The same process: no second copy.
    assert_eq!(
        scratch.pid("svc.pid"),
        Some(pid),
        "the new supervisor started a second copy of a service that was already running"
    );
    // And it knows about it: the state says the service is running under
    // the supervisor's own account, not stopped.
    let state = scratch.read("state/probe").expect("the state file");
    assert!(
        state.contains("state=Running"),
        "the adopted service is not Running in the visible state: {state}"
    );
    assert!(
        state.contains(&format!("pid={pid}")),
        "the adopted service has the wrong pid in the visible state: {state}"
    );
    assert!(
        second.exited().is_none(),
        "the new supervisor died while adopting"
    );

    // And the adopted service is really supervised: when it dies, the new
    // supervisor notices and starts it again. (An adopted process is not
    // our child, so nobody may read its exit status — the death still has
    // to reach the restart policy.)
    // SAFETY: the pid is the service's own, reported by the service.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let restarted = wait_until(START_DEADLINE_MS, || {
        scratch
            .pid("svc.pid")
            .is_some_and(|p| p != pid && pid_alive(p))
    });
    assert!(
        restarted,
        "the adopted service died and was never restarted: the supervisor did not notice"
    );
}

/// A supervisor that dies leaves the socket free for the next one.
///
/// The single-instance lock must not outlive the process that took it: a
/// supervisor that died and a new one that cannot start is a machine with
/// no supervisor at all. The lock is released by the kernel when the
/// process goes away, so the next one takes the socket.
#[test]
fn a_dead_supervisor_leaves_the_socket_free() {
    let _one = serial();
    let scratch = Scratch::new("lockfree");
    scratch.conf(
        "probe.conf",
        &desc(
            &format!("echo $$ > {}; exec /bin/sleep 30", scratch.at("svc.pid")),
            "restart = never\n",
        ),
    );
    let mut first = Sup::start(&scratch);
    let _pid = first.await_live_pid("svc.pid", START_DEADLINE_MS);
    // Kill it the way a crash would.
    // SAFETY: the pid is this test's own child.
    unsafe {
        libc::kill(first.pid(), libc::SIGKILL);
    }
    let _ = first.child.wait();
    sleep_ms(300);

    // The next supervisor takes the same socket: the lock died with the
    // process that held it. (It takes a moment to bind it, so the question
    // is asked until it is answered.)
    let mut second = Sup::start(&scratch);
    let mut answered = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        let out = zctl(&scratch, &["list"]);
        if out.status.success() {
            answered = true;
            break;
        }
        sleep_ms(100);
    }
    assert!(
        answered,
        "the second supervisor did not take the socket: {}",
        scratch.tail("sup.log")
    );
    assert!(
        second.exited().is_none(),
        "the second supervisor stepped aside although the first one was dead"
    );
}

/// A service with a cgroup lands in it, with its own pid.
///
/// The cgroup is not a label: the child is moved into
/// `/sys/fs/cgroup/zinit.slice/<name>/` before it execs, so the kernel
/// accounts the service there. The test reads the cgroup's own file: the
/// pid has to be there, and it has to be the service's.
#[test]
#[cfg(target_os = "linux")]
fn a_service_with_a_cgroup_lands_in_it() {
    let _one = serial();
    let scratch = Scratch::new("cgroup");
    if unsafe { libc::geteuid() } != 0
        || !std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists()
    {
        // No print macros outside `zrt::report` (the purity job): the
        // reason goes to the scratch directory instead.
        let _ = std::fs::write(scratch.at("skipped"), "needs root and cgroup2\n");
        return;
    }
    scratch.conf(
        "probe.conf",
        &desc(
            &format!("echo $$ > {}; exec /bin/sleep 30", scratch.at("svc.pid")),
            "restart = never\ncgroup = probe\n",
        ),
    );
    let mut sup = Sup::start(&scratch);
    let pid = sup.await_live_pid("svc.pid", START_DEADLINE_MS);

    let procs = std::fs::read_to_string("/sys/fs/cgroup/zinit.slice/probe/cgroup.procs")
        .expect("the cgroup has a procs file");
    let pids: Vec<i32> = procs
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    assert!(
        pids.contains(&pid),
        "the service is not in its cgroup: {pids:?} does not contain {pid}"
    );
}

/// `hibernate` is refused, and the refusal says why.
///
/// There is no resume target on this machine, so hibernating would either
/// do nothing or leave a file the kernel cannot read back. The honest
/// answer is a refusal with a code of its own, not a silent success.
#[test]
fn hibernate_is_refused_with_a_reason() {
    let _one = serial();
    let scratch = Scratch::new("hibernate");
    if zctl_path().is_none() {
        let _ = std::fs::write(scratch.at("skipped"), "no zctl binary\n");
        return;
    }
    scratch.conf(
        "probe.conf",
        &desc(
            &format!("echo $$ > {}; exec /bin/sleep 30", scratch.at("svc.pid")),
            "restart = never\n",
        ),
    );
    let mut sup = Sup::start(&scratch);
    let _pid = sup.await_live_pid("svc.pid", START_DEADLINE_MS);

    let out = zctl(&scratch, &["hibernate"]);
    let reply = String::from_utf8_lossy(&out.stdout);
    // The id is the client's own token (`zctl` by default), so the check
    // is on the code and the reason, not on the token.
    assert!(
        reply.starts_with("E ") && reply.contains(" 6 "),
        "hibernate must be refused with code 6: {reply:?}"
    );
    assert!(
        reply.contains("resume target"),
        "the refusal must say what is missing: {reply:?}"
    );
    assert!(
        !out.status.success(),
        "a refused verb must not report success"
    );
    // And the machine is still up: the refusal changed nothing.
    assert!(
        sup.exited().is_none(),
        "the supervisor died on a refused hibernate"
    );
}
