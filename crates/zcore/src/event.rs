//! Events: facts reported by the outside world.
//!
//! An [`Event`] is something that **already happened**. The core never asks the
//! world a question and never receives a "please do X" - that is
//! [`Action`](crate::Action)'s job. Keeping the two directions separate is what
//! makes the state machine a pure function.
//!
//! Every event carries the [`Idx`] of the service it concerns. There are no
//! global events: a signal that concerns every service is fanned out by the
//! runtime into one event per affected service before it reaches the core.

use crate::types::Idx;

/// Something that happened to a service.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    /// `execve` succeeded in the child. The process image is now the service.
    /// This is the first point at which we can consider the process real.
    ExecOk(Idx),

    /// `fork` or `execve` failed. `errno` is positive and meaningful.
    SpawnFailed { idx: Idx, errno: i32 },

    /// The service signalled readiness through the notify fd, or the
    /// `ready` adapter succeeded.
    Ready(Idx),

    /// `ready-timeout` expired.
    ///
    /// For a non-strict [`Ready`](crate::Ready), the runtime emits `Ready` instead. This
    /// event only reaches the core for [`Ready::Strict`](crate::Ready::Strict), where expiry is a
    /// genuine failure.
    ReadyTimeout(Idx),

    /// `start-timeout` expired while still `Starting`.
    StartTimeout(Idx),

    /// The child exited normally with `code`.
    Exited { idx: Idx, code: i32 },

    /// The child was killed by `signal`.
    Signalled { idx: Idx, signal: i32 },

    /// The child was reaped but was not one we spawned - it was inherited
    /// from a pre-existing process. The runtime emits this for orphans it
    /// adopts, so that `Desired::Down` bookkeeping stays correct.
    OrphanReaped(Idx),

    /// `stop-timeout` expired while `Stopping`. The runtime will have escalated
    /// to `SIGKILL`; this event records that it had to.
    StopTimeout(Idx),

    /// A required dependency left `Running` while we are `Up`. Used to decide
    /// whether to cascade a stop.
    DepLost { idx: Idx, dep: Idx },

    /// A required dependency failed to start, so this service can never become
    /// ready. Distinct from `DepLost`: nothing ever came up.
    DepFailed { idx: Idx, dep: Idx },

    /// The restart budget for this service is exhausted. The service stays
    /// `(Stopped, Up)` and is left alone until the bucket refills or an
    /// operator runs `zctl kick`.
    BudgetExhausted(Idx),

    /// The service is pinned: a stop was requested but must not proceed.
    Pinned(Idx),

    /// An I/O operation the runtime performs on the service's behalf failed -
    /// opening its log, creating its cgroup, writing its pidfile. The service
    /// itself is unaffected; the runtime decides severity.
    IoError { idx: Idx, errno: i32 },
}

impl Event {
    /// The service this event concerns.
    pub const fn idx(&self) -> Idx {
        match *self {
            Event::ExecOk(i)
            | Event::Ready(i)
            | Event::ReadyTimeout(i)
            | Event::StartTimeout(i)
            | Event::StopTimeout(i)
            | Event::BudgetExhausted(i)
            | Event::Pinned(i)
            | Event::OrphanReaped(i) => i,
            Event::SpawnFailed { idx, .. }
            | Event::Exited { idx, .. }
            | Event::Signalled { idx, .. }
            | Event::DepLost { idx, .. }
            | Event::DepFailed { idx, .. }
            | Event::IoError { idx, .. } => idx,
        }
    }

    /// A short stable name, for logs and for test failure messages.
    pub const fn name(&self) -> &'static str {
        match *self {
            Event::ExecOk(_) => "exec-ok",
            Event::SpawnFailed { .. } => "spawn-failed",
            Event::Ready(_) => "ready",
            Event::ReadyTimeout(_) => "ready-timeout",
            Event::StartTimeout(_) => "start-timeout",
            Event::Exited { .. } => "exited",
            Event::Signalled { .. } => "signalled",
            Event::OrphanReaped(_) => "orphan-reaped",
            Event::StopTimeout(_) => "stop-timeout",
            Event::DepLost { .. } => "dep-lost",
            Event::DepFailed { .. } => "dep-failed",
            Event::BudgetExhausted(_) => "budget-exhausted",
            Event::Pinned(_) => "pinned",
            Event::IoError { .. } => "io-error",
        }
    }

    /// True when this event means the process is gone.
    ///
    /// The core does not branch on this itself, but the runtime needs it to
    /// decide whether to reap.
    pub const fn means_process_gone(&self) -> bool {
        matches!(
            *self,
            Event::Exited { .. } | Event::Signalled { .. } | Event::OrphanReaped(_)
        )
    }

    /// The exit code implied by this event, if it is a death.
    ///
    /// `None` means "killed by a signal", which [`Restart::wants_restart`](crate::Restart::wants_restart)
    /// treats as a failure.
    pub const fn exit_code(&self) -> Option<i32> {
        match *self {
            Event::Exited { code, .. } => Some(code),
            Event::Signalled { .. } | Event::OrphanReaped(_) => None,
            _ => None,
        }
    }
}

impl core::fmt::Display for Event {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idx_is_readable_on_every_variant() {
        let events = [
            Event::ExecOk(3),
            Event::SpawnFailed { idx: 3, errno: 2 },
            Event::Ready(3),
            Event::ReadyTimeout(3),
            Event::StartTimeout(3),
            Event::Exited { idx: 3, code: 0 },
            Event::Signalled { idx: 3, signal: 9 },
            Event::OrphanReaped(3),
            Event::StopTimeout(3),
            Event::DepLost { idx: 3, dep: 1 },
            Event::DepFailed { idx: 3, dep: 1 },
            Event::BudgetExhausted(3),
            Event::Pinned(3),
            Event::IoError { idx: 3, errno: 13 },
        ];
        for e in &events {
            assert_eq!(e.idx(), 3, "{e} reported the wrong index");
            assert!(!e.name().is_empty());
        }
        assert_eq!(events.len(), 14, "a variant was added without a test case");
    }

    #[test]
    fn death_detection() {
        assert!(Event::Exited { idx: 0, code: 0 }.means_process_gone());
        assert!(Event::Signalled { idx: 0, signal: 15 }.means_process_gone());
        assert!(!Event::Ready(0).means_process_gone());
        assert_eq!(Event::Exited { idx: 0, code: 7 }.exit_code(), Some(7));
        assert_eq!(Event::Signalled { idx: 0, signal: 9 }.exit_code(), None);
    }
}
