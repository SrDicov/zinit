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
/// How long a supervisor that must *refuse* may take to say so.
const EXIT_DEADLINE_MS: u64 = 10_000;
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
        let child = sup_command(scratch)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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

/// Run `zinit --sup <scratch>` to completion and return its status and stderr.
///
/// For the refusal tests only: they expect the supervisor to die quickly, so a
/// pipe cannot fill up and its stderr is the interesting part of the failure.
fn run_to_exit(scratch: &Scratch) -> (ExitStatus, String) {
    let mut child = sup_command(scratch)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cannot spawn `zinit --sup`");

    let start = Instant::now();
    let status = loop {
        match child.try_wait().expect("waiting on the supervisor failed") {
            Some(status) => break status,
            None if start.elapsed() >= Duration::from_millis(EXIT_DEADLINE_MS) => {
                // Clean up before failing: this is the one path where the
                // supervisor may have started services nobody recorded.
                let group = child.id() as i32;
                // SAFETY: signal 9 to a process group this test created.
                unsafe { libc::kill(-group, libc::SIGKILL) };
                let _ = child.wait();
                panic!(
                    "the supervisor is still running after {EXIT_DEADLINE_MS}ms with {dir} \
                     loaded: a configuration it must refuse left it supervising anyway, and \
                     whatever it started is still running",
                    dir = scratch.path().display()
                );
            }
            None => sleep_ms(POLL_MS),
        }
    };
    // Safe to read to the end: the process has exited, so the pipe is at EOF.
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read;
        let _ = pipe.read_to_string(&mut stderr);
    }
    (status, stderr)
}

/// The refusal contract: a supervisor that cannot build a plan says so and
/// exits non-zero (`DESIGN.md` §2.4 — never exit 0 on an error). Anything else
/// means the machine booted half-configured.
fn assert_refused(status: &ExitStatus, stderr: &str, what: &str) {
    match status.code() {
        Some(0) => panic!(
            "the supervisor exited 0 with {what} configured (stderr: {stderr}): booting half a \
             system is the one outcome this must never produce"
        ),
        Some(_) => {}
        None => panic!(
            "the supervisor died from a signal ({status}) with {what} configured instead of \
             exiting non-zero (stderr: {stderr})"
        ),
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

/// A cycle in the graph has no topological order, so there is no plan. The
/// supervisor must refuse the whole directory rather than start the half of it
/// it happens to understand.
#[test]
fn a_dependency_cycle_refuses_to_start() {
    let _one = serial();
    let scratch = Scratch::new("cycle");
    // Both services are perfectly runnable. It is the *plan* that cannot exist,
    // so both would leave a pid behind if the supervisor started anything.
    for (file, depends, pidfile) in [("a.conf", "b", "a.pid"), ("b.conf", "a", "b.pid")] {
        let command = announce_then_sleep(&scratch.at(pidfile), 30);
        let body = format!("depends = {depends}\n{}", desc(&command, ""));
        scratch.conf(file, &body);
    }

    let (status, stderr) = run_to_exit(&scratch);
    assert_refused(&status, &stderr, "a dependency cycle");
    assert_nothing_left_running(&scratch, &["a.pid", "b.pid"]);
}

/// One unparseable file stops the boot. A supervisor that skips the broken file
/// and starts the rest is a machine that comes up in a configuration nobody
/// wrote, which is worse than one that stays down and says why.
#[test]
fn an_unparseable_config_refuses_to_start() {
    let _one = serial();
    let scratch = Scratch::new("unparseable");
    // A valid service next to the broken one: the point is that one bad file
    // stops everything, not that it stops only its own service.
    let command = announce_then_sleep(&scratch.at("good.pid"), 30);
    scratch.conf("good.conf", &desc(&command, ""));
    // `restart = sometimes` is not a policy. One fatal error, in one directive.
    let broken = format!("{}\nrestart = sometimes\n", desc("exec /bin/sleep 30", ""));
    scratch.conf("broken.conf", &broken);

    let (status, stderr) = run_to_exit(&scratch);
    assert_refused(&status, &stderr, "an unparseable service description");
    assert_nothing_left_running(&scratch, &["good.pid"]);
}
