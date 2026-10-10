//! `zinit check` — validate configuration without starting anything.
//!
//! The same loader the supervisor boots from and would `reload-all` from,
//! driven to merely freeze the plan and then throw it away: parse every layer, merge every drop-in, refuse
//! orphan drop-ins, cross-check merged combinations, resolve `user =`
//! accounts, order the graph. Anything that would stop a boot stops here
//! instead, with the file and the line, and nothing ever forks.
//!
//! Unix rules for a checker: silent success (the exit code is the answer),
//! diagnostics on stderr, one concern per invocation.
//!
//! ```text
//! exit 0: the plan a boot would govern.
//! exit 1: the configuration is refused (stderr says which file stopped it).
//! exit 2: usage error (handled by the caller, as for `--sup`).
//! ```
//!
//! What `check` deliberately does *not* do: resolve service binaries, open
//! log sinks, or bind sockets. Those depend on the supervisor's runtime
//! environment (its `PATH`, its mounts), and a checker that judged them from
//! an operator shell would bless configurations the boot refuses and refuse
//! ones the boot accepts. The contract is exact: `check` passes if and only
//! if a boot would accept the plan.

use std::path::Path;
use std::process::ExitCode;

use zrt::report::announce_degradation;

use crate::sup::{load_all, resolve_sources};

/// Validate one configuration and report the verdict.
///
/// `dir` follows the `--sup` convention: an explicit directory is the whole
/// configuration, otherwise `ZINIT_CONFIG_DIRS`, otherwise the compiled
/// layer defaults. Warnings encountered while loading are announced as they
/// are found (the loader's own doing, shared with every boot); a refusal
/// prints its file-qualified reason and answers `1`.
pub fn run(dir: Option<&Path>) -> ExitCode {
    let (layers, gen_dir) = resolve_sources(dir);
    match load_all(&layers, &gen_dir) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            announce_degradation(&format!("check: {e}"));
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Scratch directories unique across parallel tests: the tag alone (the
    /// `sup.rs` convention) would collide if two tests ever shared one.
    static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

    fn scratch(tag: &str) -> std::path::PathBuf {
        let n = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("zinit-check-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).expect("write fixture");
    }

    #[test]
    fn valid_configuration_is_silent_success() {
        let dir = scratch("valid");
        write(&dir, "a.conf", "command = /bin/true\nready = none\n");
        write(&dir, "b.conf", "command = /bin/true\ndepends = a\n");
        assert_eq!(run(Some(&dir)), ExitCode::SUCCESS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_directive_is_refused() {
        let dir = scratch("bad-directive");
        write(&dir, "a.conf", "command = /bin/true\nfrobnicate = yes\n");
        assert_eq!(run(Some(&dir)), ExitCode::from(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn orphan_drop_in_is_refused() {
        let dir = scratch("orphan");
        write(&dir, "a.conf", "command = /bin/true\n");
        let dropdir = dir.join("ghost.d");
        std::fs::create_dir_all(&dropdir).expect("dropdir");
        write(&dropdir, "10-x.conf", "command = /bin/false\n");
        assert_eq!(run(Some(&dir)), ExitCode::from(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dependency_cycle_is_refused() {
        let dir = scratch("cycle");
        write(&dir, "a.conf", "command = /bin/true\ndepends = b\n");
        write(&dir, "b.conf", "command = /bin/true\ndepends = a\n");
        assert_eq!(run(Some(&dir)), ExitCode::from(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unresolvable_user_is_refused() {
        let dir = scratch("bad-user");
        write(
            &dir,
            "a.conf",
            "command = /bin/true\nuser = zinit-no-such-user-xyz\n",
        );
        assert_eq!(run(Some(&dir)), ExitCode::from(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merged_nonsense_is_refused() {
        // Each file alone is innocent: the base never mentions pid files,
        // the drop-in never mentions a type. Together they are indefensible.
        let dir = scratch("merged");
        write(&dir, "a.conf", "command = /bin/true\n");
        let dropdir = dir.join("a.d");
        std::fs::create_dir_all(&dropdir).expect("dropdir");
        write(&dropdir, "10-pid.conf", "pid-file = /run/a.pid\n");
        assert_eq!(run(Some(&dir)), ExitCode::from(1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
