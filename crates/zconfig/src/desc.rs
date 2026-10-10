//! The **unresolved** service description: everything a `key = value` file can
//! say, and nothing that requires knowing about the other services.
//!
//! The split with `zcore::ServicePlan` is the whole point of this module. A
//! `ServicePlan` stores [`zcore::Idx`] and a frozen topological order, which is
//! perfect for the reconciler and useless for parsing one file in isolation.
//! A `ServiceDesc` is the mirror image: dependency **names**, no indices, no
//! graph, no ordering. `graph.rs` is the only place allowed to turn one into
//! the other, so "this name does not exist" is reported exactly once, with the
//! right span, instead of turning into an infinite recursion or a half-built
//! plan.
//!
//! Nothing here reads a file, reads a clock, or performs I/O. The caller passes
//! the text; this file only decides what the text *means*.
//!
//! ```text
//!    /etc/zinit/services.d/sshd.conf
//!            │  read by zsup (the only crate that touches the filesystem)
//!            ▼
//!   &str ──parser::parse_service──▶ ServiceDesc ◀── THIS FILE
//!                                      │
//!                                      ▼ graph::link
//!                              zcore::Plan
//! ```
//!
//! # What is deliberately *not* here
//!
//! * **No budget numbers.** [`crate::value::default_budget`] is the single answer to
//!   "what is a restart budget when nobody said", and
//!   [`crate::value::RestartSpec::default`] spends it. An absent `restart` line and an
//!   explicit `restart = on-failure` therefore cannot drift apart, because
//!   neither one re-states a number that `value.rs` already owns.
//! * **No notes.** A warning is a [`crate::diagnostic::Diagnostic`] and travels
//!   in a [`crate::diagnostic::DiagnosticBag`], not welded onto the data. A
//!   description of a service has no opinion about how loudly it was parsed.
//! * **No field for an unresolved `user =`.** `zconfig` has no `/etc/passwd`,
//!   so `user = www-data` is a legal description it cannot resolve. The
//!   tempting shape is an `Option<String>` beside the numeric
//!   [`run_as`](ServiceDesc::run_as) — and that shape is a privilege
//!   escalation waiting to happen: every reader has to remember to check it,
//!   and the one that forgets starts the service as **root**. An `Option` that
//!   "someone downstream will check" is checked by nobody, every time.
//!
//!   So there is no such field. `parser.rs` **rejects** a named `user =` with
//!   a message that says what to write instead. That is a worse user
//!   experience than accepting it and is the correct trade: a loud refusal
//!   costs the operator one edit, and a silently-ignored privilege drop costs
//!   them a root shell. The alternative — carrying the name and refusing to
//!   build a plan while it is set — is only safe if the refusal is
//!   *enforced*, and the field would have to be checked in every place that
//!   reads a description, which is precisely the "one reader forgets" bug
//!   again. Note also that `ServiceDesc` is `Clone` and public: a field
//!   carrying a half-resolved decision is one `..Default::default()` away from
//!   being dropped silently.
//! * **`ready_timeout_ms` is a real field.** It is easy to assume this is a
//!   duplicate of `start_timeout_ms` and that the directive should be
//!   refused. It is not: `zcore` arms the two deadlines independently
//!   (`arm_start_deadlines`), and folding them would make a documented
//!   directive unobservable. See [`DEFAULT_READY_TIMEOUT_MS`].

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use zcore::{Budget, ListenAddr, Restart, SeccompAction, ServiceKind};

use crate::parser::Directive;
use crate::value::{LogSpec, ReadySpec, ReadySpecKind, RestartSpec, RunAs};

/// Default `stop-timeout`: 10 s, then `SIGKILL`.
///
/// Ten seconds is long enough for a daemon to close listeners and flush, and
/// short enough that a reboot is not visibly stuck. It is the value DESIGN.md
/// §5 mandates, so the default and the documented value cannot drift apart.
///
/// Lives here rather than in `value.rs` because nothing else parses it:
/// `value.rs` owns the *grammar* of a duration, this module owns the *policy*
/// of what a service gets when the directive is absent.
pub const DEFAULT_STOP_TIMEOUT_MS: u64 = 10_000;

/// Default `start-timeout`: 60 s.
///
/// Generous on purpose: a timeout that fires while a legitimate service is
/// still binding sockets produces a restart loop, and a restart loop is a worse
/// failure than a slow start.
pub const DEFAULT_START_TIMEOUT_MS: u64 = 60_000;

/// Default `ready-timeout`: 30 s.
///
/// Shorter than [`DEFAULT_START_TIMEOUT_MS`] on purpose, and separate from it
/// for a reason: the two answer different questions. `start-timeout` is about
/// the process *existing*; `ready-timeout` is about it being *usable*. A
/// service that opens its port in 2 s and finishes initialising in 40 s is
/// ready long before it is up, and folding the two together makes the
/// documented `ready-timeout` directive unobservable.
///
/// `zcore` arms the two deadlines independently (`arm_start_deadlines`), and
/// only consults this one when there is a handshake to wait for: a deadline
/// nobody can satisfy is a way to make a config file lie.
pub const DEFAULT_READY_TIMEOUT_MS: u64 = 30_000;

/// Longest service name accepted, in bytes.
///
/// Names end up as path components under `/etc/zinit/services.d` and as object
/// names in the supervisor's API, so the cap is `NAME_MAX` on every filesystem
/// this is likely to meet. A longer one is refused rather than truncated: a
/// truncated name is a *different* service, and two different files silently
/// becoming one is precisely the class of bug this module exists to prevent.
pub const MAX_SERVICE_NAME_BYTES: usize = 255;

// ═══════════════════════════════════════════════════════════════════════════
// Names
// ═══════════════════════════════════════════════════════════════════════════

/// Why a service name is not usable.
///
/// Every variant here is a name that would be **ambiguous or unsafe** as a path
/// component or as an API object name. The list is short on purpose: a name
/// that is merely ugly is allowed, because a rule that rejects names people
/// want teaches them to work around the tool instead of fixing the name.
///
/// # Why this is a hard error and not a sanitiser
///
/// The obvious alternative is to rewrite `a/b` to `a-b` and carry on. That
/// was dinit's mistake and it is worth being explicit about the mechanism:
/// `dinit-check`'s `validate_service_name` had an unconditional
/// `return true;` *above* its validation loop, which made every check below it
/// dead code. Any name the loop would have rejected was accepted, including
/// `../../etc/passwd`, and the service was then created at a path the operator
/// did not choose.
///
/// The lesson is not "add a test". It is that a validator with a
/// always-succeed path is indistinguishable from no validator, and the failure
/// is a traversal. So:
///
/// * the check is a single `for` over the name with **no early `return
///   true`** — the only `return`s are failures, and the success case is the
///   fall-through at the end of the loop (see [`validate_service_name`]);
/// * the name is never rewritten, only accepted or rejected;
/// * [`validate_service_name`] is public, so the crate that opens the file can
///   refuse a name *before* it has touched the filesystem at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NameError {
    /// The empty string. Not a name, and the one that would otherwise produce
    /// a file literally called `""`.
    Empty,
    /// Longer than [`MAX_SERVICE_NAME_BYTES`].
    TooLong {
        /// The length that was rejected, in bytes.
        len: usize,
    },
    /// Contains `/`, which would make the name a path rather than a component
    /// of one. `\` is rejected for the same reason: a name that is safe on one
    /// platform and a traversal on another is a name that will be pasted.
    ContainsPathSeparator,
    /// Contains `..`. Refused **anywhere** in the name, not just as a whole
    /// name and not just at the start: the leading-dot rule below already
    /// rejects a name that *starts* with a dot, so an interior `..` is the only
    /// case this catches, and "the name looks like a path fragment" is a
    /// mistake worth refusing rather than a name worth keeping.
    ContainsDotDot,
    /// Starts with `.`. Rejected as a class, not as a spelling: `.` and `..`
    /// are traversals and `.hidden` is a dotfile that most `cp -r` and most
    /// file managers skip, so a service whose description happens to be
    /// dot-prefixed is a service that mysteriously never starts.
    StartsWithDot,
    /// Starts with `@`. Reserved by the systemd template convention and by
    /// nothing in this format, so it is almost always a paste from elsewhere.
    StartsWithAt,
    /// Contains whitespace. A name is a single token; anything with a space in
    /// it cannot be typed in the CLI, the API or a `systemctl`-alike without
    /// quoting, and quoting is how it gets forgotten.
    Whitespace(char),
    /// Contains a control character or `NUL`. `NUL` in particular cannot be
    /// passed to `execve` at all, so accepting it here would produce a
    /// description that is valid to `zcheck` and impossible to run.
    Control(char),
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameError::Empty => f.write_str("a service name cannot be empty"),
            NameError::TooLong { len } => write!(
                f,
                "the service name is {len} bytes, over the {MAX_SERVICE_NAME_BYTES} byte limit"
            ),
            NameError::ContainsPathSeparator => f.write_str(
                "a service name cannot contain `/` or `\\`: it is a file name, not a path",
            ),
            NameError::ContainsDotDot => f.write_str("a service name cannot contain `..`"),
            NameError::StartsWithDot => f.write_str(
                "a service name cannot start with `.`: `.` and `..` are path \
                 traversals and a dot-prefixed file is skipped by ordinary tools",
            ),
            NameError::StartsWithAt => f.write_str("a service name cannot start with `@`"),
            NameError::Whitespace(c) => write!(
                f,
                "a service name cannot contain the whitespace `{c:?}`: it is a single token"
            ),
            NameError::Control(c) => write!(
                f,
                "a service name cannot contain the control character U+{:04X}, and `NUL` \
                 cannot be passed to `execve` at all",
                *c as u32
            ),
        }
    }
}

/// Validate a service name. `Ok(())` iff the name is usable as a path
/// component and as an API object name.
///
/// The shape of this function is the point, so it is worth stating what it
/// deliberately does **not** do:
///
/// * there is no `return true` — the only `return`s below are failures, and
///   success is the `Ok(())` after the loop. A validator that can return
///   "valid" before it has looked at the name is a validator that validates
///   nothing, and the only kind of bug that matters here is silent.
/// * the name is never modified. A sanitiser that rewrites `a/b` into `a-b`
///   creates two descriptions of one intent and reports neither; refusing to
///   name something is the honest answer.
/// * every rejection is a *character* rejection, not a pattern match, so a
///   name nobody thought of is still checked.
pub fn validate_service_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.len() > MAX_SERVICE_NAME_BYTES {
        return Err(NameError::TooLong { len: name.len() });
    }

    // The loop is the validator, and it is the *only* place a name is judged.
    // It covers the **first** character too, which is not a detail: an earlier
    // version pulled the first character out to test it against `.` and `@`,
    // and the per-character rules below therefore never saw it — so
    // `/absolute` and `\absolute` were accepted, and a service name that is
    // a path is exactly the traversal this function exists to prevent. There
    // is no special case for the first character because a special case is
    // where a rule gets forgotten.
    //
    // One pass, one `char` at a time, no slicing: `prev` carries the previous
    // character so `..` is detected without rescanning the string, and no
    // index is ever computed — which is what keeps this correct for a name in
    // any script.
    let mut prev: Option<char> = None;
    for (i, ch) in name.chars().enumerate() {
        if i == 0 {
            if ch == '.' {
                return Err(NameError::StartsWithDot);
            }
            if ch == '@' {
                return Err(NameError::StartsWithAt);
            }
        }
        if ch == '/' || ch == '\\' {
            return Err(NameError::ContainsPathSeparator);
        }
        if ch == '.' && prev == Some('.') {
            return Err(NameError::ContainsDotDot);
        }
        // Control before whitespace: `\n` and `\t` are both, and a name
        // containing one is a broken file rather than a badly-spelled token.
        if ch.is_control() {
            return Err(NameError::Control(ch));
        }
        if ch.is_whitespace() {
            return Err(NameError::Whitespace(ch));
        }
        prev = Some(ch);
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// ServiceDesc
// ═══════════════════════════════════════════════════════════════════════════

/// Everything one service file said, with dependencies left as names.
///
/// [`Default`] is **not** "an empty service": it is a plausible service that
/// happens to declare nothing yet. The parser starts from it and fills fields
/// in, which keeps single-valued directives out of `Option` — a repeated
/// directive is then a "you already set this" error instead of a silent
/// last-one-wins.
///
/// # The dependency lists
///
/// [`depends_required`](Self::depends_required) and
/// [`depends_optional`](Self::depends_optional) are two `Vec<String>` and not a
/// struct with two fields in it. The struct version reads better and is one
/// more type that can disagree with itself about what "empty" means; the two
/// vectors are what `zcore::ServicePlan` has, what `graph.rs` walks, and what a
/// diagnostic that has to quote the list actually wants.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ServiceDesc {
    /// Service name, as resolved by the caller from the file name, and
    /// validated by [`validate_service_name`] before it got here.
    pub name: String,
    /// `process` unless a `type =` directive says otherwise.
    pub kind: ServiceKind,
    /// Command line, exactly as written. Empty means "the linker decides",
    /// which is legal only for [`ServiceKind::Target`].
    pub command: String,
    /// Names that must be `Running` before this service may start.
    pub depends_required: Vec<String>,
    /// Names that are waited for opportunistically. A missing one is a warning
    /// in `graph.rs`, not an error.
    pub depends_optional: Vec<String>,
    /// Restart policy, its budget and its delay.
    ///
    /// One field, not three: DESIGN.md §4.3 keeps the budget as a pair
    /// precisely so the two numbers cannot drift apart, and splitting the delay
    /// out into a `ServiceDesc` field would put it back in a place where it can.
    pub restart: RestartSpec,
    /// Readiness strategy.
    pub ready: ReadySpec,
    /// How long to wait for a clean stop before `SIGKILL`.
    pub stop_timeout_ms: u64,
    /// How long to keep probing the readiness handshake before declaring the
    /// service `Running` anyway. See [`DEFAULT_READY_TIMEOUT_MS`].
    pub ready_timeout_ms: u64,
    /// How long to wait for the process to reach `Running` before escalating —
    /// and, because `zcore` derives the readiness deadline from it, how long to
    /// wait for a readiness handshake before declaring the service up anyway.
    pub start_timeout_ms: u64,
    /// `KEY=VALUE` pairs, in declaration order, duplicates preserved.
    pub env: Vec<(String, String)>,
    /// UID/GID, present only when the file spelled both out numerically.
    ///
    /// Always `Some` or always absent: a `user =` naming an account is rejected
    /// rather than dropped (see the module docs). There is no third state in
    /// which the operator asked for a privilege drop and this crate silently
    /// ran the service as root.
    /// The identity to run as, when it was written numerically.
    ///
    /// At most one of `run_as` and `unresolved_run_as` is set.
    pub run_as: Option<RunAs>,
    /// What `user =` said, verbatim, when it named rather than numbered.
    ///
    /// `zconfig` has no `/etc/passwd`, so it cannot turn `mysql` into a uid,
    /// and refusing the spelling would push operators toward `user = 0` or
    /// toward deleting the line - both worse than not having the directive.
    ///
    /// So the name is kept, unresolved, and **the plan cannot be built while it
    /// is set**: [`build_plan`](crate::plan::build_plan) returns
    /// `GraphError::UnresolvedIdentity`. The runtime resolves the names and
    /// builds again. The enforcement lives there and nowhere else, which is
    /// the only way this promise is worth making.
    pub unresolved_run_as: Option<String>,
    /// Where stdout and stderr go.
    pub log: LogSpec,
    /// Whether a failure of this service should take the system down.
    pub is_critical: bool,
    /// Slice name under `zinit.slice`, or `None` for "no limit imposed".
    pub cgroup: Option<String>,
    /// Resource limits as `("nofile" | "nproc" | "as", value)`.
    pub rlimits: Vec<(String, u64)>,
    /// Path of the file this came from, for diagnostics. Set by the caller.
    pub source: String,
    /// 1-indexed line where this description starts, for diagnostics.
    pub source_line: u32,
    /// True when `command` or an `env` value contains a `${...}` reference.
    ///
    /// The reference is *not* resolved here. `zconfig` has no environment and
    /// no filesystem, so expanding `${VAR}` would mean silently inventing a
    /// value. The flag exists so `zrt` can expand against the real
    /// environment, and so `zcheck` can refuse to certify a description that
    /// depends on something it cannot see.
    pub needs_env_expansion: bool,
    /// Directives this file set explicitly, in first-seen order.
    ///
    /// The parser records every successfully applied directive here. A
    /// freshly defaulted description has none set. Drop-in merging consults
    /// this list: only fields the overlay file *said* override the base —
    /// anything else would let a default the parser filled into the overlay
    /// silently clobber an explicit value in the base, which is exactly the
    /// "which one won" question the duplicate rule (`E005`) exists to prevent
    /// *within* one file.
    pub explicit: Vec<Directive>,
    /// Path of the pid file, for `type = forking` only. The parser refuses
    /// the directive on any other kind rather than storing a path nobody
    /// will read.
    pub pid_file: Option<String>,
    /// Controlling terminal path, for `type = console` only. The parser
    /// refuses the directive on any other kind (same shape as `pid_file`),
    /// and refuses relative paths: terminal resolution against an invisible
    /// working directory would open somewhere else.
    pub tty: Option<String>,
    /// Watchdog budget in whole seconds. The parser refuses it without
    /// `ready = notify` (pings arrive on the notify fd) and refuses zero
    /// (an instant kill timer is never what was meant).
    pub watchdog_sec: Option<u64>,
    /// Sockets to pre-bind, in declaration order, duplicates kept. The only
    /// accumulating directive besides `env`: each line — and each drop-in —
    /// adds one socket.
    pub listens: Vec<ListenAddr>,
    /// Linux capabilities to drop from the bounding set, canonical
    /// lowercase names. Merged as a union across layers: dropping twice is
    /// idempotent, and a layer can only narrow, never widen.
    pub drop_caps: Vec<String>,
    /// What a seccomp violation does, when confinement is configured. `None`
    /// is unconfined — including an explicit `syscall-filter = off`, which
    /// still records presence so a drop-in can lift a base's filter.
    pub syscall_filter: Option<SeccompAction>,
    /// Extra allowed syscall names beyond the always-allowed baseline.
    /// Merged as a union across layers, like capabilities.
    pub syscall_allow: Vec<String>,
}

impl Default for ServiceDesc {
    /// A plausible process service that has declared nothing.
    ///
    /// Every default is a call into the module that owns it —
    /// [`crate::value::RestartSpec::default`] for the restart policy and budget,
    /// [`crate::value::LogSpec::default`] for the sink. No number is re-typed here,
    /// because a number written in two places is a number that will be wrong in
    /// one of them.
    ///
    /// The one default that is *not* delegated is [`ready`](Self::ready), which
    /// is `notify` and not [`crate::value::ReadySpec::default`]'s `none`. DESIGN.md §5
    /// specifies `notify` as the format's default, and `ReadySpec::default` is
    /// the type's zero value rather than the format's promise; the two are
    /// different questions and only one of them is this struct's to answer.
    fn default() -> ServiceDesc {
        ServiceDesc {
            name: String::new(),
            kind: ServiceKind::Process,
            command: String::new(),
            depends_required: Vec::new(),
            depends_optional: Vec::new(),
            restart: RestartSpec::default(),
            ready: ReadySpec {
                kind: ReadySpecKind::Notify,
                arg: String::new(),
            },
            ready_timeout_ms: DEFAULT_READY_TIMEOUT_MS,
            stop_timeout_ms: DEFAULT_STOP_TIMEOUT_MS,
            start_timeout_ms: DEFAULT_START_TIMEOUT_MS,
            env: Vec::new(),
            run_as: None,
            unresolved_run_as: None,
            // `LogSpec::default()` is `file:` with an empty path, which `zrt`
            // fills in with `/var/log/zinit/<svc>.log`. zconfig cannot: the
            // service name is known here, but writing it down would freeze a
            // path decision into the parser.
            log: LogSpec::default(),
            is_critical: true,
            cgroup: None,
            rlimits: Vec::new(),
            source: String::new(),
            source_line: 0,
            needs_env_expansion: false,
            explicit: Vec::new(),
            pid_file: None,
            tty: None,
            watchdog_sec: None,
            listens: Vec::new(),
            drop_caps: Vec::new(),
            syscall_filter: None,
            syscall_allow: Vec::new(),
        }
    }
}

impl ServiceDesc {
    /// A description for `name` with every default in place.
    ///
    /// Does **not** validate the name: this constructor is infallible by design
    /// so that `graph.rs` and `plan.rs` can build a description in their tests
    /// without a parser in the way. Anything that produces a name from outside
    /// — a file name, an API argument — goes through
    /// [`ServiceDesc::try_new`] or [`validate_service_name`].
    pub fn new(name: impl Into<String>) -> ServiceDesc {
        ServiceDesc {
            name: name.into(),
            ..ServiceDesc::default()
        }
    }

    /// Like [`ServiceDesc::new`], but refuses an unusable name.
    ///
    /// This is the constructor the parser uses, and the reason it exists is
    /// that "the name is checked" should be a property of the *type* that
    /// carries the name rather than a line somebody remembered to write in
    /// `zsup`.
    pub fn try_new(name: impl Into<String>) -> Result<ServiceDesc, NameError> {
        let name = name.into();
        validate_service_name(&name)?;
        Ok(ServiceDesc {
            name,
            ..ServiceDesc::default()
        })
    }

    /// Attach the origin of this description, for diagnostics.
    pub fn with_source(mut self, source: impl Into<String>, line: u32) -> ServiceDesc {
        self.source = source.into();
        self.source_line = line;
        self
    }

    /// Record that the file being parsed set `directive`.
    ///
    /// Called once per successfully applied directive by the parser, which is
    /// the only writer. Readers (the drop-in merge below) only ever query.
    pub fn mark_explicit(&mut self, directive: Directive) {
        if !self.explicit.contains(&directive) {
            self.explicit.push(directive);
        }
    }

    /// True when the file being parsed set `directive` explicitly, rather
    /// than the parser filling in a default for it.
    pub fn is_explicit(&self, directive: Directive) -> bool {
        self.explicit.contains(&directive)
    }

    /// Merge a drop-in description over this base one, in place.
    ///
    /// Fields the drop-in set explicitly win; fields it never mentioned keep
    /// the base value — a default the parser filled into the drop-in must
    /// never clobber an explicit value in the base. The accumulating fields
    /// (`env` with same-key override, `depends` and `rlimits` appends,
    /// `listen`/`drop_caps`/`syscall_allow` unioned) add rather than replace.
    /// `restart` merges field-wise (policy, budget numbers, delay separately),
    /// because a wholesale copy would let a drop-in saying `restart = always`
    /// silently reset the base's custom budget to the parser's default.
    /// `source` records both files; `name` is never renamed here (the loader
    /// only merges same-named files).
    pub fn overlay_onto(&mut self, over: ServiceDesc) {
        if over.is_explicit(Directive::Command) {
            self.command = over.command.clone();
        }
        if over.is_explicit(Directive::Type) {
            self.kind = over.kind;
        }
        if over.is_explicit(Directive::Restart) {
            self.restart.policy = over.restart.policy;
        }
        if over.is_explicit(Directive::RestartBudget) {
            self.restart.budget.capacity = over.restart.budget.capacity;
            self.restart.budget.window_ms = over.restart.budget.window_ms;
        }
        if over.is_explicit(Directive::RestartDelay) {
            self.restart.budget.delay_ms = over.restart.budget.delay_ms;
        }
        if over.is_explicit(Directive::Ready) {
            self.ready = over.ready.clone();
        }
        if over.is_explicit(Directive::ReadyTimeout) {
            self.ready_timeout_ms = over.ready_timeout_ms;
        }
        if over.is_explicit(Directive::StopTimeout) {
            self.stop_timeout_ms = over.stop_timeout_ms;
        }
        if over.is_explicit(Directive::StartTimeout) {
            self.start_timeout_ms = over.start_timeout_ms;
        }
        if over.is_explicit(Directive::User) {
            self.run_as = over.run_as;
            self.unresolved_run_as = over.unresolved_run_as.clone();
        }
        if over.is_explicit(Directive::Log) {
            self.log = over.log.clone();
        }
        if over.is_explicit(Directive::Critical) {
            self.is_critical = over.is_critical;
        }
        if over.is_explicit(Directive::Cgroup) {
            self.cgroup = over.cgroup.clone();
        }
        if over.is_explicit(Directive::PidFile) {
            self.pid_file = over.pid_file.clone();
        }
        if over.is_explicit(Directive::Tty) {
            self.tty = over.tty.clone();
        }
        if over.is_explicit(Directive::WatchdogSec) {
            self.watchdog_sec = over.watchdog_sec;
        }
        // `listen` accumulates like `env`, but entries are never overridden:
        // two identical lines are the same socket twice (harmless — the
        // supervisor binds by name and the second bind is a no-op re-check),
        // while two different lines are two sockets. Exact duplicates are
        // skipped so a base file and a drop-in saying the same thing do not
        // double the descriptor set.
        for addr in &over.listens {
            if !self.listens.iter().any(|a| a == addr) {
                self.listens.push(addr.clone());
            }
        }
        // Capability and syscall sets only narrow: union, deduplicated.
        for cap in &over.drop_caps {
            if !self.drop_caps.iter().any(|c| c == cap) {
                self.drop_caps.push(cap.clone());
            }
        }
        if over.is_explicit(Directive::SyscallFilter) {
            self.syscall_filter = over.syscall_filter;
        }
        for name in &over.syscall_allow {
            if !self.syscall_allow.iter().any(|n| n == name) {
                self.syscall_allow.push(name.clone());
            }
        }
        if over.is_explicit(Directive::Depends) {
            for name in &over.depends_required {
                if !self.depends_required.iter().any(|n| n == name) {
                    self.depends_required.push(name.clone());
                }
            }
            for name in &over.depends_optional {
                if !self.depends_optional.iter().any(|n| n == name) {
                    self.depends_optional.push(name.clone());
                }
            }
        }
        for (k, v) in &over.env {
            match self.env.iter_mut().rev().find(|(ek, _)| ek == k) {
                Some(slot) => slot.1 = v.clone(),
                None => self.env.push((k.clone(), v.clone())),
            }
        }
        for (k, v) in &over.rlimits {
            match self.rlimits.iter_mut().find(|(ek, _)| ek == k) {
                Some(slot) => slot.1 = *v,
                None => self.rlimits.push((k.clone(), *v)),
            }
        }
        self.needs_env_expansion |= over.needs_env_expansion;
        if !over.source.is_empty() {
            if !self.source.is_empty() {
                self.source.push_str(" + ");
            }
            self.source.push_str(&over.source);
        }
        for d in over.explicit {
            self.mark_explicit(d);
        }
    }

    /// True when this service runs no process and therefore forks nothing.
    pub fn is_virtual(&self) -> bool {
        !self.kind.has_process()
    }

    /// The effective restart budget.
    ///
    /// A method rather than a field read because the budget lives inside
    /// `RestartSpec` next to the policy that spends it, and reaching through
    /// two levels of struct at every call site is how a delay gets set on one
    /// copy and not the other.
    pub fn budget(&self) -> Budget {
        self.restart.budget
    }

    /// The restart policy. A named accessor because `plan.rs` asks for it by
    /// name and `desc.restart.policy` at every call site is a mouthful that
    /// hides which of the three fields of the spec is meant.
    pub fn restart_policy(&self) -> Restart {
        self.restart.policy
    }

    /// Whether `name` is declared at all, in either list.
    pub fn depends_on(&self, name: &str) -> bool {
        self.depends_required.iter().any(|n| n == name)
            || self.depends_optional.iter().any(|n| n == name)
    }

    /// Whether `name` is declared as a *required* edge.
    pub fn requires(&self, name: &str) -> bool {
        self.depends_required.iter().any(|n| n == name)
    }

    /// Total number of edges, required plus optional.
    pub fn depends_len(&self) -> usize {
        self.depends_required.len() + self.depends_optional.len()
    }

    /// Look up an environment entry by key.
    ///
    /// Returns the **last** match, mirroring what `execve` does when the vector
    /// is applied left to right, so a description that sets the same variable
    /// twice behaves identically in `zcheck` and in the runtime.
    pub fn env_get(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Look up one resource limit by name (`nofile`, `nproc` or `as`).
    pub fn rlimit(&self, name: &str) -> Option<u64> {
        self.rlimits
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| *v)
    }

    /// True when a `command` is required and missing.
    ///
    /// A `target` has no process, so no command is correct for it. For any
    /// other kind an empty command is a hard error, and this predicate is the
    /// single place that decides so.
    /// True when `user =` named an account that has not been resolved.
    ///
    /// `build_plan` refuses while this is true, so there is exactly one
    /// caller and the check cannot be forgotten.
    pub fn identity_is_unresolved(&self) -> bool {
        self.unresolved_run_as.is_some()
    }

    /// The unresolved account name, for the error message.
    pub fn unresolved_identity(&self) -> Option<&str> {
        self.unresolved_run_as.as_deref()
    }

    /// Return a copy with `unresolved_run_as` cleared, for the runtime's
    /// resolution pass. Refuses to fabricate a `run_as` that was not derived
    /// from the name.
    pub fn with_resolved_identity(
        mut self,
        resolved: (u32, u32),
    ) -> Result<ServiceDesc, crate::diagnostic::Diagnostic> {
        if self.unresolved_run_as.is_none() {
            return Err(crate::diagnostic::Diagnostic::error(
                None,
                "E390",
                "this description has no unresolved identity to resolve",
                None,
            ));
        }
        self.run_as = Some(RunAs {
            uid: resolved.0,
            gid: resolved.1,
        });
        self.unresolved_run_as = None;
        Ok(self)
    }

    pub fn missing_command(&self) -> bool {
        self.kind.has_process() && self.command.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    // ── rule 1: names ─────────────────────────────────────────────────────

    /// The dinit `validate_service_name` bug, written as a test.
    ///
    /// `../../etc/passwd` is the payload: it is accepted by a validator whose
    /// success path returns before the loop, and it turns a service
    /// description into a write outside `/etc/zinit/services.d`.
    #[test]
    fn names_that_would_escape_the_services_directory_are_refused() {
        for bad in [
            "../../etc/passwd",
            "../sshd",
            "..",
            ".",
            "a/b",
            "/absolute",
            "trailing/",
            "back\\slash",
            "windows\\..\\evil",
            ".hidden",
            "..hidden",
            "@include",
            "con espacio",
            "tab\there",
            "new\nline",
            "nul\0byte",
            "\u{7}bell",
            "a..b",
            "service.d/../..",
        ] {
            let e = validate_service_name(bad);
            assert!(
                e.is_err(),
                "must refuse {bad:?}, got {:?}",
                validate_service_name(bad)
            );
        }
    }

    /// The complement: names that are legal must not be refused.
    ///
    /// Without this half the validator could be replaced by
    /// `return Err(..)` and every name test would still pass, which is the
    /// other half of the dinit bug.
    #[test]
    fn ordinary_service_names_are_accepted() {
        for good in [
            "sshd",
            "network.target",
            "all.target",
            "boot.target",
            "dbus",
            "x",
            "a",
            "zinit.slice",
            "nfs-client4",
            "user@host",
            "café",
            "ñandú",
            "日本語",
            "🎉",
            "-leading-dash",
            "trailing-dash-",
            "UPPER",
            "with.dots.and-dashes_123",
        ] {
            assert_eq!(validate_service_name(good), Ok(()), "must accept {good:?}");
        }
    }

    /// Every rejection reason is reachable, and each one is distinct.
    ///
    /// A `return Err(..)` that is never produced is as much a bug as a
    /// `return Ok(())` that is produced too early, and this is the only test
    /// that would notice.
    #[test]
    fn every_name_error_variant_is_reachable_and_specific() {
        assert_eq!(validate_service_name(""), Err(NameError::Empty));
        assert_eq!(
            validate_service_name(&"x".repeat(MAX_SERVICE_NAME_BYTES)),
            Ok(())
        );
        assert_eq!(
            validate_service_name(&"x".repeat(MAX_SERVICE_NAME_BYTES + 1)),
            Err(NameError::TooLong {
                len: MAX_SERVICE_NAME_BYTES + 1
            })
        );
        assert_eq!(
            validate_service_name("a/b"),
            Err(NameError::ContainsPathSeparator)
        );
        assert_eq!(
            validate_service_name("a\\b"),
            Err(NameError::ContainsPathSeparator)
        );
        assert_eq!(
            validate_service_name("a..b"),
            Err(NameError::ContainsDotDot)
        );
        assert_eq!(validate_service_name(".x"), Err(NameError::StartsWithDot));
        assert_eq!(validate_service_name("@x"), Err(NameError::StartsWithAt));
        assert_eq!(
            validate_service_name("a b"),
            Err(NameError::Whitespace(' '))
        );
        // `\t` is both whitespace and a control character; control wins,
        // because a name containing one is a broken file rather than a token
        // that was spelled oddly.
        assert_eq!(validate_service_name("a\tb"), Err(NameError::Control('\t')));
        assert_eq!(validate_service_name("a\nb"), Err(NameError::Control('\n')));
        assert_eq!(validate_service_name("a\0b"), Err(NameError::Control('\0')));
    }

    /// The length cap is counted in **bytes**, and a multi-byte name is
    /// measured by what actually lands in a filename.
    #[test]
    fn the_name_length_cap_counts_bytes() {
        // 128 two-byte characters is 256 bytes: over the cap, though only 128
        // characters. A character count would accept this name and the
        // filesystem would not.
        let long = "ñ".repeat(128);
        assert!(long.chars().count() < MAX_SERVICE_NAME_BYTES);
        assert_eq!(
            validate_service_name(&long),
            Err(NameError::TooLong { len: 256 })
        );
    }

    /// The error text names the actual problem, so `zcheck` output is
    /// actionable without a manual.
    #[test]
    fn name_errors_explain_themselves() {
        assert!(NameError::ContainsPathSeparator.to_string().contains("`/`"));
        assert!(NameError::StartsWithDot.to_string().contains("traversal"));
        assert!(
            NameError::Whitespace(' ')
                .to_string()
                .contains("single token")
        );
        assert!(NameError::Control('\0').to_string().contains("execve"));
        assert!(NameError::Empty.to_string().contains("empty"));
    }

    /// `try_new` refuses, `new` does not, and the difference is deliberate.
    #[test]
    fn try_new_validates_and_new_does_not() {
        assert_eq!(ServiceDesc::try_new("sshd").expect("valid").name, "sshd");
        assert!(ServiceDesc::try_new("../evil").is_err());
        // `new` is the infallible constructor graph/plan build fixtures with.
        assert_eq!(ServiceDesc::new("../evil").name, "../evil");
    }

    // ── defaults ─────────────────────────────────────────────────────────

    #[test]
    fn new_matches_documented_defaults() {
        let d = ServiceDesc::new("sshd");
        assert_eq!(d.name, "sshd");
        assert_eq!(d.kind, ServiceKind::Process);
        assert_eq!(d.restart.policy, Restart::OnFailure);
        assert_eq!(d.ready.kind, ReadySpecKind::Notify);
        assert_eq!(d.stop_timeout_ms, 10_000);
        assert_eq!(d.start_timeout_ms, 60_000);
        assert!(d.is_critical);
        assert!(d.run_as.is_none());
        assert!(!d.needs_env_expansion);
        assert!(d.env.is_empty());
        assert!(d.rlimits.is_empty());
        assert!(d.cgroup.is_none());
        assert!(d.depends_required.is_empty() && d.depends_optional.is_empty());
        assert_eq!(d.depends_len(), 0);
    }

    /// The restart default is whatever `value.rs` says it is, obtained by
    /// calling it. If the numbers ever move, this moves with them for free.
    #[test]
    fn the_restart_default_is_whatever_value_rs_says() {
        let d = ServiceDesc::new("x");
        assert_eq!(d.restart, RestartSpec::default());
        assert_eq!(d.budget(), crate::value::default_budget());
        assert_eq!(d.restart_policy(), Restart::OnFailure);
    }

    /// The log default is whatever `value.rs` says it is.
    #[test]
    fn the_log_default_is_whatever_value_rs_says() {
        assert_eq!(ServiceDesc::new("x").log, LogSpec::default());
        assert_eq!(
            ServiceDesc::new("x").log,
            LogSpec {
                sink: zcore::LogSink::default()
            }
        );
    }

    #[test]
    fn default_equals_new_with_an_empty_name() {
        assert_eq!(ServiceDesc::new(""), ServiceDesc::default());
    }

    /// `Option::unwrap_or(default)` in three places is how `Some(0)` silently
    /// becomes "5 restarts a minute" one day.
    #[test]
    fn a_restart_of_never_keeps_a_zero_budget_rather_than_the_default() {
        let never = RestartSpec::parse("never").expect("valid");
        let d = ServiceDesc {
            restart: never,
            ..ServiceDesc::new("x")
        };
        assert_eq!(d.budget().capacity, 0);
    }

    // ── accessors ────────────────────────────────────────────────────────

    #[test]
    fn targets_do_not_need_a_command() {
        let d = ServiceDesc {
            kind: ServiceKind::Target,
            ..ServiceDesc::new("network.target")
        };
        assert!(!d.missing_command());
        assert!(d.is_virtual());
    }

    #[test]
    fn processes_without_a_command_are_incomplete() {
        assert!(ServiceDesc::new("x").missing_command());
    }

    #[test]
    fn env_lookup_takes_the_last_binding() {
        let mut d = ServiceDesc::new("x");
        d.env.push(("A".into(), "1".into()));
        d.env.push(("A".into(), "2".into()));
        assert_eq!(d.env_get("A"), Some("2"));
        assert_eq!(d.env_get("B"), None);
    }

    #[test]
    fn rlimit_lookup() {
        let mut d = ServiceDesc::new("x");
        d.rlimits.push(("nofile".into(), 4096));
        assert_eq!(d.rlimit("nofile"), Some(4096));
        assert_eq!(d.rlimit("as"), None);
    }

    #[test]
    fn dependency_lookups() {
        let mut d = ServiceDesc::new("x");
        d.depends_required.push("network".into());
        d.depends_optional.push("logind".into());
        assert_eq!(d.depends_len(), 2);
        assert!(d.depends_on("network"));
        assert!(d.depends_on("logind"));
        assert!(d.requires("network"));
        assert!(!d.requires("logind"), "optional is not required");
        assert!(!d.depends_on("nope"));
    }

    #[test]
    fn with_source_is_chainable() {
        let d = ServiceDesc::new("x").with_source("/etc/zinit/services.d/x.conf", 4);
        assert_eq!(d.source, "/etc/zinit/services.d/x.conf");
        assert_eq!(d.source_line, 4);
    }

    /// `ServiceDesc::default()` must not panic and must not be reachable
    /// through any path that re-states a number `value.rs` owns.
    #[test]
    fn default_is_constructible_without_a_parser() {
        let d: ServiceDesc = Default::default();
        assert_eq!(d, ServiceDesc::new(""));
        // A hand-built description is possible, which is what the `graph` and
        // `plan` test fixtures rely on.
        let custom = ServiceDesc {
            name: "y".to_string(),
            kind: ServiceKind::Script,
            command: "/bin/sh".to_string(),
            depends_required: vec!["a".to_string()],
            ..ServiceDesc::default()
        };
        assert_eq!(custom.kind, ServiceKind::Script);
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::*;
    use crate::parser::parse_service;

    fn parsed(name: &str, text: &str) -> ServiceDesc {
        parse_service(name, text).expect("test fixture must parse")
    }

    #[test]
    fn an_overlay_that_says_nothing_changes_nothing() {
        let mut base = parsed("svc", "command = /bin/a\nstop-timeout = 20s\n");
        let over = parsed("svc", "command = /bin/a\n");
        let before = base.clone();
        base.overlay_onto(over);
        assert_eq!(base.stop_timeout_ms, before.stop_timeout_ms);
        assert_eq!(base.command, "/bin/a");
    }

    #[test]
    fn explicit_values_win_and_defaults_do_not_clobber() {
        let mut base = parsed(
            "svc",
            "command = /bin/a\nstop-timeout = 20s\nready = tcp:80\n",
        );
        // The overlay sets only `command`. Its parser-filled defaults for
        // `stop-timeout` (10s) and `ready` (notify) must not leak through.
        let over = parsed("svc", "command = /bin/b\n");
        base.overlay_onto(over);
        assert_eq!(base.command, "/bin/b");
        assert_eq!(base.stop_timeout_ms, 20_000);
        assert_eq!(base.ready.kind, crate::value::ReadySpecKind::Tcp);
    }

    #[test]
    fn tty_overrides_only_when_explicit() {
        let mut base = parsed("svc", "type = console\ncommand = /bin/a\ntty = /dev/tty1\n");
        let over = parsed("svc", "type = console\ncommand = /bin/a\n");
        base.overlay_onto(over);
        assert_eq!(base.tty.as_deref(), Some("/dev/tty1"));
        let mut base2 = parsed("svc", "type = console\ncommand = /bin/a\n");
        let over2 = parsed("svc", "type = console\ncommand = /bin/a\ntty = /dev/tty2\n");
        base2.overlay_onto(over2);
        assert_eq!(base2.tty.as_deref(), Some("/dev/tty2"));
    }

    #[test]
    fn restart_merges_field_wise_not_wholesale() {
        let mut base = parsed(
            "svc",
            "command = /bin/a\nrestart = on-failure\nrestart-budget = 3 restarts per 10s\n",
        );
        // `restart = always` must take the policy and leave the budget alone.
        let over = parsed("svc", "command = /bin/a\nrestart = always\n");
        base.overlay_onto(over);
        assert_eq!(base.restart.policy, zcore::Restart::Always);
        assert_eq!(base.restart.budget.capacity, 3);
        // And the reverse: a budget-only drop-in keeps the policy.
        let mut base2 = parsed("svc", "command = /bin/a\nrestart = never\n");
        let over2 = parsed(
            "svc",
            "command = /bin/a\nrestart-budget = 7 restarts per 60s\n",
        );
        base2.overlay_onto(over2);
        assert_eq!(base2.restart.policy, zcore::Restart::Never);
        assert_eq!(base2.restart.budget.capacity, 7);
    }

    #[test]
    fn env_merges_with_same_key_override() {
        let mut base = parsed("svc", "command = /bin/a\nenv = A=1\nenv = B=2\n");
        let over = parsed("svc", "command = /bin/a\nenv = B=override\nenv = C=3\n");
        base.overlay_onto(over);
        assert_eq!(base.env_get("A"), Some("1"));
        assert_eq!(base.env_get("B"), Some("override"));
        assert_eq!(base.env_get("C"), Some("3"));
    }

    #[test]
    fn depends_accumulate_without_exact_duplicates() {
        let mut base = parsed("svc", "command = /bin/a\ndepends = net, db\n");
        let over = parsed("svc", "command = /bin/a\ndepends = db, cache\n");
        base.overlay_onto(over);
        assert_eq!(base.depends_required.len(), 3);
        assert!(base.requires("net") && base.requires("db") && base.requires("cache"));
    }

    #[test]
    fn rlimits_override_per_resource() {
        let mut base = parsed("svc", "command = /bin/a\nrlimit-nofile = 1024\n");
        let over = parsed(
            "svc",
            "command = /bin/a\nrlimit-nofile = 4096\nrlimit-nproc = 64\n",
        );
        base.overlay_onto(over);
        assert_eq!(base.rlimit("nofile"), Some(4096));
        assert_eq!(base.rlimit("nproc"), Some(64));
    }

    #[test]
    fn sources_chain_for_diagnostics() {
        let mut base = parsed("svc", "command = /bin/a\n").with_source("base.conf", 1);
        let over = parsed("svc", "command = /bin/b\n").with_source("base.d/10.conf", 1);
        base.overlay_onto(over);
        assert_eq!(base.source, "base.conf + base.d/10.conf");
        assert_eq!(base.command, "/bin/b");
    }
}
