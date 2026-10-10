//! Typed spawn failures: why a service never became a process.
//!
//! # Why a dedicated error type instead of bare `io::Error`
//!
//! Most spawn failures *are* kernel errnos (`fork` returning `ENOMEM`, `open`
//! on the log returning `EACCES`), and those travel as [`std::io::Error`]
//! untouched — the errno is the message. But a supervisor also refuses spawns
//! for reasons no errno describes: the plan says `target` (which never forks),
//! the command line is empty, the binary is not on `PATH`, the log asks for
//! `syslog` before it is wired. Returning "invalid input" for all of those
//! would make every refusal indistinguishable in the log, and the operator
//! staring at a service that will not start deserves better than that.
//!
//! So the refusal reasons are a closed enum, and each one converts into an
//! `io::Error` with a stable kind and a message that names the service. The
//! public API stays `io::Result` throughout — the supervisor has one error
//! vocabulary — while tests and the supervisor can still match on the
//! [`SpawnError`] before it is converted, which is how `target-never-forks`
//! is asserted rather than hoped for.

use std::io;

/// Why a service could not be spawned, when the kernel is not the reason.
///
/// Every variant names the service it concerns, because a spawn error without
/// a service name is a log line nobody can act on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SpawnError {
    /// The plan entry is a `target`: virtual, no process, never forks.
    ///
    /// Attempting to spawn one is always a supervisor bug — the reconciler
    /// never emits `Spawn` for a target — so this is a loud refusal rather
    /// than a quiet skip. A quiet skip would hide the bug until a target
    /// silently never provided the ordering it exists for.
    TargetHasNoProcess {
        /// The service that was asked to fork.
        name: String,
    },
    /// `command` is empty (or only whitespace) for a kind that runs a process.
    EmptyCommand {
        /// The service with nothing to execute.
        name: String,
    },
    /// The service index does not exist in the plan.
    UnknownService {
        /// The index that was requested.
        idx: usize,
    },
    /// `argv[0]` names a binary that is neither an absolute path nor on
    /// `PATH`.
    ///
    /// Refused **before** the fork rather than left to fail in the child:
    /// a missing binary is a configuration error, and reporting it now —
    /// with the resolved search path — beats discovering it as an
    /// `Exited(127)` three seconds later with no hint of what was missing.
    NotFound {
        /// The service whose binary is missing.
        name: String,
        /// What was looked for: the binary name and the searched `PATH`.
        wanted: String,
    },
    /// A value in the spawn contract is malformed: an empty `argv` word, a
    /// NUL byte in a path or in the environment.
    BadValue {
        /// The service concerned.
        name: String,
        /// What was wrong, in plain words.
        what: String,
    },
    /// An `rlimit-*` name this crate does not know (`nofile`, `nproc` and
    /// `as` are the whole vocabulary; see [`crate::identity`]).
    UnknownRlimit {
        /// The offending directive value.
        key: String,
    },
    /// A cgroup was requested on a platform without cgroup support.
    ///
    /// Returned — not silently ignored — so the supervisor can print the
    /// `DESIGN.md` §8.1 degradation aviso naming this exact service.
    CgroupUnsupported {
        /// The service whose limits were dropped.
        name: String,
    },
}

impl core::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SpawnError::TargetHasNoProcess { name } => write!(
                f,
                "service `{name}` is a target: it has no process and must never fork"
            ),
            SpawnError::EmptyCommand { name } => {
                write!(f, "service `{name}` has no command to execute")
            }
            SpawnError::UnknownService { idx } => {
                write!(f, "no service with index {idx} in the plan")
            }
            SpawnError::NotFound { name, wanted } => {
                write!(f, "service `{name}`: executable not found: {wanted}")
            }
            SpawnError::BadValue { name, what } => {
                write!(f, "service `{name}`: {what}")
            }
            SpawnError::UnknownRlimit { key } => write!(
                f,
                "unknown rlimit `{key}`: expected `nofile`, `nproc` or `as`"
            ),
            SpawnError::CgroupUnsupported { name } => write!(
                f,
                "service `{name}` asks for a cgroup on a platform without cgroup support"
            ),
        }
    }
}

impl From<SpawnError> for io::Error {
    /// Every refusal becomes an `io::Error` with a stable kind, so the whole
    /// crate can speak one error vocabulary.
    ///
    /// The kinds are chosen so a reader can tell configuration from kernel at
    /// a glance: refusals that the operator fixes by editing a file are
    /// `InvalidInput`, things missing from the machine are `NotFound`, and
    /// things missing from *this build* are `Unsupported`.
    fn from(e: SpawnError) -> io::Error {
        let kind = match &e {
            SpawnError::TargetHasNoProcess { .. }
            | SpawnError::EmptyCommand { .. }
            | SpawnError::BadValue { .. }
            | SpawnError::UnknownRlimit { .. } => io::ErrorKind::InvalidInput,
            SpawnError::UnknownService { .. } | SpawnError::NotFound { .. } => {
                io::ErrorKind::NotFound
            }
            SpawnError::CgroupUnsupported { .. } => {
                io::ErrorKind::Unsupported
            }
        };
        io::Error::new(kind, e.to_string())
    }
}

impl std::error::Error for SpawnError {}
