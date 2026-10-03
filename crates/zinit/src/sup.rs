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
//! [`zcore::apply`] or [`zcore::reconcile`], and this module never writes a
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
    Action, Desired, Event, Idx, LogLevel, Plan, Runtime, SignalKind, State, Tick, Transition,
    apply, reconcile,
};
use zrt::childproc::{ChildTracker, new_child_tracker};
use zrt::reactor::{Interest, Reactor, new_reactor};
use zrt::report::announce_degradation;
use zrt::signals::{Signal, SignalSet, SignalSource, block_all, new_signal_source};
use zservice::identity::resolve_user_group;
use zservice::log::{LogHandle, open_sink, write_line};
use zservice::{ManagedService, SpawnCtx};

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

/// Run the supervisor: load `config_dir`, converge on it, never return.
///
/// Every failure below is announced and reported as an `Err`, which is what
/// `main` turns into a non-zero exit — the outcome PID 1 relaunches. Services
/// that are already running are left alone on the way out: nothing here kills
/// them, because PID 1's whole argument is that a restarted supervisor
/// reattaches to live services instead of disturbing them.
pub fn run(config_dir: &Path) -> io::Result<()> {
    let (plan, descs) = load(config_dir)?;
    Sup::new(plan, descs)?.supervise()
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
    /// gates against. Cleared when the service starts again, so a service that
    /// recovers and then exhausts itself a second time is heard about twice.
    budget_announced: Vec<Idx>,
    /// The services whose core invariants were last found violated, so a
    /// standing violation is announced once instead of once per pass.
    last_offenders: Vec<Idx>,
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

impl Sup {
    /// Build the primitives and the per-service slots.
    ///
    /// Every failure here is fatal, and the ordering is load-bearing: signals
    /// are blocked *before* the signal source is created, because a signalfd
    /// built over signals that are still deliverable never fires, and nothing
    /// about that failure is visible except a supervisor that quietly stops
    /// reaping (`zrt::signals` module docs).
    fn new(plan: Plan, descs: Vec<Option<ServiceDesc>>) -> io::Result<Sup> {
        if let Err(e) = block_all() {
            return Err(fatal(format!("cannot block signals: {e}")));
        }

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
            let tick = self.reconcile_now(now);
            self.execute(&tick.actions, now);
            if !tick.changed.is_empty() {
                self.check_invariants();
            }
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
            for due in [core.ready_due_at, core.start_due_at, core.stop_due_at] {
                if let Some(at) = due {
                    deadline = deadline.min(at);
                }
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
                if let Some(event) = self.slots[idx].svc.on_waitpid(status) {
                    self.feed(&event, now);
                }
                self.slots[idx].svc.cleanup();
                // `cleanup` closed the custodial fds, so the numbers the reactor
                // knew are meaningless now.
                self.slots[idx].registered = None;
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

            // 2. Readiness, but only for a service that is genuinely waiting for
            //    something. `poll_ready` answers `Ready` on *every* call once
            //    there is no handshake left to serve (`ready = none`, or a
            //    `notify` service whose pipe never arrived), and feeding that to
            //    the core would mark a service `Running` before its image was
            //    confirmed and cancel its `start-timeout`. Being `Starting` is
            //    the only state where the question means anything; every other
            //    one already knows the answer.
            //
            //    The pacing of `ping` and `tcp` lives in `zservice`; `arm` is
            //    what guarantees this call is made at all.
            if self.plan.services[idx].ready.is_handshake()
                && self.runtime.state_at(idx) == State::Starting
            {
                let ready = match self.slots[idx].svc.poll_ready(now) {
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

            // 3. Expired deadlines, mapped to events by the core itself.
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

            // 4. A stop nobody asked the core for.
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
                // The one thing an action cannot do quietly: a service that
                // is down and staying down. Announced rather than logged, because
                // the log file it would go to is exactly what an operator needs
                // to find afterwards — and announced *once*, because the core
                // repeats this action on every pass for as long as the bucket is
                // empty.
                Action::BudgetExhausted(idx) => {
                    if !self.budget_announced.contains(idx) {
                        self.budget_announced.push(*idx);
                        let name = self.name_of(*idx);
                        announce_degradation(&format!(
                            "service `{name}` is down and staying down: its restart \
                             budget is empty and nothing will retry it"
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
        let sp = &self.plan.services[idx];
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
        // A service that starts again has earned a fresh complaint if it runs
        // its budget dry a second time.
        self.budget_announced.retain(|&announced| announced != idx);
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

/// Read `config_dir`, resolve identities, and freeze the plan.
///
/// The three failures here are the only fatal ones, and all three are
/// unarguable: a file that does not parse, a graph that cannot be ordered, and
/// an account that cannot be resolved. A supervisor that started anyway would be
/// supervising a configuration it does not believe in — and in the identity
/// case, running a service as root because its user is missing, which is the
/// exact bug `zconfig`'s unresolved-identity trapdoor exists to prevent.
fn load(config_dir: &Path) -> io::Result<(Plan, Vec<Option<ServiceDesc>>)> {
    let entries = match std::fs::read_dir(config_dir) {
        Ok(entries) => entries,
        Err(e) => return Err(fatal(format!("{}: {e}", config_dir.display()))),
    };

    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => return Err(fatal(format!("{}: {e}", config_dir.display()))),
        };
        // ponytail: `*.conf` only. `DESIGN.md` §5.1 also spells
        // `network.target` as a file name; targets work today through
        // `type = target`, which is the same code path, so a second filename
        // convention would only be a second way to spell one thing.
        if path.extension() == Some(OsStr::new("conf")) {
            files.push(path);
        }
    }
    // Sorted, so the "first bad file" an operator is told about does not depend
    // on the order the filesystem happened to hand back.
    files.sort();

    let mut descs = Vec::with_capacity(files.len());
    for path in &files {
        let Some(name) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            return Err(fatal(format!("{}: unusable file name", path.display())));
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => return Err(fatal(format!("{}: {e}", path.display()))),
        };
        let file = path.display().to_string();
        match parse_service_with_diagnostics(&name, &text) {
            Ok(parsed) => {
                // Non-fatal findings. A bag of warnings is a *passing* run in
                // `zconfig`'s own words, so they are reported and the service is
                // loaded anyway.
                for warning in parsed.warnings.iter() {
                    announce_degradation(warning.render(&file).trim());
                }
                let mut desc = parsed.desc;
                desc.source = file;
                descs.push(desc);
            }
            Err(e) => return Err(fatal(format!("{}: {e}", path.display()))),
        }
    }

    // Identities. `zconfig` has no `/etc/passwd`, so `user = mysql` arrives
    // unresolved and `build_plan` refuses to freeze it. This is the pass the
    // plan builder is waiting for.
    for desc in &mut descs {
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

    let plan = match build_plan(&descs) {
        Ok(plan) => plan,
        Err(e) => return Err(fatal(format!("bad plan: {e}"))),
    };

    // Align descriptions to plan indices: the plan orders services
    // topologically, `read_dir` does not. A spawn with the wrong description is
    // a bug no type in this program would catch.
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

/// Announce `message` and turn it into the `Err` that stops the supervisor.
fn fatal(message: String) -> io::Error {
    announce_degradation(&message);
    io::Error::other(message)
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