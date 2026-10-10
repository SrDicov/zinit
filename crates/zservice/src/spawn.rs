//! Fork+exec: turning a frozen plan entry into a live process.
//!
//! # The hard part, stated plainly (`DESIGN.md` §9.1)
//!
//! Past `fork()`, the child may only call async-signal-safe functions. Rust
//! gives no such guarantee automatically: a `Vec::push` in the child can take
//! a malloc lock held by a dead thread and hang forever, and a panic would
//! unwind through a C frame. Three mitigations, all structural:
//!
//! 1. **Everything is prepared before the fork.** `argv`, `envp`, the log fd,
//!    the notify pipe, the exec-error pipe, the resolved binary path, the
//!    rlimits, the cgroup path, the tty path. The child only *references*.
//! 2. **The post-fork path is one `#[inline(never)]` function with a single
//!    exit: `execve`.** Any failure writes the errno to the exec-error pipe
//!    and `_exit(127)`. It never returns to normal code.
//! 3. **Release builds on `panic = "abort"`**, and there is no `unwrap` on
//!    the child path to panic with.
//!
//! # The child sequence, and why it is in this order
//!
//! ```text
//! sigmask → setsid → stdio/notify fds → chdir(/) → rlimits → cgroup →
//!     setgroups → setresgid → setresuid → verify → no_new_privs →
//!     TIOCSCTTY → execve  (any failure → errno pipe + _exit(127))
//! ```
//!
//! * **An empty signal mask, before anything else.** `fork` copies the
//!   supervisor's blocked set and `execve` *preserves* it (only dispositions
//!   are reset), so a child that does not clear the mask runs with every
//!   catchable signal blocked — including `SIGTERM`, which is exactly how a
//!   service is asked to stop. Blocked *and* default means *pending forever*:
//!   the service survives its own `SIGTERM` and only the `stop-timeout`
//!   `SIGKILL` escalation can end it. A real, observed bug, not a hypothetical;
//!   `crates/zinit/tests/supervises_real_service.rs` is its regression test.
//! * **`setsid` first.** The supervisor stops a service with `kill(-pgid)`;
//!   the child must be a session (and therefore process-group) leader before
//!   anything else, or helpers forked in between would escape the group.
//! * **fds next ("first as a contract").** stdin from `/dev/null`, stdout and
//!   stderr to the log fd, the notify write-end onto fd 3. File descriptors
//!   are the service's interface promise, installed before anything that can
//!   fail for privilege reasons — a half-redirected stdio is never observed
//!   because any later failure `_exit`s the child outright.
//! * **`chdir("/")`.** A service must not pin the supervisor's working
//!   directory (or any mount the operator wants to unmount). Consequence,
//!   stated as a limitation: relative paths in `command` resolve from `/`.
//! * **rlimits.** Still-privileged tuning applied while the child is still
//!   root, so lowering works unconditionally and there is no "re-open after the
//!   drop" second path.
//! * **cgroup join before the drop.** Writing `cgroup.procs` needs privilege;
//!   after the drop it fails. (`DESIGN.md` §9 rule 3.)
//! * **`setgroups(0, NULL)`, `setresgid`, `setresuid`, then verify.**
//!   Real three-id variants, never `seteuid` alone (rule 1), with
//!   `getuid() == geteuid()` checked afterwards (rule 2). The drop is the
//!   last privileged act; what follows cannot regain anything.
//! * **`PR_SET_NO_NEW_PRIVS` (Linux, best-effort).** Defence in depth after
//!   the drop: a setuid binary the service execs cannot escalate it. Failure
//!   here does *not* abort the spawn — on kernels without the flag the uid
//!   drop is still complete — which is documented rather than hidden.
//! * **`TIOCSCTTY` for `console` services with a tty.** Session leader (done
//!   above) plus open tty plus `TIOCSCTTY` is the only ordering the kernel
//!   accepts.
//! * **`execve`, never `execvp`.** The binary was resolved against `PATH` in
//!   the parent; the child performs no search, opens nothing, allocates
//!   nothing. `execvpe` is glibc-isms the BSD builds cannot use, so the
//!   environment travels as an explicit `envp`.
//!
//! # What the parent owes the supervisor
//!
//! [`spawn`] returns [`Spawned`]: pid, pgid, the custodial fds. The supervisor
//! **must** register the pid in its child tracker before returning to the
//! event loop (`zrt::childproc::fork_tracked` documents why the window
//! matters); `spawn` cannot do it because it deliberately takes no tracker —
//! its signature is `(plan, idx, ctx)` and nothing else.
//!
//! Exec success is reported out-of-band through the exec-error pipe: `EOF`
//! means the image was replaced ([`zcore::Event::ExecOk`]), four bytes mean
//! the errno of whatever failed ([`zcore::Event::SpawnFailed`]), followed by
//! a second word naming the failing step (diagnostic only; production
//! readers consume the errno and close). See
//! [`ManagedService`](crate::service::ManagedService), which owns that pipe.

use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use zcore::{Idx, Plan, ServiceKind, StrictReady};

use crate::SpawnError;
use crate::identity::{self, ChildRlimit};
use crate::log;

/// Parameters for one spawn: everything `ServicePlan` deliberately lacks.
///
/// A frozen [`zcore::ServicePlan`] carries policy (timeouts, restart budget,
/// readiness, numeric ids) but no command line, no environment, no cgroup
/// name — those live in [`zconfig::ServiceDesc`], which the supervisor holds
/// beside the plan. [`SpawnCtx`] is the resolved, per-start view: the
/// supervisor builds one from the description each time it spawns, so a
/// reloaded description takes effect on the next start without a plan
/// rebuild.
#[derive(Clone, Debug)]
pub struct SpawnCtx<'a> {
    /// Full command line, exactly as configured.
    ///
    /// `process`/`console` split it on ASCII whitespace (no quoting — the
    /// escape hatch for anything complex is `type = script`) after expanding
    /// `$VAR`/`${VAR}`/`${VAR:-default}` in the **parent**, against the
    /// supervisor's own environment.
    ///
    /// `script` runs it as `/bin/sh -c <command>` **without** any parent-side
    /// expansion. A script's variables belong to the service, and expanding
    /// them in the supervisor would either erase them (unset there) or bind
    /// them to the wrong value (set there) — in both cases stealing the one
    /// job the shell is there to do. The shell is the expansion authority
    /// for `type = script`, and for everything else the parent is.
    pub command: &'a str,
    /// Extra environment, applied over the supervisor's own. Values support
    /// `$VAR`, `${VAR}` and `${VAR:-default}` expansion (see
    /// [`expand_env_value`]).
    pub env: &'a [(String, String)],
    /// Numeric `(uid, gid)` to run as, or `None` for "stay as the supervisor".
    ///
    /// Names never reach this struct: the supervisor resolves them with
    /// [`identity::resolve_user_group`] first, because `zconfig` refuses to
    /// freeze an unresolved identity and this is the backstop that keeps that
    /// promise even if the resolution pass is skipped.
    pub run_as: Option<(u32, u32)>,
    /// `(name, value)` rlimits (`nofile`, `nproc`, `as`). Soft limit only.
    pub rlimits: &'a [(String, u64)],
    /// Slice under `zinit.slice`, or an absolute cgroup path. `None` for none.
    pub cgroup: Option<&'a str>,
    /// Where stdout/stderr go. Borrowed from the frozen plan entry.
    pub log: &'a zcore::LogSink,
    /// Service name, for diagnostics.
    pub service_name: &'a str,
    /// Controlling terminal for `console` services. `None` means "no tty to
    /// take over": a console spawn without one behaves as `process` (see the
    /// limitations section below).
    pub tty: Option<&'a Path>,
    /// Pre-bound listen sockets as `(fd, logical name)`, owned by the caller.
    ///
    /// The supervisor binds these once and holds them across restarts; every
    /// spawn only *lends* them to the child (dup'd onto 3.., with the names
    /// in `$LISTEN_FDNAMES`). Empty for no socket activation. `spawn` parks
    /// them high itself before the fork, so callers pass whatever numbers
    /// they hold — no fd-number discipline is required of them.
    pub listen: &'a [(RawFd, String)],
}

/// Cap on listen sockets per spawn. The child dups them onto 3.. one by one
/// with no allocator to park them in, which is only sound while no target
/// can clobber a not-yet-moved source (see `ManagedService::ensure_listeners`,
/// which parks every source at 100+). 32 is far past any sane service and
/// keeps 3+32 < 100 with room to spare; past it the description is refused,
/// not truncated — half a socket set is a service that binds the wrong half.
pub const MAX_LISTEN_FDS: usize = 32;

/// Descriptor the notify write-end lands on: 3 with no listeners, 3+N with.
///
/// Listen sockets take 3.. first — the `$LISTEN_FDS` contract fixes them
/// there, starting at `SD_LISTEN_FDS_START` — so the notifier moves past
/// them rather than colliding.
fn notify_no(ctx: &SpawnCtx<'_>) -> u32 {
    3 + ctx.listen.len() as u32
}

/// What the parent keeps after a successful fork.
///
/// Every fd here is owned by the supervisor: the log fd (the child's
/// stdout/stderr), the notify read-end (registered in the reactor while the
/// service is `Starting`), and the exec-error read-end (polled once for the
/// `ExecOk`/`SpawnFailed` verdict, then closed). Custodianship is explicit
/// because a leaked notify pipe reads as "never ready" and a leaked log fd
/// pins the log file past rotation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Spawned {
    /// The child's pid.
    pub pid: i32,
    /// The child's process group (== pid, after the child led a new one).
    pub pgid: i32,
    /// Log fd (child's stdout/stderr), or `None` when the sink needed none.
    pub log_fd: Option<RawFd>,
    /// Notify-pipe read-end, for `notify` readiness. `None` otherwise.
    pub notify_fd: Option<RawFd>,
    /// Exec-error pipe read-end. Polled once, then closed. `None` is
    /// unreachable in practice and means "the pipe failed; treat the first
    /// reap as the verdict".
    pub exec_fd: Option<RawFd>,
}

/// Resolve `drop-capabilities` names to numbers (Linux).
///
/// Empty stays empty on every platform. A non-empty list anywhere else
/// refuses the spawn: capabilities are a Linux concept, and running the
/// service undropped where the file says dropped would be more privilege
/// than configured — the one outcome confinement must never produce.
fn resolve_cap_drops(service_name: &str, caps: &[String]) -> io::Result<Vec<u32>> {
    if caps.is_empty() {
        return Ok(Vec::new());
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("service `{service_name}`: capability drops are Linux-only"),
        ))
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut out = Vec::with_capacity(caps.len());
        for cap in caps {
            match zconfig::parse_capability(cap) {
                Some(nr) => out.push(nr),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("service `{service_name}`: unknown capability `{cap}`"),
                    ));
                }
            }
        }
        Ok(out)
    }
}

/// Assemble the seccomp filter for one spawn (Linux).
///
/// `None` policy means unconfined (empty program). Anything else resolves
/// every allowed name against the verified per-arch table and builds the
/// program; an unknown name, an unverified architecture, or an oversized
/// list refuses the spawn rather than shipping a filter with a hole.
fn resolve_seccomp_filter(
    service_name: &str,
    policy: &Option<zcore::SeccompPolicy>,
) -> io::Result<Vec<zrt::seccomp::Insn>> {
    let Some(policy) = policy else {
        return Ok(Vec::new());
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // The policy cannot apply here; naming it keeps the signature honest
        // on targets that refuse it.
        let _ = policy;
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("service `{service_name}`: syscall filters are Linux-only"),
        ))
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut numbers = Vec::with_capacity(policy.allow.len());
        for name in &policy.allow {
            match zrt::seccomp::syscall_nr(name) {
                Some(nr) => numbers.push(nr),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("service `{service_name}`: unknown syscall `{name}`"),
                    ));
                }
            }
        }
        let action = match policy.action {
            zcore::SeccompAction::Enforce => zrt::seccomp::OnViolation::Kill,
            zcore::SeccompAction::Errno => zrt::seccomp::OnViolation::Errno,
        };
        match zrt::seccomp::build(&numbers, action) {
            Some(filter) => Ok(filter),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("service `{service_name}`: no seccomp filter buildable here"),
            )),
        }
    }
}

/// Fork+exec service `idx` of `plan`.
///
/// Reads policy (kind, readiness, timeouts) from the frozen plan and
/// execution parameters from `ctx`. On success the child is already running
/// (or `exec`ing); on failure nothing was forked and the error says why —
/// including the [`SpawnError`] refusals that fire before any syscall with
/// side effects (`target`, empty command, missing binary).
///
/// The caller must register `pid` in its child tracker before returning to
/// the event loop; see the module docs.
pub fn spawn(plan: &Plan, idx: Idx, ctx: &SpawnCtx<'_>) -> io::Result<Spawned> {
    let sp = plan
        .services
        .get(idx)
        .ok_or_else(|| io::Error::from(SpawnError::UnknownService { idx }))?;
    if !sp.kind.has_process() {
        return Err(SpawnError::TargetHasNoProcess {
            name: ctx.service_name.to_string(),
        }
        .into());
    }
    if ctx.command.trim().is_empty() {
        return Err(SpawnError::EmptyCommand {
            name: ctx.service_name.to_string(),
        }
        .into());
    }
    if ctx.listen.len() > MAX_LISTEN_FDS {
        return Err(SpawnError::BadValue {
            name: ctx.service_name.to_string(),
            what: String::from("too many listen sockets"),
        }
        .into());
    }

    let wants_notify = matches!(
        sp.ready,
        zcore::Ready::Notify | zcore::Ready::Strict(StrictReady::Notify)
    );

    // ── argv, fully built before the fork ──────────────────────────────
    //
    // Expansion happens in the *parent* for `process`/`console`, and never for
    // `script`.
    //
    // For `script` the command is handed to `/bin/sh -c` verbatim. Expanding
    // first would be an active bug, not a convenience: a script is expected
    // to reference variables the *service's own* `env` block defines, and the
    // parent either has them unset (so `$PORT` collapses to empty and the
    // shell never sees it) or has them set to the supervisor's value (so the
    // service silently runs against the supervisor's environment). Either way
    // the script cannot do its own expansion, which is the entire reason to
    // choose `type = script`.
    let expanded_cmd = if sp.kind == ServiceKind::Script {
        String::from(ctx.command)
    } else {
        expand_env_value(ctx.command, &env_lookup)
    };
    let words: Vec<String> = if sp.kind == ServiceKind::Script {
        vec![]
    } else {
        expanded_cmd
            .split_whitespace()
            .map(str::to_string)
            .collect()
    };
    // `/bin/sh -c <command>` for scripts; the words otherwise. A command of
    // only whitespace passed the emptiness check above and dies here, loudly.
    let (argv0, argv_words): (String, Vec<String>) = if sp.kind == ServiceKind::Script {
        (
            String::from("/bin/sh"),
            vec![
                String::from("/bin/sh"),
                String::from("-c"),
                expanded_cmd.clone(),
            ],
        )
    } else {
        if words.is_empty() {
            return Err(SpawnError::EmptyCommand {
                name: ctx.service_name.to_string(),
            }
            .into());
        }
        (words[0].clone(), words)
    };
    let merged_env = merged_environment(
        ctx.env,
        wants_notify.then_some(notify_no(ctx)),
        &ctx.listen
            .iter()
            .map(|(_, n)| n.clone())
            .collect::<Vec<_>>(),
    )?;
    let path_env = path_of(&merged_env);
    let resolved = resolve_binary(&argv0, &path_env).ok_or_else(|| SpawnError::NotFound {
        name: ctx.service_name.to_string(),
        wanted: format!("{argv0} (PATH={path_env})"),
    })?;

    // ── CStrings and pointer arrays. These own every byte the child reads. ──
    let path_c =
        CString::new(resolved.as_os_str().as_bytes()).map_err(|_| SpawnError::BadValue {
            name: ctx.service_name.to_string(),
            what: String::from("resolved binary path contains a NUL byte"),
        })?;
    let mut argv_c: Vec<CString> = Vec::with_capacity(argv_words.len());
    for w in &argv_words {
        argv_c.push(CString::new(w.as_str()).map_err(|_| SpawnError::BadValue {
            name: ctx.service_name.to_string(),
            what: String::from("command word contains a NUL byte"),
        })?);
    }
    let mut argv_ptrs: Vec<*const libc::c_char> = argv_c.iter().map(|c| c.as_ptr()).collect();
    argv_ptrs.push(core::ptr::null());
    let mut env_c: Vec<CString> = Vec::with_capacity(merged_env.len());
    for (k, v) in &merged_env {
        let mut bytes = k.clone();
        bytes.push(b'=');
        bytes.extend_from_slice(v);
        env_c.push(CString::new(bytes).map_err(|_| SpawnError::BadValue {
            name: ctx.service_name.to_string(),
            what: String::from("environment entry contains a NUL byte"),
        })?);
    }
    let mut env_ptrs: Vec<*const libc::c_char> = env_c.iter().map(|c| c.as_ptr()).collect();
    env_ptrs.push(core::ptr::null());

    // ── fds, all CLOEXEC, all before the fork ───────────────────────────
    let log_handle = log::open_sink(ctx.log).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("service `{}`: cannot open log: {e}", ctx.service_name),
        )
    })?;
    // The handle is closed on spawn failure and forgotten (owned by the
    // supervisor) on success. `mem::forget` reads badly but is honest: the
    // fd must outlive this function, and the owner is the returned Spawned.
    let log_fd = log_handle.fd();
    core::mem::forget(log_handle);

    let (notify_r, notify_w) = if wants_notify {
        let (r, w) = zrt::sys::pipe2(true)?;
        (r, w)
    } else {
        (-1, -1)
    };
    let (exec_r, exec_w) = zrt::sys::pipe2(true)?;

    let rlimits = identity::prepare_rlimits(ctx.rlimits).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("service `{}`: bad rlimit: {e}", ctx.service_name),
        )
    })?;

    // Confinement, resolved before the fork. Names become numbers and the
    // filter is assembled here, on the parent side, because the child may
    // not allocate and must not parse. Anything unresolvable refuses the
    // spawn: a half-confined service is a service that believes it is
    // confined.
    let cap_drops = resolve_cap_drops(ctx.service_name, &sp.drop_caps)?;
    let seccomp_filter = resolve_seccomp_filter(ctx.service_name, &sp.seccomp)?;

    let cgroup_procs: Option<CString> = match ctx.cgroup {
        None => None,
        Some(slice) => {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            {
                let procs = identity::ensure_cgroup(ctx.service_name, slice)?;
                Some(CString::new(procs.as_os_str().as_bytes()).map_err(|_| {
                    SpawnError::BadValue {
                        name: ctx.service_name.to_string(),
                        what: String::from("cgroup path contains a NUL byte"),
                    }
                })?)
            }
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            {
                let _ = slice;
                None
            }
        }
    };

    let tty_c: Option<CString> = match (sp.kind, ctx.tty) {
        (ServiceKind::Console, Some(t)) => Some(CString::new(t.as_os_str().as_bytes()).map_err(
            |_| SpawnError::BadValue {
                name: ctx.service_name.to_string(),
                what: String::from("tty path contains a NUL byte"),
            },
        )?),
        _ => None,
    };

    // An empty signal mask, built here because past the fork the child may not
    // do setup work. `fork` copies the supervisor's blocked set and `execve`
    // preserves it, so a child that does not clear it would run with `SIGTERM`
    // blocked — pending, never delivered, un-stoppable until `SIGKILL`.
    let mut empty_mask: libc::sigset_t = unsafe { core::mem::zeroed() };
    // SAFETY: `empty_mask` is a live, zeroed `sigset_t` and `sigemptyset` only
    // clears bits inside it. POSIX says it cannot fail.
    unsafe { libc::sigemptyset(&raw mut empty_mask) };

    // Listen fd numbers, by value: the child dups them onto 3.. and must not
    // touch the supervisor's (name, fd) pairs.
    let listen_numbers: Vec<RawFd> = ctx.listen.iter().map(|(fd, _)| *fd).collect();
    // Parked copies, high: dup targets below (3..) can never clobber a
    // source this way, no matter what numbers the caller holds. Parked
    // here — not by the caller — so the guarantee holds for every spawn,
    // including direct ones that never went through `ensure_listeners`.
    // The caller's originals stay open and owned by the caller; these
    // copies die in the parent right after the fork.
    let mut parked: Vec<RawFd> = Vec::with_capacity(listen_numbers.len());
    for fd in &listen_numbers {
        match zrt::sys::park_high(*fd) {
            Ok(high) => parked.push(high),
            Err(e) => {
                for high in parked {
                    let _ = zrt::sys::close_quietly(high);
                }
                cleanup_failed_spawn(log_fd, notify_r, notify_w, exec_r, exec_w);
                return Err(io::Error::new(
                    e.kind(),
                    format!(
                        "service `{}`: cannot park listen sockets ({e})",
                        ctx.service_name
                    ),
                ));
            }
        }
    }
    // The `LISTEN_PID=` digit field the child fills with its own pid (see
    // `child_step_listen_pid`). Found by prefix; an entry the operator
    // smuggled in with the wrong shape is ignored rather than patched.
    // Plain bytes: the child writes ASCII digits, which need no signedness.
    let mut listen_pid_digits: *mut u8 = core::ptr::null_mut();
    if !ctx.listen.is_empty() {
        for c in &env_c {
            let bytes = c.as_bytes();
            if bytes.len() == 11 + 10 && bytes.starts_with(b"LISTEN_PID=") {
                // SAFETY: `c` is alive in this frame (inherited across the
                // fork); offset 11 is the first of exactly 10 digit bytes.
                listen_pid_digits = unsafe { c.as_ptr().add(11) as *mut u8 };
                break;
            }
        }
    }

    let plan_child = ChildPlan {
        path: path_c.as_ptr(),
        argv: argv_ptrs.as_ptr(),
        envp: env_ptrs.as_ptr(),
        log_fd,
        notify_w,
        notify_r,
        notify_no: notify_no(ctx) as libc::c_int,
        listen_fds: parked.as_ptr(),
        listen_len: parked.len(),
        listen_pid_digits,
        cap_drops: cap_drops.as_ptr(),
        cap_len: cap_drops.len(),
        seccomp_filter: seccomp_filter.as_ptr(),
        seccomp_len: seccomp_filter.len(),
        exec_w,
        cgroup_procs: cgroup_procs
            .as_ref()
            .map_or(core::ptr::null(), |c| c.as_ptr()),
        uid: ctx.run_as.map_or(0, |(u, _)| u),
        gid: ctx.run_as.map_or(0, |(_, g)| g),
        drop_ids: ctx.run_as.is_some(),
        rlimits: rlimits.as_ptr(),
        rlimit_len: rlimits.len(),
        is_console: sp.kind == ServiceKind::Console,
        tty: tty_c.as_ref().map_or(core::ptr::null(), |c| c.as_ptr()),
        sigmask: &raw const empty_mask,
    };

    // SAFETY: `fork` takes no arguments and has no preconditions. The dangers
    // are all child-side (async-signal-safe calls only, no allocator, no
    // locks), and `child_main` below meets every one of them.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let e = io::Error::last_os_error();
        for high in parked {
            let _ = zrt::sys::close_quietly(high);
        }
        cleanup_failed_spawn(log_fd, notify_r, notify_w, exec_r, exec_w);
        return Err(e);
    }
    if pid == 0 {
        child_main(plan_child);
    }
    // The parked copies served their one purpose (surviving the fork with
    // numbers nothing can collide with). The child holds its dups; the
    // caller's originals stay with the caller.
    for high in parked {
        let _ = zrt::sys::close_quietly(high);
    }

    // ── parent ──────────────────────────────────────────────────────────
    // Close the child's ends. The exec-error write-end MUST close here: EOF
    // on `exec_r` is how the parent learns the exec succeeded, and it only
    // arrives when no write-end remains open anywhere in the parent.
    let _ = zrt::sys::close(notify_w);
    let _ = zrt::sys::close(exec_w);
    if notify_r >= 0 {
        if let Err(e) = zrt::sys::set_nonblocking(notify_r, true) {
            cleanup_failed_spawn(log_fd, notify_r, -1, exec_r, -1);
            let _ = zrt::sys::kill_process(pid, zrt::signals::Signal::Kill);
            let _ = zrt::sys::waitpid_blocking(pid);
            return Err(e);
        }
    }
    if let Err(e) = zrt::sys::set_nonblocking(exec_r, true) {
        cleanup_failed_spawn(log_fd, notify_r, -1, exec_r, -1);
        let _ = zrt::sys::kill_process(pid, zrt::signals::Signal::Kill);
        let _ = zrt::sys::waitpid_blocking(pid);
        return Err(e);
    }
    Ok(Spawned {
        pid,
        pgid: pid,
        log_fd: Some(log_fd),
        notify_fd: if notify_r >= 0 { Some(notify_r) } else { None },
        exec_fd: Some(exec_r),
    })
}

/// Close every fd of a spawn that never became a service.
fn cleanup_failed_spawn(
    log_fd: RawFd,
    notify_r: RawFd,
    notify_w: RawFd,
    exec_r: RawFd,
    exec_w: RawFd,
) {
    // Best effort, in any order: this runs after `fork` failed (no child to
    // leak) or before killing a half-built one. `EBADF` (`-1` sentinels) is
    // mapped to success inside `close_quietly`.
    for fd in [log_fd, notify_r, notify_w, exec_r, exec_w] {
        if fd >= 0 {
            let _ = zrt::sys::close_quietly(fd);
        }
    }
}

/// Raw parts the child may read. Plain ints and pointers — `Copy` — so the
/// child function takes it by value and touches no borrowed Rust state.
///
/// Every pointer below points at memory owned by the parent's frame across
/// the fork (which the child inherits as a copy). The child must `execve` or
/// `_exit` before returning: any use past that point would outlive the frame.
#[derive(Clone, Copy)]
struct ChildPlan {
    /// Resolved executable path.
    path: *const libc::c_char,
    /// NUL-terminated argv.
    argv: *const *const libc::c_char,
    /// NUL-terminated envp.
    envp: *const *const libc::c_char,
    /// Log fd, dup'd onto 1 and 2.
    log_fd: RawFd,
    /// Notify write-end, dup'd onto `notify_no` (`-1` when no handshake).
    notify_w: RawFd,
    /// Notify read-end, closed in the child (`-1` when none).
    notify_r: RawFd,
    /// Descriptor the notify write-end lands on: 3 with no listeners, 3+N
    /// with N listeners (which take 3.. first, per the `$LISTEN_FDS`
    /// contract).
    notify_no: libc::c_int,
    /// Listen fds to hand over, dup'd onto 3.. in order. Every source is
    /// parked at 100+ by `spawn` itself right before the fork, so no target
    /// below 100 (`MAX_LISTEN_FDS` caps the count) can clobber a
    /// not-yet-moved source, whatever numbers the caller holds.
    listen_fds: *const RawFd,
    /// How many `listen_fds` points at.
    listen_len: usize,
    /// Digit field of the `LISTEN_PID=` env entry, filled with our own pid
    /// before `execve` (null when no listeners: no placeholder was built).
    /// Plain bytes (`*mut u8`), not `c_char`: the values written are ASCII
    /// digits either way, and a `c_char` field would need a conversion cast
    /// that is a no-op (rightly refused) on unsigned-char platforms.
    listen_pid_digits: *mut u8,
    /// Capability numbers to drop from the bounding set. Linux-only concept:
    /// the parent refuses a non-empty list off Linux (see `resolve_cap_drops`
    /// and `child_step_capdrop`, which consult the length on every target so
    /// the fields stay live everywhere).
    cap_drops: *const u32,
    /// How many `cap_drops` points at.
    cap_len: usize,
    /// Assembled seccomp program (Linux). Empty (`len == 0`) is unconfined;
    /// off Linux the parent refuses a configured filter, so empty is also
    /// all that can arrive there.
    seccomp_filter: *const zrt::seccomp::Insn,
    /// How many `seccomp_filter` points at.
    seccomp_len: usize,
    /// Exec-error write-end. CLOEXEC: a successful exec closes it, which is
    /// exactly the parent's "it worked" signal.
    exec_w: RawFd,
    /// `cgroup.procs` path, or null when no cgroup was requested.
    cgroup_procs: *const libc::c_char,
    /// Target ids; meaningful only when `drop_ids`.
    uid: u32,
    /// Target ids; meaningful only when `drop_ids`.
    gid: u32,
    /// Whether to drop privileges at all.
    drop_ids: bool,
    /// Pre-resolved rlimits (resource, soft).
    rlimits: *const ChildRlimit,
    /// Length of `rlimits`.
    rlimit_len: usize,
    /// Console service: attempt `TIOCSCTTY` when `tty` is non-null.
    is_console: bool,
    /// Controlling terminal path, or null.
    tty: *const libc::c_char,
    /// Empty signal mask, installed by the child before anything else.
    sigmask: *const libc::sigset_t,
}

// SAFETY rationale for `Send`-free raw pointers: `ChildPlan` never crosses a
// thread boundary. It is built on the supervisor's stack, copied by `fork`
// into the child, and consumed there before `execve`. No thread, no race.

/// The only code that ever runs in the child: install the service, `execve`.
///
/// `#[inline(never)]` so the single-exit child path is its own frame in every
/// backtrace and profile — when this function appears somewhere unexpected,
/// something has gone structurally wrong. It diverges (`!`): returning to the
/// supervisor's code in the child is the bug `DESIGN.md` §9.1 exists to
/// prevent. See the module docs for the ordering rationale, step by step.
#[inline(never)]
fn child_main(p: ChildPlan) -> ! {
    // Each step reports its errno to the parent through the exec-error pipe
    // and then dies with 127. The pipe write is best-effort (the parent may
    // have gone away); the `_exit(127)` is not.
    // Zero: let `setpgid` pick, which means "my own pid", the group the
    // supervisor will later signal with `kill(-pid)`.
    if !child_step_sigmask(p) {
        child_fail(p.exec_w, CHILD_STEP_SIGMASK);
    }
    if !child_step_session(0) {
        child_fail(p.exec_w, CHILD_STEP_SESSION);
    }
    if !child_step_fds(p) {
        child_fail(p.exec_w, CHILD_STEP_FDS);
    }
    if !child_step_chdir() {
        child_fail(p.exec_w, CHILD_STEP_CHDIR);
    }
    if !child_step_rlimits(p) {
        child_fail(p.exec_w, CHILD_STEP_RLIMITS);
    }
    if !child_step_cgroup(p) {
        child_fail(p.exec_w, CHILD_STEP_CGROUP);
    }
    if !child_step_ids(p) {
        child_fail(p.exec_w, CHILD_STEP_IDS);
    }
    if !child_step_capdrop(p) {
        child_fail(p.exec_w, CHILD_STEP_CAPDROP);
    }
    child_step_no_new_privs();
    if !child_step_ctty(p) {
        child_fail(p.exec_w, CHILD_STEP_CTTY);
    }
    // Last: our own pid into the `LISTEN_PID=` digit field, so daemons that
    // check it (`sd_listen_fds`) accept the descriptors below. Past this
    // point the child only execs; nothing may allocate, so a fixed field —
    // not a formatted string — is the whole trick.
    child_step_listen_pid(p);
    // Last of all: the filter. Everything after the install — including a
    // failing `execve`'s own error report — runs *under* it, which is why
    // the pid write above and the tty handover earlier both precede it.
    if !child_step_seccomp(p) {
        child_fail(p.exec_w, CHILD_STEP_SECCOMP);
    }
    // SAFETY: `path` is a NUL-terminated string prepared before the fork;
    // `argv`/`envp` are NUL-terminated pointer arrays to NUL-terminated
    // strings, alive in this frame's inherited copy. `execve` reads them and
    // retains nothing; on success it never returns.
    unsafe {
        libc::execve(p.path, p.argv.cast_mut(), p.envp.cast_mut());
    }
    // Only reached on failure. The errno is from `execve` itself.
    child_fail(p.exec_w, CHILD_STEP_EXECVE);
}

/// Report the current errno to the parent, then `_exit(127)`.
///
/// 127 is the universal "cannot execute" status (the shell convention): the
/// supervisor reaps an ordinary `Exited(127)`, and the reconciler spends
/// budget on it like any other failed start. Never silent, never special.
///
/// Eight bytes go down the pipe: the errno first, then the failing step
/// (see `CHILD_STEP_*`). Production readers (`poll_exec`) consume the first
/// four and close — the step word is diagnostic only, discarded with the
/// close — while tests drain all eight to name the culprit.
fn child_fail(exec_w: RawFd, step: u32) -> ! {
    let errno = io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EINVAL) as u32;
    if exec_w >= 0 {
        let mut msg = [0u8; 8];
        msg[..4].copy_from_slice(&errno.to_le_bytes());
        msg[4..].copy_from_slice(&step.to_le_bytes());
        // SAFETY: one `write(2)` of eight bytes to a pipe — far below
        // `PIPE_BUF`, so atomic; partial writes are irrelevant, the parent
        // treats "any bytes" as failure. Best effort: the return is ignored.
        unsafe {
            libc::write(exec_w, msg.as_ptr().cast::<libc::c_void>(), msg.len());
            libc::close(exec_w);
        }
    }
    // SAFETY: `_exit` never returns, takes an int, runs no destructors and
    // flushes nothing — the double-flush-safe exit for a forked child.
    unsafe { libc::_exit(127) };
}

/// Failing-step tags for the exec-error pipe's second word.
///
/// Numbers, not names: the child may not allocate or format. Order follows
/// `child_main`.
const CHILD_STEP_SIGMASK: u32 = 1;
const CHILD_STEP_SESSION: u32 = 2;
const CHILD_STEP_FDS: u32 = 3;
const CHILD_STEP_CHDIR: u32 = 4;
const CHILD_STEP_RLIMITS: u32 = 5;
const CHILD_STEP_CGROUP: u32 = 6;
const CHILD_STEP_IDS: u32 = 7;
const CHILD_STEP_CAPDROP: u32 = 8;
const CHILD_STEP_CTTY: u32 = 9;
const CHILD_STEP_SECCOMP: u32 = 10;
const CHILD_STEP_EXECVE: u32 = 11;

/// New session and process group, before anything else.
///
/// The supervisor stops trees with `kill(-pgid)`. Without this call the
/// child would share the supervisor's group, and signalling "the service"
/// would signal the supervisor too — the self-`SIGKILL` class of bug.
/// Take back a usable signal mask.
///
/// The supervisor blocks every catchable signal at start-up because its own
/// event loop must not be interrupted at an arbitrary instruction. `fork`
/// copies that mask and `execve` does not restore it, so a child that did not
/// clear it would run with `SIGTERM` blocked — pending forever, and un-stoppable
/// until the `stop-timeout` `SIGKILL`.
///
/// First in the sequence because nothing after it should run under the
/// supervisor's mask. Pending signals are cleared by `fork`, so unblocking
/// here cannot deliver one that was queued for the supervisor.
///
/// SAFETY: `p.sigmask` points at a parent-prepared, fully initialised
/// `sigset_t` alive in this frame's inherited copy; `pthread_sigmask` reads it
/// and retains nothing.
fn child_step_sigmask(p: ChildPlan) -> bool {
    // SAFETY: see the SAFETY note above the function.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, p.sigmask, core::ptr::null_mut()) == 0 }
}

fn child_step_session(pgid_want: i32) -> bool {
    // `setsid` is the goal - a new session, so no controlling terminal can be
    // inherited and `kill(-pgid)` is the only way in - but it fails with
    // `EPERM` when the caller is already a process-group leader, and a fresh
    // fork *is* one whenever the supervisor was started by a shell that made
    // itself a leader. Under `cargo test` the harness is exactly that.
    //
    // The earlier version called `setsid`, treated `EPERM` as fatal, and the
    // child `_exit(127)`ed. Intermittent, and for a reason no log line would
    // ever have explained: whether the service started depended on how the
    // supervisor itself had been launched.
    //
    // Falling back to `setpgid(0, 0)` gives up the session - the child could
    // in principle inherit a controlling tty - but keeps the process group,
    // which is the property the supervisor's stop path actually depends on. A
    // service without a session is recoverable; a service that never starts
    // is not.
    //
    // SAFETY: both calls take plain integers. `setsid` keeps no state;
    // `setpgid(0, ...)` makes this process the leader of a group with the
    // given id, which for `pgid_want == 0` means its own pid.
    unsafe {
        if libc::setsid() >= 0 {
            return true;
        }
        let want = if pgid_want > 0 { pgid_want } else { 0 };
        libc::setpgid(0, want) == 0 || libc::getpgrp() == want
    }
}

/// Install the fd contract: `/dev/null` → 0, log → 1,2, notify → `notify_no`,
/// listen sockets → 3...
///
/// Runs before any privilege-affecting step so the interface promise is in
/// place regardless of what fails later (and a later failure `_exit`s, so a
/// half-installed stdio is never observed by anyone but the dying child).
/// Originals are closed: the only fds crossing the `exec` are 0, 1, 2, the
/// listeners and possibly the notifier — everything the supervisor holds
/// stays `CLOEXEC` on its side.
///
/// The listen handover is single-phase and safe by construction: every
/// source is parked at 100+ (see `SpawnCtx::listen`) while every target is
/// below it (`MAX_LISTEN_FDS` caps the count), so no `dup2` target can ever
/// clobber a not-yet-moved source. `dup2` onto an already-correct number is
/// a no-op success, and the matching close is skipped with it.
fn child_step_fds(p: ChildPlan) -> bool {
    // SAFETY: every call below is a raw syscall on ints. `open` of a static
    // literal, `dup2`, `close` — all async-signal-safe, no allocation, no
    // locks. Failure of any one aborts the spawn.
    unsafe {
        // The notify read-end goes first, before anything is dup'd anywhere:
        // a later `dup2` may legitimately reuse its number for a target
        // (the targets are fixed low numbers, the sources are whatever was
        // free), and closing it *after* such a reuse would amputate the very
        // pipe just installed — a live child with a dead handshake, the one
        // failure this function's ordering exists to prevent. The parent
        // keeps its own copy; only this end dies here.
        if p.notify_r >= 0 {
            libc::close(p.notify_r);
        }
        let null_r = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if null_r < 0 {
            return false;
        }
        if libc::dup2(null_r, 0) < 0 {
            return false;
        }
        if null_r > 0 {
            libc::close(null_r);
        }
        if libc::dup2(p.log_fd, 1) < 0 {
            return false;
        }
        if libc::dup2(p.log_fd, 2) < 0 {
            return false;
        }
        if p.log_fd > 2 {
            libc::close(p.log_fd);
        }
        if p.notify_w >= 0 {
            if libc::dup2(p.notify_w, p.notify_no) < 0 {
                return false;
            }
            if p.notify_w != p.notify_no {
                libc::close(p.notify_w);
            }
        }
        for i in 0..p.listen_len {
            // SAFETY: `listen_fds` points at `listen_len` parent-prepared
            // ints; the loop bounds the reads.
            let fd = *p.listen_fds.add(i);
            let target = 3 + i as libc::c_int;
            if libc::dup2(fd, target) < 0 {
                return false;
            }
            if fd != target {
                libc::close(fd);
            }
        }
        // The exec-error write-end stays open (CLOEXEC): a successful exec
        // closes it, which is the parent's success signal. Closed explicitly
        // on the failure path in `child_fail`.
        true
    }
}

/// Leave the supervisor's working directory. A service that holds a
/// directory (or mount) busy pins it for the whole uptime of the machine.
fn child_step_chdir() -> bool {
    // SAFETY: static literal, NUL-terminated by construction.
    unsafe { libc::chdir(c"/".as_ptr()) == 0 }
}

/// Install the soft resource limits. Hard limits travel untouched.
///
/// Runs while still fully privileged, so lowering works unconditionally;
/// raising the soft limit past the hard one fails loudly (`EPERM` →
/// `_exit(127)`) instead of silently capping — the operator asked for a
/// number, and "a smaller number" is not that number.
fn child_step_rlimits(p: ChildPlan) -> bool {
    for i in 0..p.rlimit_len {
        // SAFETY: `rlimits` points at `rlimit_len` parent-prepared entries in
        // the inherited copy; reading them is bounds-checked by the loop.
        let r: ChildRlimit = unsafe { *p.rlimits.add(i) };
        let lim = libc::rlimit {
            rlim_cur: r.cur as libc::rlim_t,
            rlim_max: r.max as libc::rlim_t,
        };
        // SAFETY: `setrlimit` reads one `rlimit` through the pointer and
        // retains nothing. `r.resource as _` resolves against the callee's
        // (private since libc 0.2.189) resource type.
        if unsafe { libc::setrlimit(r.resource as _, &raw const lim) } != 0 {
            return false;
        }
    }
    true
}

/// Write our own pid to `cgroup.procs`, while still privileged.
///
/// The pid is formatted into a stack buffer by hand: formatting without an
/// allocator, because there is no allocator past the fork.
fn child_step_cgroup(p: ChildPlan) -> bool {
    if p.cgroup_procs.is_null() {
        return true;
    }
    // SAFETY: `cgroup_procs` is a parent-prepared NUL-terminated path;
    // `open`/`write`/`close` on ints and a stack buffer are signal-safe.
    unsafe {
        let fd = libc::open(p.cgroup_procs, libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return false;
        }
        let pid = libc::getpid();
        let (buf, len) = pid_ascii(pid);
        let mut done = 0;
        while done < len {
            let n = libc::write(
                fd,
                buf.as_ptr().add(done).cast::<libc::c_void>(),
                len - done,
            );
            if n <= 0 {
                libc::close(fd);
                return false;
            }
            done += n as usize;
        }
        libc::close(fd);
        true
    }
}

/// Format a pid as ASCII, on the stack. No allocation, no std.
fn pid_ascii(pid: i32) -> ([u8; 12], usize) {
    let mut buf = [0u8; 12];
    let mut n = pid as i64;
    let neg = n < 0;
    if neg {
        n = -n;
    }
    let mut i = buf.len();
    if n == 0 {
        i -= 1;
        buf[i] = b'0';
    } else {
        while n > 0 {
            i -= 1;
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    (buf, buf.len() - i)
}

/// The privilege drop: `setgroups`, `setresgid`, `setresuid`, verify-or-die.
///
/// The last privileged act, in `DESIGN.md` §9 order. Uses the real three-id
/// variants on Linux (atomic: no window to regain privilege), the checked
/// `setgid`/`setuid` fallback elsewhere, and verifies all four ids
/// afterwards — any mismatch fails the spawn rather than running the service
/// with more privilege than its description allows.
fn child_step_ids(p: ChildPlan) -> bool {
    if !p.drop_ids {
        return true;
    }
    // SAFETY: every call is a raw syscall on ints. `setgroups(0, NULL)` is
    // the documented empty-set form; the kernel checks the count first.
    unsafe {
        if libc::setgroups(0, core::ptr::null()) != 0 {
            return false;
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            if libc::setresgid(p.gid, p.gid, p.gid) != 0 {
                return false;
            }
            if libc::setresuid(p.uid, p.uid, p.uid) != 0 {
                return false;
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            if libc::setgid(p.gid) != 0 {
                return false;
            }
            if libc::setuid(p.uid) != 0 {
                return false;
            }
        }
        // Verify, don't trust: under some LSMs the kernel may not have done
        // what it was asked. Rule 2, `DESIGN.md` §9.
        if libc::getuid() != p.uid
            || libc::geteuid() != p.uid
            || libc::getgid() != p.gid
            || libc::getegid() != p.gid
        {
            return false;
        }
    }
    true
}

/// Forbid gaining privilege back via `exec` (Linux, best-effort).
///
/// After this, even a setuid-root binary the service runs cannot escalate
/// it. Failure is ignored on purpose: on kernels without the flag the uid
/// drop above is already complete, and aborting a correct drop for lack of a
/// hardening extra would be the tail wagging the dog. Documented, not hidden.
fn child_step_no_new_privs() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: `prctl(PR_SET_NO_NEW_PRIVS, 1)` takes int-like args; the
        // return is deliberately unchecked (see above).
        unsafe {
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        }
    }
}

/// Drop capabilities from the bounding set.
///
/// Runs right after the uid drop: reducing our own bounding set needs no
/// privilege, and everything after this point runs with exactly the
/// capabilities the operator allowed. Irreversible by design — once dropped
/// from the bounding set, a capability cannot be regained, even across a
/// setuid `execve`. Failure fails the spawn: running with *more* privilege
/// than configured is the one outcome this step must never produce.
///
/// The length is consulted on every platform so the fields stay live
/// everywhere; only Linux acts on them (off Linux the parent refuses a
/// non-empty list, so the loop below is unreachable there).
// `cap as c_ulong` is a real conversion on LP64 and the identity on ILP32,
// where the cast is redundant and rightly refused. The allow documents the
// split rather than hiding it — same pattern as the mount-flags cast in
// `zinit --init`.
#[allow(clippy::unnecessary_cast)]
fn child_step_capdrop(p: ChildPlan) -> bool {
    if p.cap_len == 0 {
        return true;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // `PR_CAPBSET_DROP` is 24 (linux/prctl.h); defined here so a missing
        // binding is impossible by construction, like the seccomp constants.
        const PR_CAPBSET_DROP: libc::c_int = 24;
        for i in 0..p.cap_len {
            // SAFETY: `cap_drops` points at `cap_len` parent-prepared numbers;
            // the loop bounds the reads. `prctl` retains nothing.
            let cap: u32 = unsafe { *p.cap_drops.add(i) };
            if unsafe { libc::prctl(PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) } != 0 {
                return false;
            }
        }
        true
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // Unreachable in practice (see above): a non-empty list cannot arrive
        // here, and failing closed beats running unconfined. Reading the
        // pointer keeps the field live on targets that never act on it.
        let _ = p.cap_drops;
        false
    }
}

/// Take over the controlling terminal for `console` services.
///
/// Only when a tty path was supplied; without one the service runs
/// session-led without a controlling terminal (a console without a tty is a
/// configuration the supervisor reports, not one it kills the boot over —
/// the failure mode of refusing would be "no login prompt", which is worse
/// than "login prompt without job control").
fn child_step_ctty(p: ChildPlan) -> bool {
    if !p.is_console || p.tty.is_null() {
        return true;
    }
    // SAFETY: raw syscalls on ints and a prepared path. `TIOCSCTTY` with
    // argument 0 is the BSD spelling; Linux accepts it as "no steal".
    unsafe {
        let fd = libc::open(p.tty, libc::O_RDWR | libc::O_CLOEXEC);
        if fd < 0 {
            return false;
        }
        #[allow(clippy::unnecessary_cast)]
        let req = libc::TIOCSCTTY as libc::c_ulong;
        // A console that cannot take its tty still runs: job control without
        // a controlling terminal degrades to plain session semantics, and the
        // supervisor says so in the service's log via the readiness path.
        let _ = libc::ioctl(fd, req as _, 0);
        libc::close(fd);
        true
    }
}

/// Write our own pid into the `LISTEN_PID=` digit field, as plain decimal.
///
/// The parent built the environment before the fork — when our pid did not
/// exist yet — and left a fixed 10-digit field for exactly this. The digits
/// go in from the left with a fresh NUL right after them, so the entry reads
/// as plain decimal (`18477`, not `0000018477`): ten digits hold any `u32`
/// pid, and the parent's terminator sits exactly where a 10-digit pid needs
/// it. `getpid` here is infallible and the field cannot overflow, so this
/// step cannot fail: there is no `bool` to return and no errno to report.
/// A null field (no listeners) is a no-op.
fn child_step_listen_pid(p: ChildPlan) {
    if p.listen_pid_digits.is_null() {
        return;
    }
    // SAFETY: the field is 10 parent-prepared bytes in this frame's inherited
    // copy, plus the parent's terminator right after them. `getpid` takes
    // nothing and retains nothing; only plain integer arithmetic below.
    unsafe {
        let mut pid = libc::getpid();
        let digits = p.listen_pid_digits;
        if pid <= 0 {
            // Unreachable in practice (`getpid` is always positive); a `0`
            // keeps the entry valid rather than leaving ten stale zeroes.
            *digits = b'0';
            *digits.add(1) = 0;
            return;
        }
        let mut rev = [0u8; 10];
        let mut len = 0usize;
        while pid > 0 && len < 10 {
            rev[len] = b'0' + (pid % 10) as u8;
            pid /= 10;
            len += 1;
        }
        let mut i = 0;
        while i < len {
            *digits.add(i) = rev[len - 1 - i];
            i += 1;
        }
        *digits.add(len) = 0;
    }
}

/// Install the seccomp-bpf filter: the last thing before `execve`.
///
/// Last on purpose. Everything after the install — including a failing
/// `execve`'s own error report — runs *under* the filter, and the filter
/// deliberately allows almost nothing past this point (not even `getpid`,
/// which is why the pid write above precedes it, and not the `open`/`ioctl`
/// the tty handover needed, which is why that precedes it too). A service
/// that needs a call the filter forbids dies loudly (killed or `EPERM`, per
/// the configured action) instead of escaping it. An empty program is
/// unconfined and always succeeds.
fn child_step_seccomp(p: ChildPlan) -> bool {
    if p.seccomp_len == 0 {
        return true;
    }
    // SAFETY: `seccomp_filter` points at `seccomp_len` parent-assembled
    // instructions in this frame's inherited copy; `install` copies the
    // program into the kernel and retains nothing.
    unsafe { zrt::seccomp::install(core::slice::from_raw_parts(p.seccomp_filter, p.seccomp_len)) }
        .is_ok()
}

/// Resolve `argv[0]` to an executable path, searching `PATH` when needed.
///
/// Absolute (or explicitly relative, containing a `/`) names are used
/// verbatim — the child performs no search. Bare names walk `PATH` left to
/// right with `access(X_OK)`, first hit wins. Parent side: directory reads
/// and allocation are legal here and forbidden in the child.
fn resolve_binary(argv0: &str, path_env: &str) -> Option<PathBuf> {
    if argv0.contains('/') {
        let p = PathBuf::from(argv0);
        if is_executable(&p) {
            return Some(p);
        }
        return None;
    }
    for dir in path_env.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = PathBuf::from(format!("{dir}/{argv0}"));
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// True when `path` exists and the supervisor could execute it.
fn is_executable(path: &Path) -> bool {
    let c = match CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };
    // SAFETY: `c` is NUL-terminated and alive; `access` reads nothing else
    // and retains nothing.
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// The supervisor's environment overlaid with the service's.
///
/// Order: inherit everything, then apply `overlay` left-to-right (last
/// binding wins, mirroring what `execve` consumers observe), then pin the
/// supervisor-owned variables: `ZINIT_NOTIFY_FD` when the handshake needs
/// it, and the `LISTEN_*` trio when sockets are passed down. Pinning
/// overwrites a service-supplied value on purpose — the numbers describe
/// descriptors this spawn created, and a service file that sets them by hand
/// describes descriptors that do not exist. Values are expanded
/// ([`expand_env_value`]) against the inherited environment.
fn merged_environment(
    overlay: &[(String, String)],
    notify_no: Option<u32>,
    listen_names: &[String],
) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (k, v) in std::env::vars_os() {
        pairs.push((k.as_encoded_bytes().to_vec(), v.as_encoded_bytes().to_vec()));
    }
    let lookup_in = |pairs: &[(Vec<u8>, Vec<u8>)], name: &str| -> Option<String> {
        pairs
            .iter()
            .rev()
            .find(|(k, _)| k == name.as_bytes())
            .and_then(|(_, v)| String::from_utf8(v.clone()).ok())
    };
    for (k, v) in overlay {
        if k.is_empty() || k.bytes().any(|b| b == b'=' || b == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad environment key `{k}`"),
            ));
        }
        let expanded = expand_env_value(v, &|name| lookup_in(&pairs, name));
        match pairs.iter_mut().rev().find(|(ek, _)| ek == k.as_bytes()) {
            Some(slot) => slot.1 = expanded.into_bytes(),
            None => pairs.push((k.as_bytes().to_vec(), expanded.into_bytes())),
        }
    }
    if let Some(no) = notify_no {
        pin(&mut pairs, "ZINIT_NOTIFY_FD", no.to_string().as_bytes());
    }
    if !listen_names.is_empty() {
        pin(
            &mut pairs,
            "LISTEN_FDS",
            listen_names.len().to_string().as_bytes(),
        );
        pin(&mut pairs, "LISTEN_PID", b"0000000000");
        let joined = listen_names.join(":");
        pin(&mut pairs, "LISTEN_FDNAMES", joined.as_bytes());
    }
    Ok(pairs)
}

/// Set `key` to `value`, replacing any binding the service (or the
/// supervisor's own environment) already gave it. Last binding wins at
/// `execve`, and these keys describe this spawn's descriptors — inheriting
/// or overlaying them would point the service at someone else's fds.
fn pin(pairs: &mut Vec<(Vec<u8>, Vec<u8>)>, key: &str, value: &[u8]) {
    match pairs.iter_mut().rev().find(|(k, _)| k == key.as_bytes()) {
        Some(slot) => slot.1 = value.to_vec(),
        None => pairs.push((key.as_bytes().to_vec(), value.to_vec())),
    }
}

/// `PATH` from a merged environment, with the conventional fallback.
fn path_of(env: &[(Vec<u8>, Vec<u8>)]) -> String {
    env.iter()
        .rev()
        .find(|(k, _)| k == b"PATH")
        .and_then(|(_, v)| String::from_utf8(v.clone()).ok())
        .unwrap_or_else(|| String::from("/usr/bin:/bin"))
}

/// Process-environment lookup for expansion at spawn time.
fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Expand `$VAR`, `${VAR}` and `${VAR:-default}` in `input`.
///
/// The `${VAR:-default}` form covers the`"unset or empty"` case, which is the
/// one service files actually need (`PORT=${PORT:-8080}`). Anything else
/// starting with `$` that is not one of these forms — `$$`, `$5`, a trailing
/// `$` — is passed through literally: inventing a value for syntax this
/// function does not understand would be worse than leaving it for the shell
/// inside a `script` service to interpret.
///
/// Parent side only (allocates). Applied to `command` and to every `env`
/// value at spawn time, against the supervisor's own environment overlaid
/// with the service's earlier bindings.
pub fn expand_env_value(input: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            match bytes[i + 2..].iter().position(|&b| b == b'}') {
                Some(end) => {
                    let inner = &input[i + 2..i + 2 + end];
                    let (name, default) = match inner.find(":-") {
                        Some(at) => (&inner[..at], Some(&inner[at + 2..])),
                        None => (inner, None),
                    };
                    let value = lookup(name);
                    let use_default = match &value {
                        None => true,
                        Some(v) => default.is_some() && v.is_empty(),
                    };
                    if use_default {
                        out.push_str(default.unwrap_or(""));
                    } else if let Some(v) = value {
                        out.push_str(&v);
                    }
                    i += 2 + end + 1;
                }
                None => {
                    // Unterminated `${`: literal. A loud config error would be
                    // nicer, but expansion is best-effort by contract — the
                    // shell inside `script` services has the final word.
                    out.push('$');
                    i += 1;
                }
            }
            continue;
        }
        // `$NAME`: longest run of alphanumerics and underscore, starting
        // with a letter or underscore. A digit first (`$5`) is a positional
        // parameter, not a name — passed through literally like `$$`.
        let mut j = i + 1;
        let starts_name = j < bytes.len() && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'_');
        if starts_name {
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let name = &input[i + 1..j];
            if let Some(v) = lookup(name) {
                out.push_str(&v);
            }
            // Unset without a default expands to empty — the shell convention,
            // and the reason `${VAR:-default}` exists for callers that care.
            i = j;
            continue;
        }
        out.push('$');
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcore::{LogSink, Plan, ServicePlan};

    fn plan_with(sp: ServicePlan) -> Plan {
        Plan {
            services: vec![sp],
            order_up: vec![0],
            order_down: vec![0],
        }
    }

    fn process_plan(name: &str) -> Plan {
        let mut sp = ServicePlan::new(String::from(name));
        sp.ready = zcore::Ready::None;
        sp.log = LogSink::None;
        plan_with(sp)
    }

    fn script_plan(name: &str) -> Plan {
        let mut sp = ServicePlan::new(String::from(name));
        sp.kind = ServiceKind::Script;
        sp.ready = zcore::Ready::None;
        sp.log = LogSink::None;
        plan_with(sp)
    }

    fn ctx<'a>(command: &'a str, env: &'a [(String, String)], log: &'a LogSink) -> SpawnCtx<'a> {
        SpawnCtx {
            command,
            env,
            run_as: None,
            rlimits: &[],
            cgroup: None,
            log,
            service_name: "test",
            tty: None,
            listen: &[],
        }
    }

    /// Reap one spawned child and close its custodial fds. Every test that
    /// forks goes through here, so no test can leak a zombie or an fd into
    /// the next one.
    fn reap(s: &Spawned) -> zrt::sys::ExitStatus {
        for fd in [s.log_fd, s.notify_fd, s.exec_fd].into_iter().flatten() {
            let _ = zrt::sys::close_quietly(fd);
        }
        zrt::sys::waitpid_blocking(s.pid).expect("reap").status
    }

    fn kill_and_reap(s: &Spawned) -> zrt::sys::ExitStatus {
        let _ = zrt::sys::kill_process(s.pid, zrt::signals::Signal::Kill);
        reap(s)
    }

    #[test]
    fn target_never_forks() {
        let mut sp = ServicePlan::new(String::from("net.target"));
        sp.kind = ServiceKind::Target;
        let plan = plan_with(sp);
        let log = LogSink::None;
        let c = ctx("", &[], &log);
        let e = spawn(&plan, 0, &c).expect_err("a target must never fork");
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert!(e.to_string().contains("target"));
    }

    #[test]
    fn empty_command_is_refused_before_the_fork() {
        let plan = process_plan("empty");
        let log = LogSink::None;
        for cmd in ["", "   "] {
            let c = ctx(cmd, &[], &log);
            assert!(spawn(&plan, 0, &c).is_err(), "`{cmd}` must be refused");
        }
    }

    #[test]
    fn unknown_service_index_is_refused() {
        let plan = process_plan("a");
        let log = LogSink::None;
        let c = ctx(crate::testutil::true_bin(), &[], &log);
        let e = spawn(&plan, 9, &c).expect_err("bad index");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn true_exits_zero() {
        let plan = process_plan("t");
        let log = LogSink::None;
        let c = ctx(crate::testutil::true_bin(), &[], &log);
        let s = spawn(&plan, 0, &c).expect("spawn");
        assert_eq!(s.pgid, s.pid, "setsid makes the child its own group");
        assert_eq!(reap(&s), zrt::sys::ExitStatus::Exited(0));
    }

    #[test]
    fn group_leader_parent_still_spawns_with_own_group() {
        // Regression for the intermittent `setsid` EPERM boot failure: a fresh
        // fork *is* a process-group leader whenever the supervisor itself was
        // launched by a group-leading shell (always, under `cargo test`), so
        // `setsid` fails and only the `setpgid(0, 0)` fallback in
        // `child_step_session` saves the spawn. This runs in that exact
        // condition, so `pgid == pid` here proves the fallback path works.
        let plan = process_plan("grp");
        let log = LogSink::None;
        let c = ctx(crate::testutil::true_bin(), &[], &log);
        let s = spawn(&plan, 0, &c).expect("spawn under group-leading parent");
        assert_eq!(s.pgid, s.pid, "fallback must still own its group");
        assert_eq!(reap(&s), zrt::sys::ExitStatus::Exited(0));
    }

    #[test]
    fn exit_code_is_preserved() {
        // Through the shell: quoting belongs to `script`, because `process`
        // splits on whitespace without quote processing (documented on
        // `SpawnCtx::command`).
        let plan = script_plan("code");
        let log = LogSink::None;
        let c = ctx("exit 42", &[], &log);
        let s = spawn(&plan, 0, &c).expect("spawn");
        assert_eq!(reap(&s), zrt::sys::ExitStatus::Exited(42));
    }

    #[test]
    fn sleep_takes_its_argument() {
        // Bare `sleep` exits 1 (missing operand); `sleep 0` exits 0. The exit
        // code therefore proves the argument survived argv construction.
        let plan = process_plan("arg");
        let log = LogSink::None;
        let c = ctx("/bin/sleep 0", &[], &log);
        let s = spawn(&plan, 0, &c).expect("spawn");
        assert_eq!(reap(&s), zrt::sys::ExitStatus::Exited(0));
    }

    #[test]
    fn script_runs_through_the_shell() {
        let mut sp = ServicePlan::new(String::from("scr"));
        sp.kind = ServiceKind::Script;
        sp.ready = zcore::Ready::None;
        sp.log = LogSink::None;
        let plan = plan_with(sp);
        let log = LogSink::None;
        let c = ctx("exit $((40 + 2))", &[], &log);
        let s = spawn(&plan, 0, &c).expect("spawn");
        assert_eq!(reap(&s), zrt::sys::ExitStatus::Exited(42));
    }

    #[test]
    fn missing_binary_is_refused_before_the_fork() {
        let plan = process_plan("gone");
        let log = LogSink::None;
        let c = ctx("/nonexistent/zinit-probe-binary", &[], &log);
        let e = spawn(&plan, 0, &c).expect_err("missing binary");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn bare_name_resolves_via_path() {
        let plan = process_plan("bare");
        let log = LogSink::None;
        let c = ctx("true", &[], &log);
        let s = spawn(&plan, 0, &c).expect("PATH search");
        assert_eq!(reap(&s), zrt::sys::ExitStatus::Exited(0));
    }

    #[test]
    fn environment_reaches_the_child() {
        let plan = script_plan("env");
        let log = LogSink::None;
        let env = [(String::from("ZINIT_PROBE"), String::from("yes"))];
        let c = ctx("test \"$ZINIT_PROBE\" = yes", &env, &log);
        let s = spawn(&plan, 0, &c).expect("spawn");
        assert_eq!(reap(&s), zrt::sys::ExitStatus::Exited(0));
    }

    #[test]
    fn expansion_forms() {
        let lookup = |n: &str| match n {
            "A" => Some(String::from("1")),
            "EMPTY" => Some(String::new()),
            _ => None,
        };
        assert_eq!(expand_env_value("$A", &lookup), "1");
        assert_eq!(expand_env_value("${A}", &lookup), "1");
        assert_eq!(expand_env_value("${MISSING:-dflt}", &lookup), "dflt");
        assert_eq!(expand_env_value("${A:-dflt}", &lookup), "1");
        assert_eq!(expand_env_value("${EMPTY:-dflt}", &lookup), "dflt");
        assert_eq!(expand_env_value("${EMPTY}", &lookup), "");
        assert_eq!(expand_env_value("${MISSING}", &lookup), "");
        assert_eq!(expand_env_value("x$MISSING y", &lookup), "x y");
        assert_eq!(
            expand_env_value("$$ $5 trailing$", &lookup),
            "$$ $5 trailing$"
        );
    }

    #[test]
    fn killed_sleep_reports_a_signal() {
        let plan = process_plan("slp");
        let log = LogSink::None;
        let c = ctx("/bin/sleep 30", &[], &log);
        let s = spawn(&plan, 0, &c).expect("spawn");
        assert_eq!(
            kill_and_reap(&s),
            zrt::sys::ExitStatus::Signaled {
                signal: 9,
                core_dumped: false
            }
        );
    }

    #[test]
    fn unknown_rlimit_fails_before_the_fork() {
        let plan = process_plan("rl");
        let log = LogSink::None;
        let rl = [(String::from("bogus"), 1u64)];
        let c = SpawnCtx {
            rlimits: &rl,
            ..ctx(crate::testutil::true_bin(), &[], &log)
        };
        assert!(spawn(&plan, 0, &c).is_err());
    }

    #[test]
    fn console_without_tty_runs_like_process() {
        let mut sp = ServicePlan::new(String::from("con"));
        sp.kind = ServiceKind::Console;
        sp.ready = zcore::Ready::None;
        sp.log = LogSink::None;
        let plan = plan_with(sp);
        let log = LogSink::None;
        let c = ctx(crate::testutil::true_bin(), &[], &log);
        let s = spawn(&plan, 0, &c).expect("spawn");
        assert_eq!(reap(&s), zrt::sys::ExitStatus::Exited(0));
    }

    #[test]
    fn notify_spawn_exposes_the_handshake_fd() {
        let mut sp = ServicePlan::new(String::from("ntf"));
        sp.kind = ServiceKind::Script;
        sp.ready = zcore::Ready::Notify;
        sp.log = LogSink::None;
        let plan = plan_with(sp);
        let log = LogSink::None;
        // The child only proves the variable exists; the pipe itself is
        // exercised in `service.rs` tests end to end.
        let c = ctx("test -n \"$ZINIT_NOTIFY_FD\"", &[], &log);
        let s = spawn(&plan, 0, &c).expect("spawn");
        assert!(s.notify_fd.is_some());
        assert!(s.exec_fd.is_some());
        assert_eq!(reap(&s), zrt::sys::ExitStatus::Exited(0));
    }

    #[test]
    fn pid_ascii_formats_without_allocation() {
        let cases = [
            (0, "0"),
            (1, "1"),
            (42, "42"),
            (65534, "65534"),
            (2147483647, "2147483647"),
        ];
        for (n, want) in cases {
            let (buf, len) = pid_ascii(n);
            // The digits are right-aligned at the end of the buffer.
            assert_eq!(&buf[buf.len() - len..], want.as_bytes(), "pid {n}");
        }
    }

    /// Drain the exec-error pipe for a failure message: eight bytes are
    /// `errno` + failing step (see `CHILD_STEP_*`); EOF is a clean exec.
    fn exec_verdict(exec: RawFd) -> String {
        fn step_name(step: u32) -> &'static str {
            match step {
                1 => "sigmask",
                2 => "session",
                3 => "fds",
                4 => "chdir",
                5 => "rlimits",
                6 => "cgroup",
                7 => "ids",
                8 => "capdrop",
                9 => "ctty",
                10 => "seccomp",
                11 => "execve",
                _ => "unknown-step",
            }
        }
        let mut ebuf = [0u8; 8];
        let mut egot = 0;
        loop {
            match zrt::sys::read(exec, &mut ebuf[egot..]) {
                Ok(0) if egot == 0 => break String::from("exec ok"),
                Ok(0) => {
                    break format!("exec pipe truncated after {egot} bytes");
                }
                Ok(n) => {
                    egot += n;
                    if egot >= 8 {
                        let errno = u32::from_le_bytes(ebuf[..4].try_into().expect("slice"));
                        let step = u32::from_le_bytes(ebuf[4..].try_into().expect("slice"));
                        break format!("exec failed at {}: errno {errno}", step_name(step));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    break String::from("exec undecided yet");
                }
                Err(e) => panic!("exec pipe read: {e}"),
            }
        }
    }

    /// Listen sockets arrive on fd 3.. with names and the child's own pid in
    /// the environment — the whole activation contract, asserted from inside
    /// a real (script) child.
    #[test]
    fn listen_sockets_arrive_with_names_and_pid() {
        let plan = script_plan("lst");
        let log = LogSink::None;
        let (a, b) = zrt::sys::socketpair().expect("socketpair");
        let listen = [(a, String::from("test"))];
        let probe = "test \"$LISTEN_FDS\" = 1 && test \"$LISTEN_FDNAMES\" = test";
        let probe_pid = "test \"$LISTEN_PID\" = $$";
        let cmd = format!("{probe} && {probe_pid}");
        let c = SpawnCtx {
            listen: &listen,
            ..ctx(&cmd, &[], &log)
        };
        let s = spawn(&plan, 0, &c).expect("spawn");
        // Ours to close: the child holds its own dups, and `reap` only
        // releases what `Spawned` owns (the listen set outlives one start).
        let _ = zrt::sys::close_quietly(a);
        let _ = zrt::sys::close_quietly(b);
        // The exec verdict, drained for the failure message: `Exited(127)`
        // below means the child path failed before `execve` (with its step
        // and errno here), not that the script's probes failed.
        let exec_verdict = exec_verdict(s.exec_fd.expect("exec pipe"));
        assert_eq!(
            reap(&s),
            zrt::sys::ExitStatus::Exited(0),
            "exec verdict was: {exec_verdict}"
        );
    }

    /// A notify handshake and a listen socket share the child without
    /// colliding: listeners take 3.., the notifier moves past them.
    #[test]
    fn notify_moves_past_listen_sockets() {
        let mut sp = ServicePlan::new(String::from("both"));
        sp.kind = ServiceKind::Script;
        sp.ready = zcore::Ready::Notify;
        sp.log = LogSink::None;
        let plan = plan_with(sp);
        let log = LogSink::None;
        let (a, b) = zrt::sys::socketpair().expect("socketpair");
        let listen = [(a, String::from("sock"))];
        let c = SpawnCtx {
            listen: &listen,
            ..ctx(
                "test \"$ZINIT_NOTIFY_FD\" = 4 && echo READY=1 >&$ZINIT_NOTIFY_FD",
                &[],
                &log,
            )
        };
        let s = spawn(&plan, 0, &c).expect("spawn");
        assert!(s.notify_fd.is_some());
        let _ = zrt::sys::close_quietly(a);
        let _ = zrt::sys::close_quietly(b);
        // The exec verdict first: a child that never exec'd (missing shell,
        // dead binary) reports here, not on the notify pipe. EOF means the
        // image was replaced; eight bytes mean step + errno; WouldBlock means
        // the child has not decided yet and the notify drain below is the
        // verdict that counts.
        let exec_verdict = exec_verdict(s.exec_fd.expect("exec pipe"));
        // The child wrote READY=1 to fd 4; drain it from our read end like
        // the supervisor's readiness poll would, with a deadline instead of
        // a single optimistic read — the child may not have run yet.
        let notify = s.notify_fd.expect("pipe");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut buf = [0u8; 16];
        let mut got = 0;
        let mut saw_eof = false;
        let mut timed_out = false;
        while got < 8 {
            match zrt::sys::read(notify, &mut buf[got..]) {
                Ok(0) => {
                    saw_eof = true;
                    break;
                }
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() > deadline {
                        timed_out = true;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("notify read: {e}"),
            }
        }
        assert!(
            buf[..got].starts_with(b"READY=1"),
            "notify on moved fd: got={got} eof={saw_eof} timeout={timed_out} exec=[{exec_verdict}] bytes={:?}",
            &buf[..got]
        );
        let _ = zrt::sys::kill_process(s.pid, zrt::signals::Signal::Kill);
        let _ = reap(&s);
    }
}
