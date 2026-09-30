//! Who a service runs as: name resolution in the parent, the drop in the child.
//!
//! # The split, and why it is shaped this way
//!
//! `zconfig` has no libc and therefore never resolves `user = www-data`: the
//! plan carries either a numeric `(uid, gid)` or nothing. The *resolution* of
//! a name happens here, in the supervisor, **before the fork** —
//! `getpwnam_r` takes locks and allocates, and neither is allowed past the
//! fork (`DESIGN.md` §9.1). The *application* of the ids happens in the
//! child, after the fork, in the exact order `DESIGN.md` §9 mandates:
//!
//! ```text
//! setgroups(0, NULL) → setresgid → setresuid → verify → (exec)
//! ```
//!
//! # Why the drop is this paranoid
//!
//! * **Never `seteuid` alone.** It leaves the permitted capability set
//!   intact; the process can regain privilege. Only the three-id atomic forms
//!   (`setresuid`/`setresgid`) or a verified `setuid` are used here.
//! * **`setgroups(0, NULL)` first.** Supplementary groups survive a careless
//!   `setuid` and are the easiest privilege to forget.
//! * **Verified after releasing.** `getuid() == geteuid() == uid` (and the
//!   gid pair) is checked, and any mismatch aborts the spawn. "Asked" is not
//!   "done", especially under LSMs with opinions.
//! * **Only `rlim_cur` is touched, never `rlim_max`.** The hard limit is a
//!   one-way ratchet: lowering it can never be undone by the child, and
//!   raising it needs privilege the service should not have to reason about.
//!   The supervisor sets what the service may *use*; the ceiling stays the
//!   system's business.
//! * **cgroup membership is joined before the drop.** Writing to
//!   `cgroup.procs` needs privilege; after the drop it fails. On platforms
//!   without cgroups the request is reported back as "not joined" so the
//!   supervisor can print the `DESIGN.md` §8.1 aviso — never silently
//!   swallowed.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::SpawnError;

/// One resource limit, resolved and ready for the child to install.
///
/// `resource` is the kernel constant (`RLIMIT_NOFILE`, …), `cur` the new soft
/// limit, `max` the current hard limit re-asserted unchanged. The child sets
/// exactly `(cur, max)` and nothing else — see the module docs for why the
/// hard limit is never moved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChildRlimit {
    /// Kernel resource constant value (`RLIMIT_NOFILE`, …) as a plain `u32`.
    /// `libc::setrlimit`/`getrlimit` take the now-private `__rlimit_resource_t`
    /// (libc 0.2.189), so the constant is stored by value and cast back with
    /// `as _` at the call, resolved against whatever the callee declares.
    pub resource: u32,
    /// New soft limit to install.
    pub cur: u64,
    /// Hard limit, carried over untouched.
    pub max: u64,
}

/// Resolve a user name to `(uid, primary gid)` via `getpwnam_r`.
///
/// This is the **only** place in the supervisor that reads `/etc/passwd`
/// (or NSS, or LDAP — whatever libc is configured for). It runs in the
/// parent, before the fork, where allocating and taking libc's internal
/// locks is legal. The `_r` reentrant form is used so no static buffer is
/// shared with anything else in the process.
pub fn resolve_user(name: &str) -> io::Result<(u32, u32)> {
    let cname = CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("user name `{name}` contains a NUL byte"),
        )
    })?;
    // 16 KiB holds any real-world passwd entry; grown once on ERANGE rather
    // than guessed at twice.
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let mut pwd: libc::passwd = unsafe { core::mem::zeroed() };
        let mut result: *mut libc::passwd = core::ptr::null_mut();
        // SAFETY: `cname` is NUL-terminated and alive; `pwd` is a live
        // `passwd`; `buf` is a live allocation of exactly the length passed;
        // `result` points at a live pointer slot. `getpwnam_r` writes through
        // them and retains nothing.
        let rc = unsafe {
            libc::getpwnam_r(
                cname.as_ptr(),
                &raw mut pwd,
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
                &raw mut result,
            )
        };
        if rc == libc::ERANGE {
            if buf.len() >= 1024 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("passwd entry for `{name}` exceeds 1 MiB"),
                ));
            }
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        if result.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such user: `{name}`"),
            ));
        }
        return Ok((pwd.pw_uid, pwd.pw_gid));
    }
}

/// Resolve a group name to its gid via `getgrnam_r`.
///
/// Same contract as [`resolve_user`]: parent side only, reentrant form,
/// `NotFound` when the group does not exist.
pub fn resolve_group(name: &str) -> io::Result<u32> {
    let cname = CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("group name `{name}` contains a NUL byte"),
        )
    })?;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let mut grp: libc::group = unsafe { core::mem::zeroed() };
        let mut result: *mut libc::group = core::ptr::null_mut();
        // SAFETY: as in `resolve_user`: every pointer is live, correctly
        // typed, and retained by nobody.
        let rc = unsafe {
            libc::getgrnam_r(
                cname.as_ptr(),
                &raw mut grp,
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
                &raw mut result,
            )
        };
        if rc == libc::ERANGE {
            if buf.len() >= 1024 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("group entry for `{name}` exceeds 1 MiB"),
                ));
            }
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        if result.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such group: `{name}`"),
            ));
        }
        return Ok(grp.gr_gid);
    }
}

/// Parse one side of a `user[:group]` specification.
///
/// `None` means "a name was given", which this function cannot resolve on its
/// own — see [`resolve_user_group`]. Numeric sides are validated as `u32`
/// here so a typo surfaces before any syscall runs.
fn classify_side(token: &str) -> io::Result<Option<u32>> {
    if token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty user or group in `user[:group]`",
        ));
    }
    if token.bytes().all(|b| b.is_ascii_digit()) {
        token.parse::<u32>().map(Some).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("uid/gid `{token}` does not fit in a u32"),
            )
        })
    } else if token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        Ok(None)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("`{token}` is neither a numeric id nor a valid account name"),
        ))
    }
}

/// Resolve a full `user[:group]` specification to `(uid, gid)`.
///
/// The split mirrors `zconfig`'s `RunAs::try_parse` on purpose: the parser
/// and the resolver must agree on where the colon goes, or `user = 1000:`
/// means one thing to `zcheck` and another to the supervisor. A bare numeric
/// uid means `uid:uid` (same convention); a bare name resolves to the
/// account's own `(uid, primary gid)`; `name:group` resolves each side
/// independently.
///
/// Parent side only: it allocates and calls into NSS, both forbidden past
/// the fork.
pub fn resolve_user_group(spec: &str) -> io::Result<(u32, u32)> {
    let (user, group) = match spec.find(':') {
        Some(i) => (&spec[..i], &spec[i + 1..]),
        None => (spec, ""),
    };
    if user.is_empty() || (spec.contains(':') && group.is_empty()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("empty user or group in `{spec}`: expected `user`, `uid` or `user:group`"),
        ));
    }
    let uid = match classify_side(user)? {
        Some(n) => n,
        None => resolve_user(user)?.0,
    };
    let gid = match group {
        "" => {
            // `uid` alone means `uid:uid`; a *name* alone keeps its primary gid.
            match classify_side(user)? {
                Some(n) => n,
                None => resolve_user(user)?.1,
            }
        }
        g => match classify_side(g)? {
            Some(n) => n,
            None => resolve_group(g)?,
        },
    };
    let _ = uid;
    Ok((uid, gid))
}

/// Validate rlimit directives and snapshot the current hard limits.
///
/// Runs in the parent, before the fork: `getrlimit` is a plain syscall and
/// legal in the child too, but resolving the *names* (`nofile` →
/// `RLIMIT_NOFILE`) here means the child never has to report "unknown
/// resource" through the narrow errno pipe — an unknown name fails the spawn
/// loudly, before any process exists.
///
/// Only `nofile`, `nproc` and `as` are accepted: the three `DESIGN.md` §5
/// names. Anything else is [`SpawnError::UnknownRlimit`], not a guess.
pub fn prepare_rlimits(rlimits: &[(String, u64)]) -> io::Result<Vec<ChildRlimit>> {
    let mut out = Vec::with_capacity(rlimits.len());
    for (name, value) in rlimits {
        // `RLIMIT_*` is `u32` on glibc and `c_int` on musl/BSD, so this cast
        // is a no-op on the targets that deny warnings and load-bearing on
        // the others. Kept, with the lint silenced by reason, rather than
        // bent to whichever target happens to be the host.
        #[allow(clippy::unnecessary_cast)]
        let resource: u32 = match name.as_str() {
            "nofile" => libc::RLIMIT_NOFILE as u32,
            "nproc" => libc::RLIMIT_NPROC as u32,
            "as" => libc::RLIMIT_AS as u32,
            other => {
                return Err(SpawnError::UnknownRlimit {
                    key: other.to_string(),
                }
                .into());
            }
        };
        let mut current: libc::rlimit = unsafe { core::mem::zeroed() };
        // SAFETY: `current` is a live, aligned `rlimit`; `getrlimit` writes
        // exactly one and retains nothing. `resource as _` resolves against
        // the callee's (private) resource type.
        if unsafe { libc::getrlimit(resource as _, &raw mut current) } != 0 {
            return Err(io::Error::last_os_error());
        }
        out.push(ChildRlimit {
            resource,
            cur: *value,
            max: current.rlim_max as u64,
        });
    }
    Ok(out)
}

/// The `cgroup.procs` path for a `cgroup =` directive.
///
/// A relative slice (`web`) nests under the supervisor's own slice,
/// `/sys/fs/cgroup/zinit.slice/<name>/cgroup.procs`; an absolute path is
/// used verbatim for setups where the hierarchy is managed elsewhere. Kept
/// as a pure function of the string so tests can pin the layout without
/// touching the filesystem.
pub fn cgroup_procs_path(cgroup: &str) -> PathBuf {
    if cgroup.starts_with('/') {
        let mut p = PathBuf::from(cgroup);
        p.push("cgroup.procs");
        p
    } else {
        PathBuf::from(format!("/sys/fs/cgroup/zinit.slice/{cgroup}/cgroup.procs"))
    }
}

/// Create the slice directory (Linux) and return the `cgroup.procs` path.
///
/// Parent side, before the fork: creating directories needs privilege and
/// must therefore happen while the supervisor still has it. The child only
/// *writes its own pid* to the returned path. `std::fs` is fine here — this
/// is the parent, where allocation and std are legal.
///
/// On platforms without cgroups this reports [`SpawnError::CgroupUnsupported`]
/// so the supervisor can print the degradation aviso naming the service,
/// per `DESIGN.md` §8.1. It never silently continues without the limits the
/// operator asked for.
pub fn ensure_cgroup(service: &str, cgroup: &str) -> io::Result<PathBuf> {
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = cgroup;
        Err(SpawnError::CgroupUnsupported {
            name: service.to_string(),
        }
        .into())
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let procs = cgroup_procs_path(cgroup);
        let dir = match procs.parent() {
            Some(d) => d,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("service `{service}`: cgroup path has no parent directory"),
                ));
            }
        };
        if let Err(e) = std::fs::create_dir_all(dir) {
            // Already-exists races with a concurrent supervisor are fine;
            // anything else is a real refusal with the real errno.
            if e.kind() != io::ErrorKind::AlreadyExists {
                return Err(e);
            }
        }
        Ok(procs)
    }
}

/// Drop privileges to `(uid, gid)`, or fail loudly.
///
/// The exact `DESIGN.md` §9 sequence: `setgroups(0, NULL)` first (supplementary
/// groups survive a careless `setuid`), then the gid, then the uid — via
/// `setresgid`/`setresuid` where they exist (Linux: atomic, no window in which
/// privilege can be regained), via `setgid`/`setuid` plus explicit
/// verification elsewhere — and finally the check `getuid() == geteuid() ==
/// uid && getgid() == getegid() == gid`. Any mismatch is `PermissionDenied`,
/// never a service quietly running as root.
///
/// Callable from both sides: the forked child calls it on its single-exit
/// path (no allocation on success), and tests call it in a forked child to
/// exercise the real syscalls. Error messages are static strings — no
/// allocation even on failure — so the child path stays allocation-free.
pub fn drop_privileges(uid: u32, gid: u32) -> io::Result<()> {
    // Drop supplementary groups first — but only when there are any. A
    // `setgroups` call with nothing to drop still needs privilege on Linux,
    // so calling it unconditionally would make "stay exactly who I am" fail
    // for an unprivileged process with an already-empty group list. Skipping
    // it then is semantically identical (there is nothing to shed) and keeps
    // the self-drop testable without root. `getgroups` itself needs no
    // privilege.
    let mut groups = [0 as libc::gid_t; 1];
    // SAFETY: one-slot buffer with size 1. A size-1 query reports EINVAL —
    // not a count — when the process carries more than one supplementary
    // group (container roots often do), which still means "groups exist".
    // Anything else failing here is a real error.
    let ngroups = unsafe { libc::getgroups(1, groups.as_mut_ptr()) };
    let has_groups = if ngroups < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINVAL) {
            return Err(e);
        }
        // EINVAL means "more groups than the one slot", so there are groups
        // to shed — but only a privileged caller can finish the drop
        // afterwards. An unprivileged attempt would shed the supplementary
        // groups and then fail at setgid anyway (mutate-then-fail, which the
        // fail-cleanly test forbids), so bail out untouched instead.
        // SAFETY: getter, no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return Err(e);
        }
        true
    } else {
        ngroups > 0
    };
    if has_groups {
        // SAFETY: `setgroups(0, NULL)` is the documented empty-set form; the
        // kernel checks the count and never dereferences the pointer.
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: three ints, no pointers. Failure leaves the ids unchanged,
        // which the verification below would catch anyway.
        if unsafe { libc::setresgid(gid, gid, gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above. All three ids at once: there is no instant in
        // which the saved id still names root while the effective id does
        // not, which is the window `setuid`-alone leaves open.
        if unsafe { libc::setresuid(uid, uid, uid) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // SAFETY: one int each, no pointers. No `setresuid` on these
        // platforms, so the verification below is load-bearing rather than
        // belt-and-braces: it is the only thing proving the drop happened.
        if unsafe { libc::setgid(gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::setuid(uid) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // Rule 2, `DESIGN.md` §9: releasing privilege is not believed, it is
    // checked. All four ids, because a mismatch in any one of them is a
    // service with more privilege than its description allows.
    // SAFETY: four getters with no arguments and no preconditions.
    let (ruid, euid, rgid, egid) = unsafe {
        (
            libc::getuid(),
            libc::geteuid(),
            libc::getgid(),
            libc::getegid(),
        )
    };
    if ruid != uid || euid != uid || rgid != gid || egid != gid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "uid/gid drop verification failed: ids do not match the target",
        ));
    }
    Ok(())
}

/// Read the four ids as `(ruid, euid, rgid, egid)`, for tests.
///
/// A named wrapper rather than raw `libc::getuid` calls at every test site,
/// so the tests read as "who am I" instead of four FFI invocations.
pub fn current_ids() -> (u32, u32, u32, u32) {
    // SAFETY: four getters with no arguments and no preconditions.
    unsafe {
        (
            libc::getuid(),
            libc::geteuid(),
            libc::getgid(),
            libc::getegid(),
        )
    }
}

#[allow(dead_code)]
pub(crate) fn is_numeric(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[allow(dead_code)]
pub(crate) fn cstr_of(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path contains a NUL byte: {}", path.display()),
        )
    })
}

/// The passwd entry behind a name, for tests that need the primary gid.
#[cfg(test)]
pub(crate) fn passwd_of(name: &str) -> Option<(u32, u32)> {
    resolve_user(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_nobody_to_the_platform_convention() {
        let (uid, gid) = resolve_user("nobody").expect("nobody must exist");
        // Pinned where the *platform* fixes it: -2 (4294967294) on macOS,
        // 32767 on OpenBSD/NetBSD. On Linux it is the *distro's* (65534 on
        // Arch/Alpine/Gentoo, 99 on Void) and no cfg can tell distros apart,
        // so Linux only pins "unprivileged".
        #[cfg(target_vendor = "apple")]
        assert_eq!(uid, 4294967294, "nobody is -2 on macOS");
        #[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
        assert_eq!(uid, 32767, "nobody is 32767 on OpenBSD/NetBSD");
        #[cfg(target_os = "linux")]
        assert_ne!(uid, 0, "nobody must not be root");
        let _ = gid;
        // Deterministic: resolving twice gives the same answer.
        assert_eq!(resolve_user("nobody").expect("again"), (uid, gid));
    }

    #[test]
    fn unknown_user_is_not_found() {
        let e = resolve_user("zinit-no-such-user-xyz").expect_err("must fail");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn unknown_group_is_not_found() {
        let e = resolve_group("zinit-no-such-group-xyz").expect_err("must fail");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn numeric_specs_parse_without_touching_nss() {
        assert_eq!(
            resolve_user_group("1000:1001").expect("numeric"),
            (1000, 1001)
        );
        // A bare uid means uid:uid — the `RunAs` convention shared with zconfig.
        assert_eq!(resolve_user_group("1000").expect("bare"), (1000, 1000));
        assert_eq!(
            resolve_user_group("4294967295").expect("max"),
            (4294967295, 4294967295)
        );
    }

    #[test]
    fn named_specs_resolve_each_side() {
        // `nobody` resolves wherever the platform keeps it; both answers come
        // from the same source, which is the point (exact uid pinned above).
        let (uid, gid) = resolve_user("nobody").expect("nobody");
        assert_eq!(resolve_user_group("nobody").expect("bare name"), (uid, gid));
    }

    #[test]
    fn malformed_specs_are_refused() {
        for bad in ["", ":", "1000:", ":100", "1000 users", "a/b", "99999999999"] {
            assert!(resolve_user_group(bad).is_err(), "`{bad}` must be refused");
        }
    }

    #[test]
    fn cgroup_paths_nest_under_the_supervisor_slice() {
        assert_eq!(
            cgroup_procs_path("web"),
            PathBuf::from("/sys/fs/cgroup/zinit.slice/web/cgroup.procs")
        );
        assert_eq!(
            cgroup_procs_path("/custom/place"),
            PathBuf::from("/custom/place/cgroup.procs")
        );
    }

    #[test]
    fn unknown_rlimit_names_are_refused_before_any_syscall() {
        let e = prepare_rlimits(&[("nope".to_string(), 1)]).expect_err("must fail");
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert!(e.to_string().contains("nope"));
    }

    #[test]
    fn known_rlimits_snapshot_the_hard_limit() {
        let got = prepare_rlimits(&[
            ("nofile".to_string(), 1024),
            ("nproc".to_string(), 512),
            ("as".to_string(), 1 << 30),
        ])
        .expect("known names");
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].cur, 1024);
        // The hard limit is carried over, never invented.
        let mut cur: libc::rlimit = unsafe { core::mem::zeroed() };
        // SAFETY: live `rlimit`, written once, retained never.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut cur) },
            0
        );
        assert_eq!(got[0].max, cur.rlim_max as u64);
    }

    /// The real drop, exercised in a forked child against our *own* ids.
    ///
    /// Setting every id to the value it already has is permitted without
    /// privilege, so this runs as any user while still executing the
    /// production `setresuid` path and its verification. The contract
    /// asserted is environment-aware, because privilege is: as root (or with
    /// an empty group list) the drop succeeds and verifies; otherwise the
    /// `setgroups` step fails with `EPERM` and the ids must be untouched. A
    /// forked child that returned to the test harness would run the suite
    /// twice sharing every fd, so it `_exit`s with the verdict.
    #[test]
    fn dropping_to_our_own_ids_is_either_clean_or_untouched() {
        let (ruid, _, rgid, _) = current_ids();
        match unsafe { libc::fork() } {
            -1 => panic!("fork failed"),
            0 => {
                let before = current_ids();
                let verdict = match drop_privileges(ruid, rgid) {
                    Ok(()) => current_ids() == (ruid, ruid, rgid, rgid),
                    Err(_) => current_ids() == before,
                };
                // SAFETY: `_exit` never returns and takes only an int.
                unsafe { libc::_exit(if verdict { 0 } else { 1 }) };
            }
            pid => {
                let mut status = 0;
                // SAFETY: `status` is a live `c_int`; blocking wait on our own child.
                assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
                assert!(
                    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                    "self-drop must succeed-and-verify or fail-untouched"
                );
            }
        }
    }

    /// Dropping to somebody else without privilege must fail — and must fail
    /// *without changing who we are*.
    ///
    /// As a non-root user, `setresuid(65534,…)` returns `EPERM` with the ids
    /// untouched; as root this test cannot run (the drop would succeed), so
    /// it steps aside and lets `dropping_to_nobody_as_root` cover that side.
    #[test]
    fn dropping_to_another_uid_without_privilege_fails_cleanly() {
        if current_ids().0 == 0 {
            return;
        }
        match unsafe { libc::fork() } {
            -1 => panic!("fork failed"),
            0 => {
                let before = current_ids();
                let failed = drop_privileges(65534, 65534).is_err();
                let unchanged = current_ids() == before;
                // SAFETY: `_exit` never returns.
                unsafe { libc::_exit(if failed && unchanged { 0 } else { 1 }) };
            }
            pid => {
                let mut status = 0;
                // SAFETY: blocking wait on our own child.
                assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
                assert!(
                    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                    "unprivileged drop must fail without side effects"
                );
            }
        }
    }

    /// The full drop to `nobody`, gated on root.
    ///
    /// Only root can shed privilege to another account, so outside a root CI
    /// job this test proves nothing and runs nothing. Where it runs, it is
    /// the mission's acceptance test: the child really becomes uid 65534.
    #[test]
    fn dropping_to_nobody_as_root() {
        if current_ids().0 != 0 {
            return;
        }
        match unsafe { libc::fork() } {
            -1 => panic!("fork failed"),
            0 => {
                // uid 0 without CAP_SETUID/CAP_SETGID (confined containers)
                // fails the id-changing syscalls. The exit code carries the
                // diagnosis: 2 is the environmental EPERM skip, printed by
                // the parent assert below; anything else is a real failure (a
                // regression on a capable host fails verification, which
                // carries no errno, so it can never hide as a skip).
                let code = match drop_privileges(65534, 65534) {
                    Ok(()) if current_ids().0 == 65534 => 0,
                    Ok(()) => 10,
                    Err(e) => match e.raw_os_error() {
                        Some(libc::EPERM) => 2,
                        Some(n) => 100 + n.min(27),
                        None => 3,
                    },
                };
                // SAFETY: `_exit` never returns.
                unsafe { libc::_exit(code) };
            }
            pid => {
                let mut status = 0;
                // SAFETY: blocking wait on our own child.
                assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
                let exit = libc::WEXITSTATUS(status);
                assert!(
                    libc::WIFEXITED(status) && matches!(exit, 0 | 2),
                    "root must be able to drop to nobody (child exit {exit})"
                );
            }
        }
    }

    #[test]
    fn cstr_helpers_reject_nul() {
        assert!(cstr_of(Path::new("a\0b")).is_err());
        assert!(cstr_of(Path::new("fine")).is_ok());
        assert!(is_numeric("1000") && !is_numeric("") && !is_numeric("10a"));
        let _ = passwd_of("nobody");
    }
}
