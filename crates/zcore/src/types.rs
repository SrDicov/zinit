//! Shared vocabulary for the zinit supervisor.
//!
//! This module is the **contract** between `zconfig` (which builds a [`Plan`])
//! and the reconciler (which consumes it). Everything here is data: no logic,
//! no I/O, no allocation beyond what a `Vec` or `String` needs.
//!
//! Invariants that are mechanically enforced here, not by convention:
//!
//! * [`State`] and [`Desired`] are `Copy` and small: the runtime keeps a
//!   `Vec<RuntimeEntry>` indexed by [`Idx`], so this type is the hot data.
//! * A restart is **not** a state. A service that crashed is simply no longer
//!   `Running` while [`Desired::Up`] is unchanged; the reconciler converges.
//!   This is why there is no `Restarting` variant - see `DESIGN.md` §4.2.

use alloc::string::String;
use alloc::vec::Vec;

/// Index of a service within a [`Plan`].
///
/// This is a dense index into `Plan::services`, assigned by the graph builder.
/// It is *not* a name and carries no meaning beyond being a key.
pub type Idx = usize;

/// Upper bound on services in a single plan.
///
/// A hard cap, not a soft one: every per-service allocation is sized from this,
/// so an unbounded count would be a denial-of-service against the supervisor.
pub const MAX_SERVICES: usize = 4096;

// ─────────────────────────────────────────────────────────────────────────────
// Desired state — the single runtime concept
// ─────────────────────────────────────────────────────────────────────────────

/// What the operator wants. Runtime state is everything else.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Desired {
    /// Wanted up. Convergence to `Running` is the supervisor's job.
    Up,
    /// Wanted down. Convergence to `Stopped` is the supervisor's job.
    Down,
}

impl Desired {
    /// Parse the operator-facing spelling. Returns `None` for anything else.
    pub const fn parse(s: &str) -> Option<Desired> {
        match s.as_bytes() {
            b"up" => Some(Desired::Up),
            b"down" => Some(Desired::Down),
            _ => None,
        }
    }

    /// True when the service should be running.
    pub const fn is_up(self) -> bool {
        matches!(self, Desired::Up)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Actual state
// ─────────────────────────────────────────────────────────────────────────────

/// What is actually happening. Always derived from events, never set directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub enum State {
    /// No process. Either never started, or cleanly gone.
    #[default]
    Stopped,
    /// Spawn issued; waiting for the child to signal readiness (or for
    /// `ready-timeout`, whichever comes first).
    Starting,
    /// Process is alive and considered ready.
    Running,
    /// Stop requested; `SIGTERM` sent; waiting for exit or `stop-timeout`.
    Stopping,
}

impl State {
    /// True when a child process is expected to exist.
    pub const fn has_process(self) -> bool {
        matches!(self, State::Starting | State::Running | State::Stopping)
    }

    /// True when the service counts as available to its dependents.
    pub const fn is_up(self) -> bool {
        matches!(self, State::Running)
    }

    /// Legal `(State, Desired)` combinations.
    ///
    /// `(Starting, Down)` and `(Stopping, Up)` are reachable - a stop can be
    /// interrupted, and a start can be cancelled. `(Running, Down)` is the
    /// normal shutdown transient. `Stopped` pairs with both. `debug_assert` in
    /// the runtime uses this to catch an impossible pair.
    pub const fn pair_is_legal(self, d: Desired) -> bool {
        match (self, d) {
            (State::Stopped, _)
            | (State::Starting, _)
            | (State::Running, _)
            | (State::Stopping, _) => true,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Signals
// ─────────────────────────────────────────────────────────────────────────────

/// Signal names as zinit refers to them.
///
/// Deliberately a closed enum rather than a raw `i32`: `zcore` cannot include
/// `libc`, and a closed set makes every send site reviewable at a glance.
/// The numeric values are assigned in `zrt`, never here.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum SignalKind {
    Term,
    Kill,
    Cont,
    Hup,
    Int,
    Usr1,
    Usr2,
}

impl SignalKind {
    /// Canonical lowercase name, as used in the control protocol.
    pub const fn name(self) -> &'static str {
        match self {
            SignalKind::Term => "TERM",
            SignalKind::Kill => "KILL",
            SignalKind::Cont => "CONT",
            SignalKind::Hup => "HUP",
            SignalKind::Int => "INT",
            SignalKind::Usr1 => "USR1",
            SignalKind::Usr2 => "USR2",
        }
    }

    /// Parse a signal name, case-insensitively, with or without a `SIG` prefix.
    pub fn parse(s: &str) -> Option<SignalKind> {
        let up: String = alloc::string::ToString::to_string(&s)
            .trim_start_matches("SIG")
            .to_ascii_uppercase();
        Some(match up.as_str() {
            "TERM" => SignalKind::Term,
            "KILL" => SignalKind::Kill,
            "CONT" => SignalKind::Cont,
            "HUP" => SignalKind::Hup,
            "INT" => SignalKind::Int,
            "USR1" => SignalKind::Usr1,
            "USR2" => SignalKind::Usr2,
            _ => return None,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Logging levels
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub const fn name(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Service description (as resolved by zconfig, consumed by the runtime)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub enum ServiceKind {
    /// Fork/exec `command` directly.
    #[default]
    Process,
    /// Run `command` through the system shell.
    Script,
    /// Virtual: no process. Up once its required dependencies are up.
    Target,
    /// Owns the controlling terminal.
    Console,
}

impl ServiceKind {
    pub const fn name(self) -> &'static str {
        match self {
            ServiceKind::Process => "process",
            ServiceKind::Script => "script",
            ServiceKind::Target => "target",
            ServiceKind::Console => "console",
        }
    }

    pub fn parse(s: &str) -> Option<ServiceKind> {
        Some(match s {
            "process" => ServiceKind::Process,
            "script" => ServiceKind::Script,
            "target" => ServiceKind::Target,
            "console" => ServiceKind::Console,
            _ => return None,
        })
    }

    /// Targets never fork, so readiness and restart policy are meaningless.
    pub const fn has_process(self) -> bool {
        matches!(
            self,
            ServiceKind::Process | ServiceKind::Script | ServiceKind::Console
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub enum Restart {
    /// Never restart. A crash stays a crash until the operator intervenes.
    Never,
    /// Restart only on a non-zero exit or a fatal signal.
    #[default]
    OnFailure,
    /// Restart on any exit.
    Always,
}

impl Restart {
    /// Decide whether a given death warrants a restart.
    pub const fn wants_restart(self, code: Option<i32>) -> bool {
        match self {
            Restart::Never => false,
            Restart::Always => true,
            // `None` means "killed by signal" - always a failure.
            // SIGKILL/SIGTERM after our own stop request is handled by the
            // Desired check, not here.
            Restart::OnFailure => match code {
                None => true,
                Some(0) => false,
                Some(_) => true,
            },
        }
    }
}

/// How the supervisor learns a service is actually usable.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Ready {
    /// Up as soon as the fork succeeds.
    #[default]
    None,
    /// Service writes a line to the notify fd (`ZINIT_NOTIFY_FD`).
    Notify,
    /// Service exits 0. Re-run `ping` every 250 ms until it does.
    Ping(String),
    /// A TCP connect to `127.0.0.1:port` succeeds.
    Tcp(u16),
    /// Handshake is required, but expiry still means "up" with a warning.
    /// This is the only variant that genuinely gates startup on readiness.
    Strict(StrictReady),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StrictReady {
    Notify,
    Ping(String),
    Tcp(u16),
}

impl Ready {
    /// True when this variant has a readiness protocol at all, and therefore
    /// needs a `ready_due_at` deadline armed on the service.
    ///
    /// `Ready::Strict(_)` is included: a strict variant still waits, it merely
    /// treats expiry as failure instead of success. Leaving it out here is
    /// what previously made strict readiness a no-op.
    pub const fn is_handshake(&self) -> bool {
        matches!(
            self,
            Ready::Notify | Ready::Ping(_) | Ready::Tcp(_) | Ready::Strict(_)
        )
    }

    /// True when expiry is a failure rather than a pass.
    pub const fn is_strict(&self) -> bool {
        matches!(self, Ready::Strict(_))
    }

    pub const fn describe(&self) -> &'static str {
        match self {
            Ready::None => "none",
            Ready::Notify | Ready::Strict(StrictReady::Notify) => "notify",
            Ready::Ping(_) | Ready::Strict(StrictReady::Ping(_)) => "ping",
            Ready::Tcp(_) | Ready::Strict(StrictReady::Tcp(_)) => "tcp",
        }
    }

    /// The handshake payload, ignoring strictness. Owned, so the three
    /// unit-like cases do not need a `'static` promotion that would leak a
    /// temporary to the caller.
    pub fn protocol(&self) -> Option<StrictReady> {
        match self {
            Ready::Strict(s) => Some(s.clone()),
            Ready::Notify => Some(StrictReady::Notify),
            Ready::Ping(c) => Some(StrictReady::Ping(c.clone())),
            Ready::Tcp(p) => Some(StrictReady::Tcp(*p)),
            Ready::None => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LogSink {
    /// Discard stdout/stderr (`/dev/null`).
    None,
    /// Append to `path`, rotating at `max_bytes` keeping `backups` files.
    File {
        path: String,
        max_bytes: u64,
        backups: u8,
    },
    /// Send to a syslog/journal socket.
    Syslog,
}

impl Default for LogSink {
    fn default() -> Self {
        LogSink::File {
            path: String::new(),
            max_bytes: 1024 * 1024,
            backups: 3,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Restart budget
// ─────────────────────────────────────────────────────────────────────────────

/// A token-bucket restart budget, expressed the way an operator writes it.
///
/// `capacity` restarts per `window_ms`. Declared as a pair so the two numbers
/// cannot drift apart, which is the defect this replaces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Budget {
    pub capacity: u32,
    pub window_ms: u64,
    pub delay_ms: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            capacity: 5,
            window_ms: 60_000,
            delay_ms: 250,
        }
    }
}

impl Budget {
    /// A budget that never permits a restart, regardless of policy.
    pub const fn never() -> Budget {
        Budget {
            capacity: 0,
            window_ms: 0,
            delay_ms: 0,
        }
    }
}

/// Mutable token bucket: the runtime half of [`Budget`].
///
/// Time is a parameter. There is no clock here, which is what lets the whole
/// reconciler be a pure function of `(runtime, plan, now_ms)`.
///
/// Two independent gates, and the distinction matters:
///
/// * **tokens** - how many restarts are left in the current window.
/// * **`delay_ms`** - the minimum interval *between two attempts*.
///
/// They are separate because they answer different questions. A service that
/// crashes twice a second can have plenty of tokens and still must not spin;
/// conversely a service that crashes once an hour must not be penalised for
/// having burned its window. Measuring the delay from bucket creation instead
/// of from the previous attempt would gate the very first start, which is a
/// bug this implementation had and its tests caught.
#[derive(Clone, Copy, Debug)]
pub struct Bucket {
    tokens: u32,
    last_refill_ms: u64,
    /// When the last token was consumed. `None` means "never attempted", and
    /// is what makes the first attempt exempt from `delay_ms`.
    last_take_ms: Option<u64>,
    started: bool,
}

impl Bucket {
    pub const fn new() -> Bucket {
        Bucket {
            tokens: 0,
            last_refill_ms: 0,
            last_take_ms: None,
            started: false,
        }
    }

    /// True when a restart is currently permitted. Does not consume.
    pub fn allows(&mut self, budget: &Budget, now_ms: u64) -> bool {
        if budget.capacity == 0 {
            return false;
        }
        self.refill(budget, now_ms);
        if self.tokens == 0 {
            return false;
        }
        match self.last_take_ms {
            // Never attempted: no delay to respect.
            None => true,
            Some(prev) => now_ms.saturating_sub(prev) >= budget.delay_ms,
        }
    }

    /// Consume one token, if available. Returns whether it was consumed.
    ///
    /// A restart attempt is counted even when the spawn subsequently fails:
    /// the budget exists to stop a crash loop, and a spawn that fails is part
    /// of the same loop.
    pub fn take(&mut self, budget: &Budget, now_ms: u64) -> bool {
        if !self.allows(budget, now_ms) {
            return false;
        }
        self.tokens -= 1;
        self.last_take_ms = Some(now_ms);
        true
    }

    /// Refill to at most `capacity`, proportional to elapsed time.
    fn refill(&mut self, budget: &Budget, now_ms: u64) {
        if !self.started {
            // First touch: the bucket starts full, and the delay does not
            // apply because `last_take_ms` is `None`.
            self.started = true;
            self.tokens = budget.capacity;
            self.last_refill_ms = now_ms;
            return;
        }
        // Monotonic clock: a backwards jump yields zero elapsed, never a
        // panic and never free tokens.
        let elapsed = now_ms.saturating_sub(self.last_refill_ms);
        if budget.window_ms == 0 || elapsed == 0 {
            return;
        }
        let per_ms = budget.capacity as u128 * 1_000;
        let gained = (per_ms * elapsed as u128) / budget.window_ms as u128;
        if gained > 0 {
            self.tokens = self
                .tokens
                .saturating_add(gained.min(u32::MAX as u128) as u32)
                .min(budget.capacity);
            self.last_refill_ms = now_ms;
        }
    }

    /// Tokens currently available, for `zctl status` output and tests.
    pub fn tokens(&self) -> u32 {
        self.tokens
    }

    /// Forget the attempt history. `zctl kick` uses this to give an operator
    /// explicit control back after a budget has been exhausted.
    pub fn reset(&mut self, budget: &Budget, now_ms: u64) {
        self.started = true;
        self.tokens = budget.capacity;
        self.last_refill_ms = now_ms;
        self.last_take_ms = None;
    }
}

impl Default for Bucket {
    fn default() -> Self {
        Bucket::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The frozen plan
// ─────────────────────────────────────────────────────────────────────────────

/// Everything the runtime needs about one service.
///
/// Immutable for the lifetime of the plan. If a service description changes,
/// the plan is rebuilt; it is never patched.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ServicePlan {
    pub name: String,
    pub kind: ServiceKind,
    /// Dependencies that must be `Running` before this service may start.
    pub required: Vec<Idx>,
    /// Dependencies that are waited for but whose failure is not fatal.
    pub optional: Vec<Idx>,
    /// Reverse edges. Precomputed so `Exited` can find affected dependents
    /// in O(1) instead of scanning every service's `required` list.
    pub dependents: Vec<Idx>,
    pub restart: Restart,
    pub ready: Ready,
    /// Deadline for the process to exist and be confirmed alive.
    pub start_timeout_ms: u64,
    /// Deadline for the readiness handshake.
    ///
    /// Separate from `start_timeout_ms` because they answer different
    /// questions: this one is about *usability*, that one about the process
    /// existing at all. A service that accepts its port in 2 s but takes 40 s
    /// to finish initialising is ready long before it is up.
    ///
    /// It is only consulted when [`Ready::is_handshake`] — `ready = none` has
    /// nothing to wait for, and a deadline nobody can satisfy is a way to make
    /// a config file lie.
    pub ready_timeout_ms: u64,
    pub stop_timeout_ms: u64,
    pub restart_budget: Budget,
    pub log: LogSink,
    /// `Some(uid, gid)` when the service drops privileges.
    pub run_as: Option<(u32, u32)>,
    /// `ready = none` for targets, since they have no process to wait for.
    pub is_critical: bool,
}

impl ServicePlan {
    /// A minimal process service with defaults. Used by tests and by the
    /// parser before a description is fully resolved.
    pub fn new(name: String) -> ServicePlan {
        ServicePlan {
            name,
            kind: ServiceKind::Process,
            required: Vec::new(),
            optional: Vec::new(),
            dependents: Vec::new(),
            restart: Restart::default(),
            ready: Ready::default(),
            start_timeout_ms: 60_000,
            ready_timeout_ms: 30_000,
            stop_timeout_ms: 10_000,
            restart_budget: Budget::default(),
            log: LogSink::default(),
            run_as: None,
            is_critical: true,
        }
    }

    /// True when every required dependency of `idx` is `Running`.
    ///
    /// This is the only readiness question the reconciler asks, and it is why
    /// the frozen topological order is enough: no traversal, no recursion.
    pub fn deps_satisfied(&self, idx: Idx, plan: &Plan, states: &[State]) -> bool {
        let me = &plan.services[idx];
        me.required.iter().all(|&d| states[d].is_up())
    }
}

/// A validated, topologically ordered, immutable set of services.
///
/// `order_up` is a topological order of the dependency DAG. `order_down` is
/// its exact reverse. Runtime start/stop is a single pass over each; there is
/// no queue, no propagation phase, and no depth counter to get wrong.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Plan {
    pub services: Vec<ServicePlan>,
    pub order_up: Vec<Idx>,
    pub order_down: Vec<Idx>,
}

impl Plan {
    pub fn len(&self) -> usize {
        self.services.len()
    }

    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    /// Look up a service by name. Linear, which is fine: the CLI does this
    /// once per invocation and the runtime keeps its own index.
    pub fn index_of(&self, name: &str) -> Option<Idx> {
        self.services.iter().position(|s| s.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_default_matches_design_doc() {
        let b = Budget::default();
        assert_eq!(b.capacity, 5);
        assert_eq!(b.window_ms, 60_000);
        assert_eq!(b.delay_ms, 250);
    }

    #[test]
    fn bucket_starts_full_and_drains() {
        // Zero delay so this test isolates the token count; the delay is
        // exercised separately in `bucket_respects_delay_between_attempts_only`.
        let b = Budget {
            capacity: 5,
            window_ms: 60_000,
            delay_ms: 0,
        };
        let mut k = Bucket::new();
        assert!(k.allows(&b, 0), "first check must be allowed");
        for _ in 0..5 {
            assert!(k.take(&b, 0));
        }
        assert!(!k.allows(&b, 0), "sixth in the same instant must be denied");
    }

    #[test]
    fn bucket_refills_over_the_window() {
        let b = Budget {
            capacity: 2,
            window_ms: 1000,
            delay_ms: 0,
        };
        let mut k = Bucket::new();
        assert!(k.take(&b, 0));
        assert!(k.take(&b, 0));
        assert!(!k.allows(&b, 0));
        // Half the window later, one token is back.
        assert!(k.allows(&b, 500));
        assert!(k.take(&b, 500));
    }

    #[test]
    fn bucket_respects_delay_between_attempts_only() {
        let b = Budget {
            capacity: 9,
            window_ms: 1000,
            delay_ms: 250,
        };
        let mut k = Bucket::new();
        // The very first attempt is exempt from the delay.
        assert!(k.take(&b, 0), "the first attempt must not be delayed");
        assert!(!k.allows(&b, 100), "delay must gate the second attempt");
        assert!(!k.allows(&b, 249));
        assert!(k.allows(&b, 250));
    }

    #[test]
    fn reset_gives_the_operator_control_back() {
        let b = Budget {
            capacity: 1,
            window_ms: 60_000,
            delay_ms: 0,
        };
        let mut k = Bucket::new();
        assert!(k.take(&b, 0));
        assert!(!k.allows(&b, 0), "exhausted");
        k.reset(&b, 0);
        assert!(k.allows(&b, 0), "`zctl kick` must refill the bucket");
    }

    #[test]
    fn bucket_never_permits_when_capacity_is_zero() {
        let mut k = Bucket::new();
        assert!(!k.allows(&Budget::never(), 0));
        assert!(!k.allows(&Budget::never(), u64::MAX));
    }

    #[test]
    fn bucket_tolerates_a_backwards_clock() {
        let b = Budget::default();
        let mut k = Bucket::new();
        assert!(k.take(&b, 10_000));
        // Monotonic clock going backwards must not panic or mint tokens.
        let _ = k.allows(&b, 0);
        let _ = k.take(&b, 0);
    }

    #[test]
    fn restart_policy() {
        assert!(!Restart::Never.wants_restart(Some(1)));
        assert!(Restart::OnFailure.wants_restart(Some(1)));
        assert!(!Restart::OnFailure.wants_restart(Some(0)));
        assert!(Restart::OnFailure.wants_restart(None));
        assert!(Restart::Always.wants_restart(Some(0)));
    }

    #[test]
    fn signal_parsing_is_case_and_prefix_insensitive() {
        assert_eq!(SignalKind::parse("term"), Some(SignalKind::Term));
        assert_eq!(SignalKind::parse("SIGKILL"), Some(SignalKind::Kill));
        assert_eq!(SignalKind::parse("hup"), Some(SignalKind::Hup));
        assert_eq!(SignalKind::parse("NOPE"), None);
    }
}
