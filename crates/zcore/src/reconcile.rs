//! The reconciler: `desired + actual` → the actions that close the gap.
//!
//! This is the heart of zinit, and it is deliberately small. `DESIGN.md` §4.2
//! explains why: dinit keeps a live mutable graph with two propagation queues
//! and a depth counter, and that structure is where its worst bugs live. zinit
//! freezes the graph into a topologically ordered array at load time, so
//! convergence is **two linear passes and nothing else**.
//!
//! * **Start**: walk [`Plan::order_up`] forward. When we reach a service, every
//!   service it depends on has a lower index, so it has already been decided.
//! * **Stop**: walk [`Plan::order_down`] (the exact reverse). A service can
//!   never be stopped before something that depends on it.
//!
//! There is no queue, no recursion, no depth counter, and no phase split. The
//! ordering *is* the algorithm.
//!
//! The one non-obvious consequence: **a restart is not a special case.** A
//! service that crashed is `(Stopped, Up)`. The reconciler sees a service that
//! should be running and is not, checks the budget, and spawns it. There is no
//! `Restarting` state and no restart queue, because there is nothing to
//! coordinate - the plan already knows the order.

use alloc::vec::Vec;

use crate::action::{Action, LogLevel, log as log_action};
use crate::runtime::Runtime;
use crate::types::{Desired, Idx, Plan, ServiceKind, State};

/// Result of one reconciliation pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// Actions the runtime must carry out, in order.
    pub actions: Vec<Action>,
    /// Services that hit an empty restart budget this pass.
    pub budget_exhausted: Vec<Idx>,
    /// Services whose state changed during this pass.
    pub changed: Vec<Idx>,
}

impl Tick {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty() && self.budget_exhausted.is_empty() && self.changed.is_empty()
    }
}

/// Compute the actions that move `runtime` one step closer to its desired
/// state, as of `now_ms` (monotonic milliseconds).
///
/// Pure: no clocks, no I/O, no globals. Calling it twice with the same
/// arguments and the same runtime yields the same output, which is what makes
/// the property tests meaningful.
pub fn reconcile(runtime: &mut Runtime, plan: &Plan, now_ms: u64) -> Tick {
    let mut tick = Tick::default();

    // ── Pass 0: demand-start (dinit's `depends-on` activation) ────────────
    //
    // A dependency that was disabled at boot (`enabled = no`, believed
    // unreachable by the convert closure) becomes wanted-up exactly when
    // a service that really pulls it is actually running — mirrors dinit's
    // activation propagation instead of only the static plan. Only services
    // that were disabled at *load* participate (operator-stopped services
    // keep `enabled = true`, so `zctl stop` is never overridden by a
    // future dependent going up). `ponytail:` racy rescan is O(services*
    // edges) per tick; promote to a dirty flag when a reload adds an edge.
    for i in 0..runtime.len() {
        if runtime.get(i).desired != Desired::Up {
            continue;
        }
        // No state gate on the dependent: a service that is wanted up but
        // blocked behind a disabled dep must be allowed to pull that dep
        // even while still Stopped. Otherwise the enable never fires (the
        // trigger can only pull; it may not spawn), and the disabled service
        // stays down forever — exactly the Artix agetty chain.
        for &d in &plan.services[i].required {
            if plan.services[d].enabled {
                continue;
            }
            let dr = runtime.get(d);
            if dr.desired != Desired::Down || dr.restart_suppressed {
                continue;
            }
            if dr.state != State::Stopped {
                continue;
            }
            runtime.set_desired(d, Desired::Up);
            tick.changed.push(d);
        }
    }

    // ── Pass 1: start, in topological order ───────────────────────────────
    //
    // Walking `order_up` forward means every dependency has already been
    // considered by the time we reach a dependent. One pass, no ordering
    // logic inside the loop.
    for &idx in &plan.order_up {
        let sp = &plan.services[idx];

        if runtime.get(idx).desired == Desired::Down {
            continue;
        }

        // Targets have no process: they are up exactly when their required
        // dependencies are. No spawn, no budget, no readiness. A dangling
        // required edge holds the target like any unmet requirement, loudly
        // at load and quietly here — never the boot.
        if !sp.kind.has_process() {
            let deps_ok = runtime.deps_satisfied(idx, plan)
                && runtime.after_satisfied(idx, plan)
                && sp.missing_required.is_empty();
            let cur = runtime.get(idx).state;
            if deps_ok && cur != State::Running {
                runtime.get_mut(idx).state = State::Running;
                runtime.get_mut(idx).pid = None;
                runtime.get_mut(idx).started_at = Some(now_ms);
                tick.actions.push(Action::MarkReady(idx));
                tick.actions.push(Action::DepsChanged(idx));
                tick.changed.push(idx);
            } else if !deps_ok && cur == State::Running {
                runtime.get_mut(idx).state = State::Stopped;
                runtime.get_mut(idx).pid = None;
                runtime.get_mut(idx).started_at = None;
                tick.actions.push(Action::MarkDown {
                    idx,
                    unexpected: false,
                });
                tick.actions.push(Action::DepsChanged(idx));
                tick.changed.push(idx);
            }
            continue;
        }

        match runtime.get(idx).state {
            State::Stopped => {
                // Should this service be running at all right now?
                if !runtime.deps_satisfied(idx, plan) {
                    continue;
                }
                // Order-only edges: wait for the first attempt, never for
                // success. See `after_satisfied` for the release rule.
                if !runtime.after_satisfied(idx, plan) {
                    continue;
                }
                // Dangling required edges: hold, never spawn into the void.
                if !sp.missing_required.is_empty() {
                    continue;
                }
                // A death the policy refused to retry. `Desired` is still
                // `Up` - the operator still wants it - but the reconciler
                // must leave it alone until `zctl kick` or `zctl start`.
                if runtime.get(idx).restart_suppressed {
                    continue;
                }
                // A oneshot that ran to completion is done, not dead.
                // `Desired` stays `Up` so status tells the truth, but there
                // is nothing to converge: only `kick` re-arms it.
                if sp.kind == ServiceKind::Oneshot && runtime.get(idx).completed {
                    continue;
                }
                // A service that has never run is not subject to the restart
                // budget: the budget exists to stop a *crash loop*, not to
                // ration first starts. `failed_starts` still counts them.
                let was_running_before =
                    runtime.get(idx).failed_starts > 0 || runtime.get(idx).last_exit.is_some();
                if was_running_before {
                    let budget = sp.restart_budget;
                    // Take, not `allows`: this is the one place a token is
                    // spent, and it is spent exactly when the spawn is
                    // decided. A budget of N therefore permits N restarts.
                    if !runtime.get_mut(idx).bucket.take(&budget, now_ms) {
                        // A take fails two ways: the bucket is empty, or the
                        // delay is still spacing attempts. Only the first is
                        // exhaustion worth reporting; a delay-gated retry is
                        // routine, and the next passes spend the token in
                        // silence.
                        if !runtime.get_mut(idx).bucket.exhausted(&budget, now_ms) {
                            continue;
                        }
                        if !tick.budget_exhausted.contains(&idx) {
                            tick.budget_exhausted.push(idx);
                            tick.actions.push(Action::BudgetExhausted(idx));
                            tick.actions.push(log_action(
                                idx,
                                LogLevel::Error,
                                "restart budget exhausted; not retrying (use `zctl kick` to reset)",
                            ));
                        }
                        continue;
                    }
                }
                tick.actions.push(Action::Spawn(idx));
            }
            State::Starting | State::Running | State::Stopping => {}
        }
    }

    // ── Pass 2: stop, in exact reverse topological order ──────────────────
    //
    // Reversed so a service is always torn down before its dependencies.
    // Note this pass only *requests* the stop; the actual `SIGTERM` is issued
    // by the transition function when `Desired` flips, so a stop is not
    // duplicated here.
    for &idx in plan.order_down.iter() {
        let st = runtime.get(idx);
        if st.desired == Desired::Up || st.pinned {
            continue;
        }
        if matches!(st.state, State::Running | State::Starting)
            && plan.services[idx].kind.has_process()
        {
            runtime.get_mut(idx).desired = Desired::Down;
            tick.changed.push(idx);
        }
    }

    tick
}

/// Decide whether a service that has just died should be started again.
///
/// Called from the transition function once a death is known and the state has
/// been set to `Stopped`. Kept separate from [`reconcile`] because it is the
/// one place where the *policy* question ("should it come back?") is asked,
/// as distinct from the *bookkeeping* question ("is it up?").
///
/// Returns `true` if `Desired` should remain `Up`, in which case the next
/// [`reconcile`] pass will spawn it subject to the budget.
pub fn should_restart(runtime: &Runtime, plan: &Plan, idx: Idx, code: Option<i32>) -> bool {
    let sp = &plan.services[idx];
    if !sp.kind.has_process() {
        return false;
    }
    if !sp.restart.wants_restart(code) {
        return false;
    }
    // A service we were deliberately stopping does not "restart".
    if runtime.get(idx).desired == Desired::Down {
        return false;
    }
    true
}

/// Deadlines that have expired at `now_ms`, as `Event`s.
///
/// Expiry is a *function of time*, not of a signal, so the core asks the
/// clock (via a parameter) rather than receiving a timer event. This keeps the
/// timeout logic testable without sleeping.
pub fn due_events(runtime: &Runtime, plan: &Plan, now_ms: u64) -> Vec<crate::Event> {
    let mut out = Vec::new();
    for (idx, sp) in plan.services.iter().enumerate() {
        let st = runtime.get(idx);
        // Watchdog first: a service that stopped checking in is hung, and a
        // hung service's other deadlines are stale news next to that fact.
        // Measured from the last ping, or from birth when none arrived yet.
        // One event per service per pass: anything else still due is reported
        // on the next pass, which is already coming.
        if let Some(watchdog_sec) = sp.watchdog_sec {
            let window_ms = watchdog_sec.saturating_mul(1000);
            if window_ms > 0
                && matches!(st.state, State::Running | State::Starting)
                && now_ms.saturating_sub(st.last_watchdog_ms.or(st.started_at).unwrap_or(now_ms))
                    >= window_ms
            {
                out.push(crate::Event::WatchdogExpired(idx));
                continue;
            }
        }
        match st.state {
            State::Starting => {
                // Ready deadline first: a service that became ready and then
                // blew its start deadline should not be reported twice.
                if let Some(due) = st.ready_due_at
                    && now_ms >= due
                {
                    if sp.ready.is_strict() {
                        out.push(crate::Event::ReadyTimeout(idx));
                    } else {
                        // Non-strict: expiry means "up anyway", with a warning.
                        out.push(crate::Event::Ready(idx));
                    }
                    continue;
                }
                if let Some(due) = st.start_due_at
                    && now_ms >= due
                {
                    out.push(crate::Event::StartTimeout(idx));
                }
            }
            State::Stopping => {
                if let Some(due) = st.stop_due_at
                    && now_ms >= due
                {
                    out.push(crate::Event::StopTimeout(idx));
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Desired, ServiceKind, ServicePlan};
    use alloc::{vec, vec::Vec};
    use std::string::String;

    fn svc(name: &str) -> ServicePlan {
        ServicePlan::new(String::from(name))
    }

    fn build(edges: &[(&str, &[&str])]) -> Plan {
        // Build a plan from name -> required names, topologically ordered by
        // repeated relaxation (a 3-line Kahn; the real one lives in zconfig).
        let mut p = Plan::default();
        for (n, _) in edges {
            p.services.push(svc(n));
        }
        for (n, deps) in edges {
            let i = p.index_of(n).unwrap();
            p.services[i].required = deps.iter().map(|d| p.index_of(d).unwrap()).collect();
        }
        // Order by repeated relaxation.
        let n = p.services.len();
        let mut order: Vec<Idx> = Vec::new();
        let mut placed = vec![false; n];
        for _ in 0..n {
            for i in 0..n {
                if placed[i] {
                    continue;
                }
                if p.services[i].required.iter().all(|&d| placed[d]) {
                    placed[i] = true;
                    order.push(i);
                }
            }
        }
        p.order_up = order.clone();
        p.order_down = order.into_iter().rev().collect();
        p
    }

    #[test]
    fn nothing_happens_when_nothing_is_wanted() {
        let p = build(&[("a", &[]), ("b", &["a"])]);
        let mut rt = Runtime::from_plan(&p);
        let t = reconcile(&mut rt, &p, 0);
        assert!(t.is_empty(), "expected no actions, got {:?}", t.actions);
    }

    #[test]
    fn a_wanted_service_spawns_in_dependency_order() {
        let p = build(&[("a", &[]), ("b", &["a"]), ("c", &["b"])]);
        let mut rt = Runtime::from_plan(&p);
        for i in 0..3 {
            rt.set_desired(i, Desired::Up);
        }
        let t = reconcile(&mut rt, &p, 0);
        let spawned: Vec<Idx> = t
            .actions
            .iter()
            .filter_map(|a| match *a {
                Action::Spawn(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(spawned.len(), 1, "only 'a' may spawn first: {t:?}");
        assert_eq!(spawned[0], p.index_of("a").unwrap());
    }

    fn spawned(t: &Tick) -> Vec<Idx> {
        t.actions
            .iter()
            .filter_map(|a| match *a {
                Action::Spawn(i) => Some(i),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn after_edge_holds_spawn_until_first_attempt() {
        let mut p = build(&[("a", &[]), ("b", &[])]);
        let (a, b) = (p.index_of("a").unwrap(), p.index_of("b").unwrap());
        p.services[b].after = vec![a];
        let mut rt = Runtime::from_plan(&p);
        rt.set_desired(a, Desired::Up);
        rt.set_desired(b, Desired::Up);
        let t = reconcile(&mut rt, &p, 0);
        assert_eq!(spawned(&t), vec![a], "b waits for a's attempt: {t:?}");
        // A mere start (attempt in flight) releases the waiter.
        rt.get_mut(a).state = State::Starting;
        let t = reconcile(&mut rt, &p, 0);
        assert_eq!(spawned(&t), vec![b], "a started, b goes: {t:?}");
    }

    #[test]
    fn after_holds_while_a_oneshot_is_mid_flight() {
        // A started daemon releases (effects unfold over its lifetime),
        // but a started oneshot does not: its effects land at completion.
        // Releasing on Starting raced the Artix ro→rw remount.
        let mut p = build(&[("a", &[]), ("b", &[])]);
        let (a, b) = (p.index_of("a").unwrap(), p.index_of("b").unwrap());
        p.services[a].kind = ServiceKind::Oneshot;
        p.services[b].after = vec![a];
        let mut rt = Runtime::from_plan(&p);
        rt.set_desired(a, Desired::Up);
        rt.set_desired(b, Desired::Up);
        rt.get_mut(a).state = State::Starting;
        let t = reconcile(&mut rt, &p, 0);
        assert!(spawned(&t).is_empty(), "mid-oneshot holds b: {t:?}");
        rt.get_mut(a).completed = true;
        let t = reconcile(&mut rt, &p, 0);
        assert_eq!(spawned(&t), vec![b], "completed oneshot releases b: {t:?}");
    }

    #[test]
    fn after_releases_on_terminal_outcomes() {
        // Completed, failed, suppressed, and disabled all release: order
        // without gating, so a dead early oneshot delays followers but
        // never wedges them. A failed (retryable) dependency still spawns
        // itself alongside the released waiter; completed and suppressed
        // ones stay down.
        let terminals: [fn(&mut Runtime, Idx); 3] = [
            |rt: &mut Runtime, a: Idx| rt.get_mut(a).completed = true,
            |rt: &mut Runtime, a: Idx| rt.get_mut(a).failed_starts = 1,
            |rt: &mut Runtime, a: Idx| rt.get_mut(a).restart_suppressed = true,
        ];
        for setup in terminals {
            let mut p = build(&[("a", &[]), ("b", &[])]);
            let (a, b) = (p.index_of("a").unwrap(), p.index_of("b").unwrap());
            p.services[a].kind = ServiceKind::Oneshot;
            p.services[b].after = vec![a];
            let mut rt = Runtime::from_plan(&p);
            rt.set_desired(a, Desired::Up);
            rt.set_desired(b, Desired::Up);
            setup(&mut rt, a);
            let t = reconcile(&mut rt, &p, 0);
            let got = spawned(&t);
            assert!(got.contains(&b), "terminal a releases b: {t:?}");
            if got.contains(&a) {
                // The failed (retryable) case also respawns a itself.
                assert_eq!(got, vec![a, b], "retry rides along: {t:?}");
            }
        }
        // Disabled (desired down) releases too.
        let mut p = build(&[("a", &[]), ("b", &[])]);
        let (a, b) = (p.index_of("a").unwrap(), p.index_of("b").unwrap());
        p.services[b].after = vec![a];
        let mut rt = Runtime::from_plan(&p);
        rt.set_desired(b, Desired::Up);
        let t = reconcile(&mut rt, &p, 0);
        assert_eq!(spawned(&t), vec![b], "down a releases b: {t:?}");
    }

    #[test]
    fn missing_required_holds_spawn_but_not_the_boot() {
        // A dangling required edge holds its service (never spawns) while
        // everything else converges: the agetty ghost that bricked a boot.
        let mut p = build(&[("ghosted", &[]), ("free", &[])]);
        let (g, f) = (p.index_of("ghosted").unwrap(), p.index_of("free").unwrap());
        p.services[g].missing_required = alloc::vec![String::from("ghost")];
        let mut rt = Runtime::from_plan(&p);
        rt.set_desired(g, Desired::Up);
        rt.set_desired(f, Desired::Up);
        let t = reconcile(&mut rt, &p, 0);
        assert_eq!(spawned(&t), alloc::vec![f], "only the healthy spawn: {t:?}");
    }

    #[test]
    fn demand_start_single_pass() {
        // A boot-enabled "base" reports that it running, and a disabled
        // "extra" that it wants pulls become wanted-up via activation:
        // one reconcile pass flips the want-state, spawning needs a
        // second pass (consistent with the synchronous model -- never
        // "magic eager spawning"). en<rfl:parameter name="kment reinits the pull story.
        let mut p = build(&[("puller", &[]), ("lazy", &[])]);
        let (puller, lazy) = (p.index_of("puller").unwrap(), p.index_of("lazy").unwrap());
        p.services[puller].required = vec![lazy];
        p.services[lazy].enabled = false; // disabled at boot
        let mut rt = Runtime::from_plan(&p);
        rt.set_desired(puller, Desired::Up);
        // Pass A: puller and lazy both converge in one pass (udisable chain).
        let t1 = reconcile(&mut rt, &p, 0);
        assert!(spawned(&t1).contains(&puller), "puller spawns: {t1:?}");
        assert!(spawned(&t1).contains(&lazy), "pulled lazy spawns: {t1:?}");
        assert!(rt.get(lazy).desired == Desired::Up, "lazy pulled up");
    }

    fn tick_extras(rt: &Runtime, idx: Idx) -> bool {
        rt.get(idx).desired == Desired::Up
    }

    #[test]
    fn a_target_goes_running_without_a_process() {
        let mut p = build(&[("base", &[])]);
        p.services[0].kind = ServiceKind::Target;
        let mut rt = Runtime::from_plan(&p);
        rt.set_desired(0, Desired::Up);
        let t = reconcile(&mut rt, &p, 0);
        assert!(
            t.actions.iter().any(|a| matches!(a, Action::MarkReady(0))),
            "a target should be marked ready: {t:?}"
        );
        assert!(
            !t.actions.iter().any(|a| matches!(a, Action::Spawn(0))),
            "a target must never fork: {t:?}"
        );
        assert!(
            rt.check_invariants(&p).is_empty(),
            "a Running target with no pid is by design, not a violation"
        );
    }

    #[test]
    fn budget_stops_a_crash_loop() {
        let mut p = build(&[("flappy", &[])]);
        let i = 0;
        p.services[i].restart_budget = crate::types::Budget {
            capacity: 2,
            window_ms: 60_000,
            delay_ms: 0,
        };
        let mut rt = Runtime::from_plan(&p);
        rt.set_desired(i, Desired::Up);

        // First start: a service that has never run is not rationed.
        let t1 = reconcile(&mut rt, &p, 0);
        assert!(t1.actions.iter().any(|a| matches!(a, Action::Spawn(_i))));

        // Simulate: it ran, it died.
        {
            let s = rt.get_mut(i);
            s.state = State::Stopped;
            s.pid = None;
            s.started_at = None;
            s.start_due_at = None;
            s.last_exit = Some(1);
            s.failed_starts = 1;
        }
        // Drain the bucket.
        for _ in 0..2 {
            let _ = rt.get_mut(i).bucket.take(&p.services[i].restart_budget, 0);
        }
        let t2 = reconcile(&mut rt, &p, 0);
        assert!(
            !t2.actions.iter().any(|a| matches!(a, Action::Spawn(_i))),
            "budget should have blocked the restart: {t2:?}"
        );
        assert!(t2.budget_exhausted.contains(&i), "and reported it");
    }

    #[test]
    fn delay_gated_retry_is_silent_not_exhausted() {
        // Tokens in hand but the delay still spacing attempts: routine, not
        // exhaustion. The reconciler must stay quiet and let a later pass
        // spend the token — announcing "budget empty" here once flooded a
        // real machine's log on every restart cycle.
        let mut p = build(&[("flappy", &[])]);
        let i = 0;
        p.services[i].restart_budget = crate::types::Budget {
            capacity: 2,
            window_ms: 60_000,
            delay_ms: 1_000,
        };
        let mut rt = Runtime::from_plan(&p);
        rt.set_desired(i, Desired::Up);
        // One restart spent at t=0: one token left, delay clock running.
        {
            let s = rt.get_mut(i);
            s.state = State::Stopped;
            s.pid = None;
            s.started_at = None;
            s.start_due_at = None;
            s.last_exit = Some(1);
            s.failed_starts = 1;
        }
        let _ = rt.get_mut(i).bucket.take(&p.services[i].restart_budget, 0);
        // At t=100 the delay still gates: no spawn, and no exhaustion noise.
        let t = reconcile(&mut rt, &p, 100);
        assert!(
            !t.actions.iter().any(|a| matches!(a, Action::Spawn(_i))),
            "delay should have held the restart: {t:?}"
        );
        assert!(
            !t.actions
                .iter()
                .any(|a| matches!(a, Action::BudgetExhausted(_i))),
            "a delay is not an empty bucket: {t:?}"
        );
        assert!(t.budget_exhausted.is_empty());
        // At t=1000 the delay has passed and the held token spends.
        let t = reconcile(&mut rt, &p, 1_000);
        assert!(
            t.actions.iter().any(|a| matches!(a, Action::Spawn(_i))),
            "the held token must spend once the delay passes: {t:?}"
        );
    }

    #[test]
    fn due_events_cover_ready_start_and_stop_deadlines() {
        let p = build(&[("a", &[])]);
        let mut rt = Runtime::from_plan(&p);
        {
            let s = rt.get_mut(0);
            s.state = State::Starting;
            s.pid = Some(1);
            s.started_at = Some(0);
            s.start_due_at = Some(100);
        }
        let ev = due_events(&rt, &p, 50);
        assert!(ev.is_empty(), "nothing is due yet");
        let ev = due_events(&rt, &p, 100);
        assert!(ev.contains(&crate::Event::StartTimeout(0)), "{ev:?}");
    }

    #[test]
    fn non_strict_ready_expiry_becomes_ready() {
        let p = build(&[("a", &[])]);
        let mut rt = Runtime::from_plan(&p);
        rt.get_mut(0).ready_due_at = Some(10);
        // needs Starting state for due_events to look
        {
            let s = rt.get_mut(0);
            s.state = State::Starting;
            s.pid = Some(1);
            s.started_at = Some(0);
            s.start_due_at = Some(1000);
        }
        let ev = due_events(&rt, &p, 10);
        assert!(
            ev.contains(&crate::Event::Ready(0)),
            "non-strict expiry should mean ready: {ev:?}"
        );
        assert!(!ev.contains(&crate::Event::ReadyTimeout(0)));
    }

    #[test]
    fn stop_pass_flips_desired_for_running_services() {
        let p = build(&[("a", &[])]);
        let mut rt = Runtime::from_plan(&p);
        rt.get_mut(0).state = State::Running;
        rt.get_mut(0).pid = Some(1);
        rt.set_desired(0, Desired::Down);
        let t = reconcile(&mut rt, &p, 0);
        assert!(t.changed.contains(&0), "stop pass should mark the change");
    }
}
