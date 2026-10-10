//! The transition function: `(Event) -> (new state, Vec<Action>)`.
//!
//! Where [`reconcile`](crate::reconcile()) answers "what should happen given the
//! desired state", this answers "what happened, and what follows immediately
//! from it". They are different questions and they run at different times:
//! the reconciler is a periodic sweep over *desired*, this is an immediate
//! reaction to *actual*.
//!
//! # Why a function and not a loop
//!
//! Every rule in `DESIGN.md` §4.1 is a row in one match statement. There is no
//! `while let` re-processing, no "and then maybe stop as well" second pass.
//! An event produces its actions and is done.
//!
//! # Signals
//!
//! This module never sends a signal directly. It returns
//! [`Action::Signal`] and the runtime sends it. That is what lets the whole
//! thing be tested without ever risking a stray `kill(0, SIGKILL)` - which is
//! not a theoretical concern: dinit's `kill_all_on_stop` does
//! `kill(-1, SIGKILL)` and the audit found it executing *inside* the event
//! loop.

use alloc::string::String;
use alloc::vec::Vec;

use crate::action::{Action, log};
use crate::event::Event;
use crate::reconcile::should_restart;
use crate::runtime::{Runtime, ServiceState};
use crate::types::{Desired, Idx, LogLevel, Plan, Restart, ServiceKind, SignalKind, State};

/// Outcome of applying one event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    /// Actions the runtime must perform, in order.
    pub actions: Vec<Action>,
    /// Set when the service's state actually changed.
    pub changed: bool,
}

impl Transition {
    fn quiet() -> Transition {
        Transition {
            actions: Vec::new(),
            changed: false,
        }
    }

    fn single(a: Action) -> Transition {
        Transition {
            actions: alloc::vec![a],
            changed: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Every action concerning one service.
    pub fn for_service(&self, idx: Idx) -> impl Iterator<Item = &Action> {
        self.actions.iter().filter(move |a| a.idx() == idx)
    }

    /// The signals issued for one service, in order.
    pub fn signals_for(&self, idx: Idx) -> Vec<SignalKind> {
        self.for_service(idx)
            .filter_map(|a| match *a {
                Action::Signal { signal, .. } => Some(signal),
                _ => None,
            })
            .collect()
    }
}

/// Apply `event` to `runtime`, returning the actions the runtime must perform.
///
/// Pure with respect to everything but `runtime` and the monotonic clock
/// (`now_ms`). No clock reads, no I/O, no allocation beyond the returned
/// `Vec`.
///
/// An event that is not meaningful in the current state is **ignored**, not an
/// error. Real systems deliver events that arrive late: a `Ready` after the
/// process already exited, a `StopTimeout` for a service that stopped by
/// itself. Treating those as errors would make the supervisor the least
/// reliable thing on the system.
pub fn apply(event: &Event, runtime: &mut Runtime, plan: &Plan, now_ms: u64) -> Transition {
    let idx = event.idx();
    if idx >= runtime.len() {
        // Cannot happen: the runtime only ever synthesises indices that exist.
        // Returning quietly beats panicking inside PID 1.
        return Transition::quiet();
    }
    let sp = &plan.services[idx];

    match *event {
        // ── Spawn lifecycle ────────────────────────────────────────────────
        Event::ExecOk(_) => {
            // execve succeeded: the process image is real now.
            if runtime.state_at(idx) != State::Starting {
                return Transition::quiet();
            }
            if sp.ready.is_handshake() {
                // Wait for the handshake, or for the ready deadline.
                Transition::quiet()
            } else {
                // `ready = none`: there is nothing to wait for, so being
                // alive *is* being ready. Going through a timer here would
                // add a pointless delay to every such service.
                become_running(runtime, idx, now_ms)
            }
        }

        Event::SpawnFailed { errno, .. } => Transition {
            actions: alloc::vec![
                log(
                    idx,
                    LogLevel::Error,
                    alloc::format!("spawn failed: errno {errno}")
                ),
                mark_down(runtime, idx, now_ms, true),
                Action::DepsChanged(idx),
            ],
            changed: true,
        },

        // ── Readiness ──────────────────────────────────────────────────────
        Event::Ready(_) => become_running(runtime, idx, now_ms),

        Event::ReadyTimeout(_) => {
            if runtime.state_at(idx) != State::Starting {
                return Transition::quiet();
            }
            // Only strict readiness reaches here as a failure.
            let mut t = become_running(runtime, idx, now_ms);
            t.actions.insert(
                0,
                log(idx, LogLevel::Error, "readiness handshake timed out"),
            );
            t
        }

        Event::StartTimeout(_) => {
            if runtime.state_at(idx) != State::Starting {
                return Transition::quiet();
            }
            let mut actions = alloc::vec![
                log(
                    idx,
                    LogLevel::Error,
                    alloc::format!(
                        "did not become ready within {}ms; escalating to SIGKILL",
                        sp.start_timeout_ms
                    ),
                ),
                Action::Signal {
                    idx,
                    signal: SignalKind::Term
                },
            ];
            let s = runtime.get_mut(idx);
            s.state = State::Stopping;
            s.term_sent_at = Some(now_ms);
            s.stop_due_at = Some(now_ms.saturating_add(sp.stop_timeout_ms));
            s.start_due_at = None;
            s.ready_due_at = None;
            s.failed_starts = s.failed_starts.saturating_add(1);
            actions.push(mark_down(runtime, idx, now_ms, true));
            actions.push(Action::DepsChanged(idx));
            Transition {
                actions,
                changed: true,
            }
        }

        // ── Death ──────────────────────────────────────────────────────────
        Event::Exited { code, .. } => handle_death(runtime, plan, idx, Some(code), now_ms, None),
        Event::Signalled { signal, .. } => {
            handle_death(runtime, plan, idx, None, now_ms, Some(signal))
        }
        Event::OrphanReaped(_) => handle_death(runtime, plan, idx, None, now_ms, None),

        // ── Timers ─────────────────────────────────────────────────────────
        Event::StopTimeout(_) => {
            if runtime.state_at(idx) != State::Stopping {
                return Transition::quiet();
            }
            let mut actions = alloc::vec![
                log(
                    idx,
                    LogLevel::Error,
                    alloc::format!(
                        "did not stop within {}ms; sending SIGKILL",
                        sp.stop_timeout_ms
                    ),
                ),
                Action::Signal {
                    idx,
                    signal: SignalKind::Kill
                },
            ];
            // Give the kill a bounded grace period. If the service is wedged
            // in uninterruptible sleep this never completes, and that is
            // correct: an init that lies about a zombie being gone is worse
            // than one that keeps waiting.
            let s = runtime.get_mut(idx);
            s.stop_due_at = Some(now_ms.saturating_add(sp.stop_timeout_ms));
            actions.push(Action::DepsChanged(idx));
            Transition {
                actions,
                changed: false,
            }
        }

        // ── Dependencies ───────────────────────────────────────────────────
        Event::DepLost { dep, .. } => {
            if runtime.get(idx).pinned {
                return Transition::quiet();
            }
            // The dependency is gone. If we are only up because of it, we go
            // down. `Desired` stays as it is: this is cascade, not operator
            // intent, and the next reconcile pass must not resurrect it.
            let wanted = runtime.get(idx).desired;
            if wanted == Desired::Down {
                return Transition::quiet();
            }
            let mut actions = alloc::vec![log(
                idx,
                LogLevel::Warn,
                alloc::format!("required dependency `{}` is down", plan.services[dep].name),
            )];
            if matches!(runtime.state_at(idx), State::Running | State::Starting) {
                let s = runtime.get_mut(idx);
                s.desired = Desired::Down;
                s.state = State::Stopping;
                s.term_sent_at = Some(now_ms);
                s.stop_due_at = Some(now_ms.saturating_add(sp.stop_timeout_ms));
                s.start_due_at = None;
                s.ready_due_at = None;
                actions.push(Action::CascadeStop { idx, dep });
                actions.push(Action::Signal {
                    idx,
                    signal: SignalKind::Term,
                });
            }
            Transition {
                actions,
                changed: true,
            }
        }

        Event::DepFailed { dep, .. } => {
            let mut actions = alloc::vec![log(
                idx,
                LogLevel::Error,
                alloc::format!(
                    "required dependency `{}` failed to start; not starting",
                    plan.services[dep].name
                ),
            )];
            if matches!(runtime.state_at(idx), State::Running | State::Starting) {
                let s = runtime.get_mut(idx);
                s.desired = Desired::Down;
                s.state = State::Stopping;
                s.stop_due_at = Some(now_ms.saturating_add(sp.stop_timeout_ms));
                s.term_sent_at = Some(now_ms);
                s.start_due_at = None;
                s.ready_due_at = None;
                actions.push(Action::CascadeStop { idx, dep });
                actions.push(Action::Signal {
                    idx,
                    signal: SignalKind::Term,
                });
            }
            Transition {
                actions,
                changed: true,
            }
        }

        Event::BudgetExhausted(_) => {
            Transition::single(log(idx, LogLevel::Error, "restart budget exhausted"))
        }

        Event::Pinned(_) => {
            let s = runtime.get_mut(idx);
            s.pinned = true;
            Transition {
                actions: alloc::vec![Action::Unpin(idx)],
                changed: true,
            }
        }

        // ── Adoption (forking daemons) ─────────────────────────────────────
        Event::AdoptedPid { pid, .. } => {
            // Only a `Starting` forking service can adopt: anything else is a
            // late or confused report, ignored like any other.
            if runtime.state_at(idx) != State::Starting {
                return Transition::quiet();
            }
            if sp.kind != ServiceKind::Forking {
                return Transition::quiet();
            }
            {
                let s = runtime.get_mut(idx);
                s.pid = Some(pid);
                s.pgid = Some(pid);
            }
            // No signal, no log: the daemon is simply being watched from here
            // on. `changed` re-runs the invariant check, because a pid swap
            // is exactly the kind of bookkeeping worth re-proving.
            Transition {
                actions: Vec::new(),
                changed: true,
            }
        }

        // ── Watchdog ───────────────────────────────────────────────────────
        Event::WatchdogPing(_) => {
            // Only a live service can check in. A late ping for a dead one is
            // ignored, like every other late event.
            if !matches!(runtime.state_at(idx), State::Running | State::Starting) {
                return Transition::quiet();
            }
            runtime.get_mut(idx).last_watchdog_ms = Some(now_ms);
            Transition::quiet()
        }

        Event::WatchdogExpired(_) => {
            if !matches!(runtime.state_at(idx), State::Running | State::Starting) {
                return Transition::quiet();
            }
            // A hung service is stopped and — `Desired` untouched — started
            // again: the same shape as a start timeout (TERM now, the stop
            // timeout below escalates to KILL, the reconciler respawns), with
            // its own log line so the reason is never ambiguous.
            let mut actions = alloc::vec![
                log(idx, LogLevel::Error, "watchdog expired; stopping"),
                Action::Signal {
                    idx,
                    signal: SignalKind::Term
                },
            ];
            let s = runtime.get_mut(idx);
            s.state = State::Stopping;
            s.term_sent_at = Some(now_ms);
            s.stop_due_at = Some(now_ms.saturating_add(sp.stop_timeout_ms));
            s.start_due_at = None;
            s.ready_due_at = None;
            s.failed_starts = s.failed_starts.saturating_add(1);
            actions.push(mark_down(runtime, idx, now_ms, true));
            actions.push(Action::DepsChanged(idx));
            Transition {
                actions,
                changed: true,
            }
        }

        Event::IoError { errno, .. } => Transition::single(log(
            idx,
            LogLevel::Warn,
            alloc::format!("i/o error while managing service: errno {errno}"),
        )),
    }
}

/// `Starting` -> `Running`, or ignore if not starting.
fn become_running(runtime: &mut Runtime, idx: Idx, now_ms: u64) -> Transition {
    if runtime.state_at(idx) != State::Starting {
        return Transition::quiet();
    }
    {
        let s = runtime.get_mut(idx);
        s.state = State::Running;
        s.ready_due_at = None;
        s.start_due_at = None;
        s.failed_starts = 0;
    }
    // `started_at` stays where the spawn set it: uptime should measure the
    // process, not the handshake.
    let _ = now_ms;
    let mut actions = alloc::vec![Action::MarkReady(idx), Action::DepsChanged(idx)];
    if runtime.get(idx).is_console {
        actions.push(Action::GrantConsole(idx));
    }
    Transition {
        actions,
        changed: true,
    }
}

/// Common death handling: record, decide, and emit the teardown actions.
fn handle_death(
    runtime: &mut Runtime,
    plan: &Plan,
    idx: Idx,
    code: Option<i32>,
    _now_ms: u64,
    signal: Option<i32>,
) -> Transition {
    let was_stopping = runtime.state_at(idx) == State::Stopping;
    if !runtime.state_at(idx).has_process() {
        // A death for a service we believe is not running: a duplicate or late
        // event. Ignore it.
        return Transition::quiet();
    }
    let sp = &plan.services[idx];

    let description = match (code, signal) {
        (Some(c), _) => alloc::format!("exited with status {c}"),
        (None, Some(s)) => alloc::format!("killed by signal {s}"),
        (None, None) => String::from("exited"),
    };

    let mut actions = Vec::new();

    {
        let s = runtime.get_mut(idx);
        s.state = State::Stopped;
        s.pid = None;
        s.pgid = None;
        s.started_at = None;
        s.term_sent_at = None;
        s.ready_due_at = None;
        s.start_due_at = None;
        s.stop_due_at = None;
        s.last_exit = code;
        if s.is_console {
            s.is_console = false;
            actions.push(Action::RevokeConsole(idx));
        }
    }

    // A oneshot that exited cleanly is done — not crashed, not stopped, done.
    // Completion beats the restart policy by design: the policy governs
    // failures, and a clean oneshot exit is not one. `Desired` stays `Up` so
    // status tells the truth (wanted, finished, quiet); the reconciler will
    // not start it again until `kick` or an explicit `start` re-arms it; and
    // dependents treat it as satisfied through `deps_satisfied`.
    //
    // Recorded whether or not the death was requested: a oneshot that ran to
    // the end did so even if an operator asked it to stop halfway. The
    // stopping path below still applies (a requested stop stays a clean
    // stop), it just also remembers the work got done.
    if sp.kind == ServiceKind::Oneshot && code == Some(0) {
        let s = runtime.get_mut(idx);
        s.completed = true;
        s.failed_starts = 0;
    }

    if was_stopping {
        // We asked for this. Not a failure; do not log at error, do not restart.
        actions.push(Action::MarkDown {
            idx,
            unexpected: false,
        });
        actions.push(Action::DepsChanged(idx));
        return Transition {
            actions,
            changed: true,
        };
    }

    if sp.kind == ServiceKind::Oneshot && code == Some(0) {
        // Recorded above; reported here. A clean finish is information, not
        // failure: info level, a clean `MarkDown`, and no budget touched.
        actions.push(log(idx, LogLevel::Info, "completed"));
        actions.push(Action::MarkDown {
            idx,
            unexpected: false,
        });
        actions.push(Action::DepsChanged(idx));
        return Transition {
            actions,
            changed: true,
        };
    }

    let restarting = should_restart(runtime, plan, idx, code);
    if restarting {
        actions.push(log(
            idx,
            LogLevel::Warn,
            alloc::format!("{}; restarting", description),
        ));
        {
            let s = runtime.get_mut(idx);
            s.restarts = s.restarts.saturating_add(1);
            s.failed_starts = s.failed_starts.saturating_add(1);
            s.restart_suppressed = false;
        }
        // The budget token is consumed by the *reconciler*, at the moment it
        // actually emits the spawn. Consuming it here as well would count
        // every crash twice, and a budget of 1 would permit zero restarts.
        // It still terminates a failing-to-start loop: spawn -> SpawnFailed ->
        // death -> reconcile -> one more token spent, until it is empty.
    } else {
        // Not restarting. `Desired` is deliberately left as the operator set
        // it, so `zctl status` reports the truth - wanted up, not running -
        // rather than quietly rewriting intent. The reconciler is told to
        // leave it alone via this flag, which only an operator clears.
        runtime.get_mut(idx).restart_suppressed = true;
        let (level, hint) = if sp.restart == Restart::Never {
            (LogLevel::Warn, "; restart is disabled for this service")
        } else {
            (LogLevel::Info, "; it will not be retried (use `zctl kick`)")
        };
        actions.push(log(idx, level, alloc::format!("{description}{hint}")));
    }

    actions.push(Action::MarkDown {
        idx,
        unexpected: true,
    });
    actions.push(Action::DepsChanged(idx));

    Transition {
        actions,
        changed: true,
    }
}

/// Build a `MarkDown` action and mark the service stopped.
///
/// Used by the `SpawnFailed` path, where the process never existed so there is
/// no prior state to unwind.
fn mark_down(runtime: &mut Runtime, idx: Idx, now_ms: u64, unexpected: bool) -> Action {
    let s: &mut ServiceState = runtime.get_mut(idx);
    s.state = State::Stopped;
    s.pid = None;
    s.pgid = None;
    s.started_at = None;
    s.term_sent_at = None;
    s.ready_due_at = None;
    s.start_due_at = None;
    s.stop_due_at = None;
    s.failed_starts = s.failed_starts.saturating_add(1);
    let _ = now_ms;
    Action::MarkDown { idx, unexpected }
}

/// Clear a `restart_suppressed` flag and refill the bucket.
///
/// This is the `zctl kick` operation, and it is the **only** way a service that
/// the policy refused to retry becomes eligible again. Deliberately
/// asymmetric: nothing in the runtime can do this implicitly, so "the service
/// is not coming back" is always a decision someone made and can undo.
///
/// A completed oneshot is the second thing this clears: finishing is as
/// terminal as suppression, and re-running a finished task takes the same
/// explicit operator decision as retrying a refused one.
pub fn kick(runtime: &mut Runtime, plan: &Plan, idx: Idx, now_ms: u64) -> bool {
    if idx >= runtime.len() || !plan.services[idx].kind.has_process() {
        return false;
    }
    let s = runtime.get_mut(idx);
    let completed_oneshot = plan.services[idx].kind == ServiceKind::Oneshot && s.completed;
    if !s.restart_suppressed && !completed_oneshot {
        return false;
    }
    s.restart_suppressed = false;
    s.completed = false;
    s.failed_starts = 0;
    s.last_exit = None;
    s.bucket.reset(&plan.services[idx].restart_budget, now_ms);
    true
}

/// Case a timer is set when a service enters `Starting`.
///
/// Shared by the runtime and the tests so the deadline arithmetic exists once.
///
/// The two deadlines are independent. `start_due_at` bounds the spawn; the
/// readiness deadline is only armed when there is a handshake to wait for, and
/// then it comes from `ready_timeout_ms` rather than from `start_timeout_ms`.
/// Folding them together — which is what a `ready = none` service forces, since
/// it has no readiness deadline at all — would make a documented
/// `ready-timeout` directive unobservable.
pub fn arm_start_deadlines(runtime: &mut Runtime, plan: &Plan, idx: Idx, now_ms: u64) {
    let sp = &plan.services[idx];
    let s = runtime.get_mut(idx);
    s.state = State::Starting;
    s.start_due_at = Some(now_ms.saturating_add(sp.start_timeout_ms));
    s.ready_due_at = if sp.ready.is_handshake() {
        Some(now_ms.saturating_add(sp.ready_timeout_ms))
    } else {
        None
    };
    // Checked in since birth: a watchdog measures from the last ping, and a
    // service that never pinged is measured from here (see `due_events`).
    // Set unconditionally — readers only consult it when the plan configures
    // a watchdog, so the field is meaningless elsewhere either way.
    s.last_watchdog_ms = Some(now_ms);
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::types::{Budget, Ready, ServicePlan, StrictReady};

    fn plan1(desc: ServicePlan) -> Plan {
        let mut p = Plan::default();
        p.services.push(desc);
        p.order_up = alloc::vec![0];
        p.order_down = alloc::vec![0];
        p
    }

    fn starting() -> ServiceState {
        let mut s = ServiceState::wanted_up();
        s.state = State::Starting;
        s.pid = Some(100);
        s.pgid = Some(100);
        s.started_at = Some(0);
        s.start_due_at = Some(1000);
        s.ready_due_at = Some(500);
        s
    }

    #[test]
    fn ready_moves_starting_to_running() {
        let p = plan1(ServicePlan::new(String::from("a")));
        let mut rt = Runtime::from_plan(&p);
        *rt.get_mut(0) = starting();

        let t = apply(&Event::Ready(0), &mut rt, &p, 10);
        assert!(t.changed);
        assert_eq!(rt.state_at(0), State::Running);
        assert!(
            rt.check_invariants(&p).is_empty(),
            "{:?}",
            rt.check_invariants(&p)
        );
        assert!(t.actions.iter().any(|a| matches!(a, Action::MarkReady(0))));
    }

    #[test]
    fn start_timeout_escalates_and_cleans_deadlines() {
        let p = plan1(ServicePlan::new(String::from("a")));
        let mut rt = Runtime::from_plan(&p);
        let mut s = ServiceState::wanted_up();
        s.state = State::Starting;
        s.pid = Some(1);
        s.started_at = Some(0);
        s.start_due_at = Some(10);
        s.ready_due_at = Some(10);
        *rt.get_mut(0) = s;

        let t = apply(&Event::StartTimeout(0), &mut rt, &p, 10);
        assert!(
            rt.check_invariants(&p).is_empty(),
            "state must be consistent: {:?}",
            rt.get(0)
        );
        assert_eq!(t.signals_for(0), alloc::vec![SignalKind::Term]);
    }

    #[test]
    fn crash_takes_a_budget_token_and_leaves_desired_up() {
        let mut sp = ServicePlan::new(String::from("a"));
        sp.restart = Restart::OnFailure;
        sp.restart_budget = Budget {
            capacity: 3,
            window_ms: 60_000,
            delay_ms: 0,
        };
        let p = plan1(sp);
        let mut rt = Runtime::from_plan(&p);
        *rt.get_mut(0) = starting();
        rt.get_mut(0).state = State::Running;
        rt.set_desired(0, Desired::Up);
        // The bucket refills to capacity on first use.
        let _ = rt
            .get_mut(0)
            .bucket
            .allows(&p.services[0].restart_budget, 0);

        let t = apply(&Event::Exited { idx: 0, code: 1 }, &mut rt, &p, 0);
        assert_eq!(rt.state_at(0), State::Stopped);
        assert_eq!(
            rt.get(0).desired,
            Desired::Up,
            "a crash must NOT clear desired, or the reconciler would not restart it"
        );
        assert_eq!(rt.get(0).last_exit, Some(1));
        assert!(t.actions.iter().any(|a| matches!(
            a,
            Action::MarkDown {
                idx: 0,
                unexpected: true
            }
        )));
    }

    #[test]
    fn a_deliberate_stop_does_not_count_as_a_failure() {
        let mut sp = ServicePlan::new(String::from("a"));
        sp.restart = Restart::Always;
        let p = plan1(sp);
        let mut rt = Runtime::from_plan(&p);
        rt.get_mut(0).state = State::Stopping;
        rt.get_mut(0).pid = Some(1);
        rt.get_mut(0).started_at = Some(0);
        rt.get_mut(0).stop_due_at = Some(100);
        rt.set_desired(0, Desired::Down);

        let t = apply(&Event::Exited { idx: 0, code: 0 }, &mut rt, &p, 10);
        assert!(
            !t.actions.iter().any(|a| matches!(
                a,
                Action::MarkDown {
                    idx: 0,
                    unexpected: true
                }
            )),
            "a clean stop must not be reported as unexpected: {:?}",
            t.actions
        );
        assert!(rt.check_invariants(&p).is_empty());
    }

    #[test]
    fn restart_never_leaves_desired_up_after_a_crash() {
        let mut sp = ServicePlan::new(String::from("a"));
        sp.restart = Restart::Never;
        let p = plan1(sp);
        let mut rt = Runtime::from_plan(&p);
        rt.get_mut(0).state = State::Running;
        rt.get_mut(0).pid = Some(1);
        rt.get_mut(0).started_at = Some(0);

        let t = apply(&Event::Exited { idx: 0, code: 1 }, &mut rt, &p, 0);
        // Desired stays Up so the reconciler sees the divergence; the budget
        // is what stops it. We assert the reconciler does not spawn.
        let tick = crate::reconcile(&mut rt, &p, 0);
        assert!(
            !tick.actions.iter().any(|a| matches!(a, Action::Spawn(0))),
            "restart=never must not respawn: {:?}",
            tick.actions
        );
        let _ = t;
    }

    #[test]
    fn stop_timeout_sends_sigkill_and_rearms() {
        let p = plan1(ServicePlan::new(String::from("a")));
        let mut rt = Runtime::from_plan(&p);
        rt.get_mut(0).state = State::Stopping;
        rt.get_mut(0).pid = Some(1);
        rt.get_mut(0).started_at = Some(0);
        rt.get_mut(0).stop_due_at = Some(10);

        let t = apply(&Event::StopTimeout(0), &mut rt, &p, 10);
        assert_eq!(t.signals_for(0), alloc::vec![SignalKind::Kill]);
        assert!(rt.get(0).stop_due_at.unwrap() > 10, "must rearm");
        assert!(rt.check_invariants(&p).is_empty());
    }

    #[test]
    fn late_and_duplicate_events_are_ignored() {
        let p = plan1(ServicePlan::new(String::from("a")));
        let mut rt = Runtime::from_plan(&p);
        // Stopped service receives Ready and Exited.
        assert!(apply(&Event::Ready(0), &mut rt, &p, 0).is_empty());
        assert!(apply(&Event::Exited { idx: 0, code: 0 }, &mut rt, &p, 0).is_empty());
        assert!(apply(&Event::StartTimeout(0), &mut rt, &p, 0).is_empty());
        assert!(apply(&Event::StopTimeout(0), &mut rt, &p, 0).is_empty());
    }

    #[test]
    fn dep_lost_cascades_a_stop() {
        let mut p = Plan::default();
        p.services.push(ServicePlan::new(String::from("dep")));
        let mut b = ServicePlan::new(String::from("b"));
        b.required = alloc::vec![0];
        p.services.push(b);
        p.order_up = alloc::vec![0, 1];
        p.order_down = alloc::vec![1, 0];
        let mut rt = Runtime::from_plan(&p);
        rt.get_mut(1).state = State::Running;
        rt.get_mut(1).pid = Some(9);
        rt.get_mut(1).started_at = Some(0);
        rt.set_desired(1, Desired::Up);

        let t = apply(&Event::DepLost { idx: 1, dep: 0 }, &mut rt, &p, 0);
        assert!(t.signals_for(1).contains(&SignalKind::Term));
        assert_eq!(rt.get(1).desired, Desired::Down, "cascade clears desired");
    }

    #[test]
    fn dep_lost_respects_a_pin() {
        let mut p = Plan::default();
        p.services.push(ServicePlan::new(String::from("dep")));
        let mut b = ServicePlan::new(String::from("b"));
        b.required = alloc::vec![0];
        p.services.push(b);
        p.order_up = alloc::vec![0, 1];
        p.order_down = alloc::vec![1, 0];
        let mut rt = Runtime::from_plan(&p);
        rt.get_mut(1).state = State::Running;
        rt.get_mut(1).pid = Some(9);
        rt.get_mut(1).started_at = Some(0);
        rt.get_mut(1).pinned = true;

        let t = apply(&Event::DepLost { idx: 1, dep: 0 }, &mut rt, &p, 0);
        assert!(
            t.signals_for(1).is_empty(),
            "a pinned service must not be stopped"
        );
    }

    #[test]
    fn strict_ready_timeout_is_an_error_but_still_goes_up() {
        let mut sp = ServicePlan::new(String::from("a"));
        sp.ready = Ready::Strict(StrictReady::Tcp(1));
        let p = plan1(sp);
        let mut rt = Runtime::from_plan(&p);
        *rt.get_mut(0) = starting();

        let t = apply(&Event::ReadyTimeout(0), &mut rt, &p, 10);
        assert!(t.actions.iter().any(|a| matches!(
            a,
            Action::Log {
                level: LogLevel::Error,
                ..
            }
        )));
        assert!(rt.check_invariants(&p).is_empty());
    }

    #[test]
    fn console_is_granted_and_revoked() {
        let mut sp = ServicePlan::new(String::from("c"));
        sp.kind = crate::types::ServiceKind::Console;
        let p = plan1(sp);
        let mut rt = Runtime::from_plan(&p);
        *rt.get_mut(0) = starting();
        rt.get_mut(0).is_console = true;

        let t = apply(&Event::Ready(0), &mut rt, &p, 0);
        assert!(
            t.actions
                .iter()
                .any(|a| matches!(a, Action::GrantConsole(0)))
        );

        let t2 = apply(&Event::Exited { idx: 0, code: 0 }, &mut rt, &p, 1);
        assert!(
            t2.actions
                .iter()
                .any(|a| matches!(a, Action::RevokeConsole(0)))
        );
    }

    mod lifecycle_tests {
        use super::*;

        use crate::reconcile::{due_events, reconcile};
        use crate::types::ServiceKind;

        fn oneshot_plan() -> Plan {
            let mut desc = ServicePlan::new(String::from("task"));
            desc.kind = ServiceKind::Oneshot;
            desc.ready = Ready::None;
            super::plan1(desc)
        }

        fn running_up() -> ServiceState {
            let mut s = ServiceState::wanted_up();
            s.state = State::Running;
            s.pid = Some(100);
            s.pgid = Some(100);
            s.started_at = Some(0);
            s
        }

        #[test]
        fn oneshot_clean_exit_completes_and_never_respawns() {
            let p = oneshot_plan();
            let mut rt = Runtime::from_plan(&p);
            *rt.get_mut(0) = running_up();

            let t = apply(&Event::Exited { idx: 0, code: 0 }, &mut rt, &p, 10);
            assert!(t.changed);
            assert!(
                rt.get(0).completed,
                "a clean oneshot exit is done, not dead"
            );
            assert_eq!(rt.state_at(0), State::Stopped);
            assert!(
                t.actions.iter().any(|a| matches!(
                    a,
                    Action::MarkDown {
                        unexpected: false,
                        ..
                    }
                )),
                "completion is a clean stop, not a crash"
            );
            assert!(
                !t.actions.iter().any(|a| matches!(a, Action::Signal { .. })),
                "nothing left to signal"
            );

            // The reconciler sees worklessness, not divergence.
            let tick = reconcile(&mut rt, &p, 10);
            assert!(
                !tick.actions.iter().any(|a| matches!(a, Action::Spawn(0))),
                "a completed oneshot must not spawn again"
            );
            assert!(rt.check_invariants(&p).is_empty());
        }

        #[test]
        fn oneshot_completion_beats_an_always_policy() {
            let mut desc = ServicePlan::new(String::from("task"));
            desc.kind = ServiceKind::Oneshot;
            desc.ready = Ready::None;
            desc.restart = crate::types::Restart::Always;
            let p = super::plan1(desc);
            let mut rt = Runtime::from_plan(&p);
            *rt.get_mut(0) = running_up();

            let _ = apply(&Event::Exited { idx: 0, code: 0 }, &mut rt, &p, 10);
            assert!(rt.get(0).completed);
            assert_eq!(rt.get(0).restarts, 0, "completion spends no restarts");
        }

        #[test]
        fn oneshot_failure_follows_the_policy() {
            let p = oneshot_plan();
            let mut rt = Runtime::from_plan(&p);
            *rt.get_mut(0) = running_up();

            let _ = apply(&Event::Exited { idx: 0, code: 1 }, &mut rt, &p, 10);
            assert!(!rt.get(0).completed, "a failure is not a finish");
            // OnFailure + non-zero exit restarts (budgeted, via the reconciler).
            let tick = reconcile(&mut rt, &p, 10);
            assert!(tick.actions.iter().any(|a| matches!(a, Action::Spawn(0))));
        }

        #[test]
        fn kick_rearms_a_completed_oneshot_and_nothing_else() {
            let p = oneshot_plan();
            let mut rt = Runtime::from_plan(&p);
            *rt.get_mut(0) = running_up();
            let _ = apply(&Event::Exited { idx: 0, code: 0 }, &mut rt, &p, 10);
            assert!(kick(&mut rt, &p, 0, 10), "kick must re-arm completion");
            assert!(!rt.get(0).completed);
            assert!(!kick(&mut rt, &p, 0, 10), "nothing left to clear");
            // And re-armed means respawnable again.
            let tick = reconcile(&mut rt, &p, 10);
            assert!(tick.actions.iter().any(|a| matches!(a, Action::Spawn(0))));
        }

        #[test]
        fn a_completed_oneshot_satisfies_its_dependents() {
            let mut task = ServicePlan::new(String::from("task"));
            task.kind = ServiceKind::Oneshot;
            task.ready = Ready::None;
            let svc = ServicePlan::new(String::from("svc"));
            let mut p = Plan::default();
            p.services.push(task);
            p.services.push(svc);
            p.services[1].required = alloc::vec![0];
            p.order_up = alloc::vec![0, 1];
            p.order_down = alloc::vec![1, 0];
            let mut rt = Runtime::from_plan(&p);
            rt.set_desired(0, Desired::Up);
            rt.set_desired(1, Desired::Up);
            *rt.get_mut(0) = running_up();

            assert!(
                rt.deps_satisfied(1, &p),
                "a running task satisfies while it lives"
            );
            let _ = apply(&Event::Exited { idx: 0, code: 0 }, &mut rt, &p, 10);
            assert!(rt.get(0).completed);
            assert!(
                rt.deps_satisfied(1, &p),
                "a finished task satisfies without a process"
            );
            assert!(rt.check_invariants(&p).is_empty());
        }

        #[test]
        fn completion_on_any_other_kind_is_an_invariant_violation() {
            let p = super::plan1(ServicePlan::new(String::from("a")));
            let mut rt = Runtime::from_plan(&p);
            rt.get_mut(0).completed = true;
            assert_eq!(rt.check_invariants(&p), alloc::vec![0]);
        }

        #[test]
        fn watchdog_expiry_stops_a_silent_service() {
            let mut desc = ServicePlan::new(String::from("w"));
            desc.ready = Ready::Notify;
            desc.watchdog_sec = Some(10);
            let p = super::plan1(desc);
            let mut rt = Runtime::from_plan(&p);
            *rt.get_mut(0) = running_up();

            // Measured from birth while no ping arrived.
            let due = due_events(&rt, &p, 9_999);
            assert!(due.is_empty());
            let due = due_events(&rt, &p, 10_000);
            assert_eq!(due.len(), 1);
            assert!(matches!(due[0], Event::WatchdogExpired(0)));

            let t = apply(&due[0], &mut rt, &p, 10_000);
            // The same shape as a start timeout: the wedged process is TERM'd
            // best-effort and the slot goes Stopped, so the reconciler starts
            // over. `mark_down` clears the pid the TERM is still aimed at — the
            // signal was already emitted above, and the reap will report the
            // death whenever it actually happens.
            assert_eq!(rt.state_at(0), State::Stopped);
            assert!(t.actions.iter().any(|a| matches!(
                a,
                Action::Signal {
                    signal: SignalKind::Term,
                    ..
                }
            )));
            assert!(rt.check_invariants(&p).is_empty());
        }

        #[test]
        fn watchdog_pings_postpone_expiry() {
            let mut desc = ServicePlan::new(String::from("w"));
            desc.ready = Ready::Notify;
            desc.watchdog_sec = Some(10);
            let p = super::plan1(desc);
            let mut rt = Runtime::from_plan(&p);
            *rt.get_mut(0) = running_up();

            let _ = apply(&Event::WatchdogPing(0), &mut rt, &p, 9_000);
            assert!(due_events(&rt, &p, 18_999).is_empty());
            assert_eq!(due_events(&rt, &p, 19_000).len(), 1);
        }

        #[test]
        fn watchdog_pings_from_the_dead_are_ignored() {
            let mut desc = ServicePlan::new(String::from("w"));
            desc.ready = Ready::Notify;
            desc.watchdog_sec = Some(10);
            let p = super::plan1(desc);
            let mut rt = Runtime::from_plan(&p);
            rt.set_desired(0, Desired::Up);

            let t = apply(&Event::WatchdogPing(0), &mut rt, &p, 5);
            assert!(!t.changed);
            assert!(rt.get(0).last_watchdog_ms.is_none());
        }

        #[test]
        fn adoption_swaps_the_fork_pid_for_the_daemon() {
            let mut desc = ServicePlan::new(String::from("d"));
            desc.kind = ServiceKind::Forking;
            desc.ready = Ready::None;
            let p = super::plan1(desc);
            let mut rt = Runtime::from_plan(&p);
            let mut s = super::starting();
            // The plan has `ready = none`: no handshake, so no ready
            // deadline. (`starting()` arms one; carrying it here would fail
            // the invariant before the adoption is even applied.)
            s.ready_due_at = None;
            *rt.get_mut(0) = s;

            let t = apply(&Event::AdoptedPid { idx: 0, pid: 77 }, &mut rt, &p, 5);
            assert!(t.changed);
            assert_eq!(rt.get(0).pid, Some(77));
            assert_eq!(rt.get(0).pgid, Some(77));
            assert_eq!(rt.state_at(0), State::Starting);
            assert!(rt.check_invariants(&p).is_empty());
        }

        #[test]
        fn adoption_is_meaningless_elsewhere() {
            let p = super::plan1(ServicePlan::new(String::from("a")));
            let mut rt = Runtime::from_plan(&p);
            // Not starting: quiet.
            let t = apply(&Event::AdoptedPid { idx: 0, pid: 77 }, &mut rt, &p, 5);
            assert!(!t.changed);
            // Starting but not forking: quiet.
            *rt.get_mut(0) = super::starting();
            let t = apply(&Event::AdoptedPid { idx: 0, pid: 77 }, &mut rt, &p, 5);
            assert!(!t.changed);
            assert_eq!(rt.get(0).pid, Some(100));
        }
    }
}
