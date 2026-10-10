//! `zrt` — the layer that talks to the kernel.
//!
//! # What this crate is for
//!
//! `zcore` is the reason zinit is worth writing: a pure state machine that
//! takes `now_ms` and a list of events and returns a list of actions. It is
//! exhaustively testable precisely because it cannot touch the outside world.
//!
//! This crate is the other half, and it has exactly one job: **carry out those
//! actions, correctly, on whatever machine happens to have booted.**
//!
//! Everything hard about an init lives here, and it comes down to four
//! problems:
//!
//! 1. **Waiting for many things at once** — [`reactor`]. One trait, three
//!    backends (`epoll`, `kqueue`, `poll`), level-triggered everywhere, and the
//!    rule that a hangup is a read.
//! 2. **Noticing that a child died** — [`childproc`]. Three backends
//!    (`pidfd`, `EVFILT_PROC`, `waitpid`), no `/proc`, and an explicit argument
//!    for why the pid-recycling race is unreachable rather than unlikely.
//! 3. **Receiving a signal as data** — [`signals`]. `signalfd` or
//!    `EVFILT_SIGNAL`, so that a signal goes through the *same* event loop as
//!    everything else and cannot be lost in a handler window. The self-pipe
//!    fallback is named, not hidden.
//! 4. **Knowing what this machine can do** — [`capdetect`]. Every capability
//!    probed at runtime, printed in one line, with a documented behaviour when
//!    it is missing.
//!
//! # What this crate is not
//!
//! It is not `zcore`: there is no state machine here, and no policy about when
//! to restart a service. That logic is pure, tested without a kernel, and
//! lives on the other side of the boundary. It is not the supervisor either:
//! there is no event loop *loop* here, only the primitive it would be built
//! from.
//!
//! # The three rules of the crate
//!
//! **No `unwrap`, no `panic!`, no `expect`.** Not "mostly". A panic in the
//! supervisor unwinds through a C frame — the child of `fork` that failed to
//! `exec` — and that is undefined behaviour; in the supervisor proper it is a
//! reboot loop. Every fallible path here returns `io::Result` with the real
//! errno, and the ones that cannot fail are `#[must_use]`-shaped instead.
//!
//! **One dependency: `libc`.** Not `nix`, not `rustix`, not `tokio`. The
//! reason is not asceticism: a generated binding cannot carry a `// SAFETY:`
//! comment explaining *why this particular call is sound*, and that comment is
//! the entire review surface of a syscall layer. Writing the FFI by hand is
//! what makes the unsafe auditable.
//!
//! **Every `unsafe` has a `SAFETY` comment.** `#![deny(unsafe_op_in_unsafe_fn)]`
//! is on, so even inside an `unsafe fn` every raw operation has to justify
//! itself. `#![forbid(unsafe_code)]` — which `zcore` uses — is not available
//! here; the discipline lives in the comments instead of in the attributes.
//!
//! # Runtime detection, not compile-time guessing
//!
//! The capability table from `DESIGN.md` §8.1, which is the whole
//! portability story in one table:
//!
//! | Capability | Linux | FreeBSD/OpenBSD/NetBSD | Here |
//! |---|---|---|---|
//! | reactor | `epoll` | `kqueue` | [`Reactor`] with 3 backends, `poll` last |
//! | child tracking | `pidfd_open` (5.3+) | `EVFILT_PROC`/`NOTE_EXIT` | [`ChildTracker`], 3 backends |
//! | signals as fds | `signalfd` | `EVFILT_SIGNAL` | [`SignalSource`], self-pipe last |
//! | uid/gid | `setresuid`/`setresgid` | **do not exist** | probed; `setuid`+verify on BSD |
//! | no-new-privs | `prctl(PR_SET_NO_NEW_PRIVS)` | — | Linux; `false` elsewhere |
//! | ASLR off | `personality(ADDR_NO_RANDOMIZE)` | — | Linux; `false` elsewhere |
//! | capabilities | raw `capget`/`capset` | uid/gid model | probed, no `libcap` |
//! | cgroups | v2, fallback v1 | jails | probed, `None` = "no-op + aviso" |
//! | `/proc` | yes | **not mounted by default** | probed, and **nothing here uses it** |
//! | reboot | `LINUX_REBOOT_CMD_*` | `RB_*` | [`RebootStyle`], never an `if` |
//!
//! Everything in that table is a runtime answer from [`capdetect`], printed
//! once at start-up by [`detect_reporting`]. Nothing in this crate depends on
//! `/proc` existing, and that is a testable property rather than an intention:
//! the child tracker is built on `pidfd` and `waitpid`, which is why
//! FreeBSD can run it at all.
//!
//! # Examples
//!
//! Build the supervisor's three primitives and log what the machine can do:
//!
//! ```
//! use zrt::{capdetect, childproc, reactor, signals};
//!
//! let caps = capdetect::detect();
//! println!("{}", caps.summary_line());
//!
//! let mut reactor = reactor::new_reactor().unwrap();
//! let mut children = childproc::new_child_tracker().unwrap();
//! let mut sigs = signals::new_signal_source(signals::SignalSet::with(
//!     signals::Signal::Chld,
//! ))
//! .unwrap();
//!
//! for fd in sigs.fds() {
//!     reactor.add(*fd, reactor::Interest::Read).unwrap();
//! }
//! for fd in children.fds() {
//!     reactor.add(*fd, reactor::Interest::Read).unwrap();
//! }
//!
//! // Block signals before anything can be delivered: the supervisor has no
//! // asynchronous handlers, and `fork_tracked` is safe to write.
//! signals::block_all().unwrap();
//! ```

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(missing_debug_implementations)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// The one dependency. It is used through the `libc::` path in `sys`; this
// `use` only makes the dependency explicit at the crate root, and is hidden
// from the unused-import lint because the name is `_`-prefixed.

pub mod capdetect;
pub mod childproc;
pub mod clock;
pub mod reactor;
pub mod report;
pub mod seccomp;
pub mod signals;
pub mod sys;

pub use capdetect::{Capabilities, CgroupVer, RebootStyle, detect, detect_reporting};
pub use childproc::{ChildKind, ChildTracker, ForkOutcome, fork_tracked};
pub use clock::{
    Clock, ClockSource, ManualClock, SystemClock, deadline_from_now, now_ms, now_realtime_ms,
};
pub use reactor::{Interest, Reactor, ReactorKind, ReadyEvent, new_reactor};
pub use signals::{
    Signal, SignalSet, SignalSource, SignalSourceKind, block_all, new_signal_source,
};

/// How a child ended. Defined next to `waitpid` in [`sys`] because that is
/// where the bit-packed status word is decoded, but re-exported here because
/// it belongs to whoever is reasoning about a service that ended.
pub use sys::{ExitStatus, Waited};

/// The crate's own version string, for the boot banner.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
