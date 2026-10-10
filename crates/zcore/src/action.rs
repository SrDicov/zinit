//! Actions: instructions for the runtime.
//!
//! An [`Action`] is the core asking the outside world to do something. The core
//! never does it itself - that asymmetry is the whole point. The reconciler and
//! the transition function are pure; the runtime in `zrt`/`zservice` is where
//! `fork`, `kill` and `openat` actually live.
//!
//! Actions are *requests*, not acknowledgements. A `Spawn` that fails in the
//! runtime comes back as an [`Event::SpawnFailed`](crate::Event::SpawnFailed);
//! the core never assumes an action succeeded.

use alloc::string::String;

use crate::types::{Idx, SignalKind};

pub use crate::types::LogLevel;

/// One instruction for the runtime.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    /// Fork and exec this service, per its [`ServicePlan`](crate::ServicePlan).
    Spawn(Idx),

    /// Send a signal to the service's process group.
    ///
    /// The core always signals the **process group**, never a bare pid: a
    /// service that forks helpers must take them down with it, and signalling
    /// a pid we do not own is a bug waiting to happen.
    Signal { idx: Idx, signal: SignalKind },

    /// Record that the service reached `Running` and cascade to dependents.
    ///
    /// Emitted by the transition function; the runtime should also notify
    /// any client waiting on the control socket.
    MarkReady(Idx),

    /// Record that the service reached `Stopped` and cascade to dependents.
    MarkDown {
        idx: Idx,
        /// True when this was unexpected (a crash) rather than requested.
        unexpected: bool,
    },

    /// Emit a log line. The core does not know where it goes.
    Log {
        idx: Idx,
        level: LogLevel,
        message: String,
    },

    /// A dependency changed state; re-evaluate services that depend on it.
    ///
    /// Emitted alongside `MarkReady`/`MarkDown`. The runtime may ignore it
    /// (the reconciler re-evaluates everything each tick anyway) but it is
    /// what lets an event-driven runtime avoid a full rescan.
    DepsChanged(Idx),

    /// Enter or leave `Stopping` because a dependency went away.
    ///
    /// The core has already changed `Desired`; this tells the runtime to send
    /// the actual signal.
    CascadeStop { idx: Idx, dep: Idx },

    /// The restart budget for this service is now empty. The runtime should
    /// log loudly: the service is backing off until the bucket refills or an
    /// operator runs `zctl kick`.
    BudgetExhausted(Idx),

    /// Hand the controlling terminal to this service.
    ///
    /// Emitted when a `console` service becomes `Running`. Only one console
    /// service may hold the tty at a time; the runtime enforces that.
    GrantConsole(Idx),

    /// Withdraw the controlling terminal from this service.
    RevokeConsole(Idx),

    /// The service is pinned; ignore pending stop requests.
    Unpin(Idx),
}

impl Action {
    /// The service this action concerns.
    pub const fn idx(&self) -> Idx {
        match *self {
            Action::Spawn(i)
            | Action::MarkReady(i)
            | Action::MarkDown { idx: i, .. }
            | Action::DepsChanged(i)
            | Action::BudgetExhausted(i)
            | Action::GrantConsole(i)
            | Action::RevokeConsole(i)
            | Action::Unpin(i) => i,
            Action::Signal { idx, .. } => idx,
            Action::Log { idx, .. } => idx,
            Action::CascadeStop { idx, .. } => idx,
        }
    }
}

/// Convenience for building a log action without a `String` literal dance.
pub fn log(idx: Idx, level: LogLevel, message: impl Into<String>) -> Action {
    Action::Log {
        idx,
        level,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_reports_its_index() {
        let actions = [
            Action::Spawn(7),
            Action::Signal {
                idx: 7,
                signal: SignalKind::Term,
            },
            Action::MarkReady(7),
            Action::MarkDown {
                idx: 7,
                unexpected: true,
            },
            log(7, LogLevel::Warn, "x"),
            Action::DepsChanged(7),
            Action::CascadeStop { idx: 7, dep: 1 },
            Action::BudgetExhausted(7),
            Action::GrantConsole(7),
            Action::RevokeConsole(7),
            Action::Unpin(7),
        ];
        for a in &actions {
            assert_eq!(a.idx(), 7, "{a:?} reported the wrong index");
        }
        assert_eq!(actions.len(), 11, "a variant was added without a test case");
    }
}
