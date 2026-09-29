//! One service, supervised: the lifecycle that ties everything together.
//!
//! # Where this sits
//!
//! [`zcore`] owns the state machine and the deadlines' *meaning*;
//! [`ManagedService`] owns the facts the machine reasons about: the pid, the
//! custodial fds, the readiness waiter, whether `TERM`/`KILL` went out. The
//! supervisor drives it in a fixed order per event-loop turn:
//!
//! ```text
//! poll_exec() → poll_ready() → check_deadlines() → stop_signal()
//! ```
//!
//! * [`ManagedService::poll_exec`] drains the exec-error pipe once: `EOF`
//!   synthesises [`zcore::Event::ExecOk`], four bytes synthesise
//!   [`zcore::Event::SpawnFailed`]. Nothing yet means nothing happened.
//! * [`ManagedService::poll_ready`] runs the readiness adapter when it is
//!   due. A completed handshake synthesises [`zcore::Event::Ready`].
//! * [`ManagedService::check_deadlines`] delegates to
//!   [`zcore::due_events`] — the mapping strict →
//!   `ReadyTimeout`, lenient → `Ready` already lives there — and stages a
//!   warning line for every lenient expiry, because "up anyway" without a
//!   warning would be the silence `DESIGN.md` §6 forbids.
//! * [`ManagedService::stop_signal`] escalates `TERM`-then-`KILL` across the
//!   stop timeout. It only *reports* which signal is due; the supervisor
//!   sends it (to the process *group*), keeping this crate kill-free.
//!
//! # What this deliberately does not do
//!
//! It never calls `kill`, never reaps, never touches the reactor. Signals and
//! reaps belong to the supervisor's event loop, which owns the child tracker
//! and the signal source. This type translates observations into events and
//! answers "what now" questions; acting on the answers stays in one place.

use std::io;
use std::os::fd::RawFd;

use zcore::{Event, Idx, Plan, Runtime, SignalKind, State, transition::arm_start_deadlines};

use crate::ready::{PollNeed, ReadyWait};
use crate::spawn::{SpawnCtx, Spawned, spawn};

/// Runtime states in which a child process is expected to exist.
///
/// Mirrors [`zcore::State::has_process`]: `Starting`, `Running`, `Stopping`.
/// Kept as a local predicate so call sites read as policy ("only signal a
/// service that has a process") rather than as a match on core internals.
fn state_has_process(s: State) -> bool {
    s.has_process()
}

/// One supervised service: the supervisor's per-service slot.
///
/// `idx` is the frozen plan index and never changes; everything else is
/// per-start state, reset by [`ManagedService::start`] and released by
/// [`ManagedService::cleanup`]. The [`Runtime`] slot (state, deadlines, pid)
/// is *not* owned here — the core owns it — so every method that changes
/// core-visible facts takes `&mut Runtime` explicitly. That split is what
/// keeps "who decided" (core) separate from "what is true" (here).
#[derive(Clone, Debug)]
pub struct ManagedService {
    /// Frozen plan index. Immutable.
    idx: Idx,
    /// The exec-error pipe reported `EOF`, so the image was replaced and the
    /// child's `setsid` has certainly run. Until this is set the process group
    /// is a prediction, and signalling it is a silent no-op.
    exec_confirmed: bool,
    /// Main child pid, once spawned.
    pid: Option<i32>,
    /// Process group (== pid: the child called `setsid` first).
    pgid: Option<i32>,
    /// Log fd, custodial copy owned by the supervisor.
    log_fd: Option<RawFd>,
    /// Notify-pipe read-end, while `Starting` with a notify handshake.
    notify_fd: Option<RawFd>,
    /// Exec-error pipe read-end, until the first conclusive read.
    exec_fd: Option<RawFd>,
    /// Pending readiness handshake for the current start.
    ready: ReadyWait,
    /// Monotonic ms at which `SIGTERM` was (first) reported due.
    term_sent_at: Option<u64>,
    /// Whether `SIGKILL` was already reported due for this stop.
    kill_sent: bool,
    /// Warning staged by a lenient readiness expiry, for the supervisor's log.
    pending_warning: Option<String>,
}

impl ManagedService {
    /// A service that has never started: no process, no fds, no waiter.
    pub fn new(idx: Idx) -> ManagedService {
        ManagedService {
            idx,
            exec_confirmed: false,
            pid: None,
            pgid: None,
            log_fd: None,
            notify_fd: None,
            exec_fd: None,
            ready: ReadyWait::None,
            term_sent_at: None,
            kill_sent: false,
            pending_warning: None,
        }
    }

    /// The frozen plan index.
    pub const fn idx(&self) -> Idx {
        self.idx
    }

    /// The main child pid, if a start is in flight or running.
    pub const fn pid(&self) -> Option<i32> {
        self.pid
    }

    /// The process group to signal, if any.
    pub const fn pgid(&self) -> Option<i32> {
        self.pgid
    }

    /// Start the service: spawn, record, arm deadlines.
    ///
    /// On success the [`Runtime`] slot is `Starting` with pid, pgid,
    /// `started_at` and both deadlines armed
    /// ([`zcore::transition::arm_start_deadlines`]), and
    /// this struct holds the custodial fds plus the readiness waiter. On
    /// failure **nothing is mutated** — neither here nor in the runtime — and
    /// the caller feeds [`Event::SpawnFailed`] to the core with the returned
    /// errno. A half-recorded start (pid known, deadlines not armed) would
    /// violate the core's `Starting` invariants, so atomicity here is not
    /// polish, it is the contract.
    pub fn start(
        &mut self,
        plan: &Plan,
        runtime: &mut Runtime,
        ctx: &SpawnCtx<'_>,
        now_ms: u64,
    ) -> io::Result<()> {
        let spawned: Spawned = spawn(plan, self.idx, ctx)?;
        // `pgid` is *predicted*, not observed: the child calls `setsid` as its
        // first action, so the group will be `pid`, but it does not exist at
        // this instant. Nothing may signal it yet — see
        // [`ManagedService::group_is_signallable`], which is the only thing
        // allowed to conclude otherwise, and only once the exec pipe says the
        // image was really replaced.
        self.pid = Some(spawned.pid);
        self.pgid = Some(spawned.pgid);
        self.exec_confirmed = false;
        self.log_fd = spawned.log_fd;
        self.notify_fd = spawned.notify_fd;
        self.exec_fd = spawned.exec_fd;
        self.ready =
            ReadyWait::from_ready(&plan.services[self.idx].ready, spawned.notify_fd, now_ms);
        self.term_sent_at = None;
        self.kill_sent = false;
        self.pending_warning = None;
        arm_start_deadlines(runtime, plan, self.idx, now_ms);
        let slot = runtime.get_mut(self.idx);
        slot.pid = Some(spawned.pid);
        slot.pgid = Some(spawned.pgid);
        slot.started_at = Some(now_ms);
        Ok(())
    }

    /// Wait, bounded, for the child to replace its image.
    ///
    /// [`poll_exec`](Self::poll_exec) is non-blocking, which is right for the
    /// event loop and useless before a group signal: the only way to stop a
    /// service that has children is `kill(-pgid)`, and that group does not
    /// exist until the child has called `setsid` and `execve`d. This closes
    /// the gap for callers that need the fact synchronously, with
    /// `start-timeout` as the ceiling so a wedged child cannot hold anyone up.
    ///
    /// Returns the same events [`poll_exec`](Self::poll_exec) would have
    /// produced, so a caller that used to poll in a loop can drop the loop and
    /// keep the semantics.
    pub fn wait_exec(&mut self, plan: &Plan, _now_ms: u64) -> io::Result<Option<Event>> {
        // The deadline is computed from the *real* monotonic clock, and
        // `now_ms` is deliberately ignored. `now_ms` is the supervisor's
        // logical time and callers pass whatever they last read; a caller that
        // started the service at logical time 0 and then waited would have
        // its deadline already in the past, and the wait would give up before
        // the child could exec. The wait is bounded by wall time it actually
        // spends, which is the only thing that matters here.
        let deadline =
            zrt::clock::now_ms().saturating_add(plan.services[self.idx].start_timeout_ms);
        loop {
            if let Some(ev) = self.poll_exec()? {
                return Ok(Some(ev));
            }
            if self.exec_fd.is_none() {
                // Drained and inconclusive: the child is gone without having
                // reported anything, and the reaper will speak for it.
                return Ok(None);
            }
            if zrt::clock::now_ms() >= deadline {
                return Ok(None);
            }
            zrt::clock::sleep_ms(1)?;
        }
    }

    /// True when the process group can be signalled without hitting `ESRCH`.
    ///
    /// Between `fork` and the child's `setsid` the predicted group does not
    /// exist. A `kill(-pgid)` sent in that window is silently lost, and a
    /// service that wedged there would then survive its own `stop-timeout`
    /// and every `SIGKILL` after it — the escalation would report success
    /// while the process kept running. The exec-error pipe is `CLOEXEC`, so
    /// its `EOF` is proof the image was replaced, and the child necessarily
    /// called `setsid` before `execve`. Until that proof, callers must
    /// signal the bare pid instead, which cannot be lost to a missing group.
    pub fn group_is_signallable(&self) -> bool {
        self.exec_confirmed
    }

    /// Drain the exec-error pipe once.
    ///
    /// `Some(ExecOk)` on `EOF` (the image was replaced — the first point at
    /// which the process counts as real), `Some(SpawnFailed{errno})` on four
    /// bytes (whatever failed in the child, with its errno), `None` while
    /// inconclusive. The pipe is closed on the first conclusive read; later
    /// calls answer `None` without touching the kernel.
    ///
    /// Known race, documented: a child killed between `fork` and `exec` also
    /// reads as `EOF`. The supervisor correlates with the reaper — a
    /// `Signalled` for a service still `Starting` supersedes an optimistic
    /// `ExecOk` — rather than this function pretending the pipe can tell
    /// "exec succeeded" from "died silently".
    pub fn poll_exec(&mut self) -> io::Result<Option<Event>> {
        let fd = match self.exec_fd {
            Some(f) => f,
            None => return Ok(None),
        };
        let mut buf = [0u8; 4];
        let mut got = 0;
        loop {
            let mut one = [0u8; 1];
            match zrt::sys::read(fd, &mut one) {
                Ok(0) => {
                    // EOF: the write-end is gone. Success closed it via
                    // CLOEXEC on exec; a silent death closed it via exit.
                    //
                    // The `setsid` in the child happened before that `execve`,
                    // so from here the predicted pgid is a real group and
                    // `kill(-pgid)` stops being a guess. The silent-death case
                    // is indistinguishable here, and is resolved by the
                    // reaper; setting the flag costs at most one `SIGKILL`
                    // aimed at a pid that is already gone.
                    self.exec_confirmed = true;
                    self.close_exec_fd();
                    return Ok(Some(Event::ExecOk(self.idx)));
                }
                Ok(_) => {
                    buf[got] = one[0];
                    got += 1;
                    if got == 4 {
                        let errno = u32::from_le_bytes(buf) as i32;
                        self.close_exec_fd();
                        return Ok(Some(Event::SpawnFailed {
                            idx: self.idx,
                            errno,
                        }));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// Run the readiness adapter when it is due.
    ///
    /// Returns `Some(Ready)` the moment the handshake completes — and closes
    /// the notify pipe then, so a second line from a chatty service cannot
    /// re-trigger readiness later. `None` means "not yet"; consult
    /// [`ManagedService::poll_need`] for when to ask again. Errors are the
    /// probe's own failures (never a merely-unready service; that is
    /// `Ok(None)`).
    pub fn poll_ready(&mut self, now_ms: u64) -> io::Result<Option<Event>> {
        if self.ready.check(now_ms)? {
            self.close_notify_fd();
            self.ready = ReadyWait::None;
            return Ok(Some(Event::Ready(self.idx)));
        }
        Ok(None)
    }

    /// What the reactor must do to serve this service's pending waits.
    ///
    /// The exec-error pipe takes priority while it is open: knowing whether
    /// the process image is real outranks knowing whether it is ready, and
    /// the pipe resolves within milliseconds of the spawn either way.
    pub fn poll_need(&self, now_ms: u64) -> PollNeed {
        if let Some(fd) = self.exec_fd {
            return PollNeed {
                fd: Some(fd),
                timeout_ms: None,
            };
        }
        self.ready.poll_need(now_ms)
    }

    /// Expired deadlines, as core events, plus a staged warning if lenient.
    ///
    /// Delegates the *mapping* to
    /// [`zcore::due_events`] — strict expiry →
    /// `ReadyTimeout`, lenient expiry → `Ready` — so the two can never
    /// disagree about what "strict" means. For every lenient `Ready` that
    /// comes back, a warning is staged for [`ManagedService::take_warning`]:
    /// "up anyway" without a log line would be exactly the silence the
    /// readiness rule forbids.
    pub fn check_deadlines(&mut self, runtime: &Runtime, plan: &Plan, now_ms: u64) -> Vec<Event> {
        let events = zcore::due_events(runtime, plan, now_ms);
        for e in &events {
            if matches!(e, Event::Ready(idx) if *idx == self.idx) {
                let ms = plan.services[self.idx].ready_timeout_ms;
                self.pending_warning = Some(format!(
                    "readiness handshake timed out after {ms}ms; marking Running anyway"
                ));
            }
        }
        events.into_iter().filter(|e| e.idx() == self.idx).collect()
    }

    /// Take the staged readiness warning, if any.
    ///
    /// The supervisor logs this at `Warn` when it feeds the synthesised
    /// `Ready` to the core. `None` on strict timeouts (the core logs those
    /// at `Error` itself) and when nothing expired.
    pub fn take_warning(&mut self) -> Option<String> {
        self.pending_warning.take()
    }

    /// Which signal, if any, the supervisor should send now.
    ///
    /// First call reports `Term` and arms `term_sent_at`/`stop_due_at` in the
    /// runtime when the core has not already (the cascade paths arm them in
    /// the transition function; a direct stop arms them here — one place that
    /// guarantees the deadline exists). After `stop-timeout` expires, one
    /// `Kill`, then `None` forever: the escalation fires exactly once because
    /// a supervisor that re-`KILL`s every tick is a supervisor hiding a wedged
    /// service from its own logs.
    ///
    /// Returns `None` when the service has no process (nothing to signal) or
    /// when the escalation already ran its course (the reaper owns the wait
    /// from here).
    pub fn stop_signal(
        &mut self,
        runtime: &mut Runtime,
        plan: &Plan,
        now_ms: u64,
    ) -> Option<SignalKind> {
        if self.pid.is_none() || !state_has_process(runtime.state_at(self.idx)) {
            return None;
        }
        if self.term_sent_at.is_none() {
            self.term_sent_at = Some(now_ms);
            let slot = runtime.get_mut(self.idx);
            slot.term_sent_at = Some(now_ms);
            if slot.stop_due_at.is_none() {
                slot.stop_due_at =
                    Some(now_ms.saturating_add(plan.services[self.idx].stop_timeout_ms));
            }
            return Some(SignalKind::Term);
        }
        if !self.kill_sent {
            let due = runtime.get(self.idx).stop_due_at.unwrap_or(u64::MAX);
            if now_ms >= due {
                self.kill_sent = true;
                return Some(SignalKind::Kill);
            }
        }
        None
    }

    /// Translate a reaped waitpid status into a core event.
    ///
    /// Exits become `Exited{code}` with the code preserved exactly; signal
    /// deaths become `Signalled{signal}`. Stop/continue reports (`Reported`,
    /// which zinit never asks for — no `WUNTRACED` anywhere) produce `None`:
    /// they are not deaths, and synthesising a death from them would restart
    /// a service that never died.
    pub fn on_waitpid(&self, status: zrt::sys::ExitStatus) -> Option<Event> {
        match status {
            zrt::sys::ExitStatus::Exited(code) => Some(Event::Exited {
                idx: self.idx,
                code,
            }),
            zrt::sys::ExitStatus::Signaled { signal, .. } => Some(Event::Signalled {
                idx: self.idx,
                signal,
            }),
            zrt::sys::ExitStatus::Reported(_) => None,
        }
    }

    /// Release every custodial fd and reset per-start state.
    ///
    /// Called after the reaper reports the child gone (and on every teardown
    /// path that abandons a start). Close errors are ignored on purpose: this
    /// runs where the only alternative to ignoring them is failing a reap,
    /// and a reap that fails over an `EBADF` resurrects zombies. The plan
    /// index survives — it is the one field that is not per-start.
    pub fn cleanup(&mut self) {
        self.close_notify_fd();
        self.close_exec_fd();
        if let Some(fd) = self.log_fd.take() {
            let _ = zrt::sys::close_quietly(fd);
        }
        self.pid = None;
        self.pgid = None;
        self.ready = ReadyWait::None;
        self.term_sent_at = None;
        self.kill_sent = false;
        self.pending_warning = None;
    }

    fn close_notify_fd(&mut self) {
        if let Some(fd) = self.notify_fd.take() {
            let _ = zrt::sys::close_quietly(fd);
        }
    }

    fn close_exec_fd(&mut self) {
        if let Some(fd) = self.exec_fd.take() {
            let _ = zrt::sys::close_quietly(fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcore::{Desired, LogSink, Ready, ServicePlan, StrictReady};

    fn plan_with(sp: ServicePlan) -> Plan {
        Plan {
            services: vec![sp],
            order_up: vec![0],
            order_down: vec![0],
        }
    }

    fn base(name: &str, ready: Ready, log: LogSink) -> (Plan, SpawnCtx<'static>) {
        // Leaked on purpose: the ctx borrows for `'static` so tests read
        // without lifetime noise. A handful of bytes per test binary run.
        fn leak(s: &str) -> &'static str {
            Box::leak(s.to_string().into_boxed_str())
        }
        let mut sp = ServicePlan::new(String::from(name));
        sp.ready = ready;
        let plan = plan_with(sp);
        let ctx = SpawnCtx {
            command: leak("/bin/sleep 30"),
            env: &[],
            run_as: None,
            rlimits: &[],
            cgroup: None,
            log: Box::leak(Box::new(log)),
            service_name: leak(name),
            tty: None,
            nice: None,
        };
        (plan, ctx)
    }

    fn true_ctx(name: &'static str, log: &'static LogSink) -> SpawnCtx<'static> {
        SpawnCtx {
            command: "/bin/true",
            env: &[],
            run_as: None,
            rlimits: &[],
            cgroup: None,
            log,
            service_name: name,
            tty: None,
            nice: None,
        }
    }

    fn reap_status(pid: i32) -> zrt::sys::ExitStatus {
        zrt::sys::waitpid_blocking(pid).expect("reap").status
    }

    /// Kill a service the way the runtime does: the process group once the
    /// exec pipe has proved the group exists, the bare pid before that.
    ///
    /// Mirrors [`ManagedService::group_is_signallable`] on purpose. The tests
    /// that go through this helper are testing the supervisor's signalling
    /// contract, and that contract is "never send a signal that cannot land".
    /// Wait until `pid` has a child, i.e. it is past the point where a shell
    /// has installed its traps and entered its loop.
    ///
    /// Reading `/proc/<pid>/task/<pid>/children` is Linux-only, and this is a
    /// test helper on a machine that happens to be Linux, so that is fine — but
    /// the timeout is what makes it safe to assert on: if the child never
    /// shows up the helper returns and the following assertion fails with a
    /// message about the mechanism rather than hanging.
    fn wait_for_grandchild(pid: i32, timeout_ms: u64) {
        let path = format!("/proc/{pid}/task/{pid}/children");
        let deadline = zrt::clock::now_ms().saturating_add(timeout_ms);
        loop {
            if let Ok(text) = std::fs::read_to_string(&path)
                && !text.trim().is_empty()
            {
                return;
            }
            if zrt::clock::now_ms() >= deadline {
                panic!("{pid} never forked a child; the service is not running yet");
            }
            zrt::clock::sleep_ms(5).expect("sleep");
        }
    }

    fn kill_group(svc: &ManagedService, sig: zrt::signals::Signal) {
        let pid = svc.pid().expect("a spawned service has a pid");
        if svc.group_is_signallable() {
            let _ = zrt::sys::kill_group(svc.pgid().unwrap_or(pid), sig);
        } else {
            let _ = zrt::sys::kill_process(pid, sig);
        }
    }

    #[test]
    fn start_arms_deadlines_and_records_the_pid() {
        let (plan, ctx) = base("s", Ready::None, LogSink::None);
        let mut rt = Runtime::from_plan(&plan);
        rt.set_desired(0, Desired::Up);
        let mut svc = ManagedService::new(0);
        svc.start(&plan, &mut rt, &ctx, 1000).expect("start");
        assert_eq!(rt.state_at(0), State::Starting);
        let slot = rt.get(0);
        assert_eq!(slot.pid, svc.pid());
        assert_eq!(slot.pgid, svc.pgid());
        assert_eq!(slot.started_at, Some(1000));
        assert!(slot.start_due_at.is_some());
        // `ready = none`: no handshake, no ready deadline — a deadline nobody
        // can satisfy is a way to make a config file lie.
        assert!(slot.ready_due_at.is_none());
        assert!(rt.check_invariants(&plan).is_empty());
        kill_group(&svc, zrt::signals::Signal::Kill);
        let pid = svc.pid().expect("pid");
        svc.cleanup();
        assert_eq!(
            reap_status(pid),
            zrt::sys::ExitStatus::Signaled {
                signal: 9,
                core_dumped: false
            }
        );
    }

    #[test]
    fn failed_start_mutates_nothing() {
        let (plan, _) = base("s", Ready::None, LogSink::None);
        let mut rt = Runtime::from_plan(&plan);
        let log: &'static LogSink = Box::leak(Box::new(LogSink::None));
        let bad = SpawnCtx {
            command: "/nonexistent/zinit-probe",
            ..true_ctx("s", log)
        };
        let mut svc = ManagedService::new(0);
        assert!(svc.start(&plan, &mut rt, &bad, 0).is_err());
        assert_eq!(rt.state_at(0), State::Stopped);
        assert!(svc.pid().is_none());
        assert!(rt.check_invariants(&plan).is_empty());
    }

    #[test]
    fn exec_ok_arrives_through_the_pipe() {
        let log: &'static LogSink = Box::leak(Box::new(LogSink::None));
        let mut sp = ServicePlan::new(String::from("t"));
        sp.ready = Ready::None;
        sp.log = LogSink::None;
        let plan = plan_with(sp);
        let mut rt = Runtime::from_plan(&plan);
        let ctx = true_ctx("t", log);
        let mut svc = ManagedService::new(0);
        svc.start(&plan, &mut rt, &ctx, 0).expect("start");
        // The exec needs a scheduling slice; poll until conclusive.
        let deadline = zrt::clock::now_ms().saturating_add(5_000);
        let verdict = loop {
            if let Some(e) = svc.poll_exec().expect("poll") {
                break e;
            }
            assert!(zrt::clock::now_ms() < deadline, "exec verdict never came");
            zrt::clock::sleep_ms(5).expect("sleep");
        };
        assert_eq!(verdict, Event::ExecOk(0));
        // Consumed: the pipe is closed, later polls are quiet.
        assert!(svc.poll_exec().expect("poll").is_none());
        let pid = svc.pid().expect("pid");
        svc.cleanup();
        assert_eq!(reap_status(pid), zrt::sys::ExitStatus::Exited(0));
    }

    #[test]
    fn waitpid_status_maps_to_core_events() {
        let svc = ManagedService::new(3);
        assert_eq!(
            svc.on_waitpid(zrt::sys::ExitStatus::Exited(5)),
            Some(Event::Exited { idx: 3, code: 5 })
        );
        assert_eq!(
            svc.on_waitpid(zrt::sys::ExitStatus::Signaled {
                signal: 15,
                core_dumped: false
            }),
            Some(Event::Signalled { idx: 3, signal: 15 })
        );
        assert_eq!(svc.on_waitpid(zrt::sys::ExitStatus::Reported(0x7f)), None);
    }

    #[test]
    fn death_in_starting_applies_cleanly() {
        let (plan, ctx) = base("s", Ready::None, LogSink::None);
        let mut rt = Runtime::from_plan(&plan);
        rt.set_desired(0, Desired::Up);
        let mut svc = ManagedService::new(0);
        svc.start(&plan, &mut rt, &ctx, 0).expect("start");
        kill_group(&svc, zrt::signals::Signal::Kill);
        let pid = svc.pid().expect("pid");
        let status = reap_status(pid);
        let event = svc.on_waitpid(status).expect("a death is an event");
        svc.cleanup();
        let t = zcore::transition::apply(&event, &mut rt, &plan, 10);
        assert_eq!(rt.state_at(0), State::Stopped);
        assert!(rt.check_invariants(&plan).is_empty());
        let _ = t;
    }

    #[test]
    fn lenient_ready_timeout_becomes_ready_with_a_warning() {
        let (plan, _) = base("s", Ready::Notify, LogSink::None);
        let mut rt = Runtime::from_plan(&plan);
        rt.get_mut(0).state = State::Starting;
        rt.get_mut(0).pid = Some(99);
        rt.get_mut(0).pgid = Some(99);
        rt.get_mut(0).started_at = Some(0);
        rt.get_mut(0).start_due_at = Some(60_000);
        rt.get_mut(0).ready_due_at = Some(1_000);
        let mut svc = ManagedService::new(0);
        let events = svc.check_deadlines(&rt, &plan, 1_000);
        assert!(events.contains(&Event::Ready(0)), "{events:?}");
        let warning = svc.take_warning().expect("lenient expiry warns");
        assert!(warning.contains("marking Running anyway"), "{warning}");
        assert!(svc.take_warning().is_none(), "one-shot");
        // And the core agrees this is Running, without blocking the boot.
        let t = zcore::transition::apply(&Event::Ready(0), &mut rt, &plan, 1_000);
        assert_eq!(rt.state_at(0), State::Running);
        let _ = t;
    }

    #[test]
    fn strict_ready_timeout_is_an_error_not_a_warning() {
        let (plan, _) = base("s", Ready::Strict(StrictReady::Notify), LogSink::None);
        let mut rt = Runtime::from_plan(&plan);
        rt.get_mut(0).state = State::Starting;
        rt.get_mut(0).pid = Some(99);
        rt.get_mut(0).pgid = Some(99);
        rt.get_mut(0).started_at = Some(0);
        rt.get_mut(0).start_due_at = Some(60_000);
        rt.get_mut(0).ready_due_at = Some(1_000);
        let mut svc = ManagedService::new(0);
        let events = svc.check_deadlines(&rt, &plan, 1_000);
        assert!(events.contains(&Event::ReadyTimeout(0)), "{events:?}");
        assert!(
            svc.take_warning().is_none(),
            "strict expires as error, not warning"
        );
    }

    #[test]
    fn start_and_stop_timeouts_surface() {
        let (plan, _) = base("s", Ready::None, LogSink::None);
        let mut rt = Runtime::from_plan(&plan);
        rt.get_mut(0).state = State::Starting;
        rt.get_mut(0).pid = Some(1);
        rt.get_mut(0).started_at = Some(0);
        rt.get_mut(0).start_due_at = Some(100);
        let mut svc = ManagedService::new(0);
        assert!(svc.check_deadlines(&rt, &plan, 50).is_empty());
        assert!(
            svc.check_deadlines(&rt, &plan, 100)
                .contains(&Event::StartTimeout(0))
        );
        rt.get_mut(0).state = State::Stopping;
        rt.get_mut(0).stop_due_at = Some(200);
        assert!(
            svc.check_deadlines(&rt, &plan, 200)
                .contains(&Event::StopTimeout(0))
        );
    }

    #[test]
    fn stop_escalates_term_then_kill_exactly_once() {
        let (plan, _) = base("s", Ready::None, LogSink::None);
        let mut rt = Runtime::from_plan(&plan);
        rt.get_mut(0).state = State::Stopping;
        rt.get_mut(0).pid = Some(1);
        rt.get_mut(0).pgid = Some(1);
        rt.get_mut(0).started_at = Some(0);
        rt.get_mut(0).stop_due_at = Some(10_000);
        let mut svc = ManagedService::new(0);
        // ManagedService tracks pids independently of the runtime slot; the
        // test plants one so there is something to signal.
        svc.pid = Some(1);
        svc.pgid = Some(1);
        assert_eq!(svc.stop_signal(&mut rt, &plan, 0), Some(SignalKind::Term));
        assert_eq!(svc.stop_signal(&mut rt, &plan, 1), None);
        assert_eq!(
            svc.stop_signal(&mut rt, &plan, 10_000),
            Some(SignalKind::Kill)
        );
        assert_eq!(svc.stop_signal(&mut rt, &plan, 20_000), None);
    }

    #[test]
    fn stop_signal_arms_the_deadline_when_the_core_did_not() {
        let (plan, _) = base("s", Ready::None, LogSink::None);
        let mut rt = Runtime::from_plan(&plan);
        rt.get_mut(0).state = State::Stopping;
        rt.get_mut(0).pid = Some(1);
        rt.get_mut(0).started_at = Some(0);
        assert!(rt.get(0).stop_due_at.is_none());
        let mut svc = ManagedService::new(0);
        svc.pid = Some(1);
        svc.pgid = Some(1);
        assert_eq!(svc.stop_signal(&mut rt, &plan, 500), Some(SignalKind::Term));
        assert_eq!(rt.get(0).stop_due_at, Some(10_500));
    }

    #[test]
    fn sigterm_then_clean_exit_end_to_end() {
        let (plan, ctx) = base("s", Ready::None, LogSink::None);
        let mut rt = Runtime::from_plan(&plan);
        rt.set_desired(0, Desired::Up);
        let mut svc = ManagedService::new(0);
        svc.start(&plan, &mut rt, &ctx, 0).expect("start");
        // Become Stopping the way the core does, then TERM the group.
        rt.get_mut(0).state = State::Stopping;
        rt.get_mut(0).stop_due_at = Some(10_000);
        assert_eq!(svc.stop_signal(&mut rt, &plan, 0), Some(SignalKind::Term));
        kill_group(&svc, zrt::signals::Signal::Term);
        let pid = svc.pid().expect("pid");
        let status = reap_status(pid);
        let event = svc.on_waitpid(status).expect("death");
        svc.cleanup();
        let t = zcore::transition::apply(&event, &mut rt, &plan, 5);
        assert_eq!(rt.state_at(0), State::Stopped);
        assert!(
            !t.actions.iter().any(|a| matches!(
                a,
                zcore::Action::MarkDown {
                    unexpected: true,
                    ..
                }
            )),
            "a requested stop is not a crash: {t:?}"
        );
        assert!(rt.check_invariants(&plan).is_empty());
    }

    #[test]
    fn sigkill_after_stop_timeout_end_to_end() {
        // `sleep` under a TERM trap: the TERM is ignored, so the stop only
        // completes via the KILL escalation. `stop-timeout` is 300 ms so the
        // test measures the mechanism, not patience.
        let mut sp = ServicePlan::new(String::from("stubborn"));
        sp.kind = zcore::ServiceKind::Script;
        sp.ready = Ready::None;
        sp.log = LogSink::None;
        sp.stop_timeout_ms = 300;
        let plan = plan_with(sp);
        // TERM is ignored by the shell, so the stop only completes via the
        // KILL escalation.
        //
        // Two earlier versions of this script were both wrong in instructive
        // ways:
        //
        // * `trap "" TERM; exec /bin/sleep 30` — `exec` replaces the shell and
        //   a replaced shell has no traps, so `sleep` died on the default TERM
        //   disposition. The test would have passed while measuring nothing.
        // * `trap '' TERM; while :; do sleep 1; done` — the shell ignores
        //   TERM but its `sleep` children do not. `kill(-pgid, TERM)` killed
        //   the child, the loop advanced, and the shell exited: the service
        //   stopped at the TERM, and the assertion "still alive" failed about
        //   three times in five.
        //
        // The `& wait` form is the one that actually models a service that
        // refuses SIGTERM: the shell traps it, and `wait` is interrupted by a
        // trapped signal rather than by a dying child, so the loop only ends
        // when the shell itself is killed.
        let ctx = SpawnCtx {
            command: Box::leak(Box::new(String::from(
                "trap '' TERM INT HUP; while :; do sleep 1 & wait $!; done",
            ))),
            env: &[],
            run_as: None,
            rlimits: &[],
            cgroup: None,
            log: Box::leak(Box::new(LogSink::None)),
            service_name: Box::leak(Box::new(String::from("stubborn"))),
            tty: None,
            nice: None,
        };
        let mut rt = Runtime::from_plan(&plan);
        rt.set_desired(0, Desired::Up);
        let mut svc = ManagedService::new(0);
        svc.start(&plan, &mut rt, &ctx, 0).expect("start");
        // The exec pipe must be drained *before* signalling anything. Until it
        // reports `EOF` the process group is a prediction, and a real service
        // with children — which is what this test's trapped shell spawns — can
        // only be stopped as a group. This is the same ordering the supervisor
        // follows, so the test cannot pass on a signalling path production
        // would never take.
        assert!(
            matches!(
                svc.wait_exec(&plan, 0).expect("wait"),
                Some(Event::ExecOk(_))
            ),
            "the child must exec before the group can be signalled"
        );
        assert!(svc.group_is_signallable());
        let pid = svc.pid().expect("pid");
        rt.get_mut(0).state = State::Stopping;
        assert_eq!(svc.stop_signal(&mut rt, &plan, 0), Some(SignalKind::Term));
        // The child has exec'd, but `/bin/sh` still has to *install* its trap
        // before a TERM can be ignored. A fixed sleep races that: the signal
        // can arrive first, hit the default disposition, and the service dies
        // at the TERM - which is exactly what the next assertion says must not
        // happen. Waiting for the shell to have a child of its own is a
        // observable "the loop is running" signal, so the test measures the
        // mechanism instead of the scheduler.
        wait_for_grandchild(svc.pid().expect("pid"), 2000);
        kill_group(&svc, zrt::signals::Signal::Term);
        zrt::clock::sleep_ms(50).expect("sleep");
        // Still alive: TERM was trapped. No escalation before the deadline.
        assert!(svc.stop_signal(&mut rt, &plan, 50).is_none());
        assert!(zrt::sys::waitpid_nohang(pid).expect("wait").is_none());
        // Past the deadline: KILL, exactly once.
        assert_eq!(svc.stop_signal(&mut rt, &plan, 500), Some(SignalKind::Kill));
        assert_eq!(svc.stop_signal(&mut rt, &plan, 600), None);
        kill_group(&svc, zrt::signals::Signal::Kill);
        // The service must be *dead*, and the reason it is dead is KILL.
        //
        // The assertion used to be "the reaped status is exactly
        // `Signaled{9}`", which is a claim about the shell's own exit and is
        // genuinely racy: `kill(-pgid)` lands on the whole group at once, and
        // whether the trapped shell or the `sleep` it is currently waiting on
        // is the one the reaper observes first varies between runs. The test
        // passed about two times in five.
        //
        // What the supervisor actually promises is weaker and checkable: the
        // process is gone, and it went because of the escalation. `Exited`
        // would mean something answered the KILL, which nothing does, so it is
        // excluded explicitly; the signal is 9 for the same reason.
        let status = reap_status(pid);
        match status {
            zrt::sys::ExitStatus::Signaled { signal: 9, .. } => {}
            other => panic!("the KILL escalation must be what killed it, got {other:?}"),
        }
        svc.cleanup();
    }

    #[test]
    fn notify_handshake_end_to_end() {
        // The child notifies; the supervisor observes readiness through the
        // pipe, feeds Ready to the core, and the service is Running.
        let mut sp = ServicePlan::new(String::from("ntfy"));
        sp.kind = zcore::ServiceKind::Script;
        sp.ready = Ready::Notify;
        sp.log = LogSink::None;
        let plan = plan_with(sp);
        let ctx = SpawnCtx {
            command: Box::leak(Box::new(String::from("echo ready >&3; exec /bin/sleep 30"))),
            env: &[],
            run_as: None,
            rlimits: &[],
            cgroup: None,
            log: Box::leak(Box::new(LogSink::None)),
            service_name: Box::leak(Box::new(String::from("ntfy"))),
            tty: None,
            nice: None,
        };
        let mut rt = Runtime::from_plan(&plan);
        rt.set_desired(0, Desired::Up);
        let mut svc = ManagedService::new(0);
        svc.start(&plan, &mut rt, &ctx, 0).expect("start");
        let deadline = zrt::clock::now_ms().saturating_add(5_000);
        let ready = loop {
            if let Some(e) = svc.poll_ready(zrt::clock::now_ms()).expect("poll") {
                break e;
            }
            assert!(zrt::clock::now_ms() < deadline, "notify never arrived");
            zrt::clock::sleep_ms(5).expect("sleep");
        };
        assert_eq!(ready, Event::Ready(0));
        let t = zcore::transition::apply(&ready, &mut rt, &plan, 10);
        assert_eq!(rt.state_at(0), State::Running);
        assert!(rt.check_invariants(&plan).is_empty());
        let _ = t;
        kill_group(&svc, zrt::signals::Signal::Kill);
        let pid = svc.pid().expect("pid");
        let status = reap_status(pid);
        svc.cleanup();
        let _ = svc.on_waitpid(status);
    }

    #[test]
    fn log_file_captures_real_child_output() {
        // The child's bytes land in the file the plan named, verbatim.
        let dir = crate::testutil::scratch("svc");
        let path = dir.join("svc.log");
        let log: &'static LogSink = Box::leak(Box::new(zcore::LogSink::File {
            path: path.to_string_lossy().into_owned(),
            max_bytes: 1 << 20,
            backups: 1,
        }));
        let mut sp = ServicePlan::new(String::from("logged"));
        sp.ready = Ready::None;
        sp.log = LogSink::None;
        let plan = plan_with(sp);
        let ctx = SpawnCtx {
            command: Box::leak(Box::new(String::from("/bin/echo hello-from-child"))),
            env: &[],
            run_as: None,
            rlimits: &[],
            cgroup: None,
            log,
            service_name: Box::leak(Box::new(String::from("logged"))),
            tty: None,
            nice: None,
        };
        let mut rt = Runtime::from_plan(&plan);
        let mut svc = ManagedService::new(0);
        svc.start(&plan, &mut rt, &ctx, 0).expect("start");
        let pid = svc.pid().expect("pid");
        let status = reap_status(pid);
        assert_eq!(status, zrt::sys::ExitStatus::Exited(0));
        svc.cleanup();
        let content = std::fs::read(&path).expect("log file exists");
        assert_eq!(content, b"hello-from-child\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
