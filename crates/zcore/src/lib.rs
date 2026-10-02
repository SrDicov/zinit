//! `zcore` — the pure heart of zinit.
//!
//! This crate models the service state machine and **nothing else**. It cannot
//! open a file, call a syscall, read the clock, or sleep. It takes `now_ms` as
//! a parameter and returns `Vec<Action>` for the runtime to carry out.
//!
//! That restriction is enforced mechanically, not by discipline:
//!
//! * `#![no_std]` — no std, no filesystem, no clock, no threads.
//! * `#![forbid(unsafe_code)]` — no escape hatch into the outside world.
//! * `extern crate alloc` — the only allocator, used solely for `Vec`/`String`
//!   collections and for the `Action::Log` payload.
//! * **No dependencies.** Not even `libc`.
//!
//! # Why this matters
//!
//! An init system has one irreplaceable safety property: when the kernel hands
//! you PID 1, nothing is left to catch your mistakes. The failure modes of a
//! supervisor are race-shaped, and races cannot be tested by running the code.
//! So the decision logic is moved into a form where it is *checkable* —
//! exhaustively, and over millions of random event sequences — and the impure
//! half is reduced to a thin shell that only executes the decisions.
//!
//! # The single concept
//!
//! Runtime state is one pair per service: [`Desired`] (what the operator wants)
//! and [`State`] (what is actually true). Everything else is convergence.
//!
//! In particular there is **no `Restarting` state**. A service that crashed is
//! no longer [`State::Running`] while [`Desired::Up`] is unchanged; the
//! reconciler sees the divergence and converges, subject to a
//! [`Budget`]. Restart is not a special case. It is reconciliation.
//!
//! See `DESIGN.md` §4 for the full state table and the six invariants.

#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod action;
pub mod event;
pub mod reconcile;
pub mod runtime;
pub mod transition;
pub mod types;

pub use action::Action;
pub use event::Event;
pub use reconcile::{Tick, due_events, reconcile, should_restart};
pub use runtime::{Runtime, ServiceState};
pub use transition::{Transition, apply, arm_start_deadlines, kick};
pub use types::{
    Bucket, Budget, Desired, Idx, LogLevel, LogSink, MAX_SERVICES, Plan, Ready, Restart,
    ServiceKind, ServicePlan, SignalKind, State, StrictReady,
};

/// Errors the core can produce on its own.
///
/// Deliberately tiny: anything requiring the outside world surfaces as an
/// [`Action`], and the runtime is what can actually fail at the kernel.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// The plan is inconsistent with the runtime's view of it. Always a bug.
    PlanMismatch { what: &'static str, idx: Idx },
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::PlanMismatch { what, idx } => {
                write!(f, "plan mismatch: {what} for service #{idx}")
            }
        }
    }
}
