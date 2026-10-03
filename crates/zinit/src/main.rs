//! `zinit` — the binary.
//!
//! Two entry points, and the split between them is the whole point of the
//! architecture (`DESIGN.md` §3):
//!
//! ```text
//! PID 1:  zinit --init      block signals, mount the minimum, reap forever
//!   └─ zinit --sup          the plan, the reactor, the reconciler
//!        └─ services        one process each
//! ```
//!
//! The supervisor is a *child* of PID 1, not PID 1 itself. A panic, an OOM or a
//! protocol bug in the brain then costs one restartable process instead of the
//! machine's governor, and the part that must never fail — reaping orphans,
//! which only PID 1 can do — stays in something small enough to audit by eye.
//!
//! No CLI framework. There are two flags and a directory; `clap` would be more
//! dependencies than behaviour.

mod init;
mod sup;

use std::path::PathBuf;
use std::process::ExitCode;

use zrt::report::announce_degradation;

/// Where service descriptions live when `--sup` is given no directory.
const DEFAULT_CONFIG_DIR: &str = "/etc/zinit/services.d";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);

    let Some(flag) = args.next() else {
        // No arguments at all is the one case worth being loud about: PID 1 is
        // invoked by the kernel as `/sbin/init` or `/sbin/zinit`, with nothing.
        // Exiting non-zero makes the kernel panic rather than leaving the
        // machine with no init, which is the correct outcome either way.
        usage();
        return ExitCode::from(2);
    };

    match flag.as_str() {
        "--init" => {
            // Diverges: a PID 1 that returned would take the machine's only init
            // down with it. No exit code is reachable from this arm.
            init::run();
        }
        "--sup" => {
            let dir: PathBuf = args.next().map_or_else(
                || PathBuf::from(DEFAULT_CONFIG_DIR),
                PathBuf::from,
            );
            if args.next().is_some() {
                announce_degradation("--sup takes at most one argument: a directory");
                usage();
                return ExitCode::from(2);
            }
            match sup::run(&dir) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    // The supervisor cannot supervise. It says why, and it says
                    // it once: PID 1 will relaunch it, and a twenty-times-a-second
                    // stderr flood would bury the boot log that matters.
                    announce_degradation(&format!("supervisor stopped: {e}"));
                    ExitCode::from(1)
                }
            }
        }
        "--version" => {
            // stderr, not stdout: on a console system stdout is the terminal a
            // `console` service is trying to take over.
            announce_degradation(&format!("zinit {}", zrt::VERSION));
            ExitCode::SUCCESS
        }
        other => {
            announce_degradation(&format!("unknown argument `{other}`"));
            usage();
            ExitCode::from(2)
        }
    }
}

/// The two entry points, on stderr, for the same reason as `--version`.
fn usage() {
    announce_degradation("usage: zinit --init | zinit --sup [<services.d>] | zinit --version");
}