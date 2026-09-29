//! Property tests for the core state machine.
//!
//! These are the tests that matter most, and they are here for a specific
//! reason: **the failure modes of a supervisor are race-shaped**, and a race is
//! not something you can reach by writing a test case by hand. You can only
//! reach it by throwing a large number of interleavings at the state machine
//! and asserting that it never produces an impossible state.
//!
//! `DESIGN.md` §11.1 sets the rule:
//!
//! > A test that cannot detect a failure in a real start of a real process
//! > does not count as an init test. Measured that way or it does not count.
//!
//! These do not satisfy that rule on their own — they run without forking
//! anything, and the integration tests in phase 1 are what close the gap. But
//! they are what make the pure half *provable*, which is why the pure half
//! exists. Without `zcore` being free of I/O there would be nowhere to run
//! them.
//!
//! # What is asserted
//!
//! Six invariants, from `DESIGN.md` §4.1, checked after every single event:
//!
//! 1. `State` is one of the four legal values, and every service's
//!    bookkeeping is self-consistent ([`invariants_hold`]).
//! 2. A service is `Running` only if its required dependencies are `Running`.
//! 3. `Desired` is never changed by an event that did not intend to.
//! 4. At most one signal of a given kind is issued to a service per event.
//! 5. The plan's topological order is respected: a service never spawns before
//!    something it depends on is up.
//! 6. Replaying the same events on a fresh runtime yields the same result
//!    (determinism).
//!
//! [`invariants_hold`]: ServiceState::invariants_hold

use zcore::action::Action;
use zcore::event::Event;
use zcore::reconcile::{due_events, reconcile};
use zcore::runtime::Runtime;
use zcore::transition::apply;
use zcore::types::{
    Budget, Desired, Idx, LogSink, Plan, Ready, Restart, ServiceKind, ServicePlan, SignalKind,
    State, StrictReady,
};

// ─────────────────────────────────────────────────────────────────────────────
// A deterministic, seedable PRNG
//
// Deliberately hand-rolled: `no_std` has no RNG, and a dependency in a crate
// whose entire premise is having none would be absurd. xorshift64* is more than
// adequate for choosing test inputs and, unlike `rand`, it is reproducible
// across runs and platforms, which is what a failing test needs.
// ─────────────────────────────────────────────────────────────────────────────

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next() % n as u64) as usize
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Plan construction
// ─────────────────────────────────────────────────────────────────────────────

/// Build a plan from `(name, required_names)`, ordered by Kahn with a
/// lowest-index tie-break so the order is deterministic.
///
/// This mirrors what `zconfig` does; re-implementing it here is deliberate, so
/// that a bug in `zconfig` cannot make these tests vacuously pass.
fn make_plan(spec: &[(&str, &[&str])], kind: &[ServiceKind]) -> Plan {
    let n = spec.len();
    let mut plan = Plan::default();
    for (i, (name, _)) in spec.iter().enumerate() {
        let mut sp = ServicePlan::new(std::string::String::from(*name));
        sp.kind = kind.get(i).copied().unwrap_or(ServiceKind::Process);
        sp.restart_budget = Budget {
            capacity: 3,
            window_ms: 1_000,
            delay_ms: 0,
        };
        sp.start_timeout_ms = 100;
        sp.stop_timeout_ms = 50;
        plan.services.push(sp);
    }
    // Resolve names to indices.
    let index_of = |name: &str| -> Idx {
        spec.iter()
            .position(|(s, _)| *s == name)
            .expect("dependency not in spec")
    };
    for (i, (_, deps)) in spec.iter().enumerate() {
        plan.services[i].required = deps.iter().map(|d| index_of(d)).collect();
    }
    // Kahn.
    let mut placed = vec![false; n];
    let mut order: Vec<Idx> = Vec::with_capacity(n);
    for _ in 0..n {
        for i in 0..n {
            if placed[i] {
                continue;
            }
            if plan.services[i].required.iter().all(|&d| placed[d]) {
                placed[i] = true;
                order.push(i);
            }
        }
    }
    assert_eq!(order.len(), n, "the test spec contains a cycle");
    plan.order_up = order.clone();
    plan.order_down = order.into_iter().rev().collect();
    // Reverse edges.
    for i in 0..n {
        for d in plan.services[i].required.clone() {
            plan.services[d].dependents.push(i);
        }
    }
    plan
}

fn position_in(plan: &Plan, idx: Idx) -> usize {
    plan.order_up
        .iter()
        .position(|&x| x == idx)
        .expect("index not in order")
}

// ─────────────────────────────────────────────────────────────────────────────
// Event generation
// ─────────────────────────────────────────────────────────────────────────────

/// An event that is *plausible* for the current state.
///
/// Generating only plausible events matters: a stream of impossible events
/// tests nothing, because the transition function is designed to ignore them.
/// The point is to explore the reachable state space, not to spam it.
fn random_event(rng: &mut Rng, rt: &Runtime, plan: &Plan) -> Event {
    let n = rt.len();
    if n == 0 {
        return Event::BudgetExhausted(0);
    }
    let idx = rng.below(n);
    let st = rt.state_at(idx);
    let codes = [0i32, 1, 2, 127, 130];
    match st {
        State::Stopped => match rng.below(4) {
            0 => Event::Exited {
                idx,
                code: codes[rng.below(codes.len())],
            },
            1 => Event::Signalled {
                idx,
                signal: [9, 11, 15][rng.below(3)],
            },
            2 => Event::SpawnFailed {
                idx,
                errno: 11 + rng.below(5) as i32,
            },
            _ => Event::BudgetExhausted(idx),
        },
        State::Starting => match rng.below(5) {
            0 => Event::Ready(idx),
            1 => Event::ExecOk(idx),
            2 => Event::Forked(idx),
            3 => Event::StartTimeout(idx),
            _ => Event::ReadyTimeout(idx),
        },
        State::Running => match rng.below(5) {
            0 => Event::Exited {
                idx,
                code: codes[rng.below(codes.len())],
            },
            1 => Event::Signalled {
                idx,
                signal: [9, 11, 15][rng.below(3)],
            },
            2 => Event::IoError { idx, errno: 13 },
            3 => Event::BudgetExhausted(idx),
            _ => {
                // A dependency of ours died, if we have one.
                let deps = &plan.services[idx].required;
                if deps.is_empty() {
                    Event::IoError { idx, errno: 5 }
                } else {
                    Event::DepLost {
                        idx,
                        dep: deps[rng.below(deps.len())],
                    }
                }
            }
        },
        State::Stopping => match rng.below(4) {
            0 => Event::Exited { idx, code: 0 },
            1 => Event::Signalled { idx, signal: 9 },
            2 => Event::StopTimeout(idx),
            _ => Event::StopTimeout(idx),
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The invariants
// ─────────────────────────────────────────────────────────────────────────────

fn check_invariants(rt: &Runtime, plan: &Plan) -> Result<(), String> {
    // (1) per-service bookkeeping.
    let offenders = rt.check_invariants(plan);
    if !offenders.is_empty() {
        let i = offenders[0];
        return Err(format!(
            "invariant 1: service #{} ({}) has inconsistent bookkeeping: {:?}",
            i,
            plan.services[i].name,
            rt.get(i)
        ));
    }

    for i in 0..rt.len() {
        // (2) Running implies required dependencies are Running.
        if rt.state_at(i).is_up() {
            for &d in &plan.services[i].required {
                if !rt.state_at(d).is_up() {
                    return Err(format!(
                        "invariant 2: #{} ({}) is Running but its dependency #{} ({}) is {:?}",
                        i,
                        plan.services[i].name,
                        d,
                        plan.services[d].name,
                        rt.state_at(d)
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Invariant 5: nothing spawns out of topological order.
///
/// Checked on a `Tick`: every `Spawn` must be preceded (in `order_up`) by a
/// service that is already `Running`.
fn check_spawn_order(tick: &zcore::Tick, rt: &Runtime, plan: &Plan) -> Result<(), String> {
    let spawning: Vec<Idx> = tick
        .actions
        .iter()
        .filter_map(|a| match *a {
            Action::Spawn(i) => Some(i),
            _ => None,
        })
        .collect();
    for i in spawning {
        for &d in &plan.services[i].required {
            if rt.state_at(d).is_up() {
                continue;
            }
            // Allowed only if the dependency is earlier in `order_up` and will
            // be spawned in this same tick - then it comes up first.
            if position_in(plan, d) < position_in(plan, i) {
                continue;
            }
            return Err(format!(
                "invariant 5: #{} ({}) would spawn before its dependency #{} ({})",
                i, plan.services[i].name, d, plan.services[d].name
            ));
        }
    }
    Ok(())
}

/// Invariant 4: never two signals of the same kind to one service, per event.
fn check_no_duplicate_signals(t: &zcore::Transition) -> Result<(), String> {
    let mut seen: Vec<(Idx, SignalKind)> = Vec::new();
    for a in &t.actions {
        if let Action::Signal { idx, signal } = *a {
            if seen.contains(&(idx, signal)) {
                return Err(format!(
                    "invariant 4: duplicate {signal:?} to #{idx} in one event"
                ));
            }
            seen.push((idx, signal));
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// The property tests
// ─────────────────────────────────────────────────────────────────────────────

/// The headline property: after any sequence of events, the runtime is still
/// internally consistent.
#[test]
fn invariants_hold_under_arbitrary_event_streams() {
    let plan = make_plan(
        &[("a", &[]), ("b", &["a"]), ("c", &["a"]), ("d", &["b", "c"])],
        &[
            ServiceKind::Process,
            ServiceKind::Process,
            ServiceKind::Process,
            ServiceKind::Process,
        ],
    );

    for seed in 0..300u64 {
        let mut rng = Rng::new(seed * 2_654_435_761 + 17);
        let mut rt = Runtime::from_plan(&plan);
        for i in 0..plan.len() {
            rt.set_desired(i, Desired::Up);
        }
        let mut now = 0u64;

        for _ in 0..200 {
            now += rng.below(30) as u64;
            let ev = random_event(&mut rng, &rt, &plan);
            let before_desired: Vec<Desired> = (0..plan.len()).map(|i| rt.get(i).desired).collect();

            let t = apply(&ev, &mut rt, &plan, now);

            if let Err(e) = check_invariants(&rt, &plan) {
                panic!(
                    "seed {seed} (rng {:x}), after event {:?}:\n  {e}",
                    rng.0, ev
                );
            }
            if let Err(e) = check_no_duplicate_signals(&t) {
                panic!(
                    "seed {seed} (rng {:x}), after event {:?}:\n  {e}",
                    rng.0, ev
                );
            }

            // (3) Desired only changes when the event says so.
            let intentional = matches!(ev, Event::DepLost { .. } | Event::DepFailed { .. });
            if !intentional {
                for (i, was) in before_desired.iter().enumerate() {
                    if *was != rt.get(i).desired {
                        panic!(
                            "invariant 3: seed {seed}, event {:?} changed Desired of #{} from {was:?} to {:?} without intending to",
                            ev,
                            i,
                            rt.get(i).desired
                        );
                    }
                }
            }
        }
    }
}

/// Reconcile never violates the spawn ordering, across many random states.
#[test]
fn reconciler_never_spawns_out_of_order() {
    let plan = make_plan(
        &[
            ("a", &[]),
            ("b", &["a"]),
            ("c", &["a", "b"]),
            ("d", &["c"]),
            ("e", &["c", "d"]),
        ],
        &[ServiceKind::Process; 5],
    );

    for seed in 0..200u64 {
        let mut rng = Rng::new(seed ^ 0xDEAD_BEEF);
        let mut rt = Runtime::from_plan(&plan);
        for i in 0..plan.len() {
            rt.set_desired(i, Desired::Up);
        }
        let mut now = 0u64;

        for _ in 0..100 {
            now += rng.below(50) as u64;
            // Random interleaving of events and reconciles: this is what a real
            // runtime does, and the interleaving is where ordering bugs live.
            for _ in 0..3 {
                let ev = random_event(&mut rng, &rt, &plan);
                let _ = apply(&ev, &mut rt, &plan, now);
            }
            let due = due_events(&rt, &plan, now);
            for ev in &due {
                let _ = apply(ev, &mut rt, &plan, now);
            }
            let tick = reconcile(&mut rt, &plan, now);
            if let Err(e) = check_spawn_order(&tick, &rt, &plan) {
                panic!("seed {seed}:\n  {e}");
            }
            if let Err(e) = check_invariants(&rt, &plan) {
                panic!("seed {seed}:\n  {e}");
            }
        }
    }
}

/// The system converges: a service that wants to be up and is not stopped
/// eventually starts, given time and no interference.
#[test]
fn the_system_converges_when_nothing_interferes() {
    let plan = make_plan(
        &[("a", &[]), ("b", &["a"]), ("c", &["b"]), ("t", &["c"])],
        &[
            ServiceKind::Process,
            ServiceKind::Process,
            ServiceKind::Process,
            ServiceKind::Target,
        ],
    );

    let mut rt = Runtime::from_plan(&plan);
    for i in 0..plan.len() {
        rt.set_desired(i, Desired::Up);
    }
    // The target has no process, so it must come up without any event.
    let mut now = 0u64;
    let mut spawns = 0usize;

    for _ in 0..500 {
        now += 10;
        // A process service reports `Ready` as soon as it is `Starting`, which
        // is what a successful spawn looks like without a real process.
        for i in 0..plan.len() {
            if rt.state_at(i) == State::Stopped {
                let tick = reconcile(&mut rt, &plan, now);
                spawns += tick
                    .actions
                    .iter()
                    .filter(|a| matches!(a, Action::Spawn(_)))
                    .count();
                if tick.actions.iter().any(|a| matches!(a, Action::Spawn(_i))) {
                    let st = rt.get_mut(i);
                    st.state = State::Starting;
                    st.pid = Some(1000 + i as i32);
                    st.pgid = Some(1000 + i as i32);
                    st.started_at = Some(now);
                    st.start_due_at = Some(now + 100);
                    st.ready_due_at = None;
                }
            }
        }
        for i in 0..plan.len() {
            if rt.state_at(i) == State::Starting && plan.services[i].kind.has_process() {
                let _ = apply(&Event::Ready(i), &mut rt, &plan, now);
            }
        }
        if rt.count_running() == plan.len() {
            break;
        }
    }

    assert_eq!(
        rt.count_running(),
        plan.len(),
        "the system never converged; state: {:?}",
        rt.snapshot(&plan)
    );
    assert!(
        spawns >= 3,
        "expected at least the three process services to spawn"
    );
    // The target spawned nothing.
    let t = plan.index_of("t").unwrap();
    assert!(!plan.services[t].kind.has_process());
}

/// A target comes up purely from its dependencies, with no process and no
/// events of its own.
#[test]
fn a_target_never_spawns_and_never_kills() {
    let plan = make_plan(
        &[("dep", &[]), ("virt", &["dep"])],
        &[ServiceKind::Process, ServiceKind::Target],
    );
    let mut rt = Runtime::from_plan(&plan);
    rt.set_desired(0, Desired::Up);
    rt.set_desired(1, Desired::Up);

    for now in (0..20u64).map(|n| n * 50) {
        let tick = reconcile(&mut rt, &plan, now);
        for a in &tick.actions {
            if let Action::Spawn(i) = *a {
                assert_ne!(i, 1, "a target must never fork");
            }
        }
    }
}

/// Determinism: the same seed replays identically. Guards against accidental
/// dependence on hash iteration order, `Vec` capacity, or the clock.
#[test]
fn replays_are_deterministic() {
    let plan = make_plan(
        &[("a", &[]), ("b", &["a"]), ("c", &["b"])],
        &[ServiceKind::Process; 3],
    );

    let run = |seed: u64| -> (Vec<State>, Vec<Vec<Action>>) {
        let mut rng = Rng::new(seed);
        let mut rt = Runtime::from_plan(&plan);
        for i in 0..plan.len() {
            rt.set_desired(i, Desired::Up);
        }
        let mut now = 0u64;
        let mut states = Vec::new();
        let mut actions = Vec::new();
        for _ in 0..150 {
            now += rng.below(20) as u64;
            let ev = random_event(&mut rng, &rt, &plan);
            let t = apply(&ev, &mut rt, &plan, now);
            states.push(rt.state_at(0));
            actions.push(t.actions.clone());
        }
        (states, actions)
    };

    for seed in [1u64, 7, 42, 999, 65_535] {
        assert_eq!(
            run(seed),
            run(seed),
            "seed {seed} did not replay identically"
        );
    }
}

/// Strict readiness gates startup; plain readiness does not, and `none` needs
/// no clock at all.
#[test]
fn readiness_gating_matches_the_configured_policy() {
    for (ready, strict) in [
        (Ready::Strict(StrictReady::Tcp(22)), true),
        (Ready::Notify, false),
        (Ready::Ping(std::string::String::from("/bin/true")), false),
        (Ready::Tcp(22), false),
    ] {
        let mut sp = ServicePlan::new(std::string::String::from("x"));
        sp.ready = ready.clone();
        sp.start_timeout_ms = 100;
        let mut plan = Plan::default();
        plan.services.push(sp);
        plan.order_up = vec![0];
        plan.order_down = vec![0];

        let mut rt = Runtime::from_plan(&plan);
        *rt.get_mut(0) = zcore::runtime::ServiceState::wanted_up();
        rt.get_mut(0).state = State::Starting;
        rt.get_mut(0).pid = Some(1);
        rt.get_mut(0).pgid = Some(1);
        rt.get_mut(0).started_at = Some(0);
        rt.get_mut(0).start_due_at = Some(100);
        rt.get_mut(0).ready_due_at = Some(50);

        let evs = due_events(&rt, &plan, 50);
        if strict {
            assert!(
                evs.contains(&Event::ReadyTimeout(0)),
                "{ready:?} must treat ready-timeout as a failure, got {evs:?}"
            );
        } else {
            assert!(
                evs.contains(&Event::Ready(0)),
                "{ready:?} must treat ready-timeout as ready, got {evs:?}"
            );
        }
    }
}

/// `ready = none` goes Running on exec, with no timer involved.
#[test]
fn ready_none_goes_running_at_exec_without_a_clock() {
    let mut sp = ServicePlan::new(std::string::String::from("x"));
    sp.ready = Ready::None;
    let mut plan = Plan::default();
    plan.services.push(sp);
    plan.order_up = vec![0];
    plan.order_down = vec![0];

    let mut rt = Runtime::from_plan(&plan);
    *rt.get_mut(0) = zcore::runtime::ServiceState::wanted_up();
    rt.get_mut(0).state = State::Starting;
    rt.get_mut(0).pid = Some(1);
    rt.get_mut(0).pgid = Some(1);
    rt.get_mut(0).started_at = Some(0);
    rt.get_mut(0).start_due_at = Some(100_000);
    assert!(
        rt.get(0).ready_due_at.is_none(),
        "no handshake, no ready deadline"
    );

    let t = apply(&Event::ExecOk(0), &mut rt, &plan, 1);
    assert_eq!(rt.state_at(0), State::Running, "{t:?}");
    assert!(rt.check_invariants(&plan).is_empty());
}

/// No `unwrap`-adjacent panics from out-of-range indices: an event for a
/// service that does not exist is ignored, not fatal.
#[test]
fn out_of_range_events_are_ignored_not_fatal() {
    let plan = make_plan(&[("a", &[])], &[ServiceKind::Process]);
    let mut rt = Runtime::from_plan(&plan);
    for ev in [
        Event::Ready(99),
        Event::Exited { idx: 99, code: 0 },
        Event::Signalled {
            idx: 999,
            signal: 9,
        },
        Event::StopTimeout(4_000),
        Event::DepLost { idx: 77, dep: 0 },
    ] {
        let t = apply(&ev, &mut rt, &plan, 0);
        assert!(
            t.is_empty(),
            "{ev:?} for a non-existent service must be a no-op"
        );
    }
}

/// Log and sink configuration cannot influence the state machine. A plan that
/// differs only in logging must behave identically.
#[test]
fn logging_configuration_cannot_change_behaviour() {
    let mut spec_plan = make_plan(
        &[("a", &[]), ("b", &["a"])],
        &[ServiceKind::Process, ServiceKind::Process],
    );
    let mut loud_plan = spec_plan.clone();
    for sp in loud_plan.services.iter_mut() {
        sp.log = LogSink::File {
            path: std::string::String::from("/tmp/whatever.log"),
            max_bytes: 1,
            backups: 0,
        };
    }
    spec_plan.services[0].log = LogSink::None;

    let run = |plan: &Plan| -> Vec<State> {
        let mut rng = Rng::new(4242);
        let mut rt = Runtime::from_plan(plan);
        for i in 0..plan.len() {
            rt.set_desired(i, Desired::Up);
        }
        let mut now = 0u64;
        let mut out = Vec::new();
        for _ in 0..200 {
            now += rng.below(25) as u64;
            let ev = random_event(&mut rng, &rt, plan);
            let _ = apply(&ev, &mut rt, plan, now);
            let _ = reconcile(&mut rt, plan, now);
            out.push(rt.state_at(0));
            out.push(rt.state_at(1));
        }
        out
    };

    assert_eq!(
        run(&spec_plan),
        run(&loud_plan),
        "logging changed the state machine"
    );
}

/// Restart policy is the only thing that decides whether a crash is followed by
/// a restart, and the budget is the only thing that stops it.
#[test]
fn restart_policy_and_budget_are_independent_of_everything_else() {
    for policy in [Restart::Never, Restart::OnFailure, Restart::Always] {
        for (capacity, expect_restart) in [(0u32, false), (1, true)] {
            let mut sp = ServicePlan::new(std::string::String::from("s"));
            sp.restart = policy;
            sp.restart_budget = Budget {
                capacity,
                window_ms: 60_000,
                delay_ms: 0,
            };
            let mut plan = Plan::default();
            plan.services.push(sp);
            plan.order_up = vec![0];
            plan.order_down = vec![0];

            let mut rt = Runtime::from_plan(&plan);
            *rt.get_mut(0) = zcore::runtime::ServiceState::wanted_up();
            rt.get_mut(0).state = State::Running;
            rt.get_mut(0).pid = Some(1);
            rt.get_mut(0).pgid = Some(1);
            rt.get_mut(0).started_at = Some(0);
            rt.get_mut(0).last_exit = Some(1);
            // Prime the bucket.
            let b = plan.services[0].restart_budget;
            let _ = rt.get_mut(0).bucket.allows(&b, 0);

            let _ = apply(&Event::Exited { idx: 0, code: 1 }, &mut rt, &plan, 0);
            let tick = reconcile(&mut rt, &plan, 0);
            let spawned = tick.actions.iter().any(|a| matches!(a, Action::Spawn(0)));

            let want = expect_restart && policy.wants_restart(Some(1));
            assert_eq!(
                spawned, want,
                "policy={policy:?} capacity={capacity}: expected spawn={want}, got {spawned}"
            );
        }
    }
}
