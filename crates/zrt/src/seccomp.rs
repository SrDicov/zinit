//! Zero-dependency seccomp-bpf: a filter the supervisor assembles by hand.
//!
//! # Why hand-rolled
//!
//! There is no `libseccomp` here on purpose: the workspace allows exactly one
//! third-party dependency (`libc`), and a generated binding cannot carry the
//! `// SAFETY:` comment that is this crate's entire review surface. A classic
//! BPF (cBPF) program is a flat array of `{code, jt, jf, k}` — small enough
//! to build explicitly and to test byte-for-byte, which is exactly what the
//! tests below do.
//!
//! # Shape of the filter
//!
//! ```text
//! load arch; if arch != AUDIT_ARCH -> ACTION          (kills x32/compat bypass)
//! load nr
//! if nr == allow[0] -> ALLOW; ... ; if nr == allow[N-1] -> ALLOW
//! return ACTION                                       (fail-closed default)
//! ALLOW: return SECCOMP_RET_ALLOW
//! ```
//!
//! `ACTION` is `SECCOMP_RET_KILL_PROCESS` (`enforce`) or
//! `SECCOMP_RET_ERRNO | EPERM` (`errno`). Jumps are forward offsets computed
//! by construction (entry `i` of `N` skips `N-1-i` to reach `ALLOW`), and
//! `build` refuses lists past [`MAX_ALLOW`] rather than truncating an offset
//! into a security hole.
//!
//! # What the filter does NOT do
//!
//! It never learns: install is one-way, and a filtered process cannot widen
//! its own filter (that would need `seccomp(2)`, which is not allow-listed).
//! `bpf` and `seccomp` themselves are not in the name table at all — not even
//! as user-allowable entries — so a filtered service cannot punch its own
//! hole. `ptrace`, `io_uring`, `perf`, keyring and mount-family calls are
//! absent for the same reason, or because their numbers were never verified
//! (see below).
//!
//! # Number provenance
//!
//! Every number in the x86_64 table was read off the running kernel with
//! `ausyscall --dump`, not copied from a header found online. The other
//! architectures have **no table**: an unverified number is a hole, and a
//! hole in a kill-filter is worse than no filter. On those arches every
//! filter request fails loudly at spawn (`Unsupported`), which is the
//! degradation contract, not a silent pass.
//!
//! The always-allowed baseline ([`IMPLICIT`]) is what a dynamically linked
//! C service needs to start, speak, thread and exit: loader and libc
//! bootstrap, sockets, epoll/signalfd/event pipes, clocks, and process
//! birth/death. Everything workload-specific (`sendfile`, `splice`,
//! `ptrace`-adjacent debugging, `wait4`/`kill` of *other* processes, …)
//! stays user-allowable and default-deny.

use std::io;

/// `seccomp_data.nr` offset in the loaded struct (linux/seccomp.h).
const SECCOMP_DATA_NR_OFFSET: u32 = 0;
/// `seccomp_data.arch` offset.
const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;

/// BPF instruction classes and modes (linux/filter.h).
const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

/// Return actions (linux/seccomp.h).
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ERRNO: u32 = 0x0005_0000;

/// `prctl` option and filter mode (linux/prctl.h). Defined here rather than
/// taken from `libc` so a missing binding is impossible by construction.
const PR_SET_SECCOMP: i32 = 22;
const SECCOMP_MODE_FILTER: u64 = 1;

/// Audit architecture tokens (linux/audit.h): machine + 64-bit + little-endian.
const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
const AUDIT_ARCH_AARCH64: u32 = 0xC000_00B7;
const AUDIT_ARCH_RISCV64: u32 = 0xC000_00F3;

/// Cap on allow-listed numbers. Jump offsets are `u8`; past 200 entries the
/// program is not a filter but a second kernel, and `build` says so with
/// `None` instead of wrapping an offset.
pub const MAX_ALLOW: usize = 200;

/// One cBPF instruction. Layout-identical to `struct sock_filter`, defined
/// here so this module compiles on targets whose `libc` never heard of it;
/// only the Linux `install` path reinterprets it (same layout, no conversion).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

/// `BPF_STMT(code, k)`: no jump.
const fn stmt(code: u16, k: u32) -> Insn {
    Insn {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

/// `BPF_JUMP(code, k, jt, jf)`: skip `jt` on true, `jf` on false.
const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Insn {
    Insn { code, jt, jf, k }
}

/// The audit token for this build's architecture, or `None` where no table
/// was ever verified (see the module docs: unverified means unsupported).
pub const fn audit_arch() -> Option<u32> {
    match () {
        #[cfg(target_arch = "x86_64")]
        () => Some(AUDIT_ARCH_X86_64),
        #[cfg(target_arch = "aarch64")]
        () => Some(AUDIT_ARCH_AARCH64),
        #[cfg(target_arch = "riscv64")]
        () => Some(AUDIT_ARCH_RISCV64),
        #[cfg(not(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "riscv64"
        )))]
        () => None,
    }
}

/// Resolve a syscall name to its number on this build's architecture.
///
/// `None` on unverified architectures and for unknown names — both are
/// spawn-time refusals upstream, never silent passes. Aliases (`pread64`
/// for `pread`) exist only where both spellings name one verified number.
#[cfg(target_arch = "x86_64")]
pub fn syscall_nr(name: &str) -> Option<u32> {
    // Verified against `ausyscall --dump` on the running kernel. The table is
    // a linear search on purpose: ~150 entries resolved once per spawn, where
    // a sorted table would trade reviewability for nanoseconds.
    Some(match name {
        "read" => 0,
        "write" => 1,
        "open" => 2,
        "close" => 3,
        "stat" => 4,
        "fstat" => 5,
        "lstat" => 6,
        "poll" => 7,
        "lseek" => 8,
        "mmap" => 9,
        "mprotect" => 10,
        "munmap" => 11,
        "brk" => 12,
        "rt_sigaction" => 13,
        "rt_sigprocmask" => 14,
        "rt_sigreturn" => 15,
        "ioctl" => 16,
        "pread" | "pread64" => 17,
        "pwrite" | "pwrite64" => 18,
        "readv" => 19,
        "writev" => 20,
        "access" => 21,
        "pipe" => 22,
        "select" => 23,
        "sched_yield" => 24,
        "mremap" => 25,
        "msync" => 26,
        "mincore" => 27,
        "madvise" => 28,
        "dup" => 32,
        "dup2" => 33,
        "pause" => 34,
        "nanosleep" => 35,
        "getpid" => 39,
        "sendfile" => 40,
        "socket" => 41,
        "connect" => 42,
        "accept" => 43,
        "sendto" => 44,
        "recvfrom" => 45,
        "sendmsg" => 46,
        "recvmsg" => 47,
        "shutdown" => 48,
        "bind" => 49,
        "listen" => 50,
        "getsockname" => 51,
        "getpeername" => 52,
        "socketpair" => 53,
        "setsockopt" => 54,
        "getsockopt" => 55,
        "clone" => 56,
        "fork" => 57,
        "execve" => 59,
        "exit" => 60,
        "wait4" => 61,
        "kill" => 62,
        "uname" => 63,
        "fcntl" => 72,
        "flock" => 73,
        "fsync" => 74,
        "ftruncate" => 77,
        "fadvise64" => 221,
        "getdents" => 78,
        "getdents64" => 217,
        "getcwd" => 79,
        "chdir" => 80,
        "rename" => 82,
        "mkdir" => 83,
        "rmdir" => 84,
        "link" => 86,
        "unlink" => 87,
        "symlink" => 88,
        "readlink" => 89,
        "chmod" => 90,
        "fchmod" => 91,
        "chown" => 92,
        "fchown" => 93,
        "umask" => 95,
        "gettimeofday" => 96,
        "getrlimit" => 97,
        "getrusage" => 98,
        "sysinfo" => 99,
        "times" => 100,
        "getuid" => 102,
        "getgid" => 104,
        "setuid" => 105,
        "setgid" => 106,
        "geteuid" => 107,
        "getegid" => 108,
        "setpgid" => 109,
        "getppid" => 110,
        "setsid" => 112,
        "getgroups" => 115,
        "setgroups" => 116,
        "setresuid" => 117,
        "setresgid" => 119,
        "getpgid" => 121,
        "capget" => 125,
        "capset" => 126,
        "rt_sigpending" => 127,
        "rt_sigtimedwait" => 128,
        "sigaltstack" => 131,
        "utime" => 132,
        "mknod" => 133,
        "chroot" => 161,
        "sync" => 162,
        "pivot_root" => 155,
        "setrlimit" => 160,
        "unshare" => 272,
        "sched_setaffinity" => 203,
        "sched_getaffinity" => 204,
        "sched_setparam" => 142,
        "sched_getparam" => 143,
        "sched_setscheduler" => 144,
        "sched_getscheduler" => 145,
        "setpriority" => 141,
        "getpriority" => 140,
        "set_tid_address" => 218,
        "restart_syscall" => 219,
        "futex" => 202,
        "get_robust_list" => 274,
        "set_robust_list" => 273,
        "clock_gettime" => 228,
        "clock_getres" => 229,
        "clock_nanosleep" => 230,
        "exit_group" => 231,
        "epoll_wait" => 232,
        "epoll_ctl" => 233,
        "tgkill" => 234,
        "utimes" => 235,
        "openat" => 257,
        "mkdirat" => 258,
        "mknodat" => 259,
        "fchownat" => 260,
        "newfstatat" => 262,
        "unlinkat" => 263,
        "renameat" => 264,
        "linkat" => 265,
        "symlinkat" => 266,
        "readlinkat" => 267,
        "fchmodat" => 268,
        "faccessat" => 269,
        "ppoll" => 271,
        "pselect6" => 270,
        "utimensat" => 280,
        "epoll_pwait" => 281,
        "signalfd" => 282,
        "timerfd_create" => 283,
        "eventfd" => 284,
        "fallocate" => 285,
        "timerfd_settime" => 286,
        "accept4" => 288,
        "signalfd4" => 289,
        "eventfd2" => 290,
        "epoll_create1" => 291,
        "dup3" => 292,
        "pipe2" => 293,
        "preadv" => 295,
        "pwritev" => 296,
        "preadv2" => 327,
        "pwritev2" => 328,
        "prlimit64" => 302,
        "syncfs" => 306,
        "sendmmsg" => 307,
        "recvmmsg" => 299,
        "getrandom" => 318,
        "memfd_create" => 319,
        "renameat2" => 316,
        "statx" => 332,
        "rseq" => 334,
        "membarrier" => 324,
        "copy_file_range" => 326,
        "openat2" => 437,
        "clone3" => 435,
        "faccessat2" => 439,
        "arch_prctl" => 158,
        "sched_getcpu" | "getcpu" => 309,
        "statfs" => 137,
        "fstatfs" => 138,
        "getxattr" => 191,
        "lgetxattr" => 192,
        "set_mempolicy" => 238,
        "get_mempolicy" => 239,
        "mbind" => 237,
        "migrate_pages" => 256,
        "move_pages" => 279,
        "splice" => 275,
        "tee" => 276,
        "vmsplice" => 278,
        _ => return None,
    })
}

/// No verified table on this architecture: every lookup fails, and every
/// filter request with it. Loud refusal beats an unverified number.
#[cfg(not(target_arch = "x86_64"))]
pub fn syscall_nr(_name: &str) -> Option<u32> {
    None
}

/// Always-allowed names: what a dynamically linked service needs to start,
/// speak, thread and exit. Everything here resolved on a verified table or
/// the filter it feeds cannot be built — `build` checks that, so a typo in
/// this list fails the spawn instead of shipping a filter with a hole.
pub const IMPLICIT: &[&str] = &[
    "read",
    "write",
    "open",
    "openat",
    "openat2",
    "close",
    "stat",
    "fstat",
    "lstat",
    "newfstatat",
    "statx",
    "statfs",
    "fstatfs",
    "access",
    "faccessat",
    "faccessat2",
    "readlink",
    "readlinkat",
    "getdents",
    "getdents64",
    "getcwd",
    "chdir",
    "umask",
    "chmod",
    "fchmod",
    "chown",
    "fchown",
    "mkdir",
    "mkdirat",
    "rmdir",
    "unlink",
    "unlinkat",
    "symlink",
    "symlinkat",
    "link",
    "linkat",
    "rename",
    "renameat",
    "utime",
    "utimes",
    "utimensat",
    "lseek",
    "pread",
    "pwrite",
    "readv",
    "writev",
    "preadv",
    "pwritev",
    "ftruncate",
    "fallocate",
    "fadvise64",
    "syncfs",
    "mmap",
    "mprotect",
    "munmap",
    "mremap",
    "msync",
    "mincore",
    "madvise",
    "brk",
    "arch_prctl",
    "set_tid_address",
    "set_robust_list",
    "get_robust_list",
    "rseq",
    "membarrier",
    "futex",
    "getpid",
    "getppid",
    "getuid",
    "getgid",
    "geteuid",
    "getegid",
    "getgroups",
    "getpgid",
    "uname",
    "clock_gettime",
    "clock_getres",
    "clock_nanosleep",
    "gettimeofday",
    "nanosleep",
    "pause",
    "restart_syscall",
    "rt_sigaction",
    "rt_sigprocmask",
    "rt_sigreturn",
    "rt_sigpending",
    "rt_sigtimedwait",
    "sigaltstack",
    "tgkill",
    "clone",
    "clone3",
    "execve",
    "exit",
    "exit_group",
    "socket",
    "connect",
    "bind",
    "listen",
    "accept",
    "accept4",
    "getsockname",
    "getpeername",
    "socketpair",
    "setsockopt",
    "getsockopt",
    "sendto",
    "recvfrom",
    "sendmsg",
    "recvmsg",
    "shutdown",
    "sendmmsg",
    "recvmmsg",
    "poll",
    "ppoll",
    "select",
    "pselect6",
    "epoll_create1",
    "epoll_ctl",
    "epoll_wait",
    "epoll_pwait",
    "eventfd",
    "eventfd2",
    "signalfd",
    "signalfd4",
    "timerfd_create",
    "timerfd_settime",
    "pipe",
    "pipe2",
    "dup",
    "dup2",
    "dup3",
    "fcntl",
    "ioctl",
    "prlimit64",
    "getrlimit",
    "getrusage",
    "times",
    "sysinfo",
    "sched_yield",
    "sched_getaffinity",
    "sched_getparam",
    "sched_getscheduler",
    "getcpu",
    "getrandom",
];

/// What a violation does: kill the process, or fail the call with `EPERM`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OnViolation {
    /// `SECCOMP_RET_KILL_PROCESS`. Fail-closed: the invader dies loudly.
    Kill,
    /// `SECCOMP_RET_ERRNO | EPERM`. The call fails; the service limps on.
    Errno,
}

/// Assemble the filter for `extra` (already numbers) over the [`IMPLICIT`]
/// baseline.
///
/// Returns `None` when the program cannot be built honestly: an
/// unresolvable implicit name (a typo in this file, or an unverified
/// architecture), more than [`MAX_ALLOW`] numbers (a `u8` jump offset cannot
/// reach further), or no audit token for this build. `None` is a spawn
/// refusal upstream, never a degraded filter.
pub fn build(extra: &[u32], on_violation: OnViolation) -> Option<Vec<Insn>> {
    let arch = audit_arch()?;
    let mut allow: Vec<u32> = Vec::with_capacity(IMPLICIT.len() + extra.len());
    for name in IMPLICIT {
        allow.push(syscall_nr(name)?);
    }
    allow.extend_from_slice(extra);
    allow.sort_unstable();
    allow.dedup();
    if allow.len() > MAX_ALLOW {
        return None;
    }
    let action = match on_violation {
        OnViolation::Kill => RET_KILL_PROCESS,
        OnViolation::Errno => RET_ERRNO | libc::EPERM as u32,
    };
    let n = allow.len();
    let mut prog = Vec::with_capacity(4 + n + 1);
    // Validate the architecture first: without this, an x32-ABI process
    // (same instruction set, compacted numbers) walks straight through a
    // filter written for the native table.
    prog.push(stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_ARCH_OFFSET));
    prog.push(jump(BPF_JMP | BPF_JEQ | BPF_K, arch, 1, 0));
    prog.push(stmt(BPF_RET | BPF_K, action));
    prog.push(stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_NR_OFFSET));
    for (i, nr) in allow.iter().enumerate() {
        // On match, skip forward to ALLOW; otherwise fall through. Entry `i`
        // of `n` skips `n - 1 - i`: the default-action return plus the
        // entries after it... precisely: ALLOW sits at index `4 + n + 1`
        // while this entry sits at `4 + i`, so the skip is `n - i`.
        prog.push(jump(BPF_JMP | BPF_JEQ | BPF_K, *nr, (n - i) as u8, 0));
    }
    prog.push(stmt(BPF_RET | BPF_K, action));
    prog.push(stmt(BPF_RET | BPF_K, RET_ALLOW));
    Some(prog)
}

/// Install `filter` on the calling thread. One-way: a filtered thread cannot
/// widen its own filter (that needs `seccomp(2)`, which is not allow-listed),
/// and the filter survives `execve` — the allow-list must cover whatever the
/// new image does, not just the launcher.
///
/// Install this **last**, right before `execve**: everything after it
/// (including a failing `execve`'s own error report) runs under the filter.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn install(filter: &[Insn]) -> io::Result<()> {
    if filter.is_empty() || filter.len() > MAX_ALLOW + 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "seccomp filter has an impossible length",
        ));
    }
    // `sock_fprog`: length plus a pointer the kernel copies in. `u16` holds
    // every program `build` can produce, and the length check above is the
    // belt to that braces for hand-built programs.
    let prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut libc::sock_filter,
    };
    // SAFETY: `prog` describes exactly `filter.len()` live instructions of
    // our own layout-identical type; `prctl` copies the program and retains
    // nothing. The caller runs with no-new-privs (or equivalent privilege),
    // which is what makes an unprivileged filter install legal at all.
    let rc = unsafe {
        libc::prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER as libc::c_ulong,
            &raw const prog as *const libc::sock_fprog,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// No seccomp off Linux: the request fails loudly so the supervisor can
/// refuse the spawn instead of running it unconfined and quiet.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn install(_filter: &[Insn]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "seccomp is Linux-only",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program for two allowed calls, written out by hand: any drift in
    /// the builder shows up here as a byte difference, not a philosophy.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn two_call_program_has_exact_shape() {
        let prog = build(&[60, 231], OnViolation::Kill).expect("builds");
        // 4 header + 2 + IMPLICIT.len() entries + default + allow. Check the
        // skeleton around the interesting bits instead of the whole length.
        assert_eq!(
            prog[0],
            stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_ARCH_OFFSET)
        );
        assert_eq!(prog[2], stmt(BPF_RET | BPF_K, RET_KILL_PROCESS));
        assert_eq!(
            prog[3],
            stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_NR_OFFSET)
        );
        // Last two are always default-action then allow.
        let n = prog.len();
        assert_eq!(prog[n - 2], stmt(BPF_RET | BPF_K, RET_KILL_PROCESS));
        assert_eq!(prog[n - 1], stmt(BPF_RET | BPF_K, RET_ALLOW));
        // Every jump lands on the final ALLOW: entry at index i skips to n-1.
        for (i, insn) in prog.iter().enumerate().take(n - 2).skip(4) {
            assert_eq!(insn.jf, 0, "entry {i} must fall through on mismatch");
            assert_eq!(
                insn.jt as usize,
                n - 1 - (i + 1),
                "entry {i} must skip to ALLOW"
            );
        }
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn errno_action_changes_only_the_default() {
        let kill = build(&[], OnViolation::Kill).expect("builds");
        let errno = build(&[], OnViolation::Errno).expect("builds");
        assert_eq!(kill.len(), errno.len());
        assert_eq!(kill[kill.len() - 1], stmt(BPF_RET | BPF_K, RET_ALLOW));
        assert_eq!(
            errno[errno.len() - 2],
            stmt(BPF_RET | BPF_K, RET_ERRNO | libc::EPERM as u32)
        );
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn critical_numbers_match_the_kernel() {
        // The load-bearing entries: the wrong number here is a hole, not a typo.
        for (name, nr) in [
            ("read", 0),
            ("write", 1),
            ("mmap", 9),
            ("rt_sigreturn", 15),
            ("execve", 59),
            ("exit_group", 231),
            ("openat", 257),
            ("arch_prctl", 158),
            ("set_robust_list", 273),
        ] {
            assert_eq!(syscall_nr(name), Some(nr), "{name} moved?");
        }
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn implicit_names_all_resolve() {
        // A typo in IMPLICIT would otherwise ship a filter with a hole: the
        // builder refuses (`?` above), and this test names the offender.
        for name in IMPLICIT {
            assert!(
                syscall_nr(name).is_some(),
                "implicit entry `{name}` resolves nowhere"
            );
        }
    }

    #[test]
    fn oversized_allow_lists_are_refused_not_truncated() {
        let many: Vec<u32> = (0..300).collect();
        assert!(build(&many, OnViolation::Kill).is_none());
    }

    #[test]
    fn install_rejects_impossible_lengths() {
        assert!(install(&[]).is_err());
    }
}
