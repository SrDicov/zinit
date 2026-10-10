//! Layer 11.1c, control plane: the `AF_UNIX` socket against the real binary.
//!
//! These tests run `zinit --sup <dir>` as a subprocess — the same harness
//! shape as `supervises_real_service.rs` — and then *talk* to it: `list`,
//! `status`, `stop`, `start`, `reload-all`. The evidence is a pid that
//! `kill(pid, 0)` answers for and a reply line that parses, never a mock.
//!
//! # Isolation
//!
//! The socket and the state directory are pointed at the test's own scratch
//! directory through `ZINIT_SOCKET` / `ZINIT_STATE_DIR`, so these supervisors
//! never touch `/run` and never meet each other — or the supervisors from
//! the other test binary — on a machine-wide name. No cross-file lock needed.
//!
//! # Rules (same as the sibling suite)
//!
//! * **Bounded.** Every wait has a deadline; a hang is a failure, not a stall.
//! * **Cleaned up.** The guard kills the supervisor's process group, then any
//!   known service pid, so a failure cannot leak a restarting supervisor.
//! * **POSIX-portable.** `/bin/sh` and `/bin/sleep` only. No `/proc`, no `ps`.
//! * **Silent.** No `println!`: CI greps all of `crates/`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const POLL_MS: u64 = 25;
const START_DEADLINE_MS: u64 = 10_000;
const EXIT_DEADLINE_MS: u64 = 10_000;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs the check and delivers nothing.
    unsafe { libc::kill(pid, 0) == 0 || last_errno() == libc::EPERM }
}

fn pid_gone(pid: i32) -> bool {
    // SAFETY: as above. Only ESRCH proves the pid is unreaped and gone.
    unsafe { libc::kill(pid, 0) == -1 && last_errno() == libc::ESRCH }
}

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

// ── scratch ──────────────────────────────────────────────────────────────

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let pid = std::process::id();
        let mut dir = std::env::temp_dir();
        dir.push(format!("zinit-ctl-{tag}-{pid}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch must be creatable");
        let text = dir.display().to_string();
        assert!(
            !text.contains([' ', '\t', '\n', '#', '\'', '"', '\\']),
            "scratch {text} holds a character the config format or shell would eat"
        );
        Scratch { dir }
    }

    fn conf(&self, name: &str, body: &str) {
        std::fs::write(self.dir.join(name), body).expect("conf must be writable");
    }

    fn sock(&self) -> PathBuf {
        self.dir.join("ctl.sock")
    }

    fn statedir(&self) -> PathBuf {
        self.dir.join("state")
    }

    fn state_of(&self, svc: &str) -> Option<String> {
        std::fs::read_to_string(self.statedir().join(svc)).ok()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ── supervisor ───────────────────────────────────────────────────────────

/// `echo $$ > <pidfile>; exec /bin/sleep <s>`: the pidfile then names the
/// live sleep, because `exec` keeps the shell's pid.
fn sleeper(pidfile: &str, seconds: u32) -> String {
    format!(
        "type = script\ncommand = echo $$ > {pidfile}; exec /bin/sleep {seconds}\n\
         ready = none\nlog = none\n"
    )
}

struct Sup {
    child: Child,
    services: Vec<i32>,
}

impl Sup {
    fn start(scratch: &Scratch) -> Sup {
        use std::os::unix::process::CommandExt;
        let child = Command::new(env!("CARGO_BIN_EXE_zinit"))
            .arg("--sup")
            .arg(&scratch.dir)
            .env("ZINIT_SOCKET", scratch.sock())
            .env("ZINIT_STATE_DIR", scratch.statedir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("cannot spawn `zinit --sup`");
        Sup {
            child,
            services: Vec::new(),
        }
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }
}

impl Drop for Sup {
    fn drop(&mut self) {
        // The group first: while the supervisor lives it would restart
        // anything else killed. SIGKILL is untrappable, so one sweep ends
        // supervisor and services together; stragglers get a direct signal.
        // SAFETY: negative pid addresses the child's own group, created by
        // `process_group(0)` above; ESRCH (already gone) is fine.
        unsafe {
            libc::kill(-self.pid(), libc::SIGKILL);
        }
        let _ = self.child.wait();
        for pid in self.services.drain(..) {
            // SAFETY: pids this test's own services reported; ESRCH is fine.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

// ── client ───────────────────────────────────────────────────────────────

struct Ctl {
    io: BufReader<UnixStream>,
}

impl Ctl {
    /// Connect, polling until the supervisor has bound (it binds during its
    /// first pass) or the deadline passes.
    fn connect(sock: &Path) -> Ctl {
        assert!(
            wait_until(START_DEADLINE_MS, || sock.exists()),
            "control socket {} never appeared",
            sock.display()
        );
        let start = Instant::now();
        loop {
            if let Ok(s) = UnixStream::connect(sock) {
                s.set_read_timeout(Some(IO_TIMEOUT))
                    .expect("timeouts must be settable");
                s.set_write_timeout(Some(IO_TIMEOUT))
                    .expect("timeouts must be settable");
                return Ctl {
                    io: BufReader::new(s),
                };
            }
            assert!(
                start.elapsed() < Duration::from_millis(START_DEADLINE_MS),
                "cannot connect to {}",
                sock.display()
            );
            sleep_ms(POLL_MS);
        }
    }

    /// Send one frame, return the reply line (without the newline).
    fn ask(&mut self, frame: &str) -> String {
        self.io
            .get_mut()
            .write_all(frame.as_bytes())
            .expect("control write must succeed");
        let mut line = String::new();
        self.io
            .read_line(&mut line)
            .expect("control reply must arrive");
        assert!(!line.is_empty(), "control connection closed mid-test");
        line.trim_end().to_string()
    }
}

/// The `pid=` field of a `status` payload, when it names a live process.
fn status_pid(reply: &str) -> Option<i32> {
    reply
        .split_whitespace()
        .find_map(|w| w.strip_prefix("pid="))
        .and_then(|p| p.parse().ok())
        .filter(|&p| p > 1)
}

fn pidfile_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|&p| p > 1)
}

// ── tests ────────────────────────────────────────────────────────────────

#[test]
fn control_plane_starts_stops_and_reports() {
    let scratch = Scratch::new("basic");
    let pidfile = scratch.dir.join("svc.pid");
    let pidfile_s = pidfile.display().to_string();
    scratch.conf("svc.conf", &sleeper(&pidfile_s, 30));
    // The guard *is* `sup`: its `Drop` owns the cleanup, so the binding must
    // simply stay alive until the end of the test.
    let mut sup = Sup::start(&scratch);

    let mut ctl = Ctl::connect(&scratch.sock());
    assert!(
        wait_until(START_DEADLINE_MS, || pidfile_pid(&pidfile).is_some()),
        "service never started"
    );
    let pid = pidfile_pid(&pidfile).expect("pidfile has a pid");
    sup.services.push(pid);

    let reply = ctl.ask("C 1 list\n");
    assert!(
        reply.starts_with("R 1 0 ") && reply.contains("svc=running"),
        "unexpected list reply: {reply}"
    );

    let reply = ctl.ask("C 2 status svc\n");
    assert!(
        reply.starts_with("R 2 0 svc running "),
        "unexpected status reply: {reply}"
    );
    assert_eq!(status_pid(&reply), Some(pid));

    let reply = ctl.ask("C 3 referendum\n");
    assert!(
        reply.starts_with("E 3 2 "),
        "unknown command must error with code 2: {reply}"
    );

    let reply = ctl.ask("C 4 status ghost\n");
    assert!(
        reply.starts_with("E 4 3 "),
        "unknown service must error with code 3: {reply}"
    );

    let reply = ctl.ask("C 5 stop svc\n");
    assert_eq!(reply, "R 5 0 down", "stop reply: {reply}");
    assert!(
        wait_until(EXIT_DEADLINE_MS, || pid_gone(pid)),
        "service survived stop"
    );

    let reply = ctl.ask("C 6 status svc\n");
    assert!(
        reply.contains("stopped") && reply.contains("desired=down"),
        "stopped status: {reply}"
    );

    let reply = ctl.ask("C 7 start svc\n");
    assert_eq!(reply, "R 7 0 up", "start reply: {reply}");
    assert!(
        wait_until(START_DEADLINE_MS, || pidfile_pid(&pidfile)
            .is_some_and(|p| p != pid)),
        "service never came back"
    );
    let pid2 = pidfile_pid(&pidfile).expect("pidfile has a pid");
    assert!(pid_alive(pid2), "restarted service is not alive");
    sup.services.push(pid2);

    let reply = ctl.ask("C 8 version\n");
    assert!(reply.starts_with("R 8 0 zinit "), "version reply: {reply}");

    let state = scratch.state_of("svc");
    assert!(
        state.is_some_and(|s| s.contains("state=Running")),
        "state file must show running"
    );
}

/// A `reload-all` keeps the old pid, starts the newcomer, and retires the
/// deleted — the three promises of a hot reload, all read off live pids.
#[test]
fn reload_all_preserves_live_pids() {
    let scratch = Scratch::new("reload");
    let pid_a = scratch.dir.join("a.pid");
    let pid_a_s = pid_a.display().to_string();
    scratch.conf("a.conf", &sleeper(&pid_a_s, 30));
    let mut sup = Sup::start(&scratch);

    let mut ctl = Ctl::connect(&scratch.sock());
    assert!(
        wait_until(START_DEADLINE_MS, || pidfile_pid(&pid_a).is_some()),
        "service a never started"
    );
    let before = pidfile_pid(&pid_a).expect("pidfile has a pid");
    sup.services.push(before);

    let pid_b = scratch.dir.join("b.pid");
    scratch.conf("b.conf", &sleeper(&pid_b.display().to_string(), 30));
    let reply = ctl.ask("C 1 reload-all\n");
    assert_eq!(reply, "R 1 0 2 services", "reload reply: {reply}");

    assert!(pid_alive(before), "reload killed the survivor");
    let reply = ctl.ask("C 2 status a\n");
    assert_eq!(status_pid(&reply), Some(before), "a kept its pid: {reply}");

    assert!(
        wait_until(START_DEADLINE_MS, || pidfile_pid(&pid_b).is_some()),
        "newcomer b never started"
    );
    let b = pidfile_pid(&pid_b).expect("pidfile has a pid");
    assert!(pid_alive(b));
    sup.services.push(b);

    std::fs::remove_file(scratch.dir.join("b.conf")).expect("remove b");
    let reply = ctl.ask("C 3 reload-all\n");
    assert_eq!(reply, "R 3 0 1 services", "second reload: {reply}");
    assert!(
        wait_until(EXIT_DEADLINE_MS, || pid_gone(b)),
        "deleted service b survived its retirement"
    );
    assert!(pid_alive(before), "reload took the survivor with it");
}

#[test]
fn demand_start_wakes_on_connection() {
    use std::net::TcpStream;

    // A high port the neighbours are unlikely to want; probed free before
    // the supervisor ever hears about it, so a squatted port fails loudly
    // here instead of wedging the supervisor later.
    let port = [48121u16, 48122, 48123]
        .into_iter()
        .find(|p| std::net::TcpListener::bind(format!("127.0.0.1:{p}")).is_ok())
        .expect("a free probe port");
    let scratch = Scratch::new("demand");
    scratch.conf(
        "lazy.conf",
        &format!(
            "type = script\ncommand = exec /bin/sleep 300\nready = none\n\
             log = none\nrestart = always\nlisten = tcp:{port}\n\
             on-demand = yes\nenabled = no\n"
        ),
    );
    let _sup = Sup::start(&scratch);

    // Down means down: nothing starts without traffic.
    sleep_ms(500);
    let quiet = scratch.state_of("lazy");
    let started = quiet
        .as_deref()
        .is_some_and(|s| s.contains("state=Running"));
    assert!(
        !started,
        "on-demand service started with no connection: {quiet:?}"
    );

    // Knock until someone listens (the supervisor binds lazily), then hold
    // the door open: the backlog keeps the connection readable without
    // anyone accepting, which is exactly what the trigger watches for.
    let start = Instant::now();
    let _held = loop {
        match TcpStream::connect(format!("127.0.0.1:{port}")) {
            Ok(s) => break s,
            Err(_) => {
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "nobody bound tcp:{port}"
                );
                sleep_ms(POLL_MS);
            }
        }
    };
    let running = || scratch.state_of("lazy").is_some_and(|s| s.contains("state=Running"));
    assert!(
        wait_until(START_DEADLINE_MS, running),
        "traffic never woke the service"
    );
}
