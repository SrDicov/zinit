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
}

// ─────────────────────────────────────────────────────────────────────────────
// Signals
// ─────────────────────────────────────────────────────────────────────────────

/// Signal names as zinit refers to them.
///
/// Deliberately a closed enum rather than a raw `i32`: `zcore` cannot include
/// `libc`, and a closed set makes every send site reviewable at a glance.
/// The numeric values are assigned in `zrt`, never here. Only the two signals
/// the supervisor itself issues are modelled; anything else an operator wants
/// is not a thing this core sends.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum SignalKind {
    Term,
    Kill,
}

// ─────────────────────────────────────────────────────────────────────────────
// Logging levels
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
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
    /// Run once to a clean exit. Success is terminal: the service is marked
    /// `completed`, dependents treat it as satisfied, and only an explicit
    /// `kick` (or `start`) runs it again. A non-zero exit follows the restart
    /// policy like any other crash — completion beats the policy, failure
    /// obeys it.
    Oneshot,
    /// Legacy double-forking daemon. The direct child is expected to exit
    /// promptly; the real pid is read from `pid_file` and adopted. An adopted
    /// process is always signalled by pid, never by group: the supervisor
    /// cannot prove the daemon made its own group, and signalling a group it
    /// never owned is how a supervisor kills itself.
    Forking,
}

impl ServiceKind {
    pub const fn name(self) -> &'static str {
        match self {
            ServiceKind::Process => "process",
            ServiceKind::Script => "script",
            ServiceKind::Target => "target",
            ServiceKind::Console => "console",
            ServiceKind::Oneshot => "oneshot",
            ServiceKind::Forking => "forking",
        }
    }

    /// Targets never fork, so readiness and restart policy are meaningless.
    pub const fn has_process(self) -> bool {
        matches!(
            self,
            ServiceKind::Process
                | ServiceKind::Script
                | ServiceKind::Console
                | ServiceKind::Oneshot
                | ServiceKind::Forking
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

/// Where a service listens before it starts (socket activation).
///
/// The supervisor binds these once, holds them across restarts, and passes
/// the descriptors down at every spawn through `$LISTEN_FDS` /
/// `$LISTEN_PID` / `$LISTEN_FDNAMES`, starting at fd 3. `TcpPort` binds
/// 127.0.0.1 only: the safe default, and the same loopback the `tcp`
/// readiness probe dials, so `ready = tcp:<port>` and `listen = tcp:<port>`
/// agree with each other out of the box. A public bind needs an explicit
/// address grammar that does not exist yet — the parser, not this type, is
/// where that refusal lives.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub enum ListenSpec {
    /// TCP on loopback, this port (1-65535; 0 is refused at parse).
    TcpPort(u16),
    /// Unix stream socket at this path.
    UnixPath(String),
}

/// One bound socket plus its logical name for `$LISTEN_FDNAMES`.
///
/// The name defaults to the spec text when the directive omits it; it may
/// not contain whitespace or `:` (the names travel colon-joined), which the
/// parser enforces.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct ListenAddr {
    /// Where to listen.
    pub spec: ListenSpec,
    /// Logical name for `$LISTEN_FDNAMES`.
    pub name: String,
}

/// What a seccomp violation does to the service.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum SeccompAction {
    /// Kill the process (`SECCOMP_RET_KILL_PROCESS`). Fail-closed.
    Enforce,
    /// Fail the call with `EPERM` (`SECCOMP_RET_ERRNO`). The service limps on.
    Errno,
}

/// A seccomp-bpf filter request: the action plus the user-allowed syscall
/// names on top of the always-allowed baseline. Names resolve to numbers at
/// spawn time, on a verified per-arch table — an unresolvable name refuses
/// the spawn rather than shipping a filter with a hole.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct SeccompPolicy {
    /// What a violation does.
    pub action: SeccompAction,
    /// Extra allowed syscall names beyond the baseline.
    pub allow: Vec<String>,
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

    /// True when no token is available right now, after refilling.
    ///
    /// This is what distinguishes "the budget is empty" from "the delay is
    /// still spacing attempts": `take` fails in both cases, but only an
    /// empty bucket is exhaustion worth announcing. A delay-gated retry is
    /// routine spacing — the reconciler stays silent and a later pass spends
    /// the token. (A real machine caught the conflation: with the default
    /// 250 ms delay every restart cycle announced "budget empty" while
    /// holding tokens.)
    pub fn exhausted(&mut self, budget: &Budget, now_ms: u64) -> bool {
        if budget.capacity == 0 {
            return true;
        }
        self.refill(budget, now_ms);
        self.tokens == 0
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
        // Tokens earned, in whole tokens: a 3-per-60s budget earns one token
        // every 20 s, and nothing at all in the first 19 999 ms. No scaling
        // factor: `capacity * elapsed / window` is already in tokens, and an
        // extra `* 1_000` here once refilled 1000x too fast — a crash loop
        // the budget was built to stop sailed straight through it, caught
        // only by a real machine (a fixed-clock unit test never refills).
        let gained = (budget.capacity as u128 * elapsed as u128) / budget.window_ms as u128;
        if gained > 0 {
            self.tokens = self
                .tokens
                .saturating_add(gained.min(u32::MAX as u128) as u32)
                .min(budget.capacity);
            self.last_refill_ms = now_ms;
        }
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
    /// Path of the pid file a `Forking` daemon writes: its real pid after the
    /// double fork, read by the supervisor and adopted. `None` for every
    /// other kind — the parser refuses `pid-file` elsewhere rather than
    /// storing a path nobody will read.
    pub pid_file: Option<String>,
    /// Watchdog budget in seconds: the service must write `WATCHDOG=1` to its
    /// notify fd at least this often, or it is stopped (and, `Desired`
    /// unchanged, started again). `None` disables the watchdog. Only
    /// meaningful with `ready = notify`; the parser refuses the combination
    /// that could never be fed.
    pub watchdog_sec: Option<u64>,
    /// Sockets to pre-bind and pass down (socket activation). Empty means
    /// none; `listen` is the only accumulating directive besides `env`, so
    /// several lines — and drop-ins — each add one socket.
    pub listens: Vec<ListenAddr>,
    /// Seccomp-bpf confinement (`syscall-filter` + `syscall-allow`). `None`
    /// is unconfined. Linux-only at spawn; elsewhere a configured filter
    /// refuses the spawn rather than running it unconfined and quiet.
    pub seccomp: Option<SeccompPolicy>,
    /// Linux capabilities to drop from the bounding set, canonical names.
    /// Empty is undropped. Linux-only at spawn, like `seccomp`.
    pub drop_caps: Vec<String>,
    /// Start on the first connection to a held socket. The supervisor
    /// watches the bound listeners while the service is down; traffic sets
    /// it up like an operator `start` (budget respected, no fork bomb).
    pub on_demand: bool,
    /// Start at boot. `false` (`enabled = no`) leaves `Desired` down until
    /// an operator says otherwise — the boot default, not a lock: `zctl
    /// start` still starts it. Reloads never flip a live desire; only a
    /// fresh boot (and `kick`-style explicit intent) consults this.
    pub enabled: bool,
}

impl ServicePlan {
    /// A minimal process service with defaults. Used by tests and by the
    /// parser before a description is fully resolved.
    pub fn new(name: String) -> ServicePlan {
        ServicePlan {
            name,
            kind: ServiceKind::Process,
            required: Vec::new(),
            restart: Restart::default(),
            ready: Ready::default(),
            start_timeout_ms: 60_000,
            ready_timeout_ms: 30_000,
            stop_timeout_ms: 10_000,
            restart_budget: Budget::default(),
            log: LogSink::default(),
            run_as: None,
            pid_file: None,
            watchdog_sec: None,
            listens: Vec::new(),
            seccomp: None,
            drop_caps: Vec::new(),
            enabled: true,
            on_demand: false,
        }
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
    fn bucket_refill_rate_is_in_tokens_not_millitokens() {
        // A realistic `3 restarts per 60s`: one second later nothing has been
        // earned (3*1000/60000 rounds to zero). A `* 1_000` scaling slip here
        // once refilled 50 tokens a second — the crash loop the budget exists
        // to stop ran straight through it, and only a real machine noticed:
        // every fixed-clock test refills nothing and passed either way.
        let b = Budget {
            capacity: 3,
            window_ms: 60_000,
            delay_ms: 0,
        };
        let mut k = Bucket::new();
        for _ in 0..3 {
            assert!(k.take(&b, 0));
        }
        assert!(!k.allows(&b, 0), "drained");
        assert!(!k.allows(&b, 1_000), "one second earns no token");
        assert!(!k.allows(&b, 19_999), "nineteen seconds earn no token");
        assert!(k.allows(&b, 20_000), "twenty seconds earn one token");
        assert!(k.take(&b, 20_000));
        assert!(!k.allows(&b, 20_000), "one token means one restart");
        // A full window with no attempts refills the whole bucket.
        assert!(k.take(&b, 80_000));
        assert!(k.take(&b, 80_000));
        assert!(k.take(&b, 80_000));
        assert!(!k.allows(&b, 80_000));
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
}
