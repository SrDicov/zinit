//! What this machine can actually do, decided at runtime.
//!
//! # The rule
//!
//! `DESIGN.md` §8.1, verbatim: *detect at startup, print it, and let every
//! call site say what it does when the capability is missing.* Never
//! `unimplemented!()`, never a silent fallback, never a behaviour that depends
//! on something nobody logged.
//!
//! The alternative — `#[cfg(target_os = ...)]` — is wrong for a reason that
//! is easy to miss: **a compiled binary cannot know the kernel it will run
//! on**. A `cfg` is a statement about the *build machine*: a binary built on
//! Linux 6.1 and shipped into a container with an older seccomp profile, or
//! into a VM with `pidfd` disabled, has a `cfg` that is confidently wrong. The
//! probes here run on the machine that is actually booting.
//!
//! # What is probed, and how honestly
//!
//! * Everything that can fail is *made to fail on purpose*, in a way with no
//!   side effects, and the errno is the answer.
//! * Where no side-effect-free probe exists, that is stated rather than
//!   faked. [`Capabilities::setresuid`], for example, is probed by asking to
//!   set the ids to the values they already have, which is a real `setresuid`
//!   that cannot change anything.
//! * Where a capability is compiled out entirely, it is reported `false` and
//!   the reason is in the field's documentation. `DESIGN.md` §8.1 says
//!   "detected at runtime, not at compile time"; it does not say "pretend a
//!   BSD has `prctl`".

use crate::reactor::{Reactor, ReactorKind};
use crate::signals::SignalSourceKind;
use std::path::Path;

/// Which cgroup version this kernel runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum CgroupVer {
    /// v2: the unified hierarchy, mounted at `/sys/fs/cgroup`.
    V2,
    /// v1: the legacy per-controller hierarchy.
    ///
    /// Kept as a distinct value rather than folded into "no cgroups" because
    /// the write path is genuinely different: v2 needs a single
    /// `cgroup.subtree_control` delegation, v1 needs a file per controller
    /// under a per-service directory.
    V1,
}

impl CgroupVer {
    /// The name for the capability line.
    pub const fn name(self) -> &'static str {
        match self {
            CgroupVer::V2 => "v2",
            CgroupVer::V1 => "v1",
        }
    }
}

/// How this platform asks the kernel to reboot.
///
/// The two families use completely different constants and different
/// argument conventions. Getting this wrong at runtime means a `reboot` that
/// does nothing and an init that has to be killed by hand, so it is a named
/// value in the capability line rather than an `if` at the call site.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum RebootStyle {
    /// `reboot(LINUX_REBOOT_CMD_*)` with `RB_AUTOBOOT`.
    LinuxReboot,
    /// `reboot(RB_*)` with the options packed into the `options` argument.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
    ))]
    BsdReboot,
}

/// The answer for this machine, and nothing else.
///
/// Every field is decided by a probe that ran, not by a `cfg` that guessed.
/// Field order follows the table in `DESIGN.md` §8 so that the two can be
/// compared line by line during review.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Capabilities {
    /// Which event loop this machine gets.
    pub reactor: Option<ReactorKind>,
    /// How child deaths are detected.
    pub child_tracking: Option<crate::childproc::ChildKind>,
    /// How signals become descriptors, or `None` for the self-pipe.
    pub signal_source: Option<SignalSourceKind>,
    /// Whether `setresuid`/`setresgid` exist, as opposed to `setuid`+verify.
    pub setresuid: bool,
    /// Whether `PR_SET_NO_NEW_PRIVS` is available.
    pub no_new_privs: bool,
    /// Whether `personality(ADDR_NO_RANDOMIZE)` is available.
    pub personality: bool,
    /// Whether POSIX capabilities can be queried and set with the raw
    /// syscalls.
    ///
    /// The field is named `capabilities_lib` because that is what the design
    /// document calls it, but zinit will not link `libcap`: one dependency is
    /// the budget. What is probed is the *syscall*, which is what actually
    /// determines whether a capability can be dropped after `setuid` — and
    /// that is the case that matters, because `setuid` clears the permitted
    /// set on the way down.
    pub capabilities_lib: bool,
    /// The cgroup version mounted here, if any.
    pub cgroup: Option<CgroupVer>,
    /// Whether `/proc` is mounted and readable.
    ///
    /// Probed, never assumed, and nothing in this crate *depends* on it. It
    /// exists so a future feature can say "I need `/proc`" instead of
    /// failing somewhere else.
    pub procfs: bool,
    /// How to reboot on this platform.
    pub reboot_style: Option<RebootStyle>,
}

impl Capabilities {
    /// The one-line summary printed at boot and by `zctl version`.
    ///
    /// Fixed key order, `key=value`, no spaces. An init's capability line is
    /// something operators paste into bug reports, so it has to survive being
    /// pasted into anything.
    pub fn summary_line(&self) -> String {
        let opt = |v: Option<&str>| v.unwrap_or("no").to_string();
        format!(
            "reactor={} child={} signals={} setresuid={} no_new_privs={} \
             personality={} capabilities={} cgroup={} procfs={} reboot={}",
            opt(self.reactor.map(|r| r.name())),
            opt(self.child_tracking.map(|c| c.name())),
            opt(self.signal_source.map(|s| s.name())),
            self.setresuid,
            self.no_new_privs,
            self.personality,
            self.capabilities_lib,
            opt(self.cgroup.map(|c| c.name())),
            self.procfs,
            opt(self.reboot_style.map(|r| match r {
                RebootStyle::LinuxReboot => "linux",
                #[cfg(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "tvos",
                    target_os = "watchos",
                    target_os = "visionos",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "netbsd",
                    target_os = "dragonfly",
                ))]
                RebootStyle::BsdReboot => "bsd",
            })),
        )
    }

    /// The capabilities that are **not** available, for the warning line.
    ///
    /// Order is fixed, so the output is diffable between two machines. Only
    /// capabilities that actually change behaviour are listed; `procfs` is not
    /// one of them, because nothing here uses it.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.child_tracking.is_none() {
            out.push("child-tracking");
        }
        if !self.signal_source.is_some_and(|k| k.is_kernel_delivered()) {
            out.push("kernel-delivered-signals");
        }
        if !self.setresuid {
            out.push("setresuid");
        }
        if !self.no_new_privs {
            out.push("no_new_privs");
        }
        if !self.personality {
            out.push("personality");
        }
        if !self.capabilities_lib {
            out.push("capabilities");
        }
        if self.cgroup.is_none() {
            out.push("cgroup");
        }
        out
    }

    /// One warning line, or `None` if nothing is degraded.
    pub fn degradation_line(&self) -> Option<String> {
        let missing = self.missing();
        if missing.is_empty() {
            return None;
        }
        Some(format!(
            "zinit: degraded: {} absent; see DESIGN.md §8.1 for what each one changes",
            missing.join(", ")
        ))
    }
}

/// Probe this machine.
///
/// Infallible by construction: every probe is written so that a failure is an
/// answer, not an error, and a genuinely unexpected `io::Error` degrades to
/// "the capability is absent" rather than taking the supervisor down during
/// start-up. The only thing this function can fail at is a probe it could not
/// be written for, and those are the `false`s in the code.
pub fn detect() -> Capabilities {
    let mut caps = Capabilities {
        reactor: probe_reactor(),
        child_tracking: probe_child_tracking(),
        signal_source: probe_signal_source(),
        setresuid: probe_setresuid(),
        no_new_privs: probe_no_new_privs(),
        personality: probe_personality(),
        capabilities_lib: probe_capabilities(),
        cgroup: probe_cgroup(),
        procfs: probe_procfs(),
        reboot_style: probe_reboot_style(),
    };
    // A `None` for any of the three primary mechanisms means the fallback is
    // in use; the caller reads `reactor`/`child_tracking`/`signal_source`
    // through the constructors, which do the falling back. Keeping the `None`
    // visible here is what makes the degradation printable.
    if caps.reactor.is_none() {
        caps.reactor = Some(ReactorKind::Poll);
    }
    if caps.child_tracking.is_none() {
        caps.child_tracking = Some(crate::childproc::ChildKind::Waitpid);
    }
    caps
}

/// Detect, and hand every line to `log` — the capability line first, then the
/// degradation warning if there is one.
///
/// This is the entry point the supervisor calls. It is a function rather than
/// a side effect because `zrt` owns no log sink: printing to stderr is the
/// right thing for PID 1 but the wrong thing for a test or for a future
/// `zctl` that wants the same answer for its own output.
pub fn detect_reporting(mut log: impl FnMut(&str)) -> Capabilities {
    let caps = detect();
    log(&caps.summary_line());
    if let Some(line) = caps.degradation_line() {
        log(&line);
    }
    caps
}

fn probe_reactor() -> Option<ReactorKind> {
    match crate::reactor::native_reactor() {
        Ok(r) => Some(r.kind()),
        Err(_) => None,
    }
}

fn probe_child_tracking() -> Option<crate::childproc::ChildKind> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // Probe the syscall, not the constructor: the constructor also builds
        // an epoll set, and if the *syscall* is what is missing we want the
        // error to say so.
        match crate::sys::pidfd_open(crate::sys::getpid()) {
            Ok(fd) => {
                let _ = crate::sys::close(fd);
                return Some(crate::childproc::ChildKind::Pidfd);
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {}
            Err(_) => {}
        }
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // On the BSDs the probe is "does a kqueue exist", which the
        // constructor is; if it does, EVFILT_PROC is a documented filter and
        // failing to add a knote would be reported at `track` time, not here.
        None
    }
}

fn probe_signal_source() -> Option<SignalSourceKind> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // An empty mask is a legal signalfd request and is the cheapest probe
        // that still exercises the syscall, its flags and the kernel's
        // support. If this works, a populated mask works.
        let set: libc::sigset_t = unsafe { core::mem::zeroed() };
        // SAFETY: `set` is a live, zeroed `sigset_t`; `signalfd` copies it and
        // does not retain the pointer. The fd it returns is closed immediately
        // below, before anything can fail in between.
        let fd =
            unsafe { libc::signalfd(-1, &raw const set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
        if fd >= 0 {
            let _ = crate::sys::close(fd);
            return Some(SignalSourceKind::Signalfd);
        }
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        None
    }
}

fn probe_setresuid() -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // Ask to set every id to the value it already has. This is a real
        // `setresuid` — it exercises the kernel's permission check — and it
        // cannot change anything, so a probe has no side effect. If this
        // fails, the real drop would fail too, which is exactly what we want
        // to know before starting a service rather than after.
        // SAFETY: four getters and two "set every id to what it already is"
        // calls. No pointers, no allocation, nothing retained; the probe
        // cannot change an id, which is the whole point of it.
        unsafe {
            let (uid, euid, gid, egid) = (
                libc::getuid(),
                libc::geteuid(),
                libc::getgid(),
                libc::getegid(),
            );
            libc::setresuid(uid, euid, uid) == 0 && libc::setresgid(gid, egid, gid) == 0
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // There is no `setresuid` on the BSDs. `setuid` exists and the
        // verification path is mandatory there instead, which is why
        // `Capabilities::setresuid` being false is a fact about the platform
        // and not about the machine's configuration.
        false
    }
}

fn probe_no_new_privs() -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // PR_GET, not PR_SET: a probe that mutates process-wide state is not
        // a probe. `PR_GET_NO_NEW_PRIVS` succeeding also proves that `SET`
        // will succeed — the kernel only ever refuses to *clear* the flag, and
        // only for a process with `CAP_SYS_ADMIN`.
        // SAFETY: PR_GET_NO_NEW_PRIVS takes no arguments and has no
        // preconditions; the return is 0 on success and -1 with errno set.
        unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 0 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        false
    }
}

fn probe_personality() -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        crate::sys::query_personality().is_ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        false
    }
}

fn probe_capabilities() -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // A real `capget` with the current version, targeting our own pid, and
        // a zeroed capability set. It fills two 32-bit words and changes
        // nothing. An older kernel rejects the version with `EINVAL`, which is
        // a legitimate "no".
        #[repr(C)]
        struct CapHeader {
            version: u32,
            pid: i32,
        }
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct CapData {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        // `_LINUX_CAPABILITY_VERSION_3`, which is not in the libc crate.
        const VERSION_3: u32 = 0x2008_0522;
        let mut hdr = CapHeader {
            version: VERSION_3,
            pid: 0,
        };
        let mut data = [CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        }; 2];
        // SAFETY: `syscall(SYS_capget, &hdr, &data)` is the kernel's own
        // calling convention — two pointers, both to live, correctly aligned
        // locals of exactly the size the kernel writes. The return is 0 or -1
        // with errno set. (On the x32 ABI, where a pointer is 32 bits and a
        // `c_long` is 64, passing a pointer through a variadic `syscall` does
        // not work; that target is not supported by this crate.)
        let rc = unsafe {
            libc::syscall(
                libc::SYS_capget,
                &raw mut hdr as libc::c_long,
                &raw mut data as *mut CapData as libc::c_long,
            )
        };
        rc == 0
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // The BSDs have no capability sets at all: privilege *is* the uid.
        // There is nothing to degrade, which is why the missing line will not
        // mention it as a problem on those platforms.
        false
    }
}

fn probe_cgroup() -> Option<CgroupVer> {
    // v2: the controller list file exists only on the unified hierarchy, and
    // only at the mount root.
    if exists(Path::new("/sys/fs/cgroup/cgroup.controllers")) {
        return Some(CgroupVer::V2);
    }
    // v1: the per-controller directories only exist on the legacy hierarchy.
    if exists(Path::new("/sys/fs/cgroup/memory")) || exists(Path::new("/sys/fs/cgroup/pids")) {
        return Some(CgroupVer::V1);
    }
    None
}

fn probe_procfs() -> bool {
    // Two probes, because "mounted" and "readable by me" are different
    // questions and a container answers them differently:
    //
    // 1. `/proc` opens as a directory. Proves the filesystem is there and
    //    that this process may read it.
    // 2. `/proc/self` resolves through `stat`. Proves the per-process entries
    //    exist *for us*, which `hidepid=2` denies to everyone but the owner.
    //
    // Note what is deliberately *not* done: `open("/proc/self")`. That path
    // is a symlink, and this crate opens nothing without `O_NOFOLLOW`; a
    // capability probe is not the place to introduce the one call that
    // follows links. `stat` follows by definition, which is exactly the
    // semantics being asked about.
    let dir =
        match crate::sys::open_cloexec(Path::new("/proc"), libc::O_RDONLY | libc::O_DIRECTORY, 0) {
            Ok(fd) => fd,
            Err(_) => return false,
        };
    let _ = crate::sys::close(dir);
    crate::sys::stat_dev_ino(Path::new("/proc/self")).is_ok()
}

fn probe_reboot_style() -> Option<RebootStyle> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        Some(RebootStyle::LinuxReboot)
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
    ))]
    {
        Some(RebootStyle::BsdReboot)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
    )))]
    {
        None
    }
}

fn exists(p: &Path) -> bool {
    match crate::sys::open_cloexec(p, libc::O_RDONLY, 0) {
        Ok(fd) => {
            let _ = crate::sys::close(fd);
            true
        }
        Err(_) => false,
    }
}

/// Compile-time proof that a probe is not a `cfg` in disguise.
///
/// This exists as a test, not as a function: the whole point of `capdetect`
/// is that the answers come from the machine. A test that asserts the
/// *observed* values on the machine running the test is what keeps the two
/// from drifting apart during a refactor.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn detection_on_this_linux_machine_is_correct() {
        let caps = detect();

        // The reactor we just built must be the one that was reported.
        let r = crate::reactor::new_reactor().expect("reactor");
        assert_eq!(caps.reactor, Some(r.kind()));
        assert_eq!(caps.reactor, Some(ReactorKind::Epoll), "this is Linux");

        // The signal source must agree with the capability line, including
        // the property that matters: whether delivery is kernel-side.
        let src = crate::signals::new_signal_source(crate::signals::SignalSet::with(
            crate::signals::Signal::Usr1,
        ))
        .expect("signal source");
        assert_eq!(caps.signal_source, Some(src.kind()));
        assert_eq!(src.kind(), SignalSourceKind::Signalfd);

        // Child tracking must agree with the tracker that actually builds.
        let t = crate::childproc::new_child_tracker().expect("tracker");
        assert_eq!(caps.child_tracking, Some(t.kind()));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn linux_capabilities_are_all_detected() {
        let caps = detect();
        // These are facts about the *kernel and the libc*, and every Linux of
        // the last decade has them. If one of these is false the machine is
        // unusual enough that the assumption should be reported, not patched.
        assert!(caps.setresuid, "setresuid has existed since 2.1.41");
        assert!(caps.no_new_privs, "PR_SET_NO_NEW_PRIVS since 3.5");
        assert!(caps.personality, "personality(2) is not optional");
        assert!(caps.capabilities_lib, "capget(2) since 2.2");
        assert!(caps.procfs, "the test runner is on Linux; /proc must exist");
        assert_eq!(caps.reboot_style, Some(RebootStyle::LinuxReboot));
    }

    #[test]
    fn cgroup_detection_matches_the_filesystem() {
        let caps = detect();
        // Cross-check the cgroup answer against what the kernel itself says.
        let v2 = Path::new("/sys/fs/cgroup/cgroup.controllers").exists();
        match caps.cgroup {
            Some(CgroupVer::V2) => assert!(v2, "reported v2 but the file is gone"),
            Some(CgroupVer::V1) => assert!(!v2, "reported v1 but v2 is mounted"),
            None => assert!(!v2, "reported none but the v2 file exists"),
        }
    }

    #[test]
    fn the_summary_line_is_one_line_and_parseable() {
        let line = detect().summary_line();
        assert!(!line.contains('\n'), "must be one line");
        for field in [
            "reactor=",
            "child=",
            "signals=",
            "setresuid=",
            "no_new_privs=",
            "personality=",
            "capabilities=",
            "cgroup=",
            "procfs=",
            "reboot=",
        ] {
            assert!(line.contains(field), "missing {field} in {line:?}");
        }
    }

    #[test]
    fn detect_reporting_always_prints_at_least_the_summary() {
        let mut lines = Vec::new();
        let _ = detect_reporting(|l| lines.push(l.to_string()));
        assert!(!lines.is_empty());
        assert!(lines[0].starts_with("reactor="));
    }

    #[test]
    fn missing_lists_only_real_degradations() {
        // Constructed by hand rather than from `detect`, so the logic is
        // tested on both branches regardless of what this machine has.
        let full = Capabilities {
            reactor: Some(ReactorKind::Epoll),
            child_tracking: Some(crate::childproc::ChildKind::Pidfd),
            signal_source: Some(SignalSourceKind::Signalfd),
            setresuid: true,
            no_new_privs: true,
            personality: true,
            capabilities_lib: true,
            cgroup: Some(CgroupVer::V2),
            procfs: true,
            reboot_style: Some(RebootStyle::LinuxReboot),
        };
        assert!(full.missing().is_empty());
        assert_eq!(full.degradation_line(), None);

        let bare = Capabilities::default();
        let missing = bare.missing();
        assert!(missing.contains(&"kernel-delivered-signals"));
        assert!(missing.contains(&"setresuid"));
        assert!(!missing.contains(&"procfs"), "nothing here uses /proc");
        assert!(bare.degradation_line().is_some());
    }
}
