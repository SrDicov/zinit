//! `zservice` — one managed service: where the pure core meets the kernel.
//!
//! # What this crate is
//!
//! [`zcore`] decides, [`zrt`] provides the syscalls, and this crate is the
//! piece in between that makes `fork()` in Rust survivable (`DESIGN.md`
//! §9.1). It owns the full lifecycle of a single service:
//!
//! * [`spawn::spawn`] — fork+exec from a [`zcore::ServicePlan`], with every pointer
//!   prepared **before** the fork and a child path that never returns.
//! * [`ready`] — the four readiness adapters (`none`, `notify`, `ping:<cmd>`,
//!   `tcp:<port>`), shaped so the supervisor's reactor can poll them.
//! * [`log`] — the append-only file sink with size rotation, one `write(2)`
//!   per line, no buffering that a `SIGKILL` could eat.
//! * [`identity`] — `user = name` resolution in the parent, plus the ordered
//!   privilege drop (`setgroups`, `setresgid`, `setresuid`, verify-or-die)
//!   the child applies through [`spawn()`].
//! * [`service::ManagedService`] — ties the
//!   four above to a [`zcore::Runtime`] slot: deadlines armed, waitpid status
//!   translated to [`zcore::Event`], stop escalation `TERM`-then-`KILL`.
//!
//! # What this crate is not
//!
//! It is not the supervisor: there is no event loop here, no reactor
//! ownership, no control socket. It is one service's worth of mechanism, and
//! the supervisor drives it (`start`, `poll_ready`, `check_deadlines`,
//! `stop_signal`, `cleanup`). Policy — when to restart, what a timeout
//! *means* — stays in [`zcore`]; this crate only reports the facts.
//!
//! # The three rules, inherited from `zrt`
//!
//! **No `unwrap`, no `panic!`, no `expect` outside tests.** A panic in the
//! supervisor is a reboot loop, and a panic in a forked child unwinds through
//! a C frame, which is undefined behaviour. Every fallible path returns
//! `io::Result` with the real errno.
//!
//! **No `println!` outside tests.** The supervisor's stdout is the console it
//! is trying to hand over; log lines go through [`log`], never stdio.
//!
//! **Every `unsafe` carries a `// SAFETY:` comment.** The post-fork child can
//! only call async-signal-safe functions, and the comment on each block is
//! what makes that reviewable.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(missing_debug_implementations)]

pub mod error;
pub mod identity;
pub mod log;
pub mod ready;
pub mod service;
pub mod spawn;

pub use error::SpawnError;
pub use identity::{ChildRlimit, cgroup_procs_path, ensure_cgroup};
pub use identity::{resolve_group, resolve_user, resolve_user_group};
pub use log::{LogHandle, open_sink, write_line};
pub use ready::{
    PING_RUN_TIMEOUT_MS, POLL_PING_INTERVAL_MS, PollNeed, ReadyWait, TCP_CONNECT_TIMEOUT_MS,
    probe_tcp, run_check,
};
pub use service::ManagedService;
pub use spawn::{SpawnCtx, Spawned, expand_env_value, spawn};

#[cfg(test)]
pub(crate) mod testutil;
