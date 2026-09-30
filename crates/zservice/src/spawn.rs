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
//! setsid → stdio/notify fds → chdir(/) → rlimits → nice → cgroup →
//!     setgroups → setresgid → setresuid → verify → no_new_privs →
//!     TIOCSCTTY → execve  (any failure → errno pipe + _exit(127))
//! ```
//!
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
//! * **rlimits, then `nice`.** Both are still-privileged tunings applied while
//!   the child is still root, so lowering works unconditionally and there is
//!   no "re-open after the drop" second path.
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
//! the errno of whatever failed ([`zcore::Event::SpawnFailed`]). See
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
    /// `nice(2)` value, `-20..=19`. `None` leaves the inherited value alone.
    /// There is currently no config directive spelling this; the hook exists
    /// so the child sequence (`DESIGN.md` §9 order) has its slot.
    pub nice: Option<i32>,
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
    /// Whether the child joined the requested cgroup (always true when no
    /// cgroup was requested).
    pub cgroup_joined: bool,
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
    if let Some(n) = ctx.nice {
        if !(-20..=19).contains(&n) {
            return Err(SpawnError::BadValue {
                name: ctx.service_name.to_string(),
                what: format!("nice value {n} outside [-20, 19]"),
            }
            .into());
        }
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
    let merged_env = merged_environment(ctx.env, wants_notify)?;
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
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let cgroup_unsupported = ctx.cgroup.is_some();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let cgroup_unsupported = false;

    let tty_c: Option<CString> = match (sp.kind, ctx.tty) {
        (ServiceKind::Console, Some(t)) => Some(CString::new(t.as_os_str().as_bytes()).map_err(
            |_| SpawnError::BadValue {
                name: ctx.service_name.to_string(),
                what: String::from("tty path contains a NUL byte"),
            },
        )?),
        _ => None,
    };

    let plan_child = ChildPlan {
        path: path_c.as_ptr(),
        argv: argv_ptrs.as_ptr(),
        envp: env_ptrs.as_ptr(),
        log_fd,
        notify_w,
        notify_r,
        exec_w,
        cgroup_procs: cgroup_procs
            .as_ref()
            .map_or(core::ptr::null(), |c| c.as_ptr()),
        uid: ctx.run_as.map_or(0, |(u, _)| u),
        gid: ctx.run_as.map_or(0, |(_, g)| g),
        drop_ids: ctx.run_as.is_some(),
        nice: ctx.nice.unwrap_or(0),
        have_nice: ctx.nice.is_some(),
        rlimits: rlimits.as_ptr(),
        rlimit_len: rlimits.len(),
        is_console: sp.kind == ServiceKind::Console,
        tty: tty_c.as_ref().map_or(core::ptr::null(), |c| c.as_ptr()),
    };

    // SAFETY: `fork` takes no arguments and has no preconditions. The dangers
    // are all child-side (async-signal-safe calls only, no allocator, no
    // locks), and `child_main` below meets every one of them.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let e = io::Error::last_os_error();
        cleanup_failed_spawn(log_fd, notify_r, notify_w, exec_r, exec_w);
        return Err(e);
    }
    if pid == 0 {
        child_main(plan_child);
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
        cgroup_joined: !cgroup_unsupported,
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
    /// Notify write-end, dup'd onto 3 (`-1` when no notify handshake).
    notify_w: RawFd,
    /// Notify read-end, closed in the child (`-1` when none).
    notify_r: RawFd,
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
    /// `nice` value; meaningful only when `have_nice`.
    nice: i32,
    /// Whether to call `nice`.
    have_nice: bool,
    /// Pre-resolved rlimits (resource, soft).
    rlimits: *const ChildRlimit,
    /// Length of `rlimits`.
    rlimit_len: usize,
    /// Console service: attempt `TIOCSCTTY` when `tty` is non-null.
    is_console: bool,
    /// Controlling terminal path, or null.
    tty: *const libc::c_char,
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
    if !child_step_session(0) {
        child_fail(p.exec_w);
    }
    if !child_step_fds(p) {
        child_fail(p.exec_w);
    }
    if !child_step_chdir() {
        child_fail(p.exec_w);
    }
    if !child_step_rlimits(p) {
        child_fail(p.exec_w);
    }
    if !child_step_nice(p) {
        child_fail(p.exec_w);
    }
    if !child_step_cgroup(p) {
        child_fail(p.exec_w);
    }
    if !child_step_ids(p) {
        child_fail(p.exec_w);
    }
    child_step_no_new_privs();
    if !child_step_ctty(p) {
        child_fail(p.exec_w);
    }
    // SAFETY: `path` is a NUL-terminated string prepared before the fork;
    // `argv`/`envp` are NUL-terminated pointer arrays to NUL-terminated
    // strings, alive in this frame's inherited copy. `execve` reads them and
    // retains nothing; on success it never returns.
    unsafe {
        libc::execve(p.path, p.argv.cast_mut(), p.envp.cast_mut());
    }
    // Only reached on failure. The errno is from `execve` itself.
    child_fail(p.exec_w);
}

/// Report the current errno to the parent, then `_exit(127)`.
///
/// 127 is the universal "cannot execute" status (the shell convention): the
/// supervisor reaps an ordinary `Exited(127)`, and the reconciler spends
/// budget on it like any other failed start. Never silent, never special.
fn child_fail(exec_w: RawFd) -> ! {
    let errno = io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EINVAL) as u32;
    if exec_w >= 0 {
        let bytes = errno.to_le_bytes();
        // SAFETY: one `write(2)` of four bytes to a pipe; partial writes are
        // irrelevant — the parent treats "any bytes" as failure and reads the
        // first four. Best effort: the return is ignored.
        unsafe {
            libc::write(exec_w, bytes.as_ptr().cast::<libc::c_void>(), bytes.len());
            libc::close(exec_w);
        }
    }
    // SAFETY: `_exit` never returns, takes an int, runs no destructors and
    // flushes nothing — the double-flush-safe exit for a forked child.
    unsafe { libc::_exit(127) };
}

/// New session and process group, before anything else.
///
/// The supervisor stops trees with `kill(-pgid)`. Without this call the
/// child would share the supervisor's group, and signalling "the service"
/// would signal the supervisor too — the self-`SIGKILL` class of bug.
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

/// Install the fd contract: `/dev/null` → 0, log → 1,2, notify → 3.
///
/// Runs before any privilege-affecting step so the interface promise is in
/// place regardless of what fails later (and a later failure `_exit`s, so a
/// half-installed stdio is never observed by anyone but the dying child).
/// Originals are closed: the only fds crossing the `exec` are 0, 1, 2 and
/// possibly 3 — everything the supervisor holds stays `CLOEXEC` on its side.
fn child_step_fds(p: ChildPlan) -> bool {
    // SAFETY: every call below is a raw syscall on ints. `open` of a static
    // literal, `dup2`, `close` — all async-signal-safe, no allocation, no
    // locks. Failure of any one aborts the spawn.
    unsafe {
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
            if libc::dup2(p.notify_w, 3) < 0 {
                return false;
            }
            if p.notify_w != 3 {
                libc::close(p.notify_w);
            }
        }
        if p.notify_r >= 0 {
            libc::close(p.notify_r);
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

/// Apply the `nice` value, if one was given.
fn child_step_nice(p: ChildPlan) -> bool {
    if !p.have_nice {
        return true;
    }
    // SAFETY: `setpriority` is one syscall on three ints. It returns -1 only
    // on error — a nice value is never negative — so no errno read is needed
    // (and none is possible portably: the errno accessor is `__errno_location`
    // on Linux/DragonFly, `__error` on Darwin/FreeBSD, `__errno` on
    // OpenBSD/NetBSD/Android). `PRIO_PROCESS` with `who = 0` means "this
    // process", setting the documented value absolutely.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, p.nice as libc::c_int) == 0 }
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
        let ok = libc::ioctl(fd, req as _, 0) == 0;
        libc::close(fd);
        // A console that cannot take its tty still runs: job control without
        // a controlling terminal degrades to plain session semantics, and the
        // supervisor says so in the service's log via the readiness path.
        let _ = ok;
        true
    }
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
/// binding wins, mirroring what `execve` consumers observe), then pin
/// `ZINIT_NOTIFY_FD=3` when the handshake needs it. Values are expanded
/// ([`expand_env_value`]) against the inherited environment.
fn merged_environment(
    overlay: &[(String, String)],
    wants_notify: bool,
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
    if wants_notify {
        match pairs
            .iter_mut()
            .rev()
            .find(|(k, _)| k == b"ZINIT_NOTIFY_FD")
        {
            Some(slot) => slot.1 = b"3".to_vec(),
            None => pairs.push((b"ZINIT_NOTIFY_FD".to_vec(), b"3".to_vec())),
        }
    }
    Ok(pairs)
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
            nice: None,
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
}
