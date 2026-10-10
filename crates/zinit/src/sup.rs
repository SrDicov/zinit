//! `zinit --sup` — the supervisor process.
//!
//! Where `zcore` decides and `zservice`/`zrt` do, this module is the loop that
//! connects them (`DESIGN.md` §3):
//!
//! ```text
//! wait(timeout) → signals → reap → poll services → reconcile → act
//! ```
//!
//! Four rules shape every line below.
//!
//! **Nothing sleeps in the loop.** Every wait is a
//! [`zrt::reactor::Reactor::wait`] whose timeout is computed from the deadlines
//! the loop already knows about, capped at [`WAIT_CAP_MS`]. A `sleep` here would
//! freeze *every* service for its duration — the defect `DESIGN.md` §1.4
//! attributes to dinit's `sleep(1)` inside `kill_all_on_stop`.
//!
//! **The core decides; this file obeys.** Every state change goes through
//! [`zcore::apply`] or [`zcore::reconcile()`], and this module never writes a
//! `State` of its own accord. The single exception is the one the core cannot
//! express: a stop the *operator* asked for, where nothing emits a `SIGTERM`
//! (see [`Sup::begin_stop`]).
//!
//! **Nothing here can panic.** A panic in this process is a supervisor that has
//! forgotten every pid it was tracking, and `panic = "abort"` turns that into a
//! machine whose services are nobody's. So: no `unwrap`, no `expect`, and every
//! index into a parallel array is guarded.
//!
//! **Zero `unsafe`.** Every syscall this file needs already has a wrapper that
//! carries its own `// SAFETY:` comment, which is the only place that comment can
//! be reviewed next to the obligation it justifies.

#![deny(missing_docs)]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};

use zconfig::parser::parse_service_with_diagnostics;
use zconfig::{RunAs, ServiceDesc, build_plan};
use zcore::{
    Action, Desired, Event, Idx, LogLevel, Plan, Runtime, ServiceKind, SignalKind, State, Tick,
    Transition, apply, kick, reconcile,
};
use zrt::childproc::{ChildTracker, new_child_tracker};
use zrt::reactor::{Interest, Reactor, new_reactor};
use zrt::report::announce_degradation;
use zrt::signals::{Signal, SignalSet, SignalSource, block_all, new_signal_source};
use zservice::identity::resolve_user_group;
use zservice::log::{LogHandle, open_sink, write_line};
use zservice::{ManagedService, SpawnCtx};

use crate::ctl::{self, Command};

/// Ceiling on one `reactor.wait`, in milliseconds.
///
/// Not a tuning knob, a correctness one: the loop wakes at least this often
/// whether or not anything is registered, so a descriptor that never made it
/// into the reactor costs a service latency instead of its lifetime. The nearest
/// real deadline always wins over this cap.
const WAIT_CAP_MS: u64 = 1_000;

/// Shortest gap between two passes while a service is waiting out a restart
/// delay.
///
/// Only ever the *lower* bound on `restart-delay`: a configuration that asks
/// for no delay at all still gets this, because a zero-delay loop over an empty
/// restart bucket is a spin, and a spin is worse than a 20 Hz heartbeat.
const MIN_RESTART_TICK_MS: u64 = 50;

/// Run the supervisor: generators, layers, converge, never return.
///
/// Every failure below is announced and reported as an `Err`, which is what
/// `main` turns into a non-zero exit — the outcome PID 1 relaunches. Services
/// that are already running are left alone on the way out: nothing here kills
/// them, because PID 1's whole argument is that a restarted supervisor
/// reattaches to live services instead of disturbing them.
pub fn run(config: Option<&Path>) -> io::Result<()> {
    let (config_dirs, gen_dir) = resolve_sources(config);
    if let Some(explicit) = config {
        if !explicit.is_dir() {
            return Err(fatal(format!("{}: not a directory", explicit.display())));
        }
    }
    if config_dirs.is_empty() {
        announce_degradation("no configuration layers: supervising nothing until reload-all");
    }
    let (plan, descs) = load_all(&config_dirs, &gen_dir)?;
    Sup::new(plan, descs, config_dirs, gen_dir)?.supervise()
}

/// The supervisor's whole mutable state.
///
/// One struct rather than eight parameters on every helper: the loop's steps all
/// need the plan, the core's state, the services and the three primitives, and
/// passing them around separately is how one of them goes stale.
struct Sup {
    /// The frozen plan. Never patched; a change means a new plan.
    plan: Plan,
    /// The core's state, indexed by `Idx`. Written only by `apply`/`reconcile`.
    runtime: Runtime,
    /// One slot per service, in plan-index order.
    slots: Vec<Slot>,
    reactor: Box<dyn Reactor>,
    sigs: Box<dyn SignalSource>,
    children: Box<dyn ChildTracker>,
    /// Set once an operator asked for a stop, so a `Ctrl-C` storm is not a stop
    /// storm.
    stopping: bool,
    /// Services already announced as out of restart budget.
    ///
    /// The core emits `BudgetExhausted` on *every* reconciliation pass for a
    /// service whose bucket is empty, and an exhausted service is reconciled on
    /// every pass for the rest of the boot. Announcing each one would be a
    /// message a second forever, which is the stderr flood `init.rs` already
    /// gates against. Announced once per supervisor lifetime (a reload starts
    /// a fresh record with its fresh plan); a delay-gated retry never emits
    /// this action at all, so routine spacing stays silent by construction.
    budget_announced: Vec<Idx>,
    /// The services whose core invariants were last found violated, so a
    /// standing violation is announced once instead of once per pass.
    last_offenders: Vec<Idx>,
    /// The control socket server, or `None` when the socket could not be
    /// bound. A supervisor that cannot be told things still supervises: the
    /// loop simply has no commands to serve.
    ctl: Option<ctl::CtlServer>,
    /// Configuration layers, low to high precedence. `reload-all` re-reads
    /// all of them (plus fresh generator output), never just one.
    config_dirs: Vec<PathBuf>,
    /// Generator directory re-run by every `reload-all`.
    gen_dir: PathBuf,
    /// Visible-state directory (contract C6): one `cat`-able file per
    /// service, rewritten only when its content changes.
    state_dir: PathBuf,
    /// False after the first failed state write. Disk trouble must degrade
    /// supervision to "no visible state", never to a per-pass error storm.
    state_usable: bool,
    /// Last content written per slot index, for the change-only dump.
    /// Emptied by a reload (names move), forcing a full re-dump.
    last_state_dump: Vec<String>,
    /// Services a `restart` is owed to: wanted down now, wanted up again as
    /// soon as the old process is gone. Cleared by an explicit `start`/`stop`
    /// and by every reload.
    pending_restart: Vec<Idx>,
    /// Services removed by a reload while their process still lives. They
    /// have no plan entry anymore, so the core cannot reason about them;
    /// they are sent TERM once, reaped like any other child, then dropped.
    retired: Vec<Retired>,
    /// Whether this process is the child subreaper: orphaned grandchildren
    /// of `Forking` services reparent here and are reaped normally. When
    /// false (non-Linux, or a refusing kernel), adopted daemons are watched
    /// with `kill(pid, 0)` polling instead — announced at startup, because a
    /// supervisor that polls where it claimed to reap is a difference bug
    /// reports cannot reproduce.
    subreaper: bool,
}

/// The supervisor's per-service slot: the mechanism, the description it is
/// started from, and the log the core's own lines go to.
struct Slot {
    /// Fds, deadlines and readiness state. The core owns the facts; this owns
    /// the mechanism.
    svc: ManagedService,
    /// The parsed description, aligned to the plan index. Everything
    /// `ServicePlan` deliberately does not carry — command, env, cgroup,
    /// rlimits — lives here, and is what `SpawnCtx` is built from at each start.
    desc: Option<ServiceDesc>,
    /// The supervisor's own handle on the service's log sink, for
    /// [`Action::Log`].
    log: Option<LogHandle>,
    /// What the reactor is currently watching for this service, if anything.
    registered: Option<RawFd>,
}

impl Slot {
    /// The service's name, or a placeholder for a slot that lost its
    /// description. That cannot happen; this keeps a log line from panicking if
    /// it ever did.
    fn name(&self) -> &str {
        self.desc.as_ref().map_or("<unknown>", |d| d.name.as_str())
    }
}

/// A service deleted by `reload-all` while its process still lives.
///
/// No plan entry, no core slot, no future: one SIGTERM already sent, and the
/// only remaining job is to reap the pid, release the custodial fds and drop
/// the entry. A second signal is never sent — escalation belongs to the core,
/// and the core cannot see this service anymore.
struct Retired {
    svc: ManagedService,
    name: String,
}

impl Sup {
    /// Build the primitives and the per-service slots.
    ///
    /// Every failure here is fatal, and the ordering is load-bearing: signals
    /// are blocked *before* the signal source is created, because a signalfd
    /// built over signals that are still deliverable never fires, and nothing
    /// about that failure is visible except a supervisor that quietly stops
    /// reaping (`zrt::signals` module docs).
    fn new(
        plan: Plan,
        descs: Vec<Option<ServiceDesc>>,
        config_dirs: Vec<PathBuf>,
        gen_dir: PathBuf,
    ) -> io::Result<Sup> {
        if let Err(e) = block_all() {
            return Err(fatal(format!("cannot block signals: {e}")));
        }

        // Become the child subreaper before anything forks: orphaned
        // grandchildren of `Forking` services then reparent to us instead of
        // PID 1, so adopted daemons are reaped like any other child. Without
        // it (non-Linux, or a refusing kernel) the supervisor watches adopted
        // pids with `kill(pid, 0)` polling — announced once here, because the
        // pid-recycling race that polling implies is exactly the kind of
        // silent difference a bug report cannot reproduce.
        let subreaper = match zrt::sys::prctl_child_subreaper() {
            Ok(()) => true,
            Err(e) => {
                announce_degradation(&format!(
                    "no child subreaper ({e}); adopted daemons are polled, not reaped"
                ));
                false
            }
        };

        // `SIGCHLD` is what a child death arrives through; the other three are
        // the operator saying stop. Everything else stays blocked rather than
        // being delivered to a default disposition.
        let mut watch = SignalSet::with(Signal::Chld);
        for sig in [Signal::Term, Signal::Int, Signal::Hup] {
            watch.insert(sig);
        }
        let sigs = match new_signal_source(watch) {
            Ok(s) => s,
            Err(e) => return Err(fatal(format!("cannot watch signals: {e}"))),
        };
        let mut reactor = match new_reactor() {
            Ok(r) => r,
            Err(e) => return Err(fatal(format!("no reactor: {e}"))),
        };
        let children = match new_child_tracker() {
            Ok(c) => c,
            Err(e) => return Err(fatal(format!("no child tracker: {e}"))),
        };

        // The two wakeup sources, registered in one loop because the difference
        // between them is zero here: a signal and a dead child are both
        // "something happened, go and look".
        for fd in sigs.fds().iter().chain(children.fds().iter()) {
            if let Err(e) = reactor.add(*fd, Interest::Read) {
                return Err(fatal(format!("cannot watch descriptor {fd}: {e}")));
            }
        }

        let mut runtime = Runtime::from_plan(&plan);
        for idx in 0..runtime.len() {
            runtime.set_desired(idx, Desired::Up);
        }

        let mut slots = Vec::with_capacity(plan.services.len());
        for (idx, (sp, desc)) in plan.services.iter().zip(descs).enumerate() {
            let log = match open_sink(&sp.log) {
                Ok(handle) => Some(handle),
                // Not fatal: the service still runs, it just cannot be told
                // anything. `log = syslog` lands here too — `zservice` refuses
                // it and the spawn will fail on its own — so the operator hears
                // about it once instead of once per start.
                Err(e) => {
                    let name = &sp.name;
                    announce_degradation(&format!("{name}: no log sink ({e})"));
                    None
                }
            };
            slots.push(Slot {
                svc: ManagedService::new(idx),
                desc,
                log,
                registered: None,
            });
        }

        // The capability line (`DESIGN.md` §8.1): a supervisor that silently
        // ended up on its weakest backend cannot have its bug reports
        // reproduced. Reported, never branched on.
        announce_degradation(&format!(
            "supervising {} services: reactor {}, signals {}, children {}",
            slots.len(),
            reactor.kind().name(),
            sigs.kind().name(),
            children.kind().name()
        ));

        // The control socket (contract C4). Bound before the loop so a client
        // that connects during the first pass is accepted, not refused. A
        // failure degrades to "no control socket": the loop below checks the
        // `Option` every pass, and everything else works without it.
        let socket_path = ctl_socket_path();
        // The parent is created, not assumed: PID 1 mounts a fresh tmpfs on
        // `/run` at boot, so `/run/zinit` never survives a reboot — without
        // this the first boot of every real machine would have no control
        // socket, found only by booting real hardware (a test's scratch dir
        // always exists, so no test could catch it).
        if let Some(parent) = socket_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                announce_degradation(&format!(
                    "cannot create {} ({e}); running without a control socket",
                    parent.display()
                ));
            }
        }
        let ctl = match ctl::CtlServer::bind(&socket_path) {
            Ok(server) => match reactor.add(server.listener_fd(), Interest::Read) {
                Ok(()) => Some(server),
                Err(e) => {
                    announce_degradation(&format!(
                        "cannot watch {} ({e}); running without a control socket",
                        socket_path.display()
                    ));
                    None
                }
            },
            Err(e) => {
                announce_degradation(&format!(
                    "no control socket at {} ({e}); running without one",
                    socket_path.display()
                ));
                None
            }
        };

        // The visible-state directory (contract C6). Created once; a failure
        // disables the dumps rather than failing the boot — supervision does
        // not depend on observability, however embarrassing that is to admit.
        let state_dir = ctl_state_dir();
        let state_usable = match std::fs::create_dir_all(&state_dir) {
            Ok(()) => true,
            Err(e) => {
                announce_degradation(&format!(
                    "cannot create {} ({e}); state dumps disabled",
                    state_dir.display()
                ));
                false
            }
        };

        Ok(Sup {
            plan,
            runtime,
            slots,
            reactor,
            sigs,
            children,
            stopping: false,
            budget_announced: Vec::new(),
            last_offenders: Vec::new(),
            ctl,
            config_dirs,
            gen_dir,
            state_dir,
            state_usable,
            last_state_dump: Vec::new(),
            pending_restart: Vec::new(),
            retired: Vec::new(),
            subreaper,
        })
    }

    /// The event loop. Never returns `Ok`.
    fn supervise(&mut self) -> io::Result<()> {
        let mut first_pass = true;
        loop {
            let now = zrt::clock::now_ms();
            // The first pass waits for nothing: nothing is registered yet and
            // the reconciler has not run, so there is no descriptor that could
            // ever wake this loop up.
            let timeout = if first_pass {
                first_pass = false;
                0
            } else {
                self.arm(now)
            };

            // The ready list is deliberately not dispatched on. Every service is
            // polled on every pass anyway, because each poll is a non-blocking
            // read that answers "nothing yet" for free, and a dispatch table
            // keyed on this vector is one more thing that can forget a service.
            // What the registrations in `arm` buy is *latency*: a notify that
            // arrives makes `wait` return at once instead of at the next
            // deadline.
            let _ready = match self.reactor.wait(Some(timeout)) {
                Ok(events) => events,
                // Not retryable: a loop that cannot wait cannot supervise.
                // Exiting non-zero is what PID 1 is for, and the services stay
                // up because nothing on this path signals them.
                Err(e) => return Err(io::Error::other(format!("reactor: {e}"))),
            };

            let now = zrt::clock::now_ms();
            self.drain_signals();
            self.reap(now);
            self.poll(now);
            self.serve_ctl(now);
            let tick = self.reconcile_now(now);
            self.execute(&tick.actions, now);
            if !tick.changed.is_empty() {
                self.check_invariants();
            }
            self.dump_state();
            self.flush_ctl();
        }
    }

    /// Bring the reactor's registrations in line with what the services need,
    /// and answer "how long may this loop sleep".
    ///
    /// Every answer is a deadline the loop can already see:
    ///
    /// * `PollNeed::timeout_ms` — when to re-run a `ping`/`tcp` probe.
    /// * `ready_due_at` / `start_due_at` / `stop_due_at` — the core's own
    ///   deadlines, without which `check_deadlines` would only run by accident.
    ///
    /// `PollNeed::timeout_ms` is where `ping` and `tcp` earn their keep: they
    /// need no descriptor, only a moment to retry, and ignoring the `timeout_ms`
    /// half is what makes them work for the first 250 ms and then stop working
    /// forever. The cap is the floor under all of it.
    fn arm(&mut self, now: u64) -> u64 {
        let mut deadline = now.saturating_add(WAIT_CAP_MS);
        for idx in 0..self.runtime.len() {
            let need = self.slots[idx].svc.poll_need(now);
            self.resync(idx, need.fd);
            if let Some(at) = need.timeout_ms {
                deadline = deadline.min(at);
            }
            let core = self.runtime.get(idx);
            for at in [core.ready_due_at, core.start_due_at, core.stop_due_at]
                .into_iter()
                .flatten()
            {
                deadline = deadline.min(at);
            }
            // A restart the core is holding back. `Bucket` does not say when its
            // delay expires, but the plan does — and waking one `delay_ms` from
            // now is never too early, because the attempt it is waiting on
            // happened at or before `now`. Without this a crash-looping service
            // would restart at the loop's idle cadence instead of at its own.
            //
            // Floored, because `restart-delay = 0` would otherwise ask this loop
            // to come back immediately, forever, for a service whose bucket is
            // already empty. `restart_suppressed` is excluded for the same
            // reason one notch higher: nothing will be spawned until an operator
            // says so, and a wakeup per pass to be told nothing is the busy loop
            // this loop does not have.
            let sp = &self.plan.services[idx];
            let waiting = sp.kind.has_process()
                && core.desired == Desired::Up
                && core.state == State::Stopped
                && !core.restart_suppressed;
            if waiting {
                let delay = sp.restart_budget.delay_ms.max(MIN_RESTART_TICK_MS);
                deadline = deadline.min(now.saturating_add(delay));
            }
            // A watchdog that expires unnoticed is a hung service kept
            // running: wake in time to catch it, like any other deadline the
            // loop can already see. Second granularity is plenty — the
            // budget itself is counted in whole seconds.
            if let Some(secs) = sp.watchdog_sec {
                if matches!(core.state, State::Running | State::Starting) {
                    let last = core.last_watchdog_ms.or(core.started_at).unwrap_or(now);
                    deadline = deadline.min(last.saturating_add(secs.saturating_mul(1000)));
                }
            }
        }
        deadline.saturating_sub(now)
    }

    /// Point the reactor at whatever descriptor service `idx` needs now, and at
    /// nothing at all when it needs none.
    ///
    /// `remove` is only ever asked for when the old descriptor is still open.
    /// `zservice` closes its own the moment it becomes conclusive
    /// (`poll_exec`, `poll_ready`, `cleanup`), and every backend drops a closed
    /// descriptor from its set by itself — so the common transition, to "nothing
    /// to watch", is bookkeeping and not a syscall that would answer `EBADF`.
    fn resync(&mut self, idx: Idx, fd: Option<RawFd>) {
        if self.slots[idx].registered == fd {
            return;
        }
        if let (Some(old), Some(_)) = (self.slots[idx].registered, fd) {
            let _ = self.reactor.remove(old);
        }
        self.slots[idx].registered = fd;
        if let Some(fd) = fd {
            // Named so the `add` and the report read as one decision rather than
            // as two unrelated statements.
            let watched = self.reactor.add(fd, Interest::Read);
            if let Err(e) = watched {
                let name = &self.plan.services[idx].name;
                announce_degradation(&format!("{name}: cannot watch fd {fd} ({e})"));
            }
        }
    }

    /// Consume every pending signal.
    ///
    /// `SIGCHLD` is a *wakeup*, not the work: the reap runs on every pass, so a
    /// burst of deaths the kernel coalesced into one `SIGCHLD` is still reported
    /// one child at a time by `reap` itself.
    fn drain_signals(&mut self) {
        let pending = match self.sigs.drain() {
            Ok(signals) => signals,
            Err(e) => {
                announce_degradation(&format!("cannot read signals: {e}"));
                return;
            }
        };
        for sig in pending {
            match sig {
                Signal::Term | Signal::Int | Signal::Hup => self.request_stop(sig),
                Signal::Chld => {}
                // Unwatched, therefore unreachable; anything else is blocked
                // rather than delivered.
                _ => {}
            }
        }
    }

    /// An operator asked for a stop: everything wanted down, and the reconciler
    /// converges from there.
    ///
    /// No `reboot`, no `poweroff`, no shutdown sequence. This build has no
    /// control socket to be told what the operator meant, and assuming that a
    /// `SIGTERM` means "power the machine off" would turn every `Ctrl-C` at a
    /// console into a reboot.
    fn request_stop(&mut self, sig: Signal) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        for idx in 0..self.runtime.len() {
            self.runtime.set_desired(idx, Desired::Down);
        }
        announce_degradation(&format!(
            "{} received: stopping every service. This build has no poweroff path, \
             so the machine stays up and idle afterwards",
            sig.name()
        ));
    }

    /// Reap every child that has finished, and hand the fact to the core.
    fn reap(&mut self, now: u64) {
        let dead = match self.children.reap() {
            Ok(dead) => dead,
            Err(e) => {
                announce_degradation(&format!("cannot reap children: {e}"));
                return;
            }
        };
        for (pid, status) in dead {
            let idx = self.slots.iter().position(|s| s.svc.pid() == Some(pid));
            if let Some(idx) = idx {
                let forking = self.plan.services[idx].kind == ServiceKind::Forking;
                if let Some(event) = self.slots[idx].svc.on_waitpid(forking, status) {
                    self.feed(&event, now);
                }
                self.slots[idx].svc.cleanup();
                // `cleanup` closed the custodial fds, so the numbers the reactor
                // knew are meaningless now.
                self.slots[idx].registered = None;
            }
            // A service deleted by `reload-all` while its process still lived.
            // There is no core slot to feed — the plan it belonged to is gone —
            // so the death is only released (fds, tracker entry) and dropped.
            if let Some(pos) = self.retired.iter().position(|r| r.svc.pid() == Some(pid)) {
                self.retired[pos].svc.cleanup();
                self.retired.remove(pos);
            }
            // Also for a pid we never started, a child inherited from the
            // process we replaced: leaving it tracked would make every later
            // reap report it again. Unknown pids are not an error — the pidfd
            // backend has already untracked it inside `reap`.
            if let Err(e) = self.children.untrack(pid) {
                announce_degradation(&format!("cannot untrack pid {pid}: {e}"));
            }
        }
    }

    /// Per-service poll: exec verdict, readiness, deadlines, and the one stop
    /// the core cannot ask for.
    fn poll(&mut self, now: u64) {
        for idx in 0..self.runtime.len() {
            // 1. Did the image really get replaced? `EOF` on the exec pipe is
            //    the proof, and it is also what makes the process group
            //    signallable.
            let exec = match self.slots[idx].svc.poll_exec() {
                Ok(event) => event,
                Err(e) => {
                    let name = self.name_of(idx);
                    announce_degradation(&format!("{name}: exec pipe: {e}"));
                    None
                }
            };
            if let Some(event) = exec {
                self.feed(&event, now);
            }

            // 2. Forking adoption: the launcher is gone, the daemon may have
            //    arrived. Runs before readiness so an adopted service can
            //    ready on the same pass its pid file appeared — and the held
            //    fork exit resolves here too, so a start that failed fast
            //    never waits out its start timeout, and a clean launcher
            //    without a daemon never wedges past it either.
            if self.plan.services[idx].kind == ServiceKind::Forking
                && self.runtime.state_at(idx) == State::Starting
                && !self.slots[idx].svc.adopted()
            {
                if let Some(event) = self.slots[idx].svc.poll_forking_adopt(&self.plan) {
                    // Register before feeding: the same no-window rule as
                    // spawn — a daemon that dies between adoption and
                    // tracking is a zombie nobody reaps.
                    if let Event::AdoptedPid { pid, .. } = event {
                        if let Err(e) = self.children.track(pid) {
                            let name = self.name_of(idx);
                            announce_degradation(&format!(
                                "{name}: cannot track adopted pid {pid} ({e})"
                            ));
                        }
                    }
                    self.feed(&event, now);
                }
                let stopping = self.runtime.state_at(idx) == State::Stopping;
                if let Some(event) = self.slots[idx].svc.poll_forking_unstick(stopping) {
                    self.feed(&event, now);
                }
            }

            // 3. Readiness while Starting; watchdog pings for life. A notify
            //    pipe that closed at Ready would starve the watchdog, so
            //    watchdog services keep theirs open (see `poll_ready`) and are
            //    polled in every live state. Anything else past Starting has
            //    nothing to say: its waiter is `None` and its pipe is gone.
            //
            //    The old comment's warning still stands for the non-watchdog
            //    path: `poll_ready` answers `Ready` on *every* call once no
            //    handshake is left, so calling it outside `Starting` (without
            //    a watchdog to justify it) would mark services `Running`
            //    before their image was confirmed.
            //
            //    The pacing of `ping` and `tcp` lives in `zservice`; `arm` is
            //    what guarantees this call is made at all.
            let await_handshake = self.plan.services[idx].ready.is_handshake()
                && self.runtime.state_at(idx) == State::Starting;
            let feeding_watchdog = self.plan.services[idx].watchdog_sec.is_some()
                && self.runtime.state_at(idx) == State::Running;
            if await_handshake || feeding_watchdog {
                let ready = match self.slots[idx].svc.poll_ready(&self.plan, now) {
                    Ok(event) => event,
                    Err(e) => {
                        let name = self.name_of(idx);
                        announce_degradation(&format!("{name}: readiness: {e}"));
                        None
                    }
                };
                if let Some(event) = ready {
                    self.feed(&event, now);
                }
            }

            // 4. Adopted liveness where the kernel does not reap for us.
            //    Without a subreaper an adopted grandchild belongs to PID 1,
            //    so our waitpid never sees it die. `kill(pid, 0)` is the
            //    backstop: ESRCH feeds `OrphanReaped` — the event for "reaped
            //    but not one we spawned", which is exactly what an adopted
            //    daemon unseen by our reaper is. The pid-recycling race is
            //    real and documented on the flag; the alternative on these
            //    platforms is never noticing at all.
            if !self.subreaper
                && self.slots[idx].svc.adopted()
                && let Some(pid) = self.slots[idx].svc.pid()
                && matches!(
                    self.runtime.state_at(idx),
                    State::Starting | State::Running | State::Stopping
                )
                && !pid_alive(pid)
            {
                self.feed(&Event::OrphanReaped(idx), now);
            }

            // 5. Expired deadlines, mapped to events by the core itself.
            let slot = &mut self.slots[idx];
            let due = slot.svc.check_deadlines(&self.runtime, &self.plan, now);
            for event in due {
                self.feed(&event, now);
            }
            // A lenient readiness expiry is announced rather than merely logged:
            // "up anyway" with no word at all is the silence `DESIGN.md` §6
            // refuses.
            if let Some(warning) = self.slots[idx].svc.take_warning() {
                announce_degradation(&warning);
            }

            // 6. A stop nobody asked the core for.
            self.begin_stop(idx, now);
        }
    }

    /// The one stop the core cannot ask for.
    ///
    /// `reconcile`'s stop pass records that a service is wanted down and stops
    /// there: the `SIGTERM` comes from the *transition function*, which only
    /// emits one when `Desired` flips as part of a cascade. An operator request
    /// flips `Desired` from outside the core, so without this the service would
    /// sit in `Running` forever — wanted down, and never asked.
    ///
    /// [`ManagedService::stop_signal`] reports the signal that is due and arms
    /// `stop_due_at` in the same call, which is why the state flip comes after
    /// it: `Stopping` without that deadline breaks an invariant the loop checks
    /// at the end of the pass.
    ///
    /// The escalation is not handled here either. Once the deadline exists,
    /// `check_deadlines` reports `StopTimeout` and the core emits the `SIGKILL`.
    fn begin_stop(&mut self, idx: Idx, now: u64) {
        if self.slots[idx].svc.pid().is_none() {
            return;
        }
        if self.runtime.get(idx).desired != Desired::Down {
            return;
        }
        if !matches!(self.runtime.state_at(idx), State::Running | State::Starting) {
            return;
        }
        let slot = &mut self.slots[idx];
        let kind = match slot.svc.stop_signal(&mut self.runtime, &self.plan, now) {
            Some(kind) => kind,
            // Somebody else owns this stop already: a cascade or a timeout armed
            // the deadline inside the core.
            None => return,
        };
        let state = self.runtime.get_mut(idx);
        state.state = State::Stopping;
        state.start_due_at = None;
        state.ready_due_at = None;
        self.send(idx, kind);
    }

    /// Signal one service: the process group once it provably exists, the bare
    /// pid until then.
    ///
    /// Between `fork` and the child's `setsid` the group does not exist, and a
    /// `kill(-pgid)` aimed at it is silently lost — which is how a wedged service
    /// survives its own `stop-timeout` and every `SIGKILL` after it.
    /// `group_is_signallable` is the only thing allowed to conclude otherwise,
    /// and only once the exec pipe has proved the image was replaced.
    fn send(&self, idx: Idx, kind: SignalKind) {
        let Some(slot) = self.slots.get(idx) else {
            return;
        };
        let Some(pid) = slot.svc.pid() else {
            return;
        };
        let signal = match kind {
            SignalKind::Term => Signal::Term,
            SignalKind::Kill => Signal::Kill,
        };
        let group = match (slot.svc.group_is_signallable(), slot.svc.pgid()) {
            (true, Some(pgid)) => Some(pgid),
            _ => None,
        };
        let outcome = match group {
            Some(pgid) => zrt::sys::kill_group(pgid, signal),
            None => zrt::sys::kill_process(pid, signal),
        };
        // `ESRCH` is the ordinary race with a child that exited between the
        // decision and the syscall, and the reap will report the death. Anything
        // else is a real failure to stop a service, and staying quiet about it
        // means it survives.
        if let Err(e) = outcome
            && e.raw_os_error() != Some(libc::ESRCH)
        {
            announce_degradation(&format!("cannot signal {}: {e}", slot.name()));
        }
    }

    /// Serve the control socket for one pass: restart completions first (they
    /// need no socket), then every queued client command.
    ///
    /// Commands only ever set `Desired`, clear suppressions, or rebuild the
    /// plan. Signalling and spawning stay where they belong: `begin_stop`
    /// and the reconciler, on this same pass, further down.
    fn serve_ctl(&mut self, now: u64) {
        // A restart owed and a process gone: want it up again. `kick`
        // re-arms whatever kept it down — a suppression, a completion, or
        // both — and is a no-op when nothing needs clearing, so there is no
        // reason to replicate its conditions here.
        let mut i = 0;
        while i < self.pending_restart.len() {
            let idx = self.pending_restart[i];
            if idx >= self.runtime.len() {
                self.pending_restart.remove(i);
            } else if self.runtime.state_at(idx) == State::Stopped {
                self.runtime.set_desired(idx, Desired::Up);
                kick(&mut self.runtime, &self.plan, idx, now);
                self.pending_restart.remove(i);
            } else {
                i += 1;
            }
        }
        let cmds = match self.ctl.as_mut() {
            Some(server) => {
                server.accept_pending(&mut *self.reactor);
                server.pump(&mut *self.reactor)
            }
            None => Vec::new(),
        };
        for cmd in cmds {
            self.handle_ctl(cmd, now);
        }
    }

    /// Flush queued control replies. Runs every pass even when no command
    /// arrived: a reply that stayed buffered is a client kept waiting.
    fn flush_ctl(&mut self) {
        if let Some(server) = self.ctl.as_mut() {
            server.flush(&mut *self.reactor);
        }
    }

    /// Carry out one parsed client command and queue its reply.
    fn handle_ctl(&mut self, cmd: Command, now: u64) {
        match cmd {
            Command::Status { client, id, name } => {
                let frame = match self.plan.index_of(&name) {
                    Some(idx) => {
                        let s = self.runtime.get(idx);
                        let sp = &self.plan.services[idx];
                        let mut payload = ctl::status_payload(&ctl::StatusFacts {
                            name: &sp.name,
                            state: s.state,
                            desired: s.desired,
                            pid: s.pid,
                            started_at_ms: s.started_at,
                            now_ms: now,
                            restarts: s.restarts,
                            budget_cap: sp.restart_budget.capacity,
                        });
                        // Terminal quiet states read identically otherwise:
                        // `stopped desired=up` is a crash awaiting retry, a
                        // completion, or a suppression. The suffix says which.
                        if s.completed {
                            payload.push_str(" completed");
                        } else if s.restart_suppressed {
                            payload.push_str(" suppressed");
                        }
                        ctl::render_ok(&id, &payload)
                    }
                    None => ctl::render_err(&id, 3, &format!("unknown service `{name}`")),
                };
                self.reply_ctl(client, &frame);
            }
            Command::List { client, id } => {
                let mut entries = Vec::with_capacity(self.plan.services.len());
                for (idx, sp) in self.plan.services.iter().enumerate() {
                    let st = match self.runtime.state_at(idx) {
                        State::Stopped => "stopped",
                        State::Starting => "starting",
                        State::Running => "running",
                        State::Stopping => "stopping",
                    };
                    entries.push(format!("{}={st}", sp.name));
                }
                let payload = format!("{} {}", entries.len(), entries.join(" "));
                self.reply_ctl(client, &ctl::render_ok(&id, &payload));
            }
            Command::Start { client, id, name } => {
                let frame = match self.plan.index_of(&name) {
                    Some(idx) => {
                        self.runtime.set_desired(idx, Desired::Up);
                        kick(&mut self.runtime, &self.plan, idx, now);
                        self.pending_restart.retain(|&p| p != idx);
                        ctl::render_ok(&id, "up")
                    }
                    None => ctl::render_err(&id, 3, &format!("unknown service `{name}`")),
                };
                self.reply_ctl(client, &frame);
            }
            Command::Stop { client, id, name } => {
                let frame = match self.plan.index_of(&name) {
                    Some(idx) => {
                        self.runtime.set_desired(idx, Desired::Down);
                        self.pending_restart.retain(|&p| p != idx);
                        ctl::render_ok(&id, "down")
                    }
                    None => ctl::render_err(&id, 3, &format!("unknown service `{name}`")),
                };
                self.reply_ctl(client, &frame);
            }
            Command::Restart { client, id, name } => {
                let frame = match self.plan.index_of(&name) {
                    Some(idx) => {
                        if !self.plan.services[idx].kind.has_process() {
                            ctl::render_err(&id, 4, "a target has no process to restart")
                        } else if matches!(
                            self.runtime.state_at(idx),
                            State::Running | State::Starting | State::Stopping
                        ) {
                            self.runtime.set_desired(idx, Desired::Down);
                            if !self.pending_restart.contains(&idx) {
                                self.pending_restart.push(idx);
                            }
                            ctl::render_ok(&id, "stopping")
                        } else {
                            self.runtime.set_desired(idx, Desired::Up);
                            kick(&mut self.runtime, &self.plan, idx, now);
                            self.pending_restart.retain(|&p| p != idx);
                            ctl::render_ok(&id, "starting")
                        }
                    }
                    None => ctl::render_err(&id, 3, &format!("unknown service `{name}`")),
                };
                self.reply_ctl(client, &frame);
            }
            Command::Kick { client, id, name } => {
                let frame = match self.plan.index_of(&name) {
                    Some(idx) => {
                        if !self.plan.services[idx].kind.has_process() {
                            ctl::render_err(&id, 4, "a target is never suppressed")
                        } else if kick(&mut self.runtime, &self.plan, idx, now) {
                            ctl::render_ok(&id, "kicked")
                        } else {
                            ctl::render_ok(&id, "nothing to kick")
                        }
                    }
                    None => ctl::render_err(&id, 3, &format!("unknown service `{name}`")),
                };
                self.reply_ctl(client, &frame);
            }
            Command::ReloadAll { client, id } => {
                let frame = match self.reload() {
                    Ok(n) => ctl::render_ok(&id, &format!("{n} services")),
                    Err(e) => ctl::render_err(&id, 5, &e),
                };
                self.reply_ctl(client, &frame);
            }
            Command::Version { client, id } => {
                let payload = format!(
                    "zinit {} reactor={} signals={} children={}",
                    zrt::VERSION,
                    self.reactor.kind().name(),
                    self.sigs.kind().name(),
                    self.children.kind().name(),
                );
                self.reply_ctl(client, &ctl::render_ok(&id, &payload));
            }
            Command::Help { client, id } => {
                self.reply_ctl(
                    client,
                    &ctl::render_ok(
                        &id,
                        "status <name> | list | start <name> | stop <name> | \
                         restart <name> | kick <name> | reload-all | version | help",
                    ),
                );
            }
        }
    }

    /// Queue one reply frame. Silent when there is no server: without a
    /// control socket no command could have arrived, so there is nobody to
    /// answer.
    fn reply_ctl(&mut self, to: RawFd, frame: &str) {
        if let Some(server) = self.ctl.as_mut() {
            server.reply(to, frame);
        }
    }

    /// Rebuild the plan from the configured layers without disturbing live pids.
    ///
    /// Surviving services keep their mechanism (`ManagedService`: pid, fds,
    /// waiter) and their core slot (state, deadlines, budget) — only the
    /// plan index moves, via [`ManagedService::reindex`], and the description
    /// is replaced so the *next* start uses the new config. New services
    /// start fresh; deleted services with a live process are retired (one
    /// SIGTERM, reaped normally, then dropped). A deleted service with no
    /// process simply ceases to exist.
    ///
    /// Any load failure — bad file, cycle, unresolvable user — aborts the
    /// reload and keeps the old plan: a supervisor that governs a
    /// configuration it does not believe in is worse than one that governs
    /// yesterday's.
    fn reload(&mut self) -> Result<usize, String> {
        let (new_plan, mut new_descs) = match load_all(&self.config_dirs, &self.gen_dir) {
            Ok(ok) => ok,
            Err(e) => return Err(e.to_string()),
        };
        let adoption = adopt_names(&self.plan, &new_plan);
        let mut old_slots: HashMap<String, Slot> = HashMap::new();
        for slot in self.slots.drain(..) {
            old_slots.insert(slot.name().to_string(), slot);
        }
        let mut new_runtime = Runtime::from_plan(&new_plan);
        let mut new_slots = Vec::with_capacity(new_plan.services.len());
        for (new_idx, sp) in new_plan.services.iter().enumerate() {
            if let Some(mut slot) = old_slots.remove(sp.name.as_str()) {
                // Survivor: move the core slot to the new index alongside
                // the mechanism. `adopt_names` found the old index by name,
                // so this pairing is name-exact, never positional.
                if let Some(old_idx) = adoption[new_idx] {
                    slot.svc.reindex(new_idx);
                    *new_runtime.get_mut(new_idx) = self.runtime.get(old_idx).clone();
                }
                slot.desc = new_descs[new_idx].take();
                new_slots.push(slot);
            } else {
                let log = match open_sink(&sp.log) {
                    Ok(handle) => Some(handle),
                    Err(e) => {
                        let name = &sp.name;
                        announce_degradation(&format!("{name}: no log sink ({e})"));
                        None
                    }
                };
                // A reload during an operator stop must not resurrect: new
                // services join the stop, they do not outvote it.
                if self.stopping {
                    new_runtime.set_desired(new_idx, Desired::Down);
                } else {
                    new_runtime.set_desired(new_idx, Desired::Up);
                }
                new_slots.push(Slot {
                    svc: ManagedService::new(new_idx),
                    desc: new_descs[new_idx].take(),
                    log,
                    registered: None,
                });
            }
        }
        for (_, mut slot) in old_slots.drain() {
            // The reactor must forget the fd while it is still open: after
            // `cleanup` closes it, a `remove` would only answer `EBADF`.
            if let Some(fd) = slot.registered.take() {
                let _ = self.reactor.remove(fd);
            }
            // The plan is gone and so are the sockets: a retired service is
            // never coming back, so its listeners close here rather than
            // lingering past the last process they served.
            slot.svc.close_listeners();
            if slot.svc.pid().is_some() {
                let name = slot.name().to_string();
                let retired = Retired {
                    svc: slot.svc,
                    name,
                };
                self.signal_retired(&retired);
                self.retired.push(retired);
            }
        }
        self.plan = new_plan;
        self.runtime = new_runtime;
        self.slots = new_slots;
        self.pending_restart.clear();
        self.budget_announced.clear();
        self.last_offenders.clear();
        // Names moved: every dump is stale, so drop the cache and let the
        // next pass rewrite what changed (which is everything, once).
        self.last_state_dump.clear();
        announce_degradation(&format!("reloaded: {} services", self.slots.len()));
        Ok(self.slots.len())
    }

    /// The one signal a retired service ever gets: `TERM`, to the group once
    /// it provably exists, to the bare pid until then — the same rule as
    /// [`Sup::send`], because the same lost-`kill(-pgid)` race applies.
    fn signal_retired(&self, r: &Retired) {
        let Some(pid) = r.svc.pid() else {
            return;
        };
        let outcome = match (r.svc.group_is_signallable(), r.svc.pgid()) {
            (true, Some(pgid)) => zrt::sys::kill_group(pgid, Signal::Term),
            _ => zrt::sys::kill_process(pid, Signal::Term),
        };
        // `ESRCH` is the ordinary race with a child that exited between the
        // reload and the syscall; the reap reports the death either way.
        if let Err(e) = outcome
            && e.raw_os_error() != Some(libc::ESRCH)
        {
            announce_degradation(&format!("cannot signal retired {}: {e}", r.name));
        }
    }

    /// Rewrite the visible-state files whose content changed (contract C6).
    ///
    /// `name=value` lines for identity and core facts (name, state, desired,
    /// pid, restarts, completed, suppressed) plus the static plan facts that
    /// explain them (watchdog budget, seccomp action, listen names):
    /// consumable with `cat`, parseable with `grep`. Change-only writes keep
    /// a crash-looping service from churning the disk every pass, and the
    /// first failed write disables the dumps — disk trouble degrades
    /// observability, never supervision.
    fn dump_state(&mut self) {
        if !self.state_usable {
            return;
        }
        if self.last_state_dump.len() != self.slots.len() {
            self.last_state_dump = vec![String::new(); self.slots.len()];
        }
        for idx in 0..self.slots.len() {
            let s = self.runtime.get(idx);
            let sp = &self.plan.services[idx];
            // Static plan facts ride along so a `cat` answers "is the
            // watchdog even on?" without a second source: confinement that
            // is invisible is confinement nobody can debug.
            let content = format!(
                "name={}\nstate={:?}\ndesired={:?}\npid={}\nrestarts={}\ncompleted={}\nsuppressed={}\nwatchdog={}\nseccomp={}\nlistens={}\n",
                sp.name,
                s.state,
                s.desired,
                s.pid.map_or_else(|| String::from("-"), |p| p.to_string()),
                s.restarts,
                s.completed,
                s.restart_suppressed,
                sp.watchdog_sec
                    .map_or_else(|| String::from("-"), |secs| format!("{secs}s")),
                match &sp.seccomp {
                    None => String::from("-"),
                    Some(policy) => format!("{:?}", policy.action),
                },
                sp.listens
                    .iter()
                    .map(|l| l.name.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            );
            if self.last_state_dump[idx] == content {
                continue;
            }
            let path = self.state_dir.join(&self.plan.services[idx].name);
            match std::fs::write(&path, content.as_bytes()) {
                Ok(()) => {
                    self.last_state_dump[idx] = content;
                }
                Err(e) => {
                    let name = &self.plan.services[idx].name;
                    announce_degradation(&format!(
                        "cannot write state for {name} ({e}); state dumps disabled"
                    ));
                    self.state_usable = false;
                    return;
                }
            }
        }
    }

    /// Carry out the actions the core asked for, in the order it asked.
    fn execute(&mut self, actions: &[Action], now: u64) {
        for action in actions {
            match action {
                Action::Spawn(idx) => self.spawn(*idx, now),
                Action::Signal { idx, signal } => self.send(*idx, *signal),
                Action::Log {
                    idx,
                    level,
                    message,
                } => self.log_line(*idx, *level, message),
                // The one thing an action cannot do quietly: a service whose
                // restart budget just ran dry. Announced rather than logged,
                // because the log file it would go to is exactly what an
                // operator needs to find afterwards — and announced once per
                // supervisor lifetime, because the core repeats this action on
                // every pass for as long as the bucket is empty. A later
                // reload clears the record (fresh plan, fresh complaints).
                Action::BudgetExhausted(idx) => {
                    if !self.budget_announced.contains(idx) {
                        self.budget_announced.push(*idx);
                        let name = self.name_of(*idx);
                        announce_degradation(&format!(
                            "service `{name}` is backing off: restart budget \
                             exhausted (use `zctl kick` to retry now)"
                        ));
                    }
                }
                // Bookkeeping the core already did to its own state: there is no
                // second copy here to update. `MarkReady`/`MarkDown` are how the
                // core cascades, and `DepsChanged` is the hint that would let an
                // event-driven loop skip a rescan — which this one does not need,
                // because it rescans everything every pass anyway.
                Action::MarkReady(_)
                | Action::MarkDown { .. }
                | Action::DepsChanged(_)
                | Action::CascadeStop { .. }
                | Action::Unpin(_) => {}
                // ponytail: no controlling terminal in this phase — no getty and
                // no `type = console` handover, so there is nothing to grant or
                // revoke. `SpawnCtx::tty` is the hook; wire it when the console
                // service lands.
                Action::GrantConsole(_) | Action::RevokeConsole(_) => {}
            }
        }
    }

    /// Start one service, and register the child **before returning to the
    /// loop**.
    ///
    /// The window matters: a child that dies between `fork` and `track` is a
    /// zombie nobody will reap, because the tracker is the only thing in this
    /// process that notices a death (`zservice::spawn` module docs,
    /// `zrt::childproc::fork_tracked`).
    ///
    /// A failure is announced and fed to the core as `SpawnFailed`, which is
    /// what makes it ordinary: the core counts it against the restart budget, so
    /// a description that can never work becomes *visible* instead of a
    /// twenty-times-a-second fork attempt.
    fn spawn(&mut self, idx: Idx, now: u64) {
        if idx >= self.slots.len() {
            return;
        }
        // Bind (or re-check) the listen set before anything else: a service
        // whose sockets cannot bind fails like any other unstartable service
        // — budgeted, announced, visible — never half-started with the port
        // still free for somebody else to take.
        if let Err(e) = self.slots[idx].svc.ensure_listeners(&self.plan) {
            let errno = e.raw_os_error().unwrap_or(libc::EIO);
            let name = self.name_of(idx);
            announce_degradation(&format!("{name}: cannot bind listen sockets ({e})"));
            self.feed(&Event::SpawnFailed { idx, errno }, now);
            return;
        }
        let sp = &self.plan.services[idx];
        // Owned snapshot: the spawn holds `&mut self.runtime` and a context
        // that borrows the description, and neither may alias the slot the
        // listeners live in. The set is tiny, so the copy is noise.
        let listen = self.slots[idx].svc.listen_snapshot();
        // Moved out for the duration: the spawn holds `&mut self.runtime` and a
        // context that borrows the description, and a local is the one shape that
        // provably leaves no borrow of `self.slots` alive across the call.
        let Some(desc) = self.slots[idx].desc.take() else {
            let name = &sp.name;
            announce_degradation(&format!("{name}: nothing to start from"));
            return;
        };
        let ctx = SpawnCtx {
            command: &desc.command,
            env: &desc.env,
            // Already resolved at load time: `zconfig` refuses to freeze a plan
            // with an unresolved identity, so this is a number or nothing —
            // never a name that could be read differently later.
            run_as: sp.run_as,
            rlimits: &desc.rlimits,
            cgroup: desc.cgroup.as_deref(),
            log: &sp.log,
            service_name: &desc.name,
            // ponytail: no getty in this phase, so a `console` service has no
            // terminal to take. It spawns as an ordinary process rather than
            // failing; pass `Some("/dev/ttyN")` when the console service lands.
            tty: None,
            listen: &listen,
        };
        let started = self.slots[idx]
            .svc
            .start(&self.plan, &mut self.runtime, &ctx, now);
        self.slots[idx].desc = Some(desc);

        if let Err(e) = started {
            let errno = match e.raw_os_error() {
                Some(code) => code,
                // A refusal before any syscall — `NotFound`, `EmptyCommand`,
                // `SyslogNotWired` — has no errno of its own, and `EIO` is the
                // honest "no process exists".
                None => libc::EIO,
            };
            let name = self.name_of(idx);
            announce_degradation(&format!("{name}: cannot spawn ({e})"));
            self.feed(&Event::SpawnFailed { idx, errno }, now);
            return;
        }
        // The announcement record stands: "backing off" was already said for
        // this service, and repeating it on every restart would turn the
        // guard in `execute` into decoration. A reload (which clears the
        // record) or an operator `kick` is what earns a fresh complaint.
        if let Some(pid) = self.slots[idx].svc.pid()
            && let Err(e) = self.children.track(pid)
        {
            let name = self.name_of(idx);
            announce_degradation(&format!("{name}: cannot track pid {pid} ({e})"));
        }
    }

    /// One core log line, into the service's own log file.
    ///
    /// The handle is the supervisor's own open of the same path the child writes
    /// to, because the child's descriptor is private to `ManagedService` and
    /// there is no accessor for it. Both descriptors are `O_APPEND`, so the lines
    /// interleave instead of overwriting each other.
    ///
    /// ponytail: two handles on one file, so a rotation performed from here
    /// renames the file out from under the child's descriptor and the child's
    /// own output lands in the rotated generation. Fix it by handing
    /// `write_line` the descriptor the child already has, once the rotation
    /// code can accept an fd — not by opening a second one on every start.
    fn log_line(&mut self, idx: Idx, level: LogLevel, message: &str) {
        let line = format!("[{}] {}: {}", level_name(level), self.name_of(idx), message);
        let Some(slot) = self.slots.get_mut(idx) else {
            return;
        };
        let Some(handle) = slot.log.as_mut() else {
            // No sink: the failure to open it was announced once, at load time.
            return;
        };
        if let Err(e) = write_line(handle, line.as_bytes()) {
            let name = self.name_of(idx);
            announce_degradation(&format!("{name}: cannot write to log: {e}"));
        }
    }

    /// The core's own consistency check, run after any pass that changed
    /// something.
    ///
    /// A violation is always a `zcore` bug, and it always means this loop is
    /// now reasoning about a state it cannot explain. Saying so out loud is the
    /// only useful response: there is nothing here that could safely "fix" it,
    /// and pretending otherwise is how an init starts lying.
    fn check_invariants(&mut self) {
        let offenders = self.runtime.check_invariants(&self.plan);
        if offenders == self.last_offenders {
            return;
        }
        if offenders.is_empty() {
            announce_degradation("core invariants hold again");
        } else {
            announce_degradation(&format!(
                "core invariant violated for {offenders:?}; this is a zcore bug \
                 and the supervisor is now guessing"
            ));
        }
        self.last_offenders = offenders;
    }

    /// Hand one event to the core and carry out whatever it decides.
    fn feed(&mut self, event: &Event, now: u64) {
        let transition = self.apply_to(event, now);
        self.execute(&transition.actions, now);
        if transition.changed {
            self.check_invariants();
        }
    }

    fn apply_to(&mut self, event: &Event, now: u64) -> Transition {
        let Self { runtime, plan, .. } = self;
        apply(event, runtime, plan, now)
    }

    fn reconcile_now(&mut self, now: u64) -> Tick {
        let Self { runtime, plan, .. } = self;
        reconcile(runtime, plan, now)
    }

    fn name_of(&self, idx: Idx) -> &str {
        self.plan
            .services
            .get(idx)
            .map_or("<unknown>", |sp| sp.name.as_str())
    }
}

/// Default configuration layers, low to high precedence: vendor, operator,
/// ephemeral. A service file in a higher layer replaces the whole file below
/// it; `<name>.d/*.conf` drop-ins from every layer merge over the winner in
/// layer order.
const DEFAULT_CONFIG_LAYERS: [&str; 3] = [
    "/usr/lib/zinit/services.d",
    "/etc/zinit/services.d",
    "/run/zinit/services.d",
];

/// Override for the layer list (colon-separated, as `PATH` is). Honoured only
/// when `--sup` names no directory: an explicit directory is the whole
/// configuration, which is what keeps the test suite hermetic.
const CONFIG_DIRS_ENV: &str = "ZINIT_CONFIG_DIRS";

/// Default generator directory (contract C2).
const DEFAULT_GENERATOR_DIR: &str = "/etc/zinit/generators.d";

/// Override for the generator directory. Same convention as the socket: tests
/// point it at scratch, production leaves the default alone.
const GENERATORS_ENV: &str = "ZINIT_GENERATORS";

/// One generator's wall-clock budget. Generators run before the first service
/// starts, so every second here is a second of boot; a generator that needs
/// longer is broken, not slow.
const GENERATOR_TIMEOUT_MS: u64 = 10_000;

/// A generator's stdout past this is not a description. The parser refuses
/// texts past 1 MiB per file; a generator is held to the same ceiling before
/// its output ever reaches one.
const GENERATOR_OUTPUT_MAX: usize = 1024 * 1024;

/// Resolve where configuration comes from: an explicit `--sup <dir>` is the
/// whole world; otherwise the environment; otherwise the compiled defaults.
/// Shared with `zinit check`, which validates exactly what a boot would read.
pub(crate) fn resolve_sources(explicit: Option<&Path>) -> (Vec<PathBuf>, PathBuf) {
    let layers = match explicit {
        Some(dir) => vec![dir.to_path_buf()],
        None => match std::env::var_os(CONFIG_DIRS_ENV) {
            Some(list) => std::env::split_paths(&list).collect(),
            None => DEFAULT_CONFIG_LAYERS
                .iter()
                .map(|s| PathBuf::from(*s))
                .collect(),
        },
    };
    let gen_dir = std::env::var_os(GENERATORS_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_GENERATOR_DIR));
    (layers, gen_dir)
}

/// Generators, then layers, then cleanup: the full "read the world" path.
///
/// Shared with `zinit check`: the validator must read exactly what a boot
/// (or a `reload-all`) would read, including generated descriptions.
///
/// The generated directory is scratch by design — every byte is in memory
/// once `load_layers` returns — so it is removed before return either way.
/// Startup and `reload-all` share this, which is what makes a reload pick up
/// new generators instead of governing yesterday's outputs.
pub(crate) fn load_all(
    config_dirs: &[PathBuf],
    gen_dir: &Path,
) -> io::Result<(Plan, Vec<Option<ServiceDesc>>)> {
    let out_dir = generated_layer_dir();
    let _ = std::fs::create_dir_all(&out_dir);
    let generated = run_generators(gen_dir, &out_dir);
    let mut layers: Vec<PathBuf> = config_dirs.to_vec();
    if !generated.is_empty() {
        layers.push(out_dir.clone());
    }
    let result = load_layers(&layers);
    let _ = std::fs::remove_dir_all(&out_dir);
    result
}

/// Scratch directory for one generator run. Pid-qualified so two supervisors
/// never share it; removed by `load_all` before it returns.
fn generated_layer_dir() -> PathBuf {
    std::env::temp_dir().join(format!("zinit-generated-{}", std::process::id()))
}

/// Run every executable in `gen_dir`, writing each valid stdout as
/// `<out_dir>/<stem>.conf` and returning what was written.
///
/// Nothing here is fatal. A generator is an unprivileged advisor: a missing
/// directory, an unreadable entry, a timeout, a non-zero exit, an oversized
/// or unparsable output are all announced and skipped. Advisors do not get to
/// halt the boot — but a generator whose output *parses* is trusted exactly
/// like a checked-in file, which is why the parse pre-check below exists:
/// without it, one garbage generator would make `load_layers` refuse the
/// whole machine.
fn run_generators(gen_dir: &Path, out_dir: &Path) -> Vec<PathBuf> {
    let mut written = Vec::new();
    let entries = match std::fs::read_dir(gen_dir) {
        Ok(entries) => entries,
        Err(_) => return written,
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(e) => {
                announce_degradation(&format!(
                    "{}: unreadable entry ({e}); skipped",
                    gen_dir.display()
                ));
            }
        }
    }
    paths.sort();
    for path in &paths {
        let stem = match path.file_stem().map(|s| s.to_string_lossy().into_owned()) {
            Some(s) => s,
            None => continue,
        };
        if zconfig::desc::validate_service_name(&stem).is_err() {
            announce_degradation(&format!(
                "{}: not a service name; generator skipped",
                path.display()
            ));
            continue;
        }
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                announce_degradation(&format!(
                    "{}: cannot stat ({e}); generator skipped",
                    path.display()
                ));
                continue;
            }
        };
        if !meta.is_file() || !is_executable(&meta) {
            // Data, documentation, or a file someone misplaced: not a
            // generator, and worth no warning. An executable bit is the opt-in.
            continue;
        }
        match run_one_generator(path) {
            Ok(Some(text)) => {
                if let Err(e) = zconfig::parser::parse_service(&stem, &text) {
                    announce_degradation(&format!(
                        "{}: output does not parse ({e}); generator skipped",
                        path.display()
                    ));
                    continue;
                }
                if std::fs::create_dir_all(out_dir).is_err() {
                    announce_degradation(&format!(
                        "{}: cannot stage generator output; generators skipped",
                        out_dir.display()
                    ));
                    return written;
                }
                let dest = out_dir.join(format!("{stem}.conf"));
                match std::fs::write(&dest, text.as_bytes()) {
                    Ok(()) => written.push(dest),
                    Err(e) => announce_degradation(&format!(
                        "{}: cannot stage ({e}); generator skipped",
                        path.display()
                    )),
                }
            }
            Ok(None) => {}
            Err(e) => announce_degradation(&format!("{}: {e}; generator skipped", path.display())),
        }
    }
    written
}

/// `true` when the file's mode marks it executable. Unix-only by nature;
/// elsewhere every regular file counts (there is nowhere else to run it).
#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

/// `true` when the file's mode marks it executable. Unix-only by nature;
/// elsewhere every regular file counts (there is nowhere else to run it).
#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    true
}

/// Run one generator to completion: `Ok(Some(text))` is stdout worth keeping,
/// `Ok(None)` is silence worth nothing, `Err(msg)` is an announced skip.
///
/// `std::process::Command` does the fork+exec (it owns that unsafety so this
/// file does not have to); the timeout is a `try_wait` poll, because a
/// generator that hangs must cost 10 seconds, not the boot.
fn run_one_generator(path: &Path) -> Result<Option<String>, String> {
    use std::process::{Command, Stdio};
    let mut child = match Command::new(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Err(format!("cannot spawn ({e})")),
    };
    let start = std::time::Instant::now();
    let budget = std::time::Duration::from_millis(GENERATOR_TIMEOUT_MS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() > budget {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("overran {GENERATOR_TIMEOUT_MS} ms; killed"));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("cannot wait ({e})"));
            }
        }
    };
    if !status.success() {
        return Err(format!("exited with {status}"));
    }
    let mut out = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        use std::io::Read;
        // One byte past the ceiling, so output exactly at the ceiling still
        // fits and anything past it is observable rather than silently cut.
        if stdout
            .take((GENERATOR_OUTPUT_MAX + 1) as u64)
            .read_to_end(&mut out)
            .is_err()
        {
            return Err(String::from("cannot read stdout"));
        }
    }
    if out.len() > GENERATOR_OUTPUT_MAX {
        return Err(format!("output past {GENERATOR_OUTPUT_MAX} bytes"));
    }
    match String::from_utf8(out) {
        Ok(text) => {
            if text.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(text))
            }
        }
        Err(_) => Err(String::from("output is not UTF-8")),
    }
}

/// Parse one file's text, announcing its warnings.
///
/// A file that does not parse is fatal: a plan built from the files that
/// happened to be readable is a configuration nobody wrote.
fn parse_file(name: &str, text: &str, file: String) -> io::Result<ServiceDesc> {
    match parse_service_with_diagnostics(name, text) {
        Ok(parsed) => {
            // Non-fatal findings. A bag of warnings is a *passing* run in
            // `zconfig`'s own words, so they are reported and the service is
            // loaded anyway.
            for warning in parsed.warnings.iter() {
                announce_degradation(warning.render(&file).trim());
            }
            let mut desc = parsed.desc;
            desc.source = file;
            Ok(desc)
        }
        Err(e) => Err(fatal(format!("{file}: {e}"))),
    }
}

/// Read one `<name>.conf` file into its (name, text, display-path) triple.
///
/// Fatal when the file cannot be identified or read: the caller already
/// decided this directory is supposed to exist, so a file that vanishes
/// mid-read is damage, not absence.
fn read_one_conf(path: &Path) -> io::Result<(String, String, String)> {
    let Some(name) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
        return Err(fatal(format!("{}: unusable file name", path.display())));
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return Err(fatal(format!("{}: {e}", path.display()))),
    };
    Ok((name, text, path.display().to_string()))
}

/// One layer's parsed content: base descriptions plus drop-ins keyed by
/// service name.
type LayerContent = (Vec<ServiceDesc>, HashMap<String, Vec<ServiceDesc>>);

/// Read one layer: base files plus `<name>.d/*.conf` drop-ins.
///
/// Fatal on any failure inside a present layer — see `parse_file`. A missing
/// *layer* never reaches here; `load_layers` skips it before calling.
fn read_layer(dir: &Path) -> io::Result<LayerContent> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => return Err(fatal(format!("{}: {e}", dir.display()))),
    };
    let mut files: Vec<PathBuf> = Vec::new();
    let mut dropdirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => return Err(fatal(format!("{}: {e}", dir.display()))),
        };
        // ponytail: `*.conf` only. `DESIGN.md` §5.1 also spells
        // `network.target` as a file name; targets work today through
        // `type = target`, which is the same code path, so a second filename
        // convention would only be a second way to spell one thing.
        if path.extension() == Some(OsStr::new("conf")) {
            files.push(path);
        } else if path.extension() == Some(OsStr::new("d")) && path.is_dir() {
            dropdirs.push(path);
        }
    }
    // Sorted, so the "first bad file" an operator is told about does not
    // depend on the order the filesystem happened to hand back.
    files.sort();
    dropdirs.sort();
    let mut bases = Vec::with_capacity(files.len());
    for path in &files {
        let (name, text, file) = read_one_conf(path)?;
        bases.push(parse_file(&name, &text, file)?);
    }
    let mut dropins: HashMap<String, Vec<ServiceDesc>> = HashMap::new();
    for dropdir in &dropdirs {
        let Some(base) = dropdir
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
        else {
            return Err(fatal(format!(
                "{}: unusable drop-in directory name",
                dropdir.display()
            )));
        };
        let subentries = match std::fs::read_dir(dropdir) {
            Ok(subentries) => subentries,
            Err(e) => return Err(fatal(format!("{}: {e}", dropdir.display()))),
        };
        let mut subs: Vec<PathBuf> = Vec::new();
        for entry in subentries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => return Err(fatal(format!("{}: {e}", dropdir.display()))),
            };
            if path.extension() == Some(OsStr::new("conf")) {
                subs.push(path);
            }
        }
        subs.sort();
        for path in &subs {
            // A drop-in file is part of the service, not a service: its own
            // stem is meaningless (usually `10-whatever`), so it is parsed
            // under the directory's name and the stem is never read.
            let (_, text, file) = read_one_conf(path)?;
            dropins
                .entry(base.clone())
                .or_default()
                .push(parse_file(&base, &text, file)?);
        }
    }
    Ok((bases, dropins))
}

/// Read every layer, merge, resolve identities, and freeze the plan.
///
/// Layers apply low to high precedence: a higher layer's `<name>.conf`
/// replaces the whole file below it, and every layer's `<name>.d/*.conf`
/// drop-ins merge over the winner in layer order. Missing layer directories
/// are skipped (a fresh system has no vendor layer); anything else — a file
/// that does not parse, an orphan drop-in, a graph that cannot be ordered,
/// an account that cannot be resolved — is fatal, for the same reason one
/// bad file was always fatal: a supervisor that started anyway would be
/// governing a configuration it does not believe in.
fn load_layers(dirs: &[PathBuf]) -> io::Result<(Plan, Vec<Option<ServiceDesc>>)> {
    let mut bases: HashMap<String, ServiceDesc> = HashMap::new();
    let mut dropins: HashMap<String, Vec<ServiceDesc>> = HashMap::new();
    for dir in dirs.iter() {
        if !dir.is_dir() {
            continue;
        }
        let (mut layer_bases, mut layer_drops) = read_layer(dir)?;
        for desc in layer_bases.drain(..) {
            // Low to high: a later (higher-precedence) file replaces.
            bases.insert(desc.name.clone(), desc);
        }
        for (name, mut overs) in layer_drops.drain() {
            dropins.entry(name).or_default().append(&mut overs);
        }
    }
    let mut names: Vec<String> = bases.keys().cloned().collect();
    names.sort();
    let mut descs = Vec::with_capacity(names.len());
    for name in &names {
        // `names` came from the keys, so the entry exists; the guard keeps a
        // map bug from becoming a silent skip.
        let Some(mut desc) = bases.remove(name) else {
            continue;
        };
        if let Some(overs) = dropins.remove(name) {
            for over in overs {
                desc.overlay_onto(over);
            }
        }
        descs.push(desc);
    }
    if let Some(orphan) = dropins.keys().next().cloned() {
        return Err(fatal(format!(
            "{orphan}.d/*.conf matches no service; orphan drop-ins are typos"
        )));
    }
    validate_merged(&descs)?;
    apply_wants(&mut descs, dirs);
    resolve_identities(&mut descs)?;
    freeze_plan(descs)
}

/// Cross-layer validation: the parser checks each file alone, but a base and
/// a drop-in can each be innocent while their merge is nonsense (a `pid-file`
/// drop-in without `type = forking`, a watchdog without a handshake). The
/// parser is the source of truth for single files; this is the backstop for
/// their combination. Fatal, like every other load refusal: a merged
/// description nobody can defend is not a description to govern.
fn validate_merged(descs: &[ServiceDesc]) -> io::Result<()> {
    for desc in descs {
        if desc.pid_file.is_some() && desc.kind != zcore::ServiceKind::Forking {
            return Err(fatal(format!(
                "{}: pid-file needs `type = forking` (same file or drop-in)",
                desc.source
            )));
        }
        if desc.watchdog_sec.is_some()
            && !(desc.kind.has_process()
                && desc.ready.kind == zconfig::value::ReadySpecKind::Notify)
        {
            return Err(fatal(format!(
                "{}: watchdog-sec needs a process with `ready = notify`",
                desc.source
            )));
        }
        if !desc.listens.is_empty() && !desc.kind.has_process() {
            return Err(fatal(format!(
                "{}: listen on a service with no process; sockets nobody spawns for bind nowhere",
                desc.source
            )));
        }
    }
    Ok(())
}

/// Read one directory and freeze the plan.
///
/// Unit tests call this directly; production startup and `reload-all` go
/// through `load_all` → `load_layers`.
#[cfg(test)]
fn load(config_dir: &Path) -> io::Result<(Plan, Vec<Option<ServiceDesc>>)> {
    load_layers(std::slice::from_ref(&config_dir.to_path_buf()))
}

/// Turn `wants/<target>/<svc>` entries into soft dependencies (contract C3).
///
/// Every entry name under a target's directory — file or symlink, the link is
/// never followed — adds an *optional* edge from the target to the entry. A
/// wanted service that is missing stays a warning in the graph, not an error:
/// "wanted" is softer than "required" by definition. Unknown targets and
/// unusable names are announced and skipped; `wants/` itself being absent is
/// the common case and means nothing at all.
fn apply_wants(descs: &mut [ServiceDesc], dirs: &[PathBuf]) {
    for dir in dirs {
        let targets = match std::fs::read_dir(dir.join("wants")) {
            Ok(targets) => targets,
            Err(_) => continue,
        };
        let mut tnames: Vec<String> = Vec::new();
        for entry in targets {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    announce_degradation(&format!(
                        "{}: unreadable wants entry ({e}); skipped",
                        dir.join("wants").display()
                    ));
                    continue;
                }
            };
            tnames.push(path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ));
        }
        tnames.sort();
        for target in &tnames {
            let entries = match std::fs::read_dir(dir.join("wants").join(target)) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            let mut wanted: Vec<String> = Vec::new();
            for entry in entries {
                let path = match entry {
                    Ok(entry) => entry.path(),
                    Err(_) => continue,
                };
                if let Some(name) = path.file_name().map(|n| n.to_string_lossy()) {
                    if !name.starts_with('.') {
                        wanted.push(name.into_owned());
                    }
                }
            }
            wanted.sort();
            for name in &wanted {
                if zconfig::desc::validate_service_name(name).is_err() {
                    announce_degradation(&format!(
                        "wants/{target}/{name}: not a service name; edge skipped"
                    ));
                    continue;
                }
                match descs.iter_mut().find(|d| d.name == *target) {
                    Some(desc) => {
                        if !desc.depends_on(name) {
                            desc.depends_optional.push(name.clone());
                        }
                    }
                    None => announce_degradation(&format!(
                        "wants/{target}: no such service; its wanted edges are ignored"
                    )),
                }
            }
        }
    }
}

/// Identities. `zconfig` has no `/etc/passwd`, so `user = mysql` arrives
/// unresolved and `build_plan` refuses to freeze it. This is the pass the
/// plan builder is waiting for — and running a service as root because its
/// user is missing is the exact bug the unresolved-identity trapdoor exists
/// to prevent.
fn resolve_identities(descs: &mut [ServiceDesc]) -> io::Result<()> {
    for desc in descs.iter_mut() {
        let Some(spec) = desc.unresolved_run_as.take() else {
            continue;
        };
        match resolve_user_group(&spec) {
            Ok((uid, gid)) => desc.run_as = Some(RunAs { uid, gid }),
            Err(e) => {
                let name = &desc.name;
                return Err(fatal(format!("{name}: user `{spec}`: {e}")));
            }
        }
    }
    Ok(())
}

/// Freeze the plan and align descriptions to its indices: the plan orders
/// services topologically, `read_dir` does not. A spawn with the wrong
/// description is a bug no type in this program would catch.
fn freeze_plan(descs: Vec<ServiceDesc>) -> io::Result<(Plan, Vec<Option<ServiceDesc>>)> {
    let plan = match build_plan(&descs) {
        Ok(plan) => plan,
        Err(e) => return Err(fatal(format!("bad plan: {e}"))),
    };
    let mut index: HashMap<&str, Idx> = HashMap::new();
    for (idx, sp) in plan.services.iter().enumerate() {
        index.insert(sp.name.as_str(), idx);
    }
    let mut aligned: Vec<Option<ServiceDesc>> = (0..plan.services.len()).map(|_| None).collect();
    for desc in descs {
        match index.get(desc.name.as_str()) {
            Some(&at) => aligned[at] = Some(desc),
            None => {
                let name = &desc.name;
                return Err(fatal(format!("{name}: not in the plan")));
            }
        }
    }
    Ok((plan, aligned))
}

/// Match new-plan names to old-plan indices, for the reload path.
///
/// Returns one entry per new-plan service: the old index of the same name, or
/// `None` for a genuinely new service. Pure and unit-tested below — a wrong
/// pairing starts the wrong description under a live pid, and no type in the
/// program would catch it.
fn adopt_names(old: &Plan, new: &Plan) -> Vec<Option<Idx>> {
    let mut index: HashMap<&str, Idx> = HashMap::new();
    for (idx, sp) in old.services.iter().enumerate() {
        index.insert(sp.name.as_str(), idx);
    }
    new.services
        .iter()
        .map(|sp| index.get(sp.name.as_str()).copied())
        .collect()
}

/// Control socket path: `$ZINIT_SOCKET` in tests, `/run/zinit/ctl` in
/// production. An override that is empty or unset means the default; the
/// supervisor never invents a third option.
fn ctl_socket_path() -> PathBuf {
    std::env::var_os(ctl::SOCKET_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(ctl::DEFAULT_SOCKET_PATH))
}

/// Visible-state directory, same override convention as the socket.
fn ctl_state_dir() -> PathBuf {
    std::env::var_os(ctl::STATE_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(ctl::DEFAULT_STATE_DIR))
}

/// Announce `message` and turn it into the `Err` that stops the supervisor.
fn fatal(message: String) -> io::Error {
    announce_degradation(&message);
    io::Error::other(message)
}

/// `kill(pid, 0)`: true unless the kernel answers ESRCH.
///
/// EPERM means "exists but not ours" — still alive. Used only for adopted
/// daemons on platforms without a subreaper (see the poll loop): everywhere
/// else the reaper, not a probe, decides death.
fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs only the permission check and delivers
    // nothing, so there is nothing to be unsafe about.
    let alive = unsafe { libc::kill(pid, 0) == 0 };
    alive || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The lowercase name of a core log level.
///
/// `zcore::LogLevel` carries no name of its own, this is the only place one is
/// needed, and a log that says `[WARN]` in one line and `[error]` in the next
/// was two tables where one would do.
fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing testable without becoming PID 1's child: a directory of
    /// descriptions must freeze into a plan whose indices line up with the
    /// descriptions they came from. That alignment is the loop's only real
    /// hazard, and nothing else here can catch it.
    #[test]
    fn a_loaded_plan_keeps_descriptions_aligned_with_indices() {
        let dir = scratch("align");
        // Written out of order on purpose: the plan is topologically sorted and
        // `read_dir` is not, which is exactly the mismatch being tested.
        write_conf(&dir, "zeta");
        write_conf(&dir, "alpha");
        let (plan, descs) = load(&dir).expect("both descriptions load");
        assert_eq!(plan.services.len(), 2);
        for (idx, sp) in plan.services.iter().enumerate() {
            let desc = descs[idx].as_ref().expect("every index has a description");
            assert_eq!(desc.name, sp.name, "description {idx} is the wrong one");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that does not parse must stop the supervisor: a plan built from
    /// the files that happened to be readable is a configuration nobody wrote.
    #[test]
    fn a_bad_description_is_fatal() {
        let dir = scratch("bad");
        let text = "not-a-directive\n";
        std::fs::write(dir.join("broken.conf"), text).expect("write");
        assert!(load(&dir).is_err(), "a bad file must not load");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dependency cycle has no topological order, so there is no plan and no
    /// way to start anything in the right sequence. Fatal, not a warning.
    #[test]
    fn a_dependency_cycle_is_fatal() {
        let dir = scratch("cycle");
        for (name, dep) in [("a", "b"), ("b", "a")] {
            let text = format!("command = /bin/true\ndepends = {dep}\n");
            std::fs::write(dir.join(format!("{name}.conf")), text).expect("write");
        }
        assert!(load(&dir).is_err(), "a cycle must not load");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reload adoption pairs by name, never by position: the case that breaks
    /// is a deletion shifting every index after it.
    #[test]
    fn reload_adoption_pairs_names_not_positions() {
        use zcore::ServicePlan;
        let mut old = Plan::default();
        for name in ["b", "a"] {
            old.services.push(ServicePlan::new(String::from(name)));
        }
        old.order_up = vec![0, 1];
        old.order_down = vec![1, 0];
        let mut new = Plan::default();
        // `b` deleted, `a` moved to index 0, `c` born at index 1.
        for name in ["a", "c"] {
            new.services.push(ServicePlan::new(String::from(name)));
        }
        new.order_up = vec![0, 1];
        new.order_down = vec![1, 0];
        assert_eq!(adopt_names(&old, &new), vec![Some(1), None]);
    }

    /// Two loads of an evolving directory adopt cleanly through the real
    /// `load` path: the survivor pairs by name and the newcomer is new.
    #[test]
    fn two_loads_of_an_evolving_directory_adopt() {
        let dir = scratch("evolve");
        write_conf(&dir, "keeper");
        let (old_plan, _) = load(&dir).expect("first load");
        write_conf(&dir, "newcomer");
        let (new_plan, descs) = load(&dir).expect("second load");
        assert_eq!(new_plan.services.len(), 2);
        let adopted = adopt_names(&old_plan, &new_plan);
        assert_eq!(adopted.len(), 2);
        assert_eq!(adopted.iter().filter(|a| a.is_some()).count(), 1);
        // Descriptions stay aligned to the *new* plan: the loop's only real
        // hazard, checked again because a reload rebuilds both vectors.
        for (idx, sp) in new_plan.services.iter().enumerate() {
            let desc = descs[idx].as_ref().expect("every index has a description");
            assert_eq!(desc.name, sp.name, "description {idx} is the wrong one");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A drop-in overrides what it says and nothing else: the overlay's
    /// parser-filled defaults must not clobber the base's explicit values.
    #[test]
    fn drop_in_overrides_win_without_clobbering_the_rest() {
        let dir = scratch("dropin");
        std::fs::write(
            dir.join("svc.conf"),
            "command = /bin/a\nstop-timeout = 20s\nready = none\n",
        )
        .expect("write");
        let ddir = dir.join("svc.d");
        std::fs::create_dir_all(&ddir).expect("mkdir");
        std::fs::write(ddir.join("10.conf"), "command = /bin/b\n").expect("write");
        let (plan, descs) = load(&dir).expect("loads with drop-in");
        assert_eq!(plan.services.len(), 1);
        let idx = plan.index_of("svc").expect("svc in plan");
        let desc = descs[idx].as_ref().expect("description");
        assert_eq!(desc.command, "/bin/b");
        assert_eq!(desc.stop_timeout_ms, 20_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A higher layer replaces the whole base file; drop-ins still merge over
    /// the winner.
    #[test]
    fn higher_layer_replaces_the_whole_file() {
        let low = scratch("layer-low");
        let high = scratch("layer-high");
        std::fs::write(low.join("svc.conf"), "command = /bin/low\nready = none\n").expect("write");
        std::fs::write(high.join("svc.conf"), "command = /bin/high\nready = none\n")
            .expect("write");
        let (plan, descs) = load_layers(&[low.clone(), high.clone()]).expect("layered load");
        let idx = plan.index_of("svc").expect("svc in plan");
        let desc = descs[idx].as_ref().expect("description");
        assert_eq!(desc.command, "/bin/high");
        assert!(
            desc.source.contains("layer-high"),
            "source names the winner"
        );
        let _ = std::fs::remove_dir_all(&low);
        let _ = std::fs::remove_dir_all(&high);
    }

    /// A missing layer is skipped; a missing *explicit* directory is fatal in
    /// `run`, not here — `load_layers` cannot tell the two apart, so it
    /// skips and the caller decides.
    #[test]
    fn missing_layers_are_skipped() {
        let dir = scratch("layer-present");
        write_conf(&dir, "svc");
        let missing = dir.join("no-such-layer");
        let (plan, _) = load_layers(&[missing, dir.clone()]).expect("loads");
        assert_eq!(plan.services.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A drop-in without a base file is an operator typo with a directory:
    /// fatal, like any other file the loader cannot believe in.
    #[test]
    fn orphan_drop_in_is_fatal() {
        let dir = scratch("orphan");
        let ddir = dir.join("ghost.d");
        std::fs::create_dir_all(&ddir).expect("mkdir");
        std::fs::write(ddir.join("10.conf"), "command = /bin/ghost\n").expect("write");
        assert!(load(&dir).is_err(), "orphan drop-in must not load");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cross-layer nonsense is refused after the merge: each file below is
    /// innocent alone (the parser passes them), but together they describe a
    /// pid file with no forking daemon and a watchdog with no handshake.
    #[test]
    fn merged_nonsense_is_refused_after_overlay() {
        let dir = scratch("merged-bad");
        std::fs::write(
            dir.join("svc.conf"),
            "type = forking\ncommand = /bin/d\npid-file = /run/d.pid\nready = none\n",
        )
        .expect("write");
        let ddir = dir.join("svc.d");
        std::fs::create_dir_all(&ddir).expect("mkdir");
        std::fs::write(ddir.join("10.conf"), "type = process\n").expect("write");
        assert!(
            load(&dir).is_err(),
            "pid-file without forking must not load even across layers"
        );
        let dir2 = scratch("merged-bad-watchdog");
        std::fs::write(
            dir2.join("svc.conf"),
            "command = /bin/x\nready = notify\nwatchdog-sec = 30\n",
        )
        .expect("write");
        let ddir2 = dir2.join("svc.d");
        std::fs::create_dir_all(&ddir2).expect("mkdir");
        std::fs::write(ddir2.join("10.conf"), "ready = none\n").expect("write");
        assert!(
            load(&dir2).is_err(),
            "watchdog without a handshake must not load even across layers"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// `wants/<target>/<svc>` entries land in the target's optional edges.
    /// Optional edges never order and never block (that is their contract),
    /// so the assertion reads the description, not the plan order.
    #[test]
    fn wants_entries_become_soft_edges() {
        let dir = scratch("wants");
        write_conf(&dir, "app");
        write_conf(&dir, "cache");
        let wdir = dir.join("wants").join("app");
        std::fs::create_dir_all(&wdir).expect("mkdir");
        std::fs::write(wdir.join("cache"), "").expect("write");
        let (plan, descs) = load(&dir).expect("loads with wants");
        let app = plan.index_of("app").expect("app in plan");
        let desc = descs[app].as_ref().expect("description");
        assert!(
            desc.depends_optional.iter().any(|d| d == "cache"),
            "wants/app/cache must be a soft edge"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A generator's stdout becomes a service; a non-executable file is data,
    /// not a generator, and is silently skipped.
    #[test]
    fn generator_stdout_becomes_a_service() {
        let gendir = scratch("gen-bin");
        let out = scratch("gen-out");
        std::fs::write(
            gendir.join("web"),
            "#!/bin/sh\nprintf 'command = /bin/true\\nready = none\\n'\n",
        )
        .expect("write");
        std::fs::write(gendir.join("notes.txt"), "not a generator\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = gendir.join("web");
            let mut perms = std::fs::metadata(&path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        let written = run_generators(&gendir, &out);
        assert_eq!(written.len(), 1, "exactly the executable counts");
        let (plan, _) = load(&out).expect("generated output loads");
        assert!(plan.index_of("web").is_some());
        let _ = std::fs::remove_dir_all(&gendir);
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A generator whose output does not parse is skipped, loudly or not —
    /// but it never reaches the loader, so it can never fatal a boot.
    #[test]
    fn garbage_generator_output_never_reaches_the_loader() {
        let gendir = scratch("gen-garbage");
        let out = scratch("gen-garbage-out");
        std::fs::write(
            gendir.join("junk"),
            "#!/bin/sh\nprintf 'not-a-directive\\n'\n",
        )
        .expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = gendir.join("junk");
            let mut perms = std::fs::metadata(&path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        let written = run_generators(&gendir, &out);
        assert!(written.is_empty(), "garbage must not become a file");
        let _ = std::fs::remove_dir_all(&gendir);
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A fresh, empty scratch directory named after `tag`.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zinit-sup-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// One process service that is ready the moment it exists, so the tests
    /// never depend on a handshake or on a binary being where it is claimed to
    /// be.
    fn write_conf(dir: &Path, name: &str) {
        let text = "command = /bin/true\nready = none\n";
        std::fs::write(dir.join(format!("{name}.conf")), text).expect("write");
    }
}
