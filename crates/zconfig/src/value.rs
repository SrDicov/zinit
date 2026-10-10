//! Parsing of *directive values*: durations, readiness adapters, restart
//! policies, `user`, `log`.
//!
//! `parser.rs` owns lines, `=` and `#`. This module owns everything to the
//! right of the first `=`, and it owns it completely: given a `&str` it either
//! returns a validated value or an error that names the value it choked on.
//! No I/O, no clock, no filesystem, no name resolution — all `no_std`, all
//! `alloc`.
//!
//! ## Why hand-rolled instead of `str::parse`
//!
//! `u64::from_str` is lenient where a config file must not be. It accepts
//! `"+10"` and (on some code paths) surrounding whitespace, and it would
//! happily read `"0x10"` as a different base than the one the operator sees in
//! the rest of the file. Worse, its error is a bare `ParseIntError` with no
//! position and no expectation attached: `zinit check` would print "invalid
//! digit found in string" for a typo of `stop-timout`. Every parser below
//! therefore hand-rolls its own scanner over ASCII bytes and returns a message
//! that says what it *expected*.
//!
//! ## The one ambiguity in the format, stated once
//!
//! A duration with no unit is **seconds**, not milliseconds. `stop-timeout = 10`
//! is ten seconds, because the number in a config file is written the way a
//! human says it out loud. Everywhere else in the format that would be a
//! trap, which is why nothing else in this module has a bare number: only
//! durations.
//!
//! ## Shape of the public API
//!
//! Four value types, each a validated struct with a `parse` that takes `&str`
//! and returns `Result<Self, XxxError<'_>>` and an `into_*` that produces the
//! `zcore` type the runtime wants. A duration is the exception: it is just
//! milliseconds, so [`parse_duration`] goes straight to a `u64`.
//!
//! The defaults an absent directive falls back to are the constants at the top
//! of this file, assembled by [`default_budget`]. Nothing else in the crate is
//! allowed to spell "5 restarts per minute" as a literal.
//!
//! ## Error shape
//!
//! Every error carries the offending text, so the message can quote it, and
//! every error converts to a [`Diagnostic`] with
//! [`ValueError::to_diagnostic`] at a position the caller already has. Errors
//! are *always* errors: nothing in this module emits a warning, because a value
//! that did not parse cannot be guessed at. The one judgement call —
//! `0 restarts per 1s` — is documented on [`RestartError::ZeroCapacity`].

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;
// Imported from `zcore::types` rather than through the `zcore` root re-export:
// the root also carries `Runtime`, `reconcile` and friends, and naming the
// module makes it obvious at a glance that the only thing crossing the crate
// boundary from `zcore` is a handful of `no_std` data types.
use zcore::types::{Budget, ListenAddr, ListenSpec, LogSink, Ready, Restart, StrictReady};

use crate::diagnostic::{Diagnostic, Span};

// ─────────────────────────────────────────────────────────────────────────────
// Defaults
//
// Every default an operator can rely on is named here, in one place, so that
// `plan.rs` filling in an absent directive and `zctl show` printing the
// effective value cannot disagree. The numbers come from DESIGN.md §5.
// ─────────────────────────────────────────────────────────────────────────────

/// `restart-delay` default: minimum spacing between two restart attempts.
pub const DEFAULT_RESTART_DELAY_MS: u64 = 250;

/// `restart-budget` default: restarts allowed per window (DESIGN.md §4.3).
pub const DEFAULT_RESTART_CAPACITY: u32 = 5;

/// `restart-budget` default: the window `DEFAULT_RESTART_CAPACITY` applies to.
pub const DEFAULT_RESTART_WINDOW_MS: u64 = 60_000;

/// `log = file:...` default: rotate once the file passes this size.
pub const DEFAULT_LOG_MAX_BYTES: u64 = 1024 * 1024;

/// `log = file:...` default: how many rotated generations to keep.
pub const DEFAULT_LOG_BACKUPS: u8 = 3;

/// **The** restart budget, spelled once, from the three constants above.
///
/// This is the single definition of "what a service gets when the description
/// says nothing". [`RestartSpec::default`], `desc.rs` filling in an absent
/// `restart-budget` and `zctl show` printing an effective configuration all
/// call it, so the three cannot drift apart — which is the whole reason
/// DESIGN.md §4.3 keeps the window and the capacity in one pair.
///
/// The numbers match `zcore::Budget`'s own `Default` today. That is a
/// coincidence worth *pinning*, not worth relying on: `zcore` is the runtime
/// and its default is a property of the token bucket, whereas this one is a
/// promise the format makes to the operator. `default_budget_matches_zcore`
/// in the tests below fails the moment either side moves alone.
pub const fn default_budget() -> Budget {
    Budget {
        capacity: DEFAULT_RESTART_CAPACITY,
        window_ms: DEFAULT_RESTART_WINDOW_MS,
        delay_ms: DEFAULT_RESTART_DELAY_MS,
    }
}

/// The budget a `restart = never` service gets: nothing, ever.
///
/// Not `default_budget()`: leaving tokens in a bucket nobody may spend from
/// only invites someone to read `capacity: 5` on a service that cannot restart
/// and conclude the number means something. [`Budget::never`] is the honest
/// encoding and it is a `const fn`, so it can be used in a constant.
pub const NEVER_BUDGET: Budget = Budget::never();

/// The capacity a bare `restart = on-failure` / `restart = always` gets.
///
/// Effectively "the budget stops being the thing that stops the loop": four
/// billion restarts a minute. The operator named a policy in full and did not
/// name a budget, so imposing one would be inventing a requirement.
pub const UNLIMITED_CAPACITY: u32 = u32::MAX;

// ─────────────────────────────────────────────────────────────────────────────
// Duration
// ─────────────────────────────────────────────────────────────────────────────

/// Milliseconds in one second.
const MS_PER_SECOND: u64 = 1_000;
/// Milliseconds in one minute.
const MS_PER_MINUTE: u64 = 60 * MS_PER_SECOND;
/// Milliseconds in one hour.
const MS_PER_HOUR: u64 = 60 * MS_PER_MINUTE;
/// Milliseconds in one day.
const MS_PER_DAY: u64 = 24 * MS_PER_HOUR;

/// Parse a duration into milliseconds.
///
/// Milliseconds everywhere, because that is the resolution `zcore`'s budget
/// arithmetic uses and the only one that survives a clock that is not perfectly
/// stable.
///
/// # Accepted
///
/// | input     | value      |
/// |-----------|------------|
/// | `10s`     | 10 000 ms  |
/// | `500ms`   | 500 ms     |
/// | `2m`      | 120 000 ms |
/// | `1h`      | 3 600 000 ms |
/// | `1d`      | 86 400 000 ms |
/// | `250`     | **250 000 ms** |
///
/// **A number with no unit is SECONDS, not milliseconds.** This is the one
/// place in the whole format where a bare number means something, and it is
/// the classic trap: `stop-timeout = 10` is ten seconds, not ten
/// milliseconds, because a number in a config file is written the way a human
/// says it out loud. `250` is 250 *seconds* — four minutes — not 250 ms. The
/// `help` text of [`DurationError::UnknownUnit`] repeats the rule back,
/// precisely so the mistake is caught the first time it is made.
///
/// # Rejected
///
/// The empty string; `0x10`; `1.5s`; `+10`; `-5`; `10s5`; `10m30s`; `10 s`
/// (the space is a syntax error, not a unit separator); `10sec`; `10S`
/// (units are lowercase, exactly as `zcore` spells them); anything that
/// overflows `u64`; and any non-ASCII digit, because the file is bytes and the
/// operator's editor wrote ASCII.
///
/// Whitespace is *not* trimmed and the value is *not* case-folded: the
/// directive's own lexing has already removed the space around `=`, and a
/// value with spaces in it is a mistake worth reporting, not a value worth
/// salvaging.
pub fn parse_duration(input: &str) -> Result<u64, DurationError<'_>> {
    if input.is_empty() {
        return Err(DurationError::Empty);
    }
    // The leading run of ASCII digits; `str::parse` would take a sign, a
    // decimal point or a radix prefix, none of which are a duration.
    let digits = input
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(input.len());
    if digits == 0 {
        // `-5`, `+10`, `1.5s`, `abc`: nothing here starts a number. A
        // leading sign must never be mistaken for a unit separator.
        return Err(DurationError::InvalidNumber { value: input });
    }
    let (number, rest) = input.split_at(digits);
    let value = parse_u64(number)?;

    if rest.is_empty() {
        // Bare number: SECONDS. `stop-timeout = 10` is ten seconds, not
        // ten milliseconds. Saturating, because a bare `18446744073709551615`
        // is a number the operator wrote, not a reason to abort the checker.
        return Ok(value.saturating_mul(MS_PER_SECOND));
    }

    let Some(scale) = unit_scale(rest) else {
        // A tail that still contains digits, a decimal point or a sign was
        // never a unit: the operator wrote one malformed number, and saying
        // "unknown unit `x10`" for `0x10` would send them looking for a list
        // of units instead of at their own typo.
        let malformed_number = rest
            .as_bytes()
            .iter()
            .any(|b| b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'-'));
        return Err(if malformed_number {
            DurationError::InvalidNumber { value: input }
        } else {
            DurationError::UnknownUnit {
                value: input,
                unit: rest,
            }
        });
    };

    value.checked_mul(scale).ok_or(DurationError::Overflow {
        value: input,
        unit: rest,
    })
}

/// Why a duration failed to parse.
///
/// Split out of [`ValueError`] so callers that only handle durations do not
/// have to match arms they cannot produce. Note what is *not* here: a
/// "missing unit" case, because a missing unit is legal (it means seconds).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DurationError<'a> {
    /// `""`. Distinct from `InvalidNumber` because "you left the value out" and
    /// "you wrote nonsense" are different mistakes with different fixes.
    Empty,
    /// A unit that is not one of the five: `10sec`, `10S`, `10 s`.
    UnknownUnit {
        /// The whole offending value.
        value: &'a str,
        /// The trailing part that was not a unit.
        unit: &'a str,
    },
    /// Not a bare run of ASCII digits: `0x10`, `1.5s`, `-5`, `10s5`.
    InvalidNumber {
        /// The whole offending value.
        value: &'a str,
    },
    /// The digits themselves exceed `u64`.
    NumberTooLarge {
        /// The whole offending value.
        value: &'a str,
    },
    /// Digits and unit are fine but the product overflows milliseconds.
    Overflow {
        /// The whole offending value.
        value: &'a str,
        /// The unit that was applied.
        unit: &'a str,
    },
}

impl fmt::Display for DurationError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DurationError::Empty => {
                f.write_str("empty duration: expected a value like `30s`, `500ms` or `1h`")
            }
            DurationError::UnknownUnit { value, unit } => write!(
                f,
                "unknown unit `{unit}` in duration `{value}`; the units are \
                 `ms`, `s`, `m`, `h` and `d`, all lowercase, with no space"
            ),
            DurationError::InvalidNumber { value } => write!(
                f,
                "`{value}` is not a number: a duration is ASCII digits followed \
                 by a unit, with no sign, no decimal point and no `0x` prefix"
            ),
            DurationError::NumberTooLarge { value } => {
                write!(f, "the number in `{value}` does not fit in a u64")
            }
            DurationError::Overflow { value, unit } => write!(
                f,
                "duration `{value}` does not fit: the value in `{unit}` \
                 overflows milliseconds"
            ),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Ready
// ─────────────────────────────────────────────────────────────────────────────

/// Which readiness adapter a `ready` value selected.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ReadySpecKind {
    /// `none`: `Running` as soon as the fork succeeds.
    #[default]
    None,
    /// `notify`: the service writes a line to the notify fd.
    Notify,
    /// `ping:<cmd>`: re-run `cmd` every 250 ms until it exits 0.
    Ping,
    /// `tcp:<port>`: connect to `127.0.0.1:<port>` until it is accepted.
    Tcp,
}

/// A validated `ready` value.
///
/// The adapter and its argument are kept apart rather than stored as a
/// `zcore::Ready`, because `zcore::Ready` also models the *non*-strict variants
/// that `zconfig` is not allowed to produce — see [`ReadySpec::into_ready`].
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ReadySpec {
    /// Which adapter.
    pub kind: ReadySpecKind,
    /// The argument as written: the command for `Ping`, the decimal port for
    /// `Tcp`, empty otherwise. Kept as text because the two `Ping` and `Tcp`
    /// numeric conversions happen in `into_ready`, where a failure is already
    /// impossible.
    ///
    /// Empty under the `Default`, which is `ready = none` — the one readiness
    /// that is always legal, needs no handshake and cannot time out.
    pub arg: String,
}

impl ReadySpec {
    /// Parse `none`, `notify`, `ping:<cmd>` or `tcp:<port>`.
    ///
    /// `ping:` with an empty command is an error, not a no-op: a ping with
    /// nothing to run would sit there succeeding forever and report the service
    /// ready before it started. `tcp:0` is an error because port 0 is not
    /// something a service listens on, and a `ready = tcp:0` is far more often
    /// an unfilled variable than a real intent.
    pub fn parse(input: &str) -> Result<ReadySpec, ReadyError<'_>> {
        let (head, tail) = split_surplus(input, b':');
        let kind = match head {
            "none" => ReadySpecKind::None,
            "notify" => ReadySpecKind::Notify,
            "ping" => ReadySpecKind::Ping,
            "tcp" => ReadySpecKind::Tcp,
            "" => return Err(ReadyError::EmptyProbe),
            other => {
                return Err(ReadyError::UnknownKind {
                    value: input,
                    kind: other,
                });
            }
        };
        // `none` and `notify` take no argument: a colon after them is a typo,
        // not a value. `ping` and `tcp` take nothing but an argument.
        let takes_no_arg = matches!(kind, ReadySpecKind::None | ReadySpecKind::Notify);
        if takes_no_arg && !tail.is_empty() {
            return Err(ReadyError::UnexpectedArgument { value: input });
        }
        if kind == ReadySpecKind::Ping && tail.trim().is_empty() {
            return Err(ReadyError::EmptyProbe);
        }
        if kind == ReadySpecKind::Tcp && tail.trim().is_empty() {
            return Err(ReadyError::EmptyPort { value: input });
        }
        if kind == ReadySpecKind::Tcp {
            if !all_digits(tail) {
                return Err(ReadyError::PortNotNumeric {
                    value: input,
                    port: tail,
                });
            }
            let port = match parse_u64(tail) {
                Ok(p) => p,
                // It is a number, just not one that fits anywhere near a port.
                Err(DurationError::NumberTooLarge { .. }) => {
                    return Err(ReadyError::PortTooLarge {
                        value: input,
                        port: tail,
                    });
                }
                Err(_) => {
                    return Err(ReadyError::PortNotNumeric {
                        value: input,
                        port: tail,
                    });
                }
            };
            if port == 0 {
                return Err(ReadyError::PortZero { value: input });
            }
            if port > u64::from(u16::MAX) {
                return Err(ReadyError::PortTooLarge {
                    value: input,
                    port: tail,
                });
            }
        }
        Ok(ReadySpec {
            kind,
            arg: tail.to_string(),
        })
    }

    /// The `zcore` readiness handshake for this value.
    ///
    /// Takes `&self` rather than `self` on purpose: `desc.rs` holds a
    /// `ReadySpec` inside a `ServiceDesc` it usually only has a reference to,
    /// and making the one conversion consume the value would force every
    /// caller to clone the whole struct to read one field out of it. The
    /// `Ping` arm clones its command either way.
    ///
    /// **Everything with a handshake comes out as [`Ready::Strict`].**
    /// DESIGN.md §6 is explicit: a service whose handshake expires must come up
    /// anyway, with a warning, because the alternative is an init that cannot
    /// boot a system full of services that do not implement `ZINIT_NOTIFY_FD`.
    /// There is no configuration value that turns strictness off: the *runtime*
    /// decides it per start — if it already saw a readiness notification the
    /// service is up, and if the timeout expired the service is reported
    /// `Running` with `ready_timeout`. Emitting a bare `Ready::Notify` /
    /// `Ready::Ping` / `Ready::Tcp` from here would be the one change that
    /// makes an existing service description unbootable.
    pub fn into_ready(&self) -> Ready {
        match self.kind {
            ReadySpecKind::None => Ready::None,
            ReadySpecKind::Notify => Ready::Strict(StrictReady::Notify),
            ReadySpecKind::Ping => Ready::Strict(StrictReady::Ping(self.arg.clone())),
            ReadySpecKind::Tcp => {
                // `parse` cannot produce a spec whose port is out of range, so
                // the fallback is only reachable from a hand-built one. It
                // degrades to 0 rather than panicking: an init that aborts
                // because a caller assembled a struct by hand is worse than one
                // that waits for a port nobody is listening on.
                let port = self.port().unwrap_or(0);
                Ready::Strict(StrictReady::Tcp(port))
            }
        }
    }

    /// The port for a `tcp` readiness probe, if any.
    ///
    /// Returns `None` for a spec that is not `tcp`, and for a port that does
    /// not fit a `u16` — which [`ReadySpec::parse`] has already rejected, so
    /// the only way to see it is a hand-built spec. A `None` on a `Tcp` spec
    /// therefore means "this spec was not built by the parser", never "this
    /// service has no port".
    pub fn port(&self) -> Option<u16> {
        if self.kind != ReadySpecKind::Tcp {
            return None;
        }
        let n = parse_u64(self.arg.as_str()).ok()?;
        if n == 0 || n > u64::from(u16::MAX) {
            return None;
        }
        Some(n as u16)
    }

    /// The `ping` command, if any.
    ///
    /// `None` for every adapter but `ping`. Exists so a caller rendering an
    /// effective configuration does not have to reach into [`ReadySpec::arg`]
    /// and reason about which adapter owns the text.
    pub fn command(&self) -> Option<&str> {
        match self.kind {
            ReadySpecKind::Ping => Some(self.arg.as_str()),
            _ => None,
        }
    }

    /// True when this spec needs a `ready_due_at` deadline armed on the service.
    ///
    /// The same predicate as `zcore::Ready::is_handshake`: only `none` answers
    /// false. Offered here so a caller can decide *whether* to arm a deadline
    /// without first materialising the `Ready`.
    pub const fn is_handshake(&self) -> bool {
        !matches!(self.kind, ReadySpecKind::None)
    }

    /// Canonical spelling, for round-tripping a description back to text.
    ///
    /// Always one of the four forms [`ReadySpec::parse`] accepts, so
    /// `ReadySpec::parse(&spec.to_string()) == Ok(spec.clone())` holds for
    /// every spec the parser produced. `ping` keeps its command verbatim,
    /// spaces and all, because re-quoting it would mean inventing a shell.
    pub fn describe(&self) -> String {
        match self.kind {
            ReadySpecKind::None => "none".to_string(),
            ReadySpecKind::Notify => "notify".to_string(),
            ReadySpecKind::Ping => format!("ping:{}", self.arg),
            ReadySpecKind::Tcp => format!("tcp:{}", self.arg),
        }
    }
}

impl fmt::Display for ReadySpec {
    /// Writes [`ReadySpec::describe`], so `zctl show` and a diagnostic that
    /// quotes a value both use the same spelling.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Why a `ready` value failed to parse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadyError<'a> {
    /// `ready = ` with nothing after it, or `ping:` with no command. One
    /// variant for both: in each case the probe has nothing to probe, and the
    /// fix is the same one — name the probe you meant.
    EmptyProbe,
    /// Not one of the four adapters.
    UnknownKind {
        /// The whole offending value.
        value: &'a str,
        /// The part before the colon.
        kind: &'a str,
    },
    /// `tcp:` with no port.
    EmptyPort {
        /// The whole offending value.
        value: &'a str,
    },
    /// `none:` / `notify:whatever` — the adapter takes no argument.
    UnexpectedArgument {
        /// The whole offending value.
        value: &'a str,
    },
    /// `tcp:http` and friends.
    PortNotNumeric {
        /// The whole offending value.
        value: &'a str,
        /// The part after the colon.
        port: &'a str,
    },
    /// `tcp:0`.
    PortZero {
        /// The whole offending value.
        value: &'a str,
    },
    /// `tcp:65536`, `tcp:99999`.
    PortTooLarge {
        /// The whole offending value.
        value: &'a str,
        /// The part after the colon, as written.
        port: &'a str,
    },
}

impl fmt::Display for ReadyError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadyError::EmptyProbe => f.write_str(
                "empty readiness probe: expected `none`, `notify`, \
                 `ping:<command>` or `tcp:<port>`",
            ),
            ReadyError::UnknownKind { value, kind } => write!(
                f,
                "unknown readiness adapter `{kind}` in `{value}`; expected \
                 `none`, `notify`, `ping:` or `tcp:`"
            ),
            ReadyError::EmptyPort { value } => {
                write!(f, "`{value}` has no port: write `tcp:22` or similar")
            }
            ReadyError::UnexpectedArgument { value } => write!(
                f,
                "`{value}` takes no argument: `none` and `notify` are written \
                 bare, with nothing after them"
            ),
            ReadyError::PortNotNumeric { value, port } => write!(
                f,
                "`{port}` is not a TCP port number in `{value}`: a readiness \
                 port is a decimal number from 1 to 65535"
            ),
            ReadyError::PortZero { value } => write!(
                f,
                "`{value}` is not usable: port 0 is never a service port, so \
                 this is almost always an unset variable"
            ),
            ReadyError::PortTooLarge { value, port } => write!(
                f,
                "TCP port `{port}` in `{value}` is out of range: the maximum \
                 is 65535"
            ),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Restart
// ─────────────────────────────────────────────────────────────────────────────

/// A validated `restart` value: the policy plus the budget it is spent from.
///
/// dinit splits this across `restart-delay` and "3 restarts in 10s"; DESIGN.md
/// §4.3 keeps them as one number pair plus a separate delay, because a token
/// bucket with its two constants written in two different directives is a
/// bucket nobody can reason about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RestartSpec {
    /// `never`, `on-failure` or `always`.
    pub policy: Restart,
    /// Token bucket the policy spends from.
    pub budget: Budget,
}

impl Default for RestartSpec {
    /// **`on-failure` spending [`default_budget`].** Byte for byte what
    /// `RestartSpec::parse("on-failure")` would produce if the bare policies
    /// shared the default budget, and the point is that an absent `restart`
    /// line and an explicit `restart = on-failure` must not be distinguishable
    /// in the two numbers the runtime reads, or `zctl show` would print two
    /// different things for a service nobody changed.
    ///
    /// Note that this is *not* what `RestartSpec::parse("on-failure")` returns:
    /// the parser gives a bare policy a capacity of `u32::MAX`, on the theory
    /// that a policy the operator wrote in full does not need a second limit
    /// they did not ask for. The default is the opposite trade — nobody wrote
    /// anything, so the conservative documented numbers apply.
    /// `restart_default_and_bare_on_failure_are_both_on_failure` pins the
    /// policy on both sides so the difference can never become a policy one.
    fn default() -> RestartSpec {
        RestartSpec {
            policy: Restart::OnFailure,
            budget: default_budget(),
        }
    }
}

impl RestartSpec {
    /// Parse a `restart` value.
    ///
    /// Accepts the three policies and the budget form
    /// `<n> restarts per <duration>` with flexible interior whitespace
    /// (`5  restarts  per  60s` is fine — collapsing runs of spaces is not
    /// something an operator can get wrong on purpose).
    ///
    /// For the three bare policies the budget is not the user's business:
    /// `never` gets [`Budget::never`] and the other two get a capacity of
    /// `u32::MAX` per [`DEFAULT_RESTART_WINDOW_MS`], i.e. four billion
    /// restarts a minute, which is to say the budget stops being the thing
    /// that stops the loop. Only a service that crashes four billion times a
    /// minute will notice, and it will notice loudly.
    ///
    /// A bare `5 restarts per 60s` — which is the `restart-budget` directive
    /// in all but name — carries no policy of its own, so the policy comes out
    /// as [`Restart::OnFailure`], the DESIGN §5 default. `plan.rs` overwrites it
    /// when the description also has a `restart` line.
    pub fn parse(input: &str) -> Result<RestartSpec, RestartError<'_>> {
        let (head, rest) = match input.find(char::is_whitespace) {
            Some(i) => (&input[..i], &input[i..]),
            None => (input, ""),
        };
        let policy = match head {
            "never" => Restart::Never,
            "on-failure" => Restart::OnFailure,
            "always" => Restart::Always,
            // A single token that is not a policy is a typo, not a budget.
            _ if rest.is_empty() => return Err(RestartError::UnknownPolicy { value: input }),
            _ => {
                let (capacity, window) = parse_budget(input)?;
                return Ok(RestartSpec {
                    policy: Restart::OnFailure,
                    budget: Budget {
                        capacity,
                        window_ms: window,
                        delay_ms: DEFAULT_RESTART_DELAY_MS,
                    },
                });
            }
        };
        // `always 5` is a budget with the policy in the wrong place. Say so,
        // instead of silently accepting the policy and dropping the number.
        if !rest.trim().is_empty() {
            return Err(RestartError::UnknownPolicy { value: input });
        }
        let budget = match policy {
            Restart::Never => NEVER_BUDGET,
            _ => Budget {
                capacity: UNLIMITED_CAPACITY,
                window_ms: DEFAULT_RESTART_WINDOW_MS,
                delay_ms: DEFAULT_RESTART_DELAY_MS,
            },
        };
        Ok(RestartSpec { policy, budget })
    }

    /// The token bucket this spec spends from.
    ///
    /// The same value as the [`budget`](Self::budget) field and as
    /// [`Budget::from`]. Present because "give me the budget" and "give me the
    /// field" read very differently at a call site, and because a plan builder
    /// that has to write `spec.budget` in four places is a plan builder that
    /// will eventually write it in three.
    pub const fn into_budget(self) -> Budget {
        self.budget
    }

    /// The policy, without unpacking the budget.
    pub const fn policy(self) -> Restart {
        self.policy
    }
}

impl From<RestartSpec> for Budget {
    /// The token bucket. Exists so `let b: Budget = spec.into();` works at a
    /// call site that has a spec and wants a budget; identical to
    /// [`RestartSpec::into_budget`].
    fn from(spec: RestartSpec) -> Budget {
        spec.budget
    }
}

impl From<RestartSpec> for Restart {
    /// The policy alone. Exists so `let p: Restart = spec.into();` works;
    /// identical to [`RestartSpec::policy`].
    fn from(spec: RestartSpec) -> Restart {
        spec.policy
    }
}

/// Why a `restart` value failed to parse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RestartError<'a> {
    /// Not a known policy and not a budget expression.
    UnknownPolicy {
        /// The whole offending value.
        value: &'a str,
    },
    /// The budget expression has the wrong shape.
    BudgetFormat {
        /// The whole offending value.
        value: &'a str,
    },
    /// `0 restarts per 1s`.
    ///
    /// **Deliberately an error even though `zcore::Budget` treats capacity 0
    /// as "never".** In `zcore` that is a *runtime* state — a bucket that
    /// drained. In a *config file* it is almost always a typo (`05`? a
    /// substituted variable? a copy-paste of `restart = never` into the wrong
    /// directive?), and the two are indistinguishable at that point. The
    /// honest thing is to refuse and make the operator write what they meant;
    /// the bucket can still legitimately reach 0 at runtime, which is what
    /// `zctl kick` is for.
    ZeroCapacity {
        /// The whole offending value.
        value: &'a str,
    },
    /// `5 restarts per 0ms`: a zero-length window is not a budget.
    ZeroWindow {
        /// The whole offending value.
        value: &'a str,
    },
}

impl fmt::Display for RestartError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestartError::UnknownPolicy { value } => write!(
                f,
                "unknown restart policy `{value}`; expected `never`, \
                 `on-failure`, `always` or `<n> restarts per <duration>`"
            ),
            RestartError::BudgetFormat { value } => write!(
                f,
                "invalid restart budget `{value}`; expected exactly \
                 `<n> restarts per <duration>`, for example \
                 `5 restarts per 60s`"
            ),
            RestartError::ZeroCapacity { value } => write!(
                f,
                "`{value}` allows no restarts at all; `0 restarts` is read as a \
                 mistake in the number, not as a synonym for `restart = never`"
            ),
            RestartError::ZeroWindow { value } => write!(
                f,
                "`{value}` has a zero-length window; a budget needs a real \
                 window, for example `5 restarts per 60s`"
            ),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// user
// ─────────────────────────────────────────────────────────────────────────────

/// A validated `user` value: the numeric pair the plan actually stores.
///
/// **A bare `uid` means `uid:uid`.** The plan has no room for a gid of
/// "whatever the primary group happens to be", and guessing one here would be
/// a silent privilege bug in a service that writes to a group-owned
/// directory. An operator who wants the real primary group writes the *name*
/// (`user = www-data`), which is deferred to `zrt` — see [`RunAs::try_parse`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RunAs {
    /// Numeric user id. `zconfig` never resolves a *name* to one — see
    /// [`RunAs::try_parse`].
    pub uid: u32,
    /// Numeric group id. DESIGN.md §5 allows `uid:gid` with either side
    /// symbolic; zconfig takes the numeric sides and leaves the rest to zrt.
    pub gid: u32,
}

impl RunAs {
    /// Parse `uid`, `uid:gid`, `name` or `name:group`, deferring names.
    ///
    /// `name` and `name:group` are *well-formed but unresolvable here*, so they
    /// come back as `Ok(None)` rather than as an error. That distinction
    /// matters: `user = www-data` is a perfectly good description that `zrt`
    /// resolves against `/etc/passwd`, while `user = 1000 www-data` is a
    /// genuine mistake. zconfig has no libc and must not grow one to tell them
    /// apart.
    ///
    /// This is the entry point `zrt` uses: it takes the spec, resolves the
    /// names against `/etc/passwd`, and turns a `None` into a real uid. Keeping
    /// it here means the split syntax is parsed exactly once, in one place.
    /// A bare numeric uid does *not* produce a `None` — it is `uid:uid` here
    /// (see [`RunAs`]), which is the one convention this module does fix.
    pub fn try_parse(input: &str) -> Result<Option<RunAs>, RunAsError<'_>> {
        let (user, group) = match input.find(':') {
            Some(i) => (&input[..i], &input[i + 1..]),
            None => (input, ""),
        };
        // `1000:` is a typo, not "the user's primary group": a bare uid means
        // `uid:uid` (see `RunAs`), and it is written by writing just the uid.
        if user.is_empty() || (input.contains(':') && group.is_empty()) {
            return Err(RunAsError::Empty { value: input });
        }

        // Syntax of *both* sides before deciding anything: a typo on the group
        // side is a typo even when the user side is a name we cannot resolve
        // here, and saying so is more useful than deferring.
        let user_is_name = match classify(user) {
            Some(Token::Name) => true,
            Some(Token::Number) => false,
            None => return Err(RunAsError::NotANumber { name: user }),
        };
        let group_is_name = match group {
            "" => false,
            g => match classify(g) {
                Some(Token::Name) => true,
                Some(Token::Number) => false,
                None => return Err(RunAsError::GroupNotANumber { user, group: g }),
            },
        };
        if user_is_name || group_is_name {
            return Ok(None);
        }

        let uid = narrow_u32(user).ok_or(RunAsError::UidTooLarge { value: user })?;
        let gid = if group.is_empty() {
            uid
        } else {
            narrow_u32(group).ok_or(RunAsError::GidTooLarge { value: group })?
        };
        Ok(Some(RunAs { uid, gid }))
    }
}

/// Why a `user` value failed to parse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunAsError<'a> {
    /// `user = ` or `user = :group`.
    Empty {
        /// The whole offending value.
        value: &'a str,
    },
    /// `user = 1000 www-data`, `user = 1000abc`: a uid is either all digits or
    /// a name, never a mixture of the two.
    NotANumber {
        /// The offending token.
        name: &'a str,
    },
    /// The group side is neither digits nor a clean name.
    GroupNotANumber {
        /// The user side, for context.
        user: &'a str,
        /// The group side.
        group: &'a str,
    },
    /// A uid above `u32::MAX`.
    UidTooLarge {
        /// The offending token.
        value: &'a str,
    },
    /// A gid above `u32::MAX`.
    GidTooLarge {
        /// The offending token.
        value: &'a str,
    },
}

impl fmt::Display for RunAsError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunAsError::Empty { value } => {
                write!(
                    f,
                    "empty user in `{value}`: expected `user`, `uid` or `user:group`"
                )
            }
            RunAsError::NotANumber { name } => write!(
                f,
                "`{name}` is neither a user name nor a numeric uid; use \
                 `name:group`, `uid` or `uid:gid`"
            ),
            RunAsError::GroupNotANumber { user, group } => write!(
                f,
                "`{group}` is not a valid group for user `{user}`; expected \
                 `uid:gid` or `name:group`"
            ),
            RunAsError::UidTooLarge { value } => {
                write!(f, "uid `{value}` does not fit in a u32")
            }
            RunAsError::GidTooLarge { value } => {
                write!(f, "gid `{value}` does not fit in a u32")
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// log
// ─────────────────────────────────────────────────────────────────────────────

/// A validated `log` value.
///
/// A thin newtype over [`LogSink`] so that `zconfig::value` has its own name to
/// export and the conversion is explicit rather than something the parser
/// reaches for directly.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogSpec {
    /// The sink, fully resolved.
    pub sink: LogSink,
}

impl LogSpec {
    /// Parse `none`, `file`, `syslog` or a `file:` target.
    ///
    /// The `file:` target has a deliberately tiny grammar, built from the two
    /// separators DESIGN.md §5 allows (`:` and `/`):
    ///
    /// ```text
    /// file:<path>                             defaults for size and backups
    /// file:<path>:<max_bytes>                 e.g. sshd.log:1048576
    /// file:<path>:<max_bytes>/<backups>       e.g. sshd.log:1048576/5
    /// ```
    ///
    /// `<max_bytes>` is decimal, no unit suffix, no thousands separators — it
    /// is a byte count, and giving it a unit would mean parsing it as
    /// something else. `<backups>` is a plain count. The split is the **last**
    /// `:` in the value, so a path containing colons (`/var/log/we:ird.log`)
    /// keeps them; size and backups are split at the **last** `/`, so a path
    /// with no directory (`sshd.log:1024/3`) works. Anything else is an error
    /// rather than a guess, because guessing a log size wrong means silently
    /// filling a filesystem.
    ///
    /// **On non-UTF-8 paths:** there is nothing to check here, on purpose. A
    /// value reaches this function as `&str`; the lexer that produced it
    /// already rejected invalid UTF-8 at the file boundary, and this crate
    /// deliberately has no `OsStr`/`PathBuf` in it. A path that is valid UTF-8
    /// but illegal for the filesystem (`/proc/x`, a NUL) is a `zrt` problem,
    /// at open time, with a real errno to report.
    pub fn parse(input: &str) -> Result<LogSpec, LogError<'_>> {
        match LogSpec::try_parse(input)? {
            Some(spec) => Ok(spec),
            None => Err(LogError::EmptyPath { value: input }),
        }
    }

    /// Like [`LogSpec::parse`], but `Ok(None)` for `file:` with an empty path.
    /// The empty path is the one case `zrt` can repair — it knows the service
    /// name and can substitute the default `/var/log/zinit/<svc>.log`.
    pub fn try_parse(input: &str) -> Result<Option<LogSpec>, LogError<'_>> {
        if input == "none" {
            return Ok(Some(LogSpec {
                sink: LogSink::None,
            }));
        }
        if input == "syslog" {
            return Ok(Some(LogSpec {
                sink: LogSink::Syslog,
            }));
        }
        if input == "file" {
            return Ok(Some(LogSpec {
                sink: LogSink::File {
                    path: String::new(),
                    max_bytes: DEFAULT_LOG_MAX_BYTES,
                    backups: DEFAULT_LOG_BACKUPS,
                },
            }));
        }
        let (kind, rest) = split_surplus(input, b':');
        if kind != "file" {
            // `none:` and `syslog:` are the case worth separating. Reporting
            // them as "unknown log sink `none`" would name a sink that exists
            // and tell the operator to look for a typo in a word they spelled
            // correctly, which is the worst kind of message: unfalsifiable from
            // where the reader is standing.
            if matches!(kind, "none" | "syslog") {
                return Err(LogError::UnexpectedArgument { value: input });
            }
            return Err(LogError::UnknownSink {
                value: input,
                sink: kind,
            });
        }

        let (path, rest) = split_last(rest, b':');
        if path.is_empty() {
            return Ok(None);
        }
        let (max_bytes, backups) = if rest.is_empty() {
            (DEFAULT_LOG_MAX_BYTES, DEFAULT_LOG_BACKUPS)
        } else {
            let (size, backups) = split_last(rest, b'/');
            if !all_digits(size) {
                return Err(LogError::InvalidLimit {
                    value: input,
                    field: "max_bytes",
                });
            }
            let max_bytes = parse_u64(size).map_err(|_| LogError::InvalidLimit {
                value: input,
                field: "max_bytes",
            })?;
            // Zero would rotate on every single line, forever, at full speed.
            if max_bytes == 0 {
                return Err(LogError::InvalidLimit {
                    value: input,
                    field: "max_bytes",
                });
            }
            let backups = if backups.is_empty() {
                DEFAULT_LOG_BACKUPS
            } else {
                if !all_digits(backups) {
                    return Err(LogError::InvalidLimit {
                        value: input,
                        field: "backups",
                    });
                }
                narrow_u8(backups).ok_or(LogError::TooManyBackups {
                    value: input,
                    backups,
                })?
            };
            (max_bytes, backups)
        };
        Ok(Some(LogSpec {
            sink: LogSink::File {
                path: path.to_string(),
                max_bytes,
                backups,
            },
        }))
    }

    /// The sink.
    pub fn into_sink(self) -> LogSink {
        self.sink
    }

    /// The sink, borrowed.
    ///
    /// The `sink` field is public, so this is not the only way in; it exists
    /// because `plan.rs` and `zctl` reach for `into_sink()` on an owned spec
    /// and `.sink()` on a borrowed one, and spelling both the same way is worth
    /// more than the four characters it saves.
    pub const fn sink(&self) -> &LogSink {
        &self.sink
    }
}

impl Default for LogSpec {
    /// A `file` sink with an empty path and the documented rotation numbers —
    /// the same thing `desc.rs` writes into a `ServiceDesc::default()`. The
    /// path is empty rather than a guess: `zrt` is the only thing that knows
    /// the service name, so `/var/log/zinit/<svc>.log` is substituted there.
    fn default() -> LogSpec {
        LogSpec {
            sink: LogSink::File {
                path: String::new(),
                max_bytes: DEFAULT_LOG_MAX_BYTES,
                backups: DEFAULT_LOG_BACKUPS,
            },
        }
    }
}

impl From<LogSpec> for LogSink {
    /// The sink. Exists so `let s: LogSink = spec.into();` works; identical to
    /// [`LogSpec::into_sink`].
    fn from(spec: LogSpec) -> LogSink {
        spec.sink
    }
}

/// Why a `log` value failed to parse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LogError<'a> {
    /// Not `none`, `file` or `syslog`.
    UnknownSink {
        /// The whole offending value.
        value: &'a str,
        /// The part before the colon.
        sink: &'a str,
    },
    /// `none:` or `syslog:something`: a sink that is real, given an argument
    /// that means nothing. Distinct from [`LogError::UnknownSink`] precisely
    /// because "unknown sink `syslog`" is a false accusation about a word the
    /// operator spelled correctly.
    UnexpectedArgument {
        /// The whole offending value.
        value: &'a str,
    },
    /// `file:` with nothing after it.
    EmptyPath {
        /// The whole offending value.
        value: &'a str,
    },
    /// The size or the backup count is not a number.
    InvalidLimit {
        /// The whole offending value.
        value: &'a str,
        /// Which of the two numbers was wrong.
        field: &'static str,
    },
    /// More than 255 backups, which the `u8` in `LogSink` cannot hold.
    TooManyBackups {
        /// The whole offending value.
        value: &'a str,
        /// The offending count, as written.
        backups: &'a str,
    },
}

impl fmt::Display for LogError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogError::UnknownSink { value, sink } => write!(
                f,
                "unknown log sink `{sink}` in `{value}`; expected `none`, \
                 `file`, `file:<path>[:<max_bytes>[/<backups>]]` or `syslog`"
            ),
            LogError::UnexpectedArgument { value } => write!(
                f,
                "`{value}` takes no argument: only `file:` has one, so write \
                 `none` or `syslog` without the colon"
            ),
            LogError::EmptyPath { value } => write!(
                f,
                "`{value}` has no path: expected \
                 `file:<path>[:<max_bytes>[/<backups>]]`"
            ),
            LogError::InvalidLimit { value, field } => write!(
                f,
                "the {field} in `{value}` is not a number: expected \
                 `file:<path>:<max_bytes>[/<backups>]` with a decimal \
                 {field}"
            ),
            LogError::TooManyBackups { value, backups } => write!(
                f,
                "{backups} backups in `{value}` is out of range: the maximum \
                 is 255"
            ),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Listen
// ─────────────────────────────────────────────────────────────────────────────

/// A `listen` value that names no listenable thing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListenError<'a> {
    /// Empty value.
    Empty,
    /// Neither `tcp` nor `unix`.
    UnknownKind { value: &'a str, kind: &'a str },
    /// `tcp:` with nothing after it.
    EmptyPort { value: &'a str },
    /// `tcp:` with a non-numeric port.
    PortNotNumeric { value: &'a str, port: &'a str },
    /// `tcp:0`: port 0 listens nowhere, and a `listen = tcp:0` is far more
    /// often an unfilled variable than a real intent.
    PortZero { value: &'a str },
    /// Port past 65535.
    PortTooLarge { value: &'a str, port: &'a str },
    /// `unix:` with nothing after it.
    EmptyPath { value: &'a str },
    /// A `:name` that carries whitespace or another colon (names travel
    /// colon-joined in `$LISTEN_FDNAMES`, so either would corrupt the framing).
    BadName { value: &'a str, name: &'a str },
}

impl fmt::Display for ListenError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListenError::Empty => {
                write!(
                    f,
                    "listen needs a value: `tcp:<port>[:<name>]` or `unix:<path>`"
                )
            }
            ListenError::UnknownKind { kind, .. } => {
                write!(f, "unknown listen kind `{kind}`: write `tcp` or `unix`")
            }
            ListenError::EmptyPort { .. } => {
                write!(f, "tcp listen needs a port: `tcp:<port>[:<name>]`")
            }
            ListenError::PortNotNumeric { port, .. } => {
                write!(f, "port `{port}` is not a number: ports are 1-65535")
            }
            ListenError::PortZero { .. } => {
                write!(f, "port 0 listens nowhere: name the real port")
            }
            ListenError::PortTooLarge { port, .. } => {
                write!(f, "port `{port}` is past 65535")
            }
            ListenError::EmptyPath { .. } => {
                write!(f, "unix listen needs a path: `unix:<path>`")
            }
            ListenError::BadName { name, .. } => {
                write!(f, "listen name `{name}` must not carry whitespace or `:`")
            }
        }
    }
}

/// Parse `tcp:<port>[:<name>]` or `unix:<path>` into a [`ListenAddr`].
///
/// A free function rather than a method: `ListenAddr` lives in `zcore` (pure
/// data, no parsing), and an inherent impl here would break the orphan rule.
/// The name defaults to the spec text (`tcp:8080`, the path) and travels
/// in `$LISTEN_FDNAMES`. A unix path is never split for a name: `:` is a
/// legal filename character, and carving a name out of a path would
/// silently bind somewhere else. An explicit empty name (`tcp:80:`) takes
/// the default — harmless, and not worth a variant.
pub fn parse_listen(input: &str) -> Result<ListenAddr, ListenError<'_>> {
    let (head, tail) = split_surplus(input, b':');
    match head {
        "tcp" => {
            let (port_s, name) = split_surplus(tail, b':');
            if port_s.is_empty() {
                return Err(ListenError::EmptyPort { value: input });
            }
            if !all_digits(port_s) {
                return Err(ListenError::PortNotNumeric {
                    value: input,
                    port: port_s,
                });
            }
            let port = match parse_u64(port_s) {
                Ok(p) => p,
                // It is a number, just not one that fits anywhere near a port.
                Err(DurationError::NumberTooLarge { .. }) => {
                    return Err(ListenError::PortTooLarge {
                        value: input,
                        port: port_s,
                    });
                }
                Err(_) => {
                    return Err(ListenError::PortNotNumeric {
                        value: input,
                        port: port_s,
                    });
                }
            };
            if port == 0 {
                return Err(ListenError::PortZero { value: input });
            }
            if port > u64::from(u16::MAX) {
                return Err(ListenError::PortTooLarge {
                    value: input,
                    port: port_s,
                });
            }
            let name = listen_name(input, name, format!("tcp:{port}"))?;
            Ok(ListenAddr {
                spec: ListenSpec::TcpPort(port as u16),
                name,
            })
        }
        "unix" => {
            if tail.is_empty() {
                return Err(ListenError::EmptyPath { value: input });
            }
            Ok(ListenAddr {
                spec: ListenSpec::UnixPath(tail.to_string()),
                name: tail.to_string(),
            })
        }
        "" => Err(ListenError::Empty),
        other => Err(ListenError::UnknownKind {
            value: input,
            kind: other,
        }),
    }
}

/// Resolve a `tcp` name against its default: empty takes the default, anything
/// carrying framing characters is refused rather than smuggled into
/// `$LISTEN_FDNAMES`.
fn listen_name<'a>(
    input: &'a str,
    name: &'a str,
    default: String,
) -> Result<String, ListenError<'a>> {
    if name.is_empty() {
        return Ok(default);
    }
    if name
        .as_bytes()
        .iter()
        .any(|b| b.is_ascii_whitespace() || *b == b':')
    {
        return Err(ListenError::BadName { value: input, name });
    }
    Ok(name.to_string())
}

/// Resolve a Linux capability name to its number.
///
/// Accepts lowercase with underscores (`sys_admin`), with an optional `cap_`
/// prefix in any case (`CAP_SYS_ADMIN`). The numbers are stable across
/// architectures (linux/capability.h), which is what makes parse-time
/// validation sound — unlike syscall numbers, these do not move per arch.
pub fn parse_capability(input: &str) -> Option<u32> {
    let lower = input.to_ascii_lowercase();
    let name = lower.strip_prefix("cap_").unwrap_or(&lower);
    // The full set through CAP_CHECKPOINT_RESTORE (39). A name outside it is
    // either a typo or a kernel newer than this table, and both are refusals
    // upstream — never a silently skipped drop.
    Some(match name {
        "chown" => 0,
        "dac_override" => 1,
        "dac_read_search" => 2,
        "fowner" => 3,
        "fsetid" => 4,
        "kill" => 5,
        "setgid" => 6,
        "setuid" => 7,
        "setpcap" => 8,
        "linux_immutable" => 9,
        "net_bind_service" => 10,
        "net_broadcast" => 11,
        "net_admin" => 12,
        "net_raw" => 13,
        "ipc_lock" => 14,
        "ipc_owner" => 15,
        "sys_module" => 16,
        "sys_rawio" => 17,
        "sys_chroot" => 18,
        "sys_ptrace" => 19,
        "sys_pacct" => 20,
        "sys_admin" => 21,
        "sys_boot" => 22,
        "sys_nice" => 23,
        "sys_resource" => 24,
        "sys_time" => 25,
        "sys_tty_config" => 26,
        "mknod" => 27,
        "lease" => 28,
        "audit_write" => 29,
        "audit_control" => 30,
        "setfcap" => 31,
        "mac_override" => 32,
        "mac_admin" => 33,
        "syslog" => 34,
        "wake_alarm" => 35,
        "block_suspend" => 36,
        "audit_read" => 37,
        "perfmon" => 38,
        "bpf" => 39,
        "checkpoint_restore" => 40,
        _ => return None,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// ValueError
// ─────────────────────────────────────────────────────────────────────────────

/// Any failure to turn text into a directive value.
///
/// One enum over six parsers, because every one of them ends up in the same
/// place — a `Diagnostic` in a `DiagnosticBag` — and because a caller that
/// wants to report "bad value on line 12" should not have to know which of the
/// five grammars it came from. The variants keep the detail, so a caller that
/// *does* care can still match on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueError<'a> {
    /// A duration directive.
    Duration(DurationError<'a>),
    /// A `ready` directive.
    Ready(ReadyError<'a>),
    /// A `restart` or `restart-budget` directive.
    Restart(RestartError<'a>),
    /// A `user` directive.
    RunAs(RunAsError<'a>),
    /// A `log` directive.
    Log(LogError<'a>),
    /// A `listen` directive.
    Listen(ListenError<'a>),
}

impl<'a> ValueError<'a> {
    /// The same failure as a positioned [`Diagnostic`].
    ///
    /// Every one of these is an [`Diagnostic::error`]. Nothing in this module
    /// ever produces a warning, and that is deliberate: a value that did not
    /// parse has no defensible interpretation, so there is nothing to warn
    /// *about* — only to reject. (See `diagnostic.rs` for why warning severity
    /// must stay inert, and why this module is the last place that could break
    /// that rule by accident.)
    pub fn to_diagnostic(self, span: Span) -> Diagnostic {
        Diagnostic::error(Some(span), self.code(), self.to_string(), self.help())
    }

    /// The stable code for this failure.
    ///
    /// # The code space
    ///
    /// `E3xx`, and this module owns the whole block:
    ///
    /// | range     | directive(s)                        |
    /// |-----------|-------------------------------------|
    /// | `E3xx`    | this file, reserved — do not take it |
    /// | `E300`-`E304` | a duration (`stop-timeout`, `start-timeout`, `ready-timeout`, `restart-delay`, a budget window) |
    /// | `E310`-`E316` | `ready`                            |
    /// | `E320`-`E323` | `restart` / `restart-budget`       |
    /// | `E330`-`E334` | `user`                            |
    /// | `E340`-`E344` | `log`                              |
    /// | `E350`-`E357` | `listen`                           |
    ///
    /// `parser.rs` owns `E0xx` and `desc.rs` owns the `W0xx` warnings.
    ///
    /// **A code, once published, is frozen.** `zcheck` output gets grepped,
    /// pasted into bug reports and matched in CI; a number that starts meaning
    /// something else is worse than a number that is never emitted. New
    /// failures take the next free code in their range. A retired code is
    /// deleted, never recycled — two different errors sharing a code would
    /// break every tool that groups or suppresses by code, which is why
    /// `value_error_codes_are_unique_per_kind` and
    /// `value_error_codes_are_pinned` both exist.
    pub fn code(&self) -> &'static str {
        match self {
            ValueError::Duration(DurationError::Empty) => "E300",
            ValueError::Duration(DurationError::UnknownUnit { .. }) => "E301",
            ValueError::Duration(DurationError::InvalidNumber { .. }) => "E302",
            ValueError::Duration(DurationError::NumberTooLarge { .. }) => "E303",
            ValueError::Duration(DurationError::Overflow { .. }) => "E304",
            ValueError::Ready(ReadyError::EmptyProbe) => "E310",
            ValueError::Ready(ReadyError::UnknownKind { .. }) => "E311",
            ValueError::Ready(ReadyError::EmptyPort { .. }) => "E312",
            ValueError::Ready(ReadyError::UnexpectedArgument { .. }) => "E313",
            ValueError::Ready(ReadyError::PortNotNumeric { .. }) => "E314",
            ValueError::Ready(ReadyError::PortZero { .. }) => "E315",
            ValueError::Ready(ReadyError::PortTooLarge { .. }) => "E316",
            ValueError::Restart(RestartError::UnknownPolicy { .. }) => "E320",
            ValueError::Restart(RestartError::BudgetFormat { .. }) => "E321",
            ValueError::Restart(RestartError::ZeroCapacity { .. }) => "E322",
            ValueError::Restart(RestartError::ZeroWindow { .. }) => "E323",
            ValueError::RunAs(RunAsError::Empty { .. }) => "E330",
            ValueError::RunAs(RunAsError::NotANumber { .. }) => "E331",
            ValueError::RunAs(RunAsError::GroupNotANumber { .. }) => "E332",
            ValueError::RunAs(RunAsError::UidTooLarge { .. }) => "E333",
            ValueError::RunAs(RunAsError::GidTooLarge { .. }) => "E334",
            ValueError::Log(LogError::UnknownSink { .. }) => "E340",
            ValueError::Log(LogError::UnexpectedArgument { .. }) => "E341",
            ValueError::Log(LogError::EmptyPath { .. }) => "E342",
            ValueError::Log(LogError::InvalidLimit { .. }) => "E343",
            ValueError::Log(LogError::TooManyBackups { .. }) => "E344",
            ValueError::Listen(ListenError::Empty) => "E350",
            ValueError::Listen(ListenError::UnknownKind { .. }) => "E351",
            ValueError::Listen(ListenError::EmptyPort { .. }) => "E352",
            ValueError::Listen(ListenError::PortNotNumeric { .. }) => "E353",
            ValueError::Listen(ListenError::PortZero { .. }) => "E354",
            ValueError::Listen(ListenError::PortTooLarge { .. }) => "E355",
            ValueError::Listen(ListenError::EmptyPath { .. }) => "E356",
            ValueError::Listen(ListenError::BadName { .. }) => "E357",
        }
    }

    /// Actionable advice, where there is any.
    fn help(&self) -> Option<String> {
        match self {
            ValueError::Duration(DurationError::InvalidNumber { .. }) => {
                Some(String::from("write a plain decimal number, e.g. `30s`"))
            }
            ValueError::Duration(DurationError::UnknownUnit { .. }) => Some(String::from(
                "units are `ms`, `s`, `m`, `h`, `d`; a number with no unit is seconds",
            )),
            ValueError::Duration(DurationError::Overflow { .. }) => Some(String::from(
                "pick a smaller duration or a coarser unit (`h`, `d`)",
            )),
            ValueError::Ready(ReadyError::EmptyProbe) => Some(String::from(
                "`ready = none` if the service has no handshake; that is always allowed",
            )),
            ValueError::Ready(ReadyError::PortZero { .. }) => Some(String::from(
                "if the port comes from the environment, there is no `ready` probe that works before the service binds it",
            )),
            ValueError::Ready(ReadyError::PortTooLarge { .. }) => {
                Some(String::from("a TCP port is 1-65535"))
            }
            ValueError::Restart(RestartError::ZeroCapacity { .. }) => Some(String::from(
                "if you meant `never`, use the `restart` directive: `restart = never`",
            )),
            ValueError::Log(LogError::UnexpectedArgument { .. }) => Some(String::from(
                "`log = none` and `log = syslog` take nothing after the colon",
            )),
            ValueError::Listen(ListenError::PortZero { .. }) => Some(String::from(
                "if the port comes from the environment, no probe can work yet",
            )),
            ValueError::Listen(ListenError::PortTooLarge { .. }) => {
                Some(String::from("a TCP port is 1-65535"))
            }
            _ => None,
        }
    }
}

impl fmt::Display for ValueError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValueError::Duration(e) => e.fmt(f),
            ValueError::Ready(e) => e.fmt(f),
            ValueError::Restart(e) => e.fmt(f),
            ValueError::RunAs(e) => e.fmt(f),
            ValueError::Log(e) => e.fmt(f),
            ValueError::Listen(e) => e.fmt(f),
        }
    }
}

// Lifting each sub-error into the umbrella, so a caller can `?` it without
// wrapping by hand. A macro because the five impls differ only in a type name —
// and because the lifetime has to be *named* here, which is exactly what
// `clippy::needless_lifetimes` would otherwise "fix" into a type error.
macro_rules! lift_into_value_error {
    ($ty:ident => $variant:ident) => {
        impl<'a> From<$ty<'a>> for ValueError<'a> {
            fn from(e: $ty<'a>) -> ValueError<'a> {
                ValueError::$variant(e)
            }
        }
    };
}

lift_into_value_error!(DurationError => Duration);
lift_into_value_error!(ReadyError => Ready);
lift_into_value_error!(RestartError => Restart);
lift_into_value_error!(RunAsError => RunAs);
lift_into_value_error!(LogError => Log);
lift_into_value_error!(ListenError => Listen);

// ─────────────────────────────────────────────────────────────────────────────
// Byte scanners
//
// All of them take `&str` and only ever split on ASCII, so a slice boundary is
// always a character boundary: a UTF-8 continuation byte is >= 0x80 and can
// never be one of the separators we look for.
// ─────────────────────────────────────────────────────────────────────────────

/// True when `s` is non-empty and every byte is an ASCII digit.
///
/// This is the check that keeps a `str` parser honest: a Unicode digit such as
/// `١٢` is *not* accepted, where `char::to_digit` would have quietly accepted
/// it and `str::parse` would have accepted `"+10"`. Refusing is right, because
/// the file is bytes and the operator's editor wrote ASCII.
fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.as_bytes().iter().all(u8::is_ascii_digit)
}

/// Parse a run of ASCII digits, reporting which of the two ways it failed.
///
/// The gate is [`all_digits`] and not `str::parse`'s own idea of a number, so
/// the two callers that fold the answer into a narrower type reject exactly
/// what they rejected before.
pub(crate) fn parse_u64(s: &str) -> Result<u64, DurationError<'_>> {
    if !all_digits(s) {
        return Err(DurationError::InvalidNumber { value: s });
    }
    s.parse::<u64>()
        .map_err(|_| DurationError::NumberTooLarge { value: s })
}

/// Parse a run of ASCII digits that must fit in a `u32`.
fn narrow_u32(s: &str) -> Option<u32> {
    u32::try_from(parse_u64(s).ok()?).ok()
}

/// Parse a run of ASCII digits that must fit in a `u8`.
fn narrow_u8(s: &str) -> Option<u8> {
    u8::try_from(parse_u64(s).ok()?).ok()
}

/// One side of a `user` value, as far as this crate can tell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Token {
    /// A number: fully resolvable here, right now, with no libc.
    Number,
    /// A symbolic name that `zrt` will have to look up in `/etc/passwd`.
    Name,
}

/// Classify one side of a `user` value, or `None` when it is neither a number
/// nor a name that could ever appear in `/etc/passwd`.
///
/// The name test is deliberately narrow, and the first-byte test is the load
/// bearing half. POSIX says a login name starts with a letter or `_`, so
/// `1000abc` cannot be one, and classifying it as "a name, defer to zrt" would
/// hand a typo to a component that has no way to report a syntax error — it
/// would just say "no such user", which is false. `www-data` is not a typo and
/// must survive this function untouched.
fn classify(s: &str) -> Option<Token> {
    if all_digits(s) {
        return Some(Token::Number);
    }
    let mut bytes = s.as_bytes().iter();
    let first = *bytes.next()?;
    // A leading digit is a number with something glued to it, not a name.
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    if bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'$')) {
        return Some(Token::Name);
    }
    None
}

/// Milliseconds in one of the five units. `None` for anything else.
///
/// The units, lowercase and closed, are the same five DESIGN.md §5 documents;
/// adding a sixth here is a change to the format, not to this function.
fn unit_scale(unit: &str) -> Option<u64> {
    Some(match unit.as_bytes() {
        b"ms" => 1,
        b"s" => MS_PER_SECOND,
        b"m" => MS_PER_MINUTE,
        b"h" => MS_PER_HOUR,
        b"d" => MS_PER_DAY,
        _ => return None,
    })
}

/// Split at the first `sep`, returning `(head, tail)`; `tail` is `""` when the
/// separator is absent.
fn split_surplus(s: &str, sep: u8) -> (&str, &str) {
    match s.as_bytes().iter().position(|&b| b == sep) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    }
}

/// Split at the **last** `sep`, returning `(head, tail)`. `tail` is `""` when
/// the separator is absent.
fn split_last(s: &str, sep: u8) -> (&str, &str) {
    match s.as_bytes().iter().rposition(|&b| b == sep) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    }
}

/// Parse `<n> restarts per <duration>` with flexible interior whitespace.
fn parse_budget(input: &str) -> Result<(u32, u64), RestartError<'_>> {
    let words: Vec<&str> = input.split_ascii_whitespace().collect();
    if words.len() != 4 || words[1] != "restarts" || words[2] != "per" {
        return Err(RestartError::BudgetFormat { value: input });
    }
    let capacity = narrow_u32(words[0]).ok_or(RestartError::BudgetFormat { value: input })?;
    if capacity == 0 {
        return Err(RestartError::ZeroCapacity { value: input });
    }
    let window =
        parse_duration(words[3]).map_err(|_| RestartError::BudgetFormat { value: input })?;
    if window == 0 {
        return Err(RestartError::ZeroWindow { value: input });
    }
    Ok((capacity, window))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// One row of a rejection table: the input, and a predicate that says which
    /// *category* of failure it must produce. Asserting on the category and not
    /// just "it failed" is the point: an error message that names the wrong
    /// problem sends the operator to fix the wrong thing.
    type Case<'a> = (&'a str, fn(&ValueError<'a>) -> bool);

    /// Every spelling `parse_duration` promises to accept, with its value.
    const VALID_DURATIONS: &[(&str, u64)] = &[
        ("10s", 10_000),
        ("500ms", 500),
        ("999ms", 999),
        ("2m", 120_000),
        ("1h", 3_600_000),
        ("1d", 86_400_000),
        ("250", 250_000),
        ("0", 0),
        ("0ms", 0),
        ("18446744073709551615ms", u64::MAX),
    ];

    #[test]
    fn duration_accepts_every_documented_spelling() {
        for &(text, ms) in VALID_DURATIONS {
            match parse_duration(text) {
                Ok(got) => assert_eq!(got, ms, "{text:?}"),
                Err(e) => panic!("{text:?} should parse, got: {e}"),
            }
        }
    }

    #[test]
    fn duration_bare_number_is_seconds() {
        // The classic trap: `250` is 250 *seconds*, not 250ms.
        assert_eq!(parse_duration("250").unwrap(), 250_000);
        assert_ne!(parse_duration("250").unwrap(), 250);
    }

    #[test]
    fn duration_rejects_the_bad_spellings_with_the_right_category() {
        let cases: &[Case<'_>] = &[
            ("", |e| {
                matches!(e, ValueError::Duration(DurationError::Empty))
            }),
            ("  ", |e| {
                matches!(e, ValueError::Duration(DurationError::InvalidNumber { .. }))
            }),
            ("0x10", |e| {
                matches!(e, ValueError::Duration(DurationError::InvalidNumber { .. }))
            }),
            ("10 s", |e| {
                matches!(e, ValueError::Duration(DurationError::UnknownUnit { .. }))
            }),
            ("10sec", |e| {
                matches!(e, ValueError::Duration(DurationError::UnknownUnit { .. }))
            }),
            ("10S", |e| {
                matches!(e, ValueError::Duration(DurationError::UnknownUnit { .. }))
            }),
            ("-5", |e| {
                matches!(e, ValueError::Duration(DurationError::InvalidNumber { .. }))
            }),
            ("+5s", |e| {
                matches!(e, ValueError::Duration(DurationError::InvalidNumber { .. }))
            }),
            ("1.5s", |e| {
                matches!(e, ValueError::Duration(DurationError::InvalidNumber { .. }))
            }),
            ("10s5", |e| {
                matches!(e, ValueError::Duration(DurationError::InvalidNumber { .. }))
            }),
            ("10m30s", |e| {
                matches!(e, ValueError::Duration(DurationError::InvalidNumber { .. }))
            }),
            ("abc", |e| {
                matches!(e, ValueError::Duration(DurationError::InvalidNumber { .. }))
            }),
            ("10w", |e| {
                matches!(e, ValueError::Duration(DurationError::UnknownUnit { .. }))
            }),
            ("99999999999999999999h", |e| {
                matches!(
                    e,
                    ValueError::Duration(DurationError::NumberTooLarge { .. })
                )
            }),
            ("18446744073709551615s", |e| {
                matches!(e, ValueError::Duration(DurationError::Overflow { .. }))
            }),
            ("١٢s", |e| {
                matches!(e, ValueError::Duration(DurationError::InvalidNumber { .. }))
            }),
        ];
        for &(text, matches) in cases {
            let err = match parse_duration(text) {
                Ok(_) => panic!("{text:?} must be rejected"),
                Err(e) => ValueError::from(e),
            };
            assert!(matches(&err), "{text:?} produced the wrong category: {err}");
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn duration_never_trips_over_a_compound_spelling() {
        // `10m30s` must not silently parse as `10m`; the whole tail is the
        // unit, and it is not one.
        assert!(parse_duration("10m30s").is_err());
        assert!(parse_duration("1h30m").is_err());
    }

    #[test]
    fn duration_bare_number_saturates_instead_of_panicking() {
        // `18446744073709551615` as bare seconds cannot fit in ms. It must not
        // abort the parser: a config file must never be able to panic the
        // checker.
        let d = parse_duration("18446744073709551615").unwrap();
        assert_eq!(d, u64::MAX);
    }

    #[test]
    fn duration_is_plain_integer_milliseconds() {
        // The representation is a `u64`, so two spellings of the same length
        // compare as the numbers they are and nothing else.
        assert!(parse_duration("1s").unwrap() > parse_duration("999ms").unwrap());
        assert_eq!(parse_duration("1m").unwrap(), 60_000);
        assert_eq!(
            parse_duration("1m").unwrap(),
            parse_duration("60000ms").unwrap()
        );
    }

    #[test]
    fn ready_accepts_the_four_adapters() {
        assert_eq!(ReadySpec::parse("none").unwrap().kind, ReadySpecKind::None);
        assert_eq!(
            ReadySpec::parse("notify").unwrap().kind,
            ReadySpecKind::Notify
        );
        let ping = ReadySpec::parse("ping:/usr/bin/check -q").unwrap();
        assert_eq!(ping.kind, ReadySpecKind::Ping);
        assert_eq!(ping.arg, "/usr/bin/check -q");
        let tcp = ReadySpec::parse("tcp:22").unwrap();
        assert_eq!(tcp.kind, ReadySpecKind::Tcp);
        assert_eq!(tcp.port(), Some(22));
    }

    #[test]
    fn ready_always_maps_to_a_strict_handshake_except_none() {
        // DESIGN.md §6: expiry must never block the boot.
        assert_eq!(ReadySpec::parse("none").unwrap().into_ready(), Ready::None);
        assert_eq!(
            ReadySpec::parse("notify").unwrap().into_ready(),
            Ready::Strict(StrictReady::Notify)
        );
        assert_eq!(
            ReadySpec::parse("ping:check").unwrap().into_ready(),
            Ready::Strict(StrictReady::Ping("check".to_string()))
        );
        assert_eq!(
            ReadySpec::parse("tcp:8080").unwrap().into_ready(),
            Ready::Strict(StrictReady::Tcp(8080))
        );
        // And never a bare, non-gating Ready.
        let r = ReadySpec::parse("tcp:8080").unwrap().into_ready();
        assert!(r.is_strict());
        assert!(
            !ReadySpec::parse("none")
                .unwrap()
                .into_ready()
                .is_handshake()
        );
    }

    #[test]
    fn ready_rejects_bad_probes() {
        let cases: &[Case<'_>] = &[
            ("", |e| {
                matches!(e, ValueError::Ready(ReadyError::EmptyProbe))
            }),
            ("ping:", |e| {
                matches!(e, ValueError::Ready(ReadyError::EmptyProbe))
            }),
            ("ping:   ", |e| {
                matches!(e, ValueError::Ready(ReadyError::EmptyProbe))
            }),
            ("tcp:", |e| {
                matches!(e, ValueError::Ready(ReadyError::EmptyPort { .. }))
            }),
            ("tcp:0", |e| {
                matches!(e, ValueError::Ready(ReadyError::PortZero { .. }))
            }),
            ("tcp:65536", |e| {
                matches!(e, ValueError::Ready(ReadyError::PortTooLarge { .. }))
            }),
            ("tcp:99999", |e| {
                matches!(e, ValueError::Ready(ReadyError::PortTooLarge { .. }))
            }),
            ("tcp:http", |e| {
                matches!(e, ValueError::Ready(ReadyError::PortNotNumeric { .. }))
            }),
            ("tcp:-1", |e| {
                matches!(e, ValueError::Ready(ReadyError::PortNotNumeric { .. }))
            }),
            ("xdp:22", |e| {
                matches!(e, ValueError::Ready(ReadyError::UnknownKind { .. }))
            }),
            ("notify:yes", |e| {
                matches!(e, ValueError::Ready(ReadyError::UnexpectedArgument { .. }))
            }),
        ];
        for &(text, matches) in cases {
            let err = match ReadySpec::parse(text) {
                Ok(_) => panic!("{text:?} must be rejected"),
                Err(e) => ValueError::from(e),
            };
            assert!(matches(&err), "{text:?} produced the wrong category: {err}");
        }
    }

    #[test]
    fn ready_accepts_the_edges_of_the_port_range() {
        assert_eq!(ReadySpec::parse("tcp:1").unwrap().port(), Some(1));
        assert_eq!(ReadySpec::parse("tcp:65535").unwrap().port(), Some(65535));
    }

    #[test]
    fn restart_accepts_policies_and_budgets() {
        assert_eq!(RestartSpec::parse("never").unwrap().policy, Restart::Never);
        assert_eq!(
            RestartSpec::parse("on-failure").unwrap().policy,
            Restart::OnFailure
        );
        assert_eq!(
            RestartSpec::parse("always").unwrap().policy,
            Restart::Always
        );

        let b = RestartSpec::parse("5 restarts per 60s").unwrap();
        assert_eq!(b.policy, Restart::OnFailure);
        assert_eq!(b.budget.capacity, 5);
        assert_eq!(b.budget.window_ms, 60_000);
        assert_eq!(b.budget.delay_ms, DEFAULT_RESTART_DELAY_MS);

        // Flexible interior whitespace is a courtesy, not a rule. The window
        // obeys the same bare-number-means-seconds rule as every duration.
        let loose = RestartSpec::parse("  10   restarts\tper   2m ").unwrap();
        assert_eq!(loose.budget.capacity, 10);
        assert_eq!(loose.budget.window_ms, 120_000);
        assert_eq!(
            RestartSpec::parse("3 restarts per 1")
                .unwrap()
                .budget
                .window_ms,
            1_000
        );
    }

    #[test]
    fn restart_rejects_malformed_values() {
        let cases: &[Case<'_>] = &[
            ("never 5", |e| {
                matches!(e, ValueError::Restart(RestartError::UnknownPolicy { .. }))
            }),
            ("always 5", |e| {
                matches!(e, ValueError::Restart(RestartError::UnknownPolicy { .. }))
            }),
            ("5 restart per 1m", |e| {
                matches!(e, ValueError::Restart(RestartError::BudgetFormat { .. }))
            }),
            ("5 restarts in 1m", |e| {
                matches!(e, ValueError::Restart(RestartError::BudgetFormat { .. }))
            }),
            ("5 restarts per", |e| {
                matches!(e, ValueError::Restart(RestartError::BudgetFormat { .. }))
            }),
            ("0 restarts per 1s", |e| {
                matches!(e, ValueError::Restart(RestartError::ZeroCapacity { .. }))
            }),
            ("5 restarts per 0ms", |e| {
                matches!(e, ValueError::Restart(RestartError::ZeroWindow { .. }))
            }),
            ("0 restarts per 0s", |e| {
                matches!(e, ValueError::Restart(RestartError::ZeroCapacity { .. }))
            }),
            ("4294967296 restarts per 1s", |e| {
                matches!(e, ValueError::Restart(RestartError::BudgetFormat { .. }))
            }),
            ("whatever", |e| {
                matches!(e, ValueError::Restart(RestartError::UnknownPolicy { .. }))
            }),
            ("", |e| {
                matches!(e, ValueError::Restart(RestartError::UnknownPolicy { .. }))
            }),
        ];
        for &(text, matches) in cases {
            let err = match RestartSpec::parse(text) {
                Ok(_) => panic!("{text:?} must be rejected"),
                Err(e) => ValueError::from(e),
            };
            assert!(matches(&err), "{text:?} produced the wrong category: {err}");
        }
    }

    #[test]
    fn runas_parses_the_numeric_forms() {
        assert_eq!(
            RunAs::try_parse("0").unwrap(),
            Some(RunAs { uid: 0, gid: 0 })
        );
        assert_eq!(
            RunAs::try_parse("1000:1000").unwrap(),
            Some(RunAs {
                uid: 1000,
                gid: 1000
            })
        );
        assert_eq!(
            RunAs::try_parse("4294967295").unwrap().map(|r| r.uid),
            Some(u32::MAX)
        );
    }

    #[test]
    fn runas_rejects_broken_syntax() {
        let cases: &[Case<'_>] = &[
            ("", |e| {
                matches!(e, ValueError::RunAs(RunAsError::Empty { .. }))
            }),
            (":www-data", |e| {
                matches!(e, ValueError::RunAs(RunAsError::Empty { .. }))
            }),
            ("1000:", |e| {
                matches!(e, ValueError::RunAs(RunAsError::Empty { .. }))
            }),
            ("4294967296", |e| {
                matches!(e, ValueError::RunAs(RunAsError::UidTooLarge { .. }))
            }),
            ("0:4294967296", |e| {
                matches!(e, ValueError::RunAs(RunAsError::GidTooLarge { .. }))
            }),
            ("0:4294967296x", |e| {
                matches!(e, ValueError::RunAs(RunAsError::GroupNotANumber { .. }))
            }),
            ("1000 users", |e| {
                matches!(e, ValueError::RunAs(RunAsError::NotANumber { .. }))
            }),
            ("root:1000 users", |e| {
                matches!(e, ValueError::RunAs(RunAsError::GroupNotANumber { .. }))
            }),
            ("1000abc", |e| {
                matches!(e, ValueError::RunAs(RunAsError::NotANumber { .. }))
            }),
        ];
        for &(text, matches) in cases {
            let err = match RunAs::try_parse(text) {
                Ok(_) => panic!("{text:?} must be rejected"),
                Err(e) => ValueError::from(e),
            };
            assert!(matches(&err), "{text:?} produced the wrong category: {err}");
        }
    }

    #[test]
    fn runas_names_that_could_exist_are_deferred_not_rejected() {
        for text in [
            "root",
            "_svc",
            "www-data",
            "svc.local",
            "a$b",
            "root:adm",
            "1000:users",
            "1000:wheel",
        ] {
            assert!(matches!(RunAs::try_parse(text), Ok(None)), "{text:?}");
        }
    }

    #[test]
    fn logsink_accepts_the_three_sinks_and_the_path_forms() {
        assert_eq!(LogSpec::parse("none").unwrap().sink, LogSink::None);
        assert_eq!(LogSpec::parse("syslog").unwrap().sink, LogSink::Syslog);
        assert_eq!(
            LogSpec::parse("file").unwrap().sink,
            LogSink::File {
                path: String::new(),
                max_bytes: DEFAULT_LOG_MAX_BYTES,
                backups: DEFAULT_LOG_BACKUPS
            }
        );
        assert_eq!(
            LogSpec::parse("file:/var/log/zinit/sshd.log").unwrap().sink,
            LogSink::File {
                path: "/var/log/zinit/sshd.log".to_string(),
                max_bytes: DEFAULT_LOG_MAX_BYTES,
                backups: DEFAULT_LOG_BACKUPS
            }
        );
        assert_eq!(
            LogSpec::parse("file:/var/log/sshd.log:2048/5")
                .unwrap()
                .sink,
            LogSink::File {
                path: "/var/log/sshd.log".to_string(),
                max_bytes: 2048,
                backups: 5
            }
        );
    }

    #[test]
    fn logsink_rejects_empty_paths_and_bad_limits() {
        let cases: &[Case<'_>] = &[
            ("", |e| {
                matches!(e, ValueError::Log(LogError::UnknownSink { .. }))
            }),
            ("stderr", |e| {
                matches!(e, ValueError::Log(LogError::UnknownSink { .. }))
            }),
            ("file:", |e| {
                matches!(e, ValueError::Log(LogError::EmptyPath { .. }))
            }),
            ("file::1024", |e| {
                matches!(e, ValueError::Log(LogError::EmptyPath { .. }))
            }),
            ("file:/l.log:0", |e| {
                matches!(e, ValueError::Log(LogError::InvalidLimit { .. }))
            }),
            ("file:/l.log:big", |e| {
                matches!(e, ValueError::Log(LogError::InvalidLimit { .. }))
            }),
            ("file:/l.log:1024/x", |e| {
                matches!(e, ValueError::Log(LogError::InvalidLimit { .. }))
            }),
        ];
        for &(text, matches) in cases {
            let err = match LogSpec::parse(text) {
                Ok(_) => panic!("{text:?} must be rejected"),
                Err(e) => ValueError::from(e),
            };
            assert!(matches(&err), "{text:?} produced the wrong category: {err}");
        }
    }

    #[test]
    fn logsink_backups_above_255_is_its_own_error() {
        // 256 does not fit the `u8` in `zcore::LogSink`, so it must be reported
        // rather than silently wrapped to 0.
        match LogSpec::parse("file:/l.log:1024/256") {
            Ok(_) => panic!("256 backups must be rejected, not wrapped to 0"),
            Err(LogError::TooManyBackups { backups, .. }) => assert_eq!(backups, "256"),
            Err(other) => panic!("expected TooManyBackups, got {other}"),
        }
        // 255 is the last one that fits.
        assert!(LogSpec::parse("file:/l.log:1024/255").is_ok());
    }

    #[test]
    fn logsink_keeps_colons_inside_the_path() {
        let spec = LogSpec::parse("file:/var/log/we:ird.log:1024").unwrap();
        assert_eq!(
            spec.sink,
            LogSink::File {
                path: "/var/log/we:ird.log".to_string(),
                max_bytes: 1024,
                backups: DEFAULT_LOG_BACKUPS
            }
        );
    }

    #[test]
    fn every_value_error_becomes_a_fatal_diagnostic_with_position() {
        let span = Span::range(7, 3, 8);
        let failures: Vec<ValueError<'_>> = alloc::vec![
            parse_duration("nope")
                .map_err(ValueError::from)
                .unwrap_err(),
            ReadySpec::parse("tcp:0")
                .map_err(ValueError::from)
                .unwrap_err(),
            RestartSpec::parse("0 restarts per 1s")
                .map_err(ValueError::from)
                .unwrap_err(),
            RunAs::try_parse("1000 users")
                .map_err(ValueError::from)
                .unwrap_err(),
            LogSpec::parse("file:")
                .map_err(ValueError::from)
                .unwrap_err(),
        ];
        for e in failures {
            let d = e.to_diagnostic(span);
            assert_eq!(d.span, Some(span));
            assert!(d.code.starts_with('E'));
            assert!(d.is_error(), "value errors are always fatal: {}", d.message);
            assert!(!d.message.is_empty());
            assert!(d.render("svc.conf").contains("svc.conf:7:3:"));
        }
    }

    /// One representative of every *kind* of failure, and the guarantee that a
    /// machine-readable code identifies exactly one of them. Two failures
    /// sharing a code would break any tooling that groups or suppresses by
    /// code, so this is checked rather than assumed.
    #[test]
    fn value_error_codes_are_unique_per_kind() {
        let codes: Vec<&str> = alloc::vec![
            ValueError::from(parse_duration("").unwrap_err()).code(),
            ValueError::from(parse_duration("10q").unwrap_err()).code(),
            ValueError::from(parse_duration("1.5s").unwrap_err()).code(),
            ValueError::from(ReadySpec::parse("").unwrap_err()).code(),
            ValueError::from(ReadySpec::parse("nope").unwrap_err()).code(),
            ValueError::from(RestartSpec::parse("0 restarts per 1s").unwrap_err()).code(),
            ValueError::from(RunAs::try_parse("root:1000 users").unwrap_err()).code(),
            ValueError::from(LogSpec::parse("file:").unwrap_err()).code(),
        ];
        let mut sorted = codes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            codes.len(),
            "codes must not collide: {codes:?}"
        );
    }

    #[test]
    fn helpers_behave() {
        assert!(all_digits("0123456789"));
        assert!(!all_digits(""));
        assert!(!all_digits("12a"));
        assert!(!all_digits("١٢"));
        assert_eq!(unit_scale("ms"), Some(1));
        assert_eq!(unit_scale("d"), Some(86_400_000));
        assert_eq!(unit_scale("w"), None);
        assert_eq!(split_last("a:b:c", b':'), ("a:b", "c"));
        assert_eq!(split_surplus("a:b:c", b':'), ("a", "b:c"));
        assert_eq!(split_last("nocolon", b':'), ("nocolon", ""));
    }

    #[test]
    fn every_display_lists_an_expectation() {
        // A message with no expectation in it is a message the operator cannot
        // act on; this table is the guard against that regressing.
        let messages: Vec<String> = alloc::vec![
            parse_duration("10w").unwrap_err().to_string(),
            parse_duration("0x10").unwrap_err().to_string(),
            ReadySpec::parse("tcp:70000").unwrap_err().to_string(),
            RestartSpec::parse("5 restart per 1m")
                .unwrap_err()
                .to_string(),
            RunAs::try_parse("1000 users").unwrap_err().to_string(),
            LogSpec::parse("stderr").unwrap_err().to_string(),
            parse_listen("blah").unwrap_err().to_string(),
            parse_listen("tcp:0").unwrap_err().to_string(),
        ];
        for m in messages {
            assert!(m.len() > 20, "message is not actionable: {m}");
        }
    }

    /// A legal line whose budget is the one a bare `on-failure` gets: the
    /// operator wrote a policy in full, so they did not also get a second limit
    /// they did not ask for.
    #[test]
    fn an_unwritable_budget_is_the_same_budget_as_a_bare_policy() {
        let unwritable = RestartSpec::parse("4294967295 restarts per 60s").unwrap();
        assert_eq!(unwritable.budget.capacity, UNLIMITED_CAPACITY);
        assert_eq!(
            unwritable.budget,
            RestartSpec::parse("on-failure").unwrap().budget
        );
    }

    #[test]
    fn duration_does_not_trim_or_case_fold() {
        // A leading space is a lexer bug, not something to salvage silently.
        assert!(parse_duration(" 10s").is_err());
        assert!(parse_duration("10s ").is_err());
        assert!(parse_duration("10S").is_err());
        assert!(parse_duration("10MS").is_err());
        assert!(parse_duration("10D").is_err());
        // Leading zeros are fine: they are still ASCII digits, and a shell
        // substitution that produces `007` did not do anything wrong.
        assert_eq!(parse_duration("007s").unwrap(), 7_000);
        assert_eq!(parse_duration("000").unwrap(), 0);
    }

    #[test]
    fn duration_overflow_is_caught_at_both_ends() {
        // The digits do not fit in a u64 at all.
        assert!(matches!(
            parse_duration("99999999999999999999999ms"),
            Err(DurationError::NumberTooLarge { .. })
        ));
        // They fit, but the product does not.
        assert!(matches!(
            parse_duration("18446744073709551615d"),
            Err(DurationError::Overflow { .. })
        ));
        // The largest value that *is* representable is still accepted.
        assert_eq!(parse_duration("18446744073709551615ms").unwrap(), u64::MAX);
    }

    // ── Ready ───────────────────────────────────────────────────────────────

    #[test]
    fn ready_default_is_none() {
        let d = ReadySpec::default();
        assert_eq!(d, ReadySpec::parse("none").unwrap());
        assert_eq!(d.kind, ReadySpecKind::None);
        assert!(d.arg.is_empty());
        assert!(!d.is_handshake());
        assert_eq!(d.into_ready(), Ready::None);
        assert_eq!(ReadySpecKind::default(), ReadySpecKind::None);
    }

    #[test]
    fn ready_describe_and_display_round_trip() {
        for text in [
            "none",
            "notify",
            "ping:/usr/bin/true",
            "ping:check -q",
            "tcp:1",
            "tcp:65535",
        ] {
            let spec = ReadySpec::parse(text).unwrap();
            assert_eq!(spec.describe(), text);
            assert_eq!(spec.to_string(), text);
            assert_eq!(ReadySpec::parse(&spec.to_string()).unwrap(), spec);
        }
    }

    #[test]
    fn ready_port_and_command_are_owned_by_their_adapter() {
        let tcp = ReadySpec::parse("tcp:8080").unwrap();
        assert_eq!(tcp.port(), Some(8080));
        assert_eq!(tcp.command(), None);
        let ping = ReadySpec::parse("ping:/bin/true").unwrap();
        assert_eq!(ping.command(), Some("/bin/true"));
        assert_eq!(ping.port(), None);
        // A non-tcp spec has a port of `None` for a *different* reason than a
        // broken one does, and `is_handshake` tells the two apart up front.
        for spec in [ReadySpec::parse("notify").unwrap(), ping.clone()] {
            assert_eq!(spec.port(), None);
            assert!(spec.is_handshake());
        }
        assert!(!ReadySpec::parse("none").unwrap().is_handshake());
    }

    #[test]
    fn ready_into_ready_does_not_panic_on_a_hand_built_spec() {
        // The fields are public, so this is constructible. An init that aborts
        // because a caller assembled a struct by hand is worse than one that
        // waits for a port nobody is listening on.
        let hand = ReadySpec {
            kind: ReadySpecKind::Tcp,
            arg: "http".to_string(),
        };
        assert_eq!(hand.port(), None);
        assert_eq!(hand.into_ready(), Ready::Strict(StrictReady::Tcp(0)));
        let zero = ReadySpec {
            kind: ReadySpecKind::Tcp,
            arg: "0".to_string(),
        };
        assert_eq!(zero.port(), None);
        let huge = ReadySpec {
            kind: ReadySpecKind::Tcp,
            arg: "70000".to_string(),
        };
        assert_eq!(huge.port(), None);
    }

    #[test]
    fn ready_tcp_keeps_the_port_as_written() {
        // `arg` is text, not a number, so a diagnostic can quote exactly what
        // the operator wrote. Leading zeros survive that promise.
        assert_eq!(ReadySpec::parse("tcp:0080").unwrap().arg, "0080");
        assert_eq!(ReadySpec::parse("tcp:0080").unwrap().port(), Some(80));
    }

    // ── Restart ─────────────────────────────────────────────────────────────

    #[test]
    fn restart_default_is_on_failure_with_the_documented_budget() {
        let d = RestartSpec::default();
        assert_eq!(d.policy, Restart::OnFailure);
        assert_eq!(d.budget, default_budget());
        assert_eq!(d.budget.delay_ms, DEFAULT_RESTART_DELAY_MS);
        // `never` must not be reachable through `Default`: a service nobody
        // configured has not been told not to restart.
        assert_ne!(d.policy, Restart::Never);
    }

    #[test]
    fn default_budget_is_built_from_the_three_constants() {
        assert_eq!(
            default_budget(),
            Budget {
                capacity: DEFAULT_RESTART_CAPACITY,
                window_ms: DEFAULT_RESTART_WINDOW_MS,
                delay_ms: DEFAULT_RESTART_DELAY_MS,
            }
        );
        // And it happens to agree with `zcore`'s own default today. Pinned, not
        // assumed: if either side moves alone, this is the test that says so.
        assert_eq!(
            default_budget(),
            Budget::default(),
            "zcore::Budget::default() and zconfig's documented default have diverged"
        );
        assert_eq!(NEVER_BUDGET, Budget::never());
        assert_eq!(NEVER_BUDGET.capacity, 0);
    }

    #[test]
    fn restart_default_and_bare_on_failure_are_both_on_failure() {
        // They differ in budget on purpose, but never in policy - a re-printed
        // description that changed a service's restart policy would be a lie.
        assert_eq!(
            RestartSpec::default().policy,
            RestartSpec::parse("on-failure").unwrap().policy
        );
    }

    #[test]
    fn restart_budget_and_policy_conversions_agree_with_their_accessors() {
        let spec = RestartSpec::parse("5 restarts per 60s").unwrap();
        assert_eq!(spec.into_budget(), spec.budget);
        assert_eq!(Budget::from(spec), spec.budget);
        assert_eq!(spec.policy(), spec.policy);
        assert_eq!(Restart::from(spec), spec.policy);
        // `RestartSpec` is `Copy`, so consuming a copy leaves the original whole.
        let copy = spec;
        let _ = copy.into_budget();
        assert_eq!(spec, RestartSpec::parse("5 restarts per 60s").unwrap());
    }

    #[test]
    fn restart_never_gets_no_tokens_at_all() {
        let never = RestartSpec::parse("never").unwrap();
        assert_eq!(never.budget, NEVER_BUDGET);
        assert_eq!(never.budget.capacity, 0);
        // A zero capacity must actually stop the loop, or `never` is a lie.
        let mut bucket = zcore::types::Bucket::new();
        assert!(!bucket.take(&never.budget, 0));
        assert!(!bucket.allows(&never.budget, u64::MAX));
    }

    #[test]
    fn restart_bare_policies_get_an_effectively_unlimited_budget() {
        for text in ["on-failure", "always"] {
            let spec = RestartSpec::parse(text).unwrap();
            assert_eq!(spec.budget.capacity, UNLIMITED_CAPACITY);
            assert_eq!(spec.budget.window_ms, DEFAULT_RESTART_WINDOW_MS);
            assert_eq!(spec.budget.delay_ms, DEFAULT_RESTART_DELAY_MS);
            // It really is unlimited, not merely large: five hundred restarts
            // in a row are all permitted.
            let mut bucket = zcore::types::Bucket::new();
            for i in 0..500u64 {
                assert!(
                    bucket.take(&spec.budget, i * 10_000),
                    "{text}: restart {i} refused"
                );
            }
        }
    }

    #[test]
    fn restart_budget_whitespace_is_a_courtesy_not_a_rule() {
        let expected = (7u32, 120_000u64);
        for text in [
            "7 restarts per 2m",
            "  7   restarts  per  2m",
            "7\trestarts\tper\t2m",
            "7 restarts per 2m   ",
        ] {
            let s = RestartSpec::parse(text).unwrap();
            assert_eq!(
                (s.budget.capacity, s.budget.window_ms),
                expected,
                "{text:?}"
            );
        }
    }

    #[test]
    fn restart_budget_window_obeys_the_bare_number_is_seconds_rule() {
        // The same trap as every other duration, in the one place it is easiest
        // to forget: the window.
        assert_eq!(
            RestartSpec::parse("5 restarts per 60")
                .unwrap()
                .budget
                .window_ms,
            60_000
        );
        assert_eq!(
            RestartSpec::parse("5 restarts per 60s")
                .unwrap()
                .budget
                .window_ms,
            60_000
        );
        // A malformed window is a BudgetFormat error naming the whole value,
        // not a Duration error the caller has to guess the context of.
        assert!(matches!(
            RestartSpec::parse("5 restarts per 1.5s"),
            Err(RestartError::BudgetFormat { .. })
        ));
    }

    // ── RunAs ───────────────────────────────────────────────────────────────

    #[test]
    fn runas_try_parse_returns_some_for_numbers_and_none_for_names() {
        // The split `zrt` relies on: `Some` = resolved here, `None` = a name
        // the runtime must look up, `Err` = a genuine syntax mistake.
        assert_eq!(
            RunAs::try_parse("1000").unwrap(),
            Some(RunAs {
                uid: 1000,
                gid: 1000
            })
        );
        assert_eq!(
            RunAs::try_parse("1000:1001").unwrap(),
            Some(RunAs {
                uid: 1000,
                gid: 1001
            })
        );
        assert_eq!(RunAs::try_parse("www-data").unwrap(), None);
        assert_eq!(RunAs::try_parse("www-data:www-data").unwrap(), None);
    }

    // ── Log ─────────────────────────────────────────────────────────────────

    #[test]
    fn logspec_default_is_the_empty_file_sink() {
        let d = LogSpec::default();
        assert_eq!(d, LogSpec::parse("file").unwrap());
        assert_eq!(
            d.sink,
            LogSink::File {
                path: String::new(),
                max_bytes: DEFAULT_LOG_MAX_BYTES,
                backups: DEFAULT_LOG_BACKUPS,
            }
        );
    }

    #[test]
    fn logspec_into_sink_agrees_with_from_and_the_field() {
        for text in ["none", "syslog", "file:/l.log"] {
            let spec = LogSpec::parse(text).unwrap();
            let expected = spec.sink.clone();
            assert_eq!(
                spec.sink(),
                &expected,
                "{text}: sink() disagrees with the field"
            );
            assert_eq!(LogSink::from(spec.clone()), expected, "{text}");
            assert_eq!(spec.clone().into_sink(), expected, "{text}");
        }
    }

    #[test]
    fn logsink_rejects_an_argument_on_a_sink_that_takes_none() {
        // `syslog:` must not be reported as "unknown log sink `syslog`" - that
        // accuses the operator of misspelling a word they spelled correctly.
        for text in ["none:", "none:foo", "syslog:", "syslog:/var/log/x"] {
            assert_eq!(
                LogSpec::parse(text).unwrap_err(),
                LogError::UnexpectedArgument { value: text },
                "{text:?}"
            );
            let m = LogSpec::parse(text).unwrap_err().to_string();
            assert!(m.contains("takes no argument"), "{text:?}: {m}");
        }
    }

    #[test]
    fn logsink_size_without_backups_uses_the_default_backups() {
        let s = LogSpec::parse("file:/l.log:2048").unwrap();
        assert_eq!(
            s.sink,
            LogSink::File {
                path: "/l.log".to_string(),
                max_bytes: 2048,
                backups: DEFAULT_LOG_BACKUPS,
            }
        );
        // A trailing slash means "size given, backups left at the default",
        // not "backups = 0", which would delete the only file on rotation.
        assert_eq!(LogSpec::parse("file:/l.log:2048/").unwrap().sink, s.sink);
    }

    #[test]
    fn logsink_try_parse_returns_none_only_for_an_empty_path() {
        assert_eq!(LogSpec::try_parse("file:").unwrap(), None);
        assert_eq!(LogSpec::try_parse("file::1024").unwrap(), None);
        assert!(LogSpec::try_parse("none").unwrap().is_some());
        assert!(LogSpec::try_parse("file:/l.log").unwrap().is_some());
        // `parse` turns the `None` into an error, and says so usefully.
        assert!(matches!(
            LogSpec::parse("file:"),
            Err(LogError::EmptyPath { .. })
        ));
    }

    // ── Listen ────────────────────────────────────────────────────────────

    #[test]
    fn listen_tcp_and_unix_parse_with_default_names() {
        let a = parse_listen("tcp:8080").unwrap();
        assert_eq!(a.spec, ListenSpec::TcpPort(8080));
        assert_eq!(a.name, "tcp:8080");
        let b = parse_listen("tcp:80:http").unwrap();
        assert_eq!(b.spec, ListenSpec::TcpPort(80));
        assert_eq!(b.name, "http");
        let c = parse_listen("unix:/run/app.sock").unwrap();
        assert_eq!(c.spec, ListenSpec::UnixPath("/run/app.sock".to_string()));
        // A colon inside a unix path is a filename character, not a name
        // separator: splitting it would silently bind somewhere else.
        let d = parse_listen("unix:/run/we:ird.sock").unwrap();
        assert_eq!(d.spec, ListenSpec::UnixPath("/run/we:ird.sock".to_string()));
    }

    #[test]
    fn listen_refusals_are_loud() {
        for bad in [
            "",
            "blah",
            "tcp:",
            "tcp:http",
            "tcp:0",
            "tcp:65536",
            "unix:",
        ] {
            assert!(parse_listen(bad).is_err(), "{bad:?} must not parse");
        }
        assert!(matches!(
            parse_listen("tcp:80:has space"),
            Err(ListenError::BadName { .. })
        ));
        assert!(matches!(
            parse_listen("tcp:80:a:b"),
            Err(ListenError::BadName { .. })
        ));
    }

    // ── Capabilities ────────────────────────────────────────────────────

    #[test]
    fn capability_numbers_are_pinned() {
        // Spot-checked against `capsh --decode` on the running kernel (every
        // entry was verified that way when the table landed). A move here is
        // a dropped wrong privilege, not a typo.
        for (name, nr) in [
            ("chown", 0),
            ("net_bind_service", 10),
            ("net_broadcast", 11),
            ("sys_admin", 21),
            ("setfcap", 31),
            ("mac_override", 32),
            ("bpf", 39),
            ("checkpoint_restore", 40),
        ] {
            assert_eq!(parse_capability(name), Some(nr), "{name} moved?");
        }
    }

    #[test]
    fn capability_names_accept_prefix_and_case() {
        assert_eq!(parse_capability("CAP_SYS_ADMIN"), Some(21));
        assert_eq!(parse_capability("cap_chown"), Some(0));
        assert_eq!(parse_capability("Sys_Time"), Some(25));
        assert_eq!(parse_capability("no_such_cap"), None);
        assert_eq!(parse_capability(""), None);
    }

    // ── Diagnostics ─────────────────────────────────────────────────────────

    /// Every single variant, in both directions: no kind shares a code with
    /// another, and no code is left unclaimed. A code table that grows a hole
    /// is a code table somebody will eventually fill with a *different* error.
    #[test]
    fn value_error_codes_are_pinned() {
        let all: Vec<(&str, String)> = alloc::vec![
            ("E300", parse_duration("").unwrap_err().to_string()),
            ("E301", parse_duration("10q").unwrap_err().to_string()),
            ("E302", parse_duration("1.5s").unwrap_err().to_string()),
            (
                "E303",
                parse_duration("99999999999999999999s")
                    .unwrap_err()
                    .to_string()
            ),
            (
                "E304",
                parse_duration("18446744073709551615s")
                    .unwrap_err()
                    .to_string()
            ),
            ("E310", ReadySpec::parse("").unwrap_err().to_string()),
            ("E311", ReadySpec::parse("xdp:1").unwrap_err().to_string()),
            ("E312", ReadySpec::parse("tcp:").unwrap_err().to_string()),
            (
                "E313",
                ReadySpec::parse("notify:x").unwrap_err().to_string()
            ),
            (
                "E314",
                ReadySpec::parse("tcp:http").unwrap_err().to_string()
            ),
            ("E315", ReadySpec::parse("tcp:0").unwrap_err().to_string()),
            (
                "E316",
                ReadySpec::parse("tcp:65536").unwrap_err().to_string()
            ),
            (
                "E320",
                RestartSpec::parse("sometimes").unwrap_err().to_string()
            ),
            (
                "E321",
                RestartSpec::parse("5 restart per 1m")
                    .unwrap_err()
                    .to_string()
            ),
            (
                "E322",
                RestartSpec::parse("0 restarts per 1s")
                    .unwrap_err()
                    .to_string()
            ),
            (
                "E323",
                RestartSpec::parse("5 restarts per 0ms")
                    .unwrap_err()
                    .to_string()
            ),
            ("E330", RunAs::try_parse("").unwrap_err().to_string()),
            (
                "E331",
                RunAs::try_parse("1000 users").unwrap_err().to_string()
            ),
            (
                "E332",
                RunAs::try_parse("1000:bad group").unwrap_err().to_string()
            ),
            (
                "E333",
                RunAs::try_parse("4294967296").unwrap_err().to_string()
            ),
            (
                "E334",
                RunAs::try_parse("0:4294967296").unwrap_err().to_string()
            ),
            ("E340", LogSpec::parse("stderr").unwrap_err().to_string()),
            ("E341", LogSpec::parse("syslog:x").unwrap_err().to_string()),
            ("E342", LogSpec::parse("file:").unwrap_err().to_string()),
            (
                "E343",
                LogSpec::parse("file:/l.log:x").unwrap_err().to_string()
            ),
            (
                "E344",
                LogSpec::parse("file:/l.log:1024/256")
                    .unwrap_err()
                    .to_string()
            ),
            ("E350", parse_listen("").unwrap_err().to_string()),
            ("E351", parse_listen("udp:53").unwrap_err().to_string()),
            ("E352", parse_listen("tcp:").unwrap_err().to_string()),
            ("E353", parse_listen("tcp:http").unwrap_err().to_string()),
            ("E354", parse_listen("tcp:0").unwrap_err().to_string()),
            ("E355", parse_listen("tcp:65536").unwrap_err().to_string()),
            ("E356", parse_listen("unix:").unwrap_err().to_string()),
            (
                "E357",
                parse_listen("tcp:80:has space").unwrap_err().to_string()
            ),
        ];
        for (code, message) in &all {
            assert!(
                !message.is_empty(),
                "code {code} was reserved with no way to produce it"
            );
        }
        let codes: Vec<&str> = all.iter().map(|(c, _)| *c).collect();
        let mut sorted = codes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            codes.len(),
            "two failures share a code: {all:?}"
        );
        // The whole block is `E3xx`, contiguous per directive, and `W`/`N` are
        // never handed out by a value error: only `Error` may be fatal here.
        for c in &codes {
            assert!(c.starts_with("E3"), "{c} is outside this module's block");
            assert_eq!(c.len(), 4, "{c} is not an `E3xx` code");
        }
        let mut expected: Vec<String> = all.iter().map(|(c, _)| c.to_string()).collect();
        expected.sort();
        assert_eq!(sorted, expected);
    }

    #[test]
    fn value_error_display_delegates_to_the_sub_error() {
        // `ValueError` is an umbrella; wrapping must not swallow the message.
        let cases: alloc::vec::Vec<(ValueError<'_>, String)> = alloc::vec![
            (
                ValueError::from(parse_duration("10q").unwrap_err()),
                parse_duration("10q").unwrap_err().to_string(),
            ),
            (
                ValueError::from(ReadySpec::parse("tcp:0").unwrap_err()),
                ReadySpec::parse("tcp:0").unwrap_err().to_string(),
            ),
            (
                ValueError::from(RestartSpec::parse("0 restarts per 1s").unwrap_err()),
                RestartSpec::parse("0 restarts per 1s")
                    .unwrap_err()
                    .to_string(),
            ),
            (
                ValueError::from(RunAs::try_parse("1000 users").unwrap_err()),
                RunAs::try_parse("1000 users").unwrap_err().to_string(),
            ),
            (
                ValueError::from(LogSpec::parse("stderr").unwrap_err()),
                LogSpec::parse("stderr").unwrap_err().to_string(),
            ),
        ];
        for (v, expected) in cases {
            assert_eq!(v.to_string(), expected);
        }
    }

    #[test]
    fn to_diagnostic_carries_a_help_line_only_where_there_is_one() {
        let with_help = ValueError::from(RestartSpec::parse("0 restarts per 1s").unwrap_err())
            .to_diagnostic(Span::point(1, 1));
        assert!(with_help.help.is_some());
        // A code the reader can act on does not need a second, vaguer line.
        let without_help = ValueError::from(LogSpec::parse("stderr").unwrap_err())
            .to_diagnostic(Span::point(1, 1));
        assert_eq!(without_help.help, None);
        // And every message names the offending value, so the reader never has
        // to go and find it.
        let quoted =
            ValueError::from(parse_duration("10w").unwrap_err()).to_diagnostic(Span::point(4, 2));
        assert!(quoted.message.contains("10w"), "{}", quoted.message);
        assert_eq!(quoted.span, Some(Span::point(4, 2)));
    }

    // ── Byte scanners ───────────────────────────────────────────────────────

    #[test]
    fn classify_separates_names_from_typos() {
        assert_eq!(classify("0"), Some(Token::Number));
        assert_eq!(classify("4294967295"), Some(Token::Number));
        assert_eq!(classify("root"), Some(Token::Name));
        assert_eq!(classify("_svc"), Some(Token::Name));
        assert_eq!(classify("a$b"), Some(Token::Name));
        assert_eq!(classify("svc.local"), Some(Token::Name));
        // Not names: a leading digit is a number with something glued to it.
        assert_eq!(classify("1abc"), None);
        // Not names: no entry in any `/etc/passwd` can hold these, so failing
        // here is honest rather than deferring a mistake to the runtime.
        assert_eq!(classify(""), None);
        assert_eq!(classify("1000 users"), None);
        assert_eq!(classify("a b"), None);
        assert_eq!(classify("a:b"), None);
        assert_eq!(classify("-root"), None);
        assert_eq!(classify("١٢"), None);
    }

    #[test]
    fn narrow_helpers_refuse_rather_than_wrap() {
        assert_eq!(narrow_u32("0"), Some(0));
        assert_eq!(narrow_u32("4294967295"), Some(u32::MAX));
        assert_eq!(narrow_u32("4294967296"), None);
        assert_eq!(narrow_u32("99999999999999999999"), None);
        assert_eq!(narrow_u32("-1"), None);
        assert_eq!(narrow_u32(""), None);
        assert_eq!(narrow_u8("255"), Some(255));
        assert_eq!(narrow_u8("256"), None);
        assert_eq!(narrow_u8("0"), Some(0));
    }

    #[test]
    fn parse_u64_reports_the_offending_run_not_the_whole_input() {
        assert_eq!(parse_u64("0"), Ok(0));
        assert_eq!(parse_u64("18446744073709551615"), Ok(u64::MAX));
        assert!(matches!(
            parse_u64("18446744073709551616"),
            Err(DurationError::NumberTooLarge { .. })
        ));
        // It is a private helper with a precondition, but the precondition is
        // enforced anyway: a caller that forgets `all_digits` gets an error, not
        // a silently wrong number.
        assert!(matches!(
            parse_u64("12a"),
            Err(DurationError::InvalidNumber { .. })
        ));
    }

    #[test]
    fn unit_scale_agrees_with_the_documented_units() {
        for (unit, ms) in [
            ("ms", 1u64),
            ("s", 1_000),
            ("m", 60_000),
            ("h", 3_600_000),
            ("d", 86_400_000),
        ] {
            assert_eq!(unit_scale(unit), Some(ms), "{unit}");
        }
        assert_eq!(unit_scale(""), None);
        assert_eq!(unit_scale("sec"), None);
        assert_eq!(unit_scale("w"), None);
    }
}
