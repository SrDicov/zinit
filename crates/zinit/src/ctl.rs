//! The control socket: `AF_UNIX` + `SOCK_STREAM` (contract C4).
//!
//! # Protocol (frozen)
//!
//! ```text
//! client →  C {id} <command> [args...]\n
//! server →  R {id} 0 [payload]\n        on success
//! server →  E {id} <code> <message>\n   on failure
//! ```
//!
//! One line per frame, `\n`-delimited (`DESIGN.md` §7). There is no length
//! prefix and no negotiation: a second framing would be a second parser to
//! keep correct, for a question the newline already answers. `{id}` is an
//! opaque client token echoed back so one connection can multiplex; it may
//! not contain whitespace (it arrives as one `split_whitespace` word, so a
//! hostile id cannot smuggle a second frame).
//!
//! Error codes: `1` bad frame, `2` unknown command or arity, `3` unknown
//! service, `4` not applicable (e.g. stopping a target), `5` reload failed.
//!
//! # Resource discipline
//!
//! A line longer than [`MAX_LINE_BYTES`] kills the connection, and a client
//! that buffers more than [`MAX_BUFFERED_BYTES`] unread or unanswered bytes
//! is dropped at the next flush. A control client that could grow the
//! supervisor's heap without bound would be a local denial of service, and
//! the supervisor is the one process that must not have one.
//!
//! # What this module is not
//!
//! It parses and frames. It never touches the plan, the runtime or a pid:
//! every [`Command`] is handed to the supervisor loop, which owns all policy.

#![deny(missing_docs)]

use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};

use zcore::{Desired, State};
use zrt::reactor::{Interest, Reactor};

/// Default control socket path (contract C4).
pub const DEFAULT_SOCKET_PATH: &str = "/run/zinit/ctl";
/// Default visible-state directory (contract C6).
pub const DEFAULT_STATE_DIR: &str = "/run/zinit/state";
/// Environment override for the socket path, honoured by `--sup`.
///
/// Tests set this to a scratch directory: the real default needs root.
pub const SOCKET_ENV: &str = "ZINIT_SOCKET";
/// Environment override for the state directory. Same reason as above.
pub const STATE_DIR_ENV: &str = "ZINIT_STATE_DIR";
/// One incoming line may not exceed this. Longer lines are rejected and the
/// client dropped.
pub const MAX_LINE_BYTES: usize = 4096;
/// Hard cap on buffered input or unflushed output per client. Past it the
/// client is dropped (input) or its replies discarded at flush (output).
pub const MAX_BUFFERED_BYTES: usize = 65536;

/// One parsed client frame, with the connection it arrived on.
///
/// The supervisor answers through [`CtlServer::reply`] to `client`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    /// `status <name>`: one service's state, desired, pid, age, restarts.
    Status {
        client: RawFd,
        id: String,
        name: String,
    },
    /// `list`: every service as `name=state`, space-separated.
    List { client: RawFd, id: String },
    /// `start <name>`: want up, clearing a suppression like `kick`.
    Start {
        client: RawFd,
        id: String,
        name: String,
    },
    /// `stop <name>`: want down.
    Stop {
        client: RawFd,
        id: String,
        name: String,
    },
    /// `restart <name>`: stop then start once the process is gone.
    Restart {
        client: RawFd,
        id: String,
        name: String,
    },
    /// `kick <name>`: clear a restart suppression and refill the bucket.
    Kick {
        client: RawFd,
        id: String,
        name: String,
    },
    /// `reload-all`: rebuild the plan without disturbing live pids.
    ReloadAll { client: RawFd, id: String },
    /// `poweroff | reboot | halt | suspend | hibernate`: machine power.
    /// `hibernate` always answers an error (no resume target exists);
    /// the rest stop every service first, then act.
    Power {
        client: RawFd,
        id: String,
        action: PowerAction,
    },
    /// `version`: binary version plus the backend capability line.
    Version { client: RawFd, id: String },
    /// `help`: the verbs this server speaks.
    Help { client: RawFd, id: String },
    /// `emergency`: put a shell on the console, whatever the plan says.
    Emergency { client: RawFd, id: String },
}

/// What a verb is allowed to do, from the caller's point of view.
///
/// Three classes, because the blast radius is not the same: reading a
/// service's state changes nothing, stopping a service changes one
/// service, and powering the machine off ends everything for everybody.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VerbClass {
    /// `status`, `list`, `version`, `help`: nobody is touched.
    Read,
    /// `start`, `stop`, `restart`, `kick`, `reload-all`: one service (or
    /// the plan) changes.
    Mutate,
    /// `poweroff`, `reboot`, `halt`, `suspend`, `hibernate`: the machine
    /// changes.
    Power,
}

impl Command {
    /// Which class does this command belong to?
    ///
    /// The split is what the credential check in [`CtlServer`] enforces: a
    /// peer that may look may not touch, and a peer that may touch services
    /// may not end the machine.
    pub fn verb_class(&self) -> VerbClass {
        match self {
            Command::Status { .. }
            | Command::List { .. }
            | Command::Version { .. }
            | Command::Help { .. } => VerbClass::Read,
            Command::Start { .. }
            | Command::Stop { .. }
            | Command::Restart { .. }
            | Command::Kick { .. }
            | Command::ReloadAll { .. } => VerbClass::Mutate,
            // Taking the console is a machine-wide act, like the power
            // verbs: it is the one thing an operator does not want a
            // stray program of their own user doing by accident.
            Command::Power { .. } | Command::Emergency { .. } => VerbClass::Power,
        }
    }
}

/// The client's token, so a refusal can name who it refuses.
fn cmd_id(cmd: &Command) -> String {
    match cmd {
        Command::Status { id, .. }
        | Command::List { id, .. }
        | Command::Start { id, .. }
        | Command::Stop { id, .. }
        | Command::Restart { id, .. }
        | Command::Kick { id, .. }
        | Command::ReloadAll { id, .. }
        | Command::Power { id, .. }
        | Command::Version { id, .. }
        | Command::Help { id, .. }
        | Command::Emergency { id, .. } => id.clone(),
    }
}

/// May this peer run this verb?
///
/// The rules, in order:
///
/// * Reading is always allowed. A peer that connected can be told what the
///   machine is doing.
/// * Root may do anything: it already owns the machine.
/// * A peer that is neither root nor the supervisor's own user may not
///   mutate. The socket is `0600`, so this case is already unreachable in
///   practice — the rule is there for the day an operator loosens the mode
///   or a group gets access.
/// * A peer that *is* the supervisor's user may mutate, but only through a
///   known client: `zctl` (or the supervisor itself). Otherwise any
///   process of that user could power the machine off by writing one line
///   into the socket.
///
/// When the platform cannot name the peer's binary (no `/proc`, no peer
/// pid), the uid stands on its own: the check degrades to what it was
/// before, rather than locking the owner out of their own machine.
///
/// Note what this is not: a boundary against a malicious peer of the same
/// user. That peer can copy `zctl` and be `zctl`. The socket's mode is
/// the real boundary; this is defence in depth and a guard against
/// accidents.
pub fn authorize(
    peer: Option<&zrt::sys::PeerCred>,
    peer_exe: Option<&Path>,
    self_uid: u32,
    class: VerbClass,
) -> bool {
    if class == VerbClass::Read {
        return true;
    }
    let Some(peer) = peer else {
        // A platform that cannot say who is calling gets the strict answer.
        return false;
    };
    if peer.uid == 0 {
        return true;
    }
    if peer.uid != self_uid {
        return false;
    }
    if class == VerbClass::Mutate {
        // Managing services is what this socket is for: the owner of the
        // socket may do it, from the client or from a script.
        return true;
    }
    // Powering the machine off is the one verb that ends everything for
    // everybody, so it takes the known client (`zctl`) or root. The socket
    // is `0600`, so this is a guard against an accident — a stray `echo`
    // into the socket — and not a boundary against the owner's own code.
    match peer_exe {
        None => true,
        Some(exe) => {
            let name = exe.file_name().map(|n| n.to_string_lossy().into_owned());
            matches!(name.as_deref(), Some("zctl") | Some("zinit"))
        }
    }
}

/// A machine-power verb. Parsed in [`Command`], executed by the supervisor
/// (stop every service first, except `suspend`, which keeps state).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PowerAction {
    /// Stop the CPU, leave the power on.
    Halt,
    /// Stop everything, then cut the power.
    Poweroff,
    /// Stop everything, then boot again.
    Reboot,
    /// Sync and sleep (RAM stays powered). No services are stopped.
    Suspend,
    /// Refused: hibernation needs a resume target that does not exist.
    Hibernate,
}

/// Error codes: `1` bad frame, `2` unknown command or arity, `3` unknown
/// service, `4` not applicable, `5` reload failed, `6` not permitted.
///
/// The first five are the frozen contract (C4). `6` was added when the
/// control socket started checking who is calling: a client whose
/// credentials do not allow the verb gets its own code, because reusing
/// `5` ("reload failed") for a refusal would tell every existing client
/// the wrong thing. A code the client does not know is still a code it can
/// print, so the extension breaks nobody.
/// A frame that parsed but means nothing: the id to blame, a code, a reason.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CtlError {
    /// Who to answer. `"0"` when the frame never named anyone.
    pub id: String,
    /// `1` bad frame, `2` unknown command or arity.
    pub code: u8,
    /// Human-readable, single-line (newlines are sanitised at render).
    pub msg: String,
}

/// Parse one stripped line (no trailing `\n`) into a [`Command`].
///
/// Pure: no I/O, no globals. A hostile line yields a [`CtlError`], never a
/// panic — the parser is reachable from an unprivileged local socket.
pub fn parse_line(client: RawFd, line: &str) -> Result<Command, CtlError> {
    let mut words = line.split_whitespace();
    match words.next() {
        Some("C") => {}
        _ => {
            return Err(CtlError {
                id: String::from("0"),
                code: 1,
                msg: String::from("frame must start with `C`"),
            });
        }
    }
    let id = match words.next() {
        Some(id) => id.to_string(),
        None => {
            return Err(CtlError {
                id: String::from("0"),
                code: 1,
                msg: String::from("frame needs an id"),
            });
        }
    };
    let verb = match words.next() {
        Some(v) => v,
        None => {
            return Err(CtlError {
                id,
                code: 2,
                msg: String::from("frame needs a command"),
            });
        }
    };
    // Arity is checked by two helpers that take the id as a parameter rather
    // than capturing it: every arm below moves `id` into its command, and a
    // closure holding it would forbid all of those moves at once.
    let rest: Vec<&str> = words.collect();
    let one_arg = |id: &str, what: &str| -> Result<String, CtlError> {
        match rest.as_slice() {
            [name] => Ok(name.to_string()),
            [] => Err(CtlError {
                id: id.to_string(),
                code: 2,
                msg: String::from("this command needs a service name"),
            }),
            _ => Err(CtlError {
                id: id.to_string(),
                code: 2,
                msg: format!("too many arguments for `{what}`"),
            }),
        }
    };
    let no_args = |id: &str, what: &str| -> Result<(), CtlError> {
        if rest.is_empty() {
            Ok(())
        } else {
            Err(CtlError {
                id: id.to_string(),
                code: 2,
                msg: format!("`{what}` takes no arguments"),
            })
        }
    };
    match verb {
        "status" => {
            let name = one_arg(&id, verb)?;
            Ok(Command::Status { client, id, name })
        }
        "start" => {
            let name = one_arg(&id, verb)?;
            Ok(Command::Start { client, id, name })
        }
        "stop" => {
            let name = one_arg(&id, verb)?;
            Ok(Command::Stop { client, id, name })
        }
        "restart" => {
            let name = one_arg(&id, verb)?;
            Ok(Command::Restart { client, id, name })
        }
        "kick" => {
            let name = one_arg(&id, verb)?;
            Ok(Command::Kick { client, id, name })
        }
        "list" => {
            no_args(&id, verb)?;
            Ok(Command::List { client, id })
        }
        "reload-all" => {
            no_args(&id, verb)?;
            Ok(Command::ReloadAll { client, id })
        }
        "poweroff" => {
            no_args(&id, verb)?;
            Ok(Command::Power {
                client,
                id,
                action: PowerAction::Poweroff,
            })
        }
        "reboot" => {
            no_args(&id, verb)?;
            Ok(Command::Power {
                client,
                id,
                action: PowerAction::Reboot,
            })
        }
        "halt" => {
            no_args(&id, verb)?;
            Ok(Command::Power {
                client,
                id,
                action: PowerAction::Halt,
            })
        }
        "suspend" => {
            no_args(&id, verb)?;
            Ok(Command::Power {
                client,
                id,
                action: PowerAction::Suspend,
            })
        }
        "hibernate" => {
            no_args(&id, verb)?;
            Ok(Command::Power {
                client,
                id,
                action: PowerAction::Hibernate,
            })
        }
        "version" => {
            no_args(&id, verb)?;
            Ok(Command::Version { client, id })
        }
        "emergency" => {
            no_args(&id, verb)?;
            Ok(Command::Emergency { client, id })
        }
        "help" => {
            no_args(&id, verb)?;
            Ok(Command::Help { client, id })
        }
        _ => Err(CtlError {
            id,
            code: 2,
            msg: String::from("unknown command (try `help`)"),
        }),
    }
}

/// Render a success frame. Empty payload means "done, nothing to add".
pub fn render_ok(id: &str, payload: &str) -> String {
    if payload.is_empty() {
        format!("R {id} 0\n")
    } else {
        format!("R {id} 0 {payload}\n")
    }
}

/// Render an error frame, sanitising the message to one line.
///
/// A message with an embedded newline would smuggle a second frame into the
/// client's parser — the same desync class `DESIGN.md` §1.4 blames for
/// dinit's mute connections, at a smaller scale.
pub fn render_err(id: &str, code: u8, msg: &str) -> String {
    let clean: String = msg
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    format!("E {id} {code} {clean}\n")
}

/// A monotonic age in the compact form `4m12s` (`DESIGN.md` §7).
///
/// Pure arithmetic over milliseconds: days, hours, minutes, seconds, largest
/// two units shown. Sub-second ages read `0s` — the resolution below a
/// second is noise on a status line.
pub fn format_age(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else if s < 86400 {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d{}h", s / 86400, (s % 86400) / 3600)
    }
}

/// One service's `status` payload: everything `cat` cannot tell you.
///
/// ```text
/// sshd running desired=up pid=421 up=4m12s restarts=2/5
/// ```
/// `pid` and `up` are `-` when no process exists. Pure and unit-tested; the
/// supervisor only fills in the arguments.
pub fn status_payload(facts: &StatusFacts<'_>) -> String {
    let st = match facts.state {
        State::Stopped => "stopped",
        State::Starting => "starting",
        State::Running => "running",
        State::Stopping => "stopping",
    };
    let de = match facts.desired {
        Desired::Up => "up",
        Desired::Down => "down",
    };
    let pid_s = facts
        .pid
        .map_or_else(|| String::from("-"), |p| p.to_string());
    let up_s = facts.started_at_ms.map_or_else(
        || String::from("-"),
        |t| format_age(facts.now_ms.saturating_sub(t)),
    );
    format!(
        "{} {st} desired={de} pid={pid_s} up={up_s} restarts={}/{}",
        facts.name, facts.restarts, facts.budget_cap
    )
}

/// The facts one `status` line reports.
///
/// Bundled so the call below does not grow a ninth argument the day the next
/// fact is added — eight positional parameters of three types is already past
/// what a reader can hold, and the linter agrees.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StatusFacts<'a> {
    /// Service name.
    pub name: &'a str,
    /// What is actually true.
    pub state: State,
    /// What the operator wants.
    pub desired: Desired,
    /// Main child pid, if any.
    pub pid: Option<i32>,
    /// Monotonic ms the current process started, if any.
    pub started_at_ms: Option<u64>,
    /// Now, for the age computation.
    pub now_ms: u64,
    /// Restarts performed.
    pub restarts: u32,
    /// Restart budget capacity (`n` in `restarts=n/N`).
    pub budget_cap: u32,
}

/// One connected client: unparsed input plus unflushed replies.
struct Client {
    fd: RawFd,
    input: Vec<u8>,
    output: Vec<u8>,
    /// A protocol violation was already answered: deliver the queued reply,
    /// then hang up. The client stays readable until then, but nothing more
    /// is parsed from it.
    close_when_drained: bool,
    /// Who is on the other end, captured at accept time.
    ///
    /// `None` means the platform could not say, and the mutating verbs are
    /// refused: a socket that cannot name its caller must not be a socket
    /// that can power the machine off.
    peer: Option<zrt::sys::PeerCred>,
    /// The path of the peer's binary, when it could be read.
    peer_exe: Option<PathBuf>,
}

/// The listening control socket and its clients.
///
/// Owns every fd it hands out. `Drop` closes them and unlinks the path, so a
/// dead supervisor never leaves a stale socket behind for the next one to
/// trip over — and a stale path at `bind` time is unlinked by
/// [`zrt::sys::unix_listener`], which is what makes a supervisor restart
/// after a crash bind cleanly.
pub struct CtlServer {
    listener: RawFd,
    path: PathBuf,
    clients: Vec<Client>,
    /// The user this supervisor runs as: the peer a same-user connection is
    /// compared against.
    self_uid: u32,
}

impl CtlServer {
    /// Bind `path` and start listening. Fails openly (caller degrades to
    /// "no control socket"); a supervisor that cannot be told things still
    /// supervises.
    pub fn bind(path: &Path) -> io::Result<CtlServer> {
        let listener = zrt::sys::unix_listener(path)?;
        Ok(CtlServer {
            listener,
            path: path.to_path_buf(),
            clients: Vec::new(),
            self_uid: zrt::sys::geteuid(),
        })
    }

    /// The listener fd, for reactor registration by the owner.
    pub fn listener_fd(&self) -> RawFd {
        self.listener
    }

    /// Accept every pending connection, registering each read-ready.
    ///
    /// Runs unconditionally every event-loop pass: the listener is
    /// non-blocking, so an empty queue costs one failing `accept` and nothing
    /// else. A failed accept ends the drain — `WouldBlock` means empty, and
    /// anything else is announced once and retried next pass. Never fatal:
    /// a control socket that cannot accept is degraded, not dead.
    pub fn accept_pending(&mut self, reactor: &mut dyn Reactor) {
        loop {
            match zrt::sys::unix_accept(self.listener) {
                Ok(fd) => {
                    if reactor.add(fd, Interest::Read).is_err() {
                        let _ = zrt::sys::close(fd);
                        continue;
                    }
                    // Who is calling, captured while the connection is still
                    // fresh: the peer can close its end, but the kernel
                    // keeps the credentials for the life of the socket.
                    let peer = zrt::sys::peercred(fd).ok();
                    let peer_exe = peer.and_then(|p| {
                        if p.pid > 0 {
                            std::fs::read_link(format!("/proc/{}/exe", p.pid)).ok()
                        } else {
                            None
                        }
                    });
                    self.clients.push(Client {
                        fd,
                        input: Vec::new(),
                        output: Vec::new(),
                        close_when_drained: false,
                        peer,
                        peer_exe,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    zrt::report::announce_degradation(&format!("control accept: {e}"));
                    break;
                }
            }
        }
    }

    /// Read every client and parse complete lines into [`Command`]s.
    ///
    /// Clients that hit EOF, a read error, or an input buffer past the cap
    /// are unregistered and closed inside; their fds are gone on return, so
    /// the caller must not touch them. A line that violates the protocol
    /// (overlong, non-UTF-8) is answered in-band (`E 0 1 …`) and the
    /// connection is hung up once that reply drains — the client hears why
    /// before it goes. Plain parse errors (`E <id> …`) do not kill the
    /// connection: one bad line is a typo, not an attack.
    pub fn pump(&mut self, reactor: &mut dyn Reactor) -> Vec<Command> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.clients.len() {
            let fd = self.clients[i].fd;
            let mut dead = false;
            loop {
                let mut chunk = [0u8; 1024];
                match zrt::sys::read(fd, &mut chunk) {
                    Ok(0) => {
                        dead = true;
                        break;
                    }
                    Ok(n) => {
                        self.clients[i].input.extend_from_slice(&chunk[..n]);
                        if self.clients[i].input.len() > MAX_BUFFERED_BYTES {
                            dead = true;
                            break;
                        }
                        if n < chunk.len() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        dead = true;
                        break;
                    }
                }
            }
            if !dead {
                loop {
                    // Quarantined: the error reply is queued, the rest of the
                    // input is evidence for nobody. It stays buffered (bounded
                    // by the cap above) until the reply drains.
                    if self.clients[i].close_when_drained {
                        break;
                    }
                    let pos = match self.clients[i].input.iter().position(|&b| b == b'\n') {
                        Some(p) => p,
                        None => break,
                    };
                    let raw: Vec<u8> = self.clients[i].input.drain(..=pos).collect();
                    let mut line = raw.as_slice();
                    if line.ends_with(b"\n") {
                        line = &line[..line.len() - 1];
                    }
                    if line.ends_with(b"\r") {
                        line = &line[..line.len() - 1];
                    }
                    if line.len() > MAX_LINE_BYTES {
                        let frame = render_err("0", 1, "line too long");
                        self.clients[i].output.extend_from_slice(frame.as_bytes());
                        self.clients[i].close_when_drained = true;
                        break;
                    }
                    let text = match core::str::from_utf8(line) {
                        Ok(t) => t,
                        Err(_) => {
                            let frame = render_err("0", 1, "line is not UTF-8");
                            self.clients[i].output.extend_from_slice(frame.as_bytes());
                            self.clients[i].close_when_drained = true;
                            break;
                        }
                    };
                    match parse_line(fd, text) {
                        Ok(cmd) => {
                            // The credential check happens here, before the
                            // command reaches the supervisor: a refused verb
                            // is answered in band and never becomes an
                            // action, so nothing to undo afterwards.
                            let allowed = authorize(
                                self.clients[i].peer.as_ref(),
                                self.clients[i].peer_exe.as_deref(),
                                self.self_uid,
                                cmd.verb_class(),
                            );
                            if allowed {
                                out.push(cmd);
                            } else {
                                let id = cmd_id(&cmd);
                                let frame = render_err(
                                    &id,
                                    6,
                                    "permission denied: this peer may not run that verb",
                                );
                                self.clients[i].output.extend_from_slice(frame.as_bytes());
                            }
                        }
                        Err(e) => {
                            let frame = render_err(&e.id, e.code, &e.msg);
                            self.clients[i].output.extend_from_slice(frame.as_bytes());
                        }
                    }
                }
            }
            if dead {
                let _ = reactor.remove(fd);
                let _ = zrt::sys::close(fd);
                self.clients.remove(i);
            } else {
                i += 1;
            }
        }
        out
    }

    /// Queue a reply frame for one client.
    ///
    /// A client that already went away simply misses it: the reply is gone
    /// but the action it reports stands. Replies are never reordered — one
    /// FIFO per client, flushed in order.
    pub fn reply(&mut self, to: RawFd, frame: &str) {
        if let Some(c) = self.clients.iter_mut().find(|c| c.fd == to) {
            c.output.extend_from_slice(frame.as_bytes());
        }
    }

    /// Write out every client's pending replies.
    ///
    /// `WouldBlock` stops a client where it is; the remainder goes out next
    /// pass. A client whose replies exceed [`MAX_BUFFERED_BYTES`] is dropped:
    /// it is not reading, and feeding it forever is the heap growth the cap
    /// exists to prevent. Fatal errors drop it at once.
    pub fn flush(&mut self, reactor: &mut dyn Reactor) {
        let mut i = 0;
        while i < self.clients.len() {
            let mut dead = false;
            while !self.clients[i].output.is_empty() {
                let fd = self.clients[i].fd;
                match zrt::sys::write(fd, &self.clients[i].output) {
                    Ok(0) => break,
                    Ok(n) => {
                        self.clients[i].output.drain(..n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        dead = true;
                        break;
                    }
                }
            }
            if !dead && self.clients[i].output.len() > MAX_BUFFERED_BYTES {
                dead = true;
            }
            // Quarantined and answered: hang up now that the reply is out.
            if !dead && self.clients[i].output.is_empty() && self.clients[i].close_when_drained {
                dead = true;
            }
            if dead {
                let fd = self.clients[i].fd;
                let _ = reactor.remove(fd);
                let _ = zrt::sys::close(fd);
                self.clients.remove(i);
            } else {
                i += 1;
            }
        }
    }
}

impl Drop for CtlServer {
    fn drop(&mut self) {
        for c in self.clients.drain(..) {
            let _ = zrt::sys::close(c.fd);
        }
        let _ = zrt::sys::close(self.listener);
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Command, CtlError> {
        parse_line(7, line)
    }

    /// Who may run what, spelled out as a table.
    ///
    /// The policy is the whole point of the check, and a policy that only
    /// exists inside an `if` in the accept path is a policy nobody can
    /// check. Each row is a peer; the columns are the two verb classes.
    #[test]
    fn the_credential_policy_is_explicit() {
        let root = zrt::sys::PeerCred {
            pid: 10,
            uid: 0,
            gid: 0,
        };
        let owner = zrt::sys::PeerCred {
            pid: 11,
            uid: 1000,
            gid: 1000,
        };
        let other = zrt::sys::PeerCred {
            pid: 12,
            uid: 1001,
            gid: 1001,
        };
        let zctl = Path::new("/sbin/zctl");
        let shell = Path::new("/bin/sh");

        // Reading is always allowed.
        for peer in [&root, &owner, &other] {
            assert!(
                authorize(Some(peer), None, 1000, VerbClass::Read),
                "a peer may always look"
            );
        }
        // Root may do anything.
        assert!(authorize(Some(&root), None, 1000, VerbClass::Mutate));
        assert!(authorize(Some(&root), None, 1000, VerbClass::Power));
        // The owner's own user may manage services.
        assert!(authorize(Some(&owner), Some(zctl), 1000, VerbClass::Mutate));
        assert!(authorize(
            Some(&owner),
            Some(shell),
            1000,
            VerbClass::Mutate
        ));
        // ...but the power verbs take the known client.
        assert!(authorize(Some(&owner), Some(zctl), 1000, VerbClass::Power));
        assert!(
            !authorize(Some(&owner), Some(shell), 1000, VerbClass::Power),
            "any program of the owner's user must not be able to power the machine off"
        );
        // Another user may not change anything.
        assert!(!authorize(
            Some(&other),
            Some(zctl),
            1000,
            VerbClass::Mutate
        ));
        assert!(!authorize(Some(&other), Some(zctl), 1000, VerbClass::Power));
        // An unknown peer may not change anything: a platform that cannot
        // say who is calling gets the strict answer.
        assert!(!authorize(None, None, 1000, VerbClass::Mutate));
        assert!(!authorize(None, None, 1000, VerbClass::Power));
        // A peer whose binary could not be named falls back to the uid.
        assert!(authorize(Some(&owner), None, 1000, VerbClass::Power));
    }

    /// The three classes of verb, spelled out.
    #[test]
    fn the_verbs_fall_into_three_classes() {
        for line in ["C 1 status sshd", "C 1 list", "C 1 version", "C 1 help"] {
            let cmd = parse(line).expect("parses");
            assert_eq!(cmd.verb_class(), VerbClass::Read, "{line}");
        }
        for line in [
            "C 1 start sshd",
            "C 1 stop sshd",
            "C 1 restart sshd",
            "C 1 kick sshd",
            "C 1 reload-all",
        ] {
            let cmd = parse(line).expect("parses");
            assert_eq!(cmd.verb_class(), VerbClass::Mutate, "{line}");
        }
        for line in [
            "C 1 poweroff",
            "C 1 reboot",
            "C 1 halt",
            "C 1 suspend",
            "C 1 hibernate",
        ] {
            let cmd = parse(line).expect("parses");
            assert_eq!(cmd.verb_class(), VerbClass::Power, "{line}");
        }
    }

    #[test]
    fn every_verb_parses_with_its_arity() {
        assert_eq!(
            parse("C 1 status sshd"),
            Ok(Command::Status {
                client: 7,
                id: String::from("1"),
                name: String::from("sshd"),
            })
        );
        assert!(matches!(parse("C 2 list"), Ok(Command::List { .. })));
        assert!(matches!(parse("C 3 start a"), Ok(Command::Start { .. })));
        assert!(matches!(parse("C 4 stop a"), Ok(Command::Stop { .. })));
        assert!(matches!(
            parse("C 5 restart a"),
            Ok(Command::Restart { .. })
        ));
        assert!(matches!(parse("C 6 kick a"), Ok(Command::Kick { .. })));
        assert!(matches!(
            parse("C 7 reload-all"),
            Ok(Command::ReloadAll { .. })
        ));
        assert!(matches!(
            parse("C 8 poweroff"),
            Ok(Command::Power {
                action: PowerAction::Poweroff,
                ..
            })
        ));
        assert!(matches!(
            parse("C 9 reboot"),
            Ok(Command::Power {
                action: PowerAction::Reboot,
                ..
            })
        ));
        assert!(matches!(
            parse("C 10 halt"),
            Ok(Command::Power {
                action: PowerAction::Halt,
                ..
            })
        ));
        assert!(matches!(
            parse("C 11 suspend"),
            Ok(Command::Power {
                action: PowerAction::Suspend,
                ..
            })
        ));
        assert!(matches!(
            parse("C 12 hibernate"),
            Ok(Command::Power {
                action: PowerAction::Hibernate,
                ..
            })
        ));
        assert!(matches!(parse("C 13 version"), Ok(Command::Version { .. })));
        assert!(matches!(parse("C 14 help"), Ok(Command::Help { .. })));
        assert!(parse("C 15 poweroff now").is_err(), "power takes no args");
    }

    #[test]
    fn malformed_frames_name_an_id_and_a_code() {
        let e = parse("garbage").expect_err("must not parse");
        assert_eq!((e.id.as_str(), e.code), ("0", 1));
        let e = parse("C 12 bogus-cmd").expect_err("unknown verb");
        assert_eq!((e.id.as_str(), e.code), ("12", 2));
        let e = parse("C 13 status").expect_err("missing name");
        assert_eq!((e.id.as_str(), e.code), ("13", 2));
        let e = parse("C 14 list extra").expect_err("extra arg");
        assert_eq!((e.id.as_str(), e.code), ("14", 2));
        let e = parse("C 15 stop a b").expect_err("extra arg");
        assert_eq!((e.id.as_str(), e.code), ("15", 2));
    }

    #[test]
    fn error_frames_cannot_smuggle_a_second_frame() {
        let frame = render_err("3", 2, "bad\nR 3 0 forged\n");
        assert_eq!(frame, "E 3 2 bad R 3 0 forged \n");
        assert_eq!(frame.lines().count(), 1);
    }

    #[test]
    fn ages_read_compact() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(4_000), "4s");
        assert_eq!(format_age(252_000), "4m12s");
        assert_eq!(format_age(3_600_000), "1h0m");
        assert_eq!(format_age(90_000_000), "1d1h");
    }

    #[test]
    fn status_payload_covers_live_and_dead() {
        let live = status_payload(&StatusFacts {
            name: "sshd",
            state: State::Running,
            desired: Desired::Up,
            pid: Some(421),
            started_at_ms: Some(1_000),
            now_ms: 253_000,
            restarts: 2,
            budget_cap: 5,
        });
        assert_eq!(
            live,
            "sshd running desired=up pid=421 up=4m12s restarts=2/5"
        );
        let dead = status_payload(&StatusFacts {
            name: "gone",
            state: State::Stopped,
            desired: Desired::Up,
            pid: None,
            started_at_ms: None,
            now_ms: 0,
            restarts: 0,
            budget_cap: 5,
        });
        assert_eq!(dead, "gone stopped desired=up pid=- up=- restarts=0/5");
    }
}
