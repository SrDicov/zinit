//! Runtime state: the per-service slot the reconciler mutates.
//!
//! [`Runtime`] owns exactly the mutable data the core needs. It does **not**
//! own pids it cannot interpret, file descriptors, or anything else that only
//! the runtime can act on - it tracks those only as opaque numbers it is told
//! about. That keeps the whole supervisor state a plain, comparable, `Vec` of
//! PODs, which is what makes the property tests in `reconcile.rs` meaningful.

use alloc::string::String;
use alloc::vec::Vec;

use crate::types::{Bucket, Budget, Desired, Idx, MAX_SERVICES, Plan, State};

/// Per-service mutable state.
#[derive(Clone, Debug)]
pub struct ServiceState {
    /// What the operator wants.
    pub desired: Desired,
    /// What is actually true.
    pub state: State,
    /// Pid of the main child, once known. `None` when no process exists.
    ///
    /// The core never validates this. It exists so the runtime can report it
    /// and so [`crate::reconcile()`] can avoid re-spawning a service it believes
    /// is already running.
    pub pid: Option<i32>,
    /// Process group id, which is what actually gets signalled.
    pub pgid: Option<i32>,
    /// Monotonic ms at which the current process started. `None` when stopped.
    pub started_at: Option<u64>,
    /// Monotonic ms at which `SIGTERM` was sent. `None` unless stopping.
    pub term_sent_at: Option<u64>,
    /// Monotonic ms at which readiness is due. `None` unless starting.
    pub ready_due_at: Option<u64>,
    /// Monotonic ms at which `start-timeout` fires. `None` unless starting.
    pub start_due_at: Option<u64>,
    /// Monotonic ms at which `stop-timeout` fires. `None` unless stopping.
    pub stop_due_at: Option<u64>,
    /// The restart budget bucket.
    pub bucket: Bucket,
    /// Restarts actually performed, for reporting. Does not gate anything -
    /// the bucket does.
    pub restarts: u32,
    /// The service is pinned and refuses to be stopped.
    pub pinned: bool,
    /// This service holds the controlling terminal.
    pub is_console: bool,
    /// Exit code of the last death, for `zctl status` and for diagnostics.
    pub last_exit: Option<i32>,
    /// Consecutive failed starts, for diagnostics. Reset on a successful ready.
    pub failed_starts: u32,
    /// The last death was not restartable under this service's policy, so the
    /// reconciler must not start it again.
    ///
    /// This is deliberately **not** modelled by clearing `Desired`. `Desired`
    /// is operator intent and must keep saying "up" so that `zctl status`
    /// reports the truth: a service that the operator wants running, is not
    /// running, and will not be retried. Conflating the two would make the
    /// display lie.
    ///
    /// Cleared by an explicit operator action (`zctl kick`, `zctl start`) or by
    /// [`Bucket::reset`], never by the passage of time.
    pub restart_suppressed: bool,
}

impl ServiceState {
    /// Fresh, stopped, wanted-down.
    pub fn new() -> ServiceState {
        ServiceState {
            desired: Desired::Down,
            state: State::Stopped,
            pid: None,
            pgid: None,
            started_at: None,
            term_sent_at: None,
            ready_due_at: None,
            start_due_at: None,
            stop_due_at: None,
            bucket: Bucket::new(),
            restarts: 0,
            pinned: false,
            is_console: false,
            last_exit: None,
            failed_starts: 0,
            restart_suppressed: false,
        }
    }

    /// A service that wants to be up and has never been started.
    pub fn wanted_up() -> ServiceState {
        ServiceState {
            desired: Desired::Up,
            ..ServiceState::new()
        }
    }

    /// Monotonic uptime in ms, or `None` if not running.
    pub fn uptime_ms(&self, now_ms: u64) -> Option<u64> {
        match (self.state, self.started_at) {
            (State::Running, Some(t)) => Some(now_ms.saturating_sub(t)),
            _ => None,
        }
    }

    /// True when the state/timer bookkeeping is self-consistent.
    ///
    /// Checked after every transition; a violation is always a core bug and
    /// is surfaced rather than papered over.
    pub fn invariants_hold(&self, ready_is_handshook: bool) -> bool {
        match self.state {
            State::Stopped => {
                // Nothing may be set that implies a live process or a pending
                // timer. A stale deadline here is how you get a service that
                // "stops" twice or never times out.
                self.pid.is_none()
                    && self.pgid.is_none()
                    && self.started_at.is_none()
                    && self.term_sent_at.is_none()
                    && self.ready_due_at.is_none()
                    && self.start_due_at.is_none()
                    && self.stop_due_at.is_none()
            }
            State::Starting => {
                self.pid.is_some()
                    && self.started_at.is_some()
                    && self.start_due_at.is_some()
                    && self.stop_due_at.is_none()
                    // Only a handshake defers the ready deadline.
                    && (self.ready_due_at.is_some() == ready_is_handshook)
            }
            State::Running => {
                self.pid.is_some()
                    && self.started_at.is_some()
                    && self.ready_due_at.is_none()
                    && self.start_due_at.is_none()
                    && self.stop_due_at.is_none()
            }
            State::Stopping => {
                self.pid.is_some()
                    && self.stop_due_at.is_some()
                    && self.start_due_at.is_none()
                    && self.ready_due_at.is_none()
            }
        }
    }
}

impl Default for ServiceState {
    fn default() -> Self {
        ServiceState::new()
    }
}

/// The whole supervisor's mutable state, indexed by [`Idx`].
///
/// Sized from [`MAX_SERVICES`] at construction, never reallocated. An init
/// that can be forced to grow a `Vec` by a malformed config is an init that
/// can be made to fail.
#[derive(Clone, Debug)]
pub struct Runtime {
    services: Vec<ServiceState>,
}

impl Runtime {
    /// An empty runtime. Services are added with [`Runtime::add_service`].
    pub fn new() -> Runtime {
        Runtime {
            services: Vec::new(),
        }
    }

    /// A runtime sized for a plan, with every service stopped and wanted-down.
    ///
    /// The usual entry point. The caller then sets `Desired::Up` on whatever
    /// should be running.
    pub fn from_plan(plan: &Plan) -> Runtime {
        let mut rt = Runtime::new();
        for _ in &plan.services {
            rt.services.push(ServiceState::new());
        }
        rt
    }

    /// Append a service. Fails once [`MAX_SERVICES`] is reached.
    pub fn add_service(&mut self) -> core::result::Result<Idx, crate::Error> {
        if self.services.len() >= MAX_SERVICES {
            return Err(crate::Error::PlanMismatch {
                what: "too many services",
                idx: 0,
            });
        }
        self.services.push(ServiceState::new());
        Ok(self.services.len() - 1)
    }

    pub fn len(&self) -> usize {
        self.services.len()
    }

    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    /// Mutable access. Panics if `idx` is out of range, which is a core bug.
    #[inline]
    pub fn get_mut(&mut self, idx: Idx) -> &mut ServiceState {
        &mut self.services[idx]
    }

    /// Shared access. Panics if `idx` is out of range, which is a core bug.
    #[inline]
    pub fn get(&self, idx: Idx) -> &ServiceState {
        &self.services[idx]
    }

    /// The states, in index order.
    ///
    /// Returns an owned `Vec` because the core is `no_std` and has no thread
    /// locals. The reconciler calls this once per tick and consumes the
    /// result immediately, so the cost is one allocation per tick, not one
    /// per service.
    pub fn states(&self) -> Vec<State> {
        self.services.iter().map(|s| s.state).collect()
    }

    /// A single state, by index. The common case in the reconciler's inner
    /// loop, and allocation-free.
    #[inline]
    pub fn state_at(&self, idx: Idx) -> State {
        self.services[idx].state
    }

    /// True when every required dependency of `idx` is `Running`.
    #[inline]
    pub fn deps_satisfied(&self, idx: Idx, plan: &Plan) -> bool {
        plan.services[idx]
            .required
            .iter()
            .all(|&d| self.services[d].state.is_up())
    }

    /// Convenience: set the desired state of a service.
    pub fn set_desired(&mut self, idx: Idx, desired: Desired) {
        self.services[idx].desired = desired;
    }

    /// Convenience: the names, for status output. Allocates; not a hot path.
    pub fn describe(&self, plan: &Plan, now_ms: u64) -> Vec<crate::action::StatusLine> {
        plan.services
            .iter()
            .enumerate()
            .map(|(idx, sp)| {
                let s = &self.services[idx];
                crate::action::StatusLine {
                    idx,
                    name: String::from(&sp.name),
                    state: s.state,
                    desired: s.desired,
                    pid: s.pid,
                    restarts_used: s.restarts,
                    restarts_available: s.bucket.tokens(),
                    uptime_ms: s.uptime_ms(now_ms).unwrap_or(0),
                }
            })
            .collect()
    }

    /// Check every service's internal invariants. Returns the offenders.
    pub fn check_invariants(&self, plan: &Plan) -> alloc::vec::Vec<Idx> {
        plan.services
            .iter()
            .enumerate()
            .filter(|(idx, sp)| !self.services[*idx].invariants_hold(sp.ready.is_handshake()))
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Sum of services currently considered up.
    pub fn count_running(&self) -> usize {
        self.services.iter().filter(|s| s.state.is_up()).count()
    }

    /// A stable, human-readable dump. Used by tests and `zctl status --json`.
    pub fn snapshot(&self, plan: &Plan) -> alloc::string::String {
        let mut out = String::new();
        for (idx, sp) in plan.services.iter().enumerate() {
            let s = &self.services[idx];
            out.push_str(&alloc::format!(
                "{} {} {:?} {:?} pid={:?} restarts={}\n",
                sp.name,
                match s.desired {
                    Desired::Up => "want-up",
                    Desired::Down => "want-down",
                },
                s.state,
                if s.pinned { "pinned" } else { "unpinned" },
                s.pid,
                s.restarts
            ));
        }
        out
    }

    /// Whether the restart policy and budget for `idx` currently permit a spawn.
    pub fn restart_permitted(&mut self, idx: Idx, plan: &Plan, now_ms: u64) -> bool {
        let budget: Budget = plan.services[idx].restart_budget;
        self.services[idx].bucket.allows(&budget, now_ms)
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ServiceKind, ServicePlan};
    use alloc::{format, string::String};

    fn plan_of(n: usize) -> Plan {
        let mut p = Plan::default();
        for i in 0..n {
            p.services.push(ServicePlan::new(format!("s{i}")));
        }
        p.order_up = (0..n).collect();
        p.order_down = (0..n).rev().collect();
        p
    }

    #[test]
    fn fresh_service_is_stopped_and_clean() {
        let s = ServiceState::new();
        assert_eq!(s.state, State::Stopped);
        assert!(s.invariants_hold(false));
        assert!(s.invariants_hold(true));
    }

    #[test]
    fn a_stopped_service_with_a_stale_deadline_is_caught() {
        let mut s = ServiceState::new();
        s.stop_due_at = Some(500);
        assert!(!s.invariants_hold(false), "a stale timer must be caught");
    }

    #[test]
    fn starting_requires_a_pid_and_a_start_deadline() {
        let mut s = ServiceState::new();
        s.state = State::Starting;
        assert!(!s.invariants_hold(false), "no pid yet");
        s.pid = Some(42);
        s.started_at = Some(0);
        s.start_due_at = Some(100);
        assert!(
            s.invariants_hold(false),
            "no handshake, so no ready deadline"
        );
        s.ready_due_at = Some(50);
        assert!(
            !s.invariants_hold(false),
            "ready deadline needs a handshake"
        );
        assert!(s.invariants_hold(true));
    }

    #[test]
    fn from_plan_sizes_correctly() {
        let p = plan_of(5);
        let rt = Runtime::from_plan(&p);
        assert_eq!(rt.len(), 5);
        assert_eq!(rt.count_running(), 0);
        assert!(rt.check_invariants(&p).is_empty());
    }

    #[test]
    fn add_service_respects_the_cap() {
        let mut rt = Runtime::new();
        for _ in 0..MAX_SERVICES {
            assert!(rt.add_service().is_ok());
        }
        assert!(rt.add_service().is_err(), "the cap must be enforced");
    }

    #[test]
    fn states_reflects_mutation() {
        let p = plan_of(3);
        let mut rt = Runtime::from_plan(&p);
        rt.get_mut(1).state = State::Running;
        let st = rt.states();
        assert_eq!(st, &[State::Stopped, State::Running, State::Stopped]);
    }

    #[test]
    fn targets_and_console_defaults_are_sane() {
        let mut sp = ServicePlan::new(String::from("t"));
        sp.kind = ServiceKind::Target;
        assert!(!sp.kind.has_process());
        let mut sp2 = ServicePlan::new(String::from("c"));
        sp2.kind = ServiceKind::Console;
        assert!(sp2.kind.has_process());
    }
}
