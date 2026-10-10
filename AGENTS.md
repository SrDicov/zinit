# AGENTS.md

Workspace: `crates/zcore`, `crates/zconfig`, `crates/zrt`, `crates/zservice`, `crates/zinit`, `crates/zctl`.
`zinit` binary exists (`--init` PID 1 / `--sup` supervisor, `crates/zinit/src/{main,init,sup}.rs`).
`zctl` is the control client (one frame in, one line out, exit 0/1).
No `zcheck` crate yet — don't invent it.

## Commands

```sh
mise use rust@stable
cargo test                    # all
cargo test -p zcore           # single crate; same for zconfig/zrt/zservice/zinit/zctl
cargo test -p zcore <filter>  # single test
cargo test -p zcore --release # property tests run fastest here (nightly cron does this)
cargo test -p zconfig no_panic  # fuzz proxy until real cargo-fuzz targets exist (Phase 5)
cargo clippy --all-targets    # must be 0 warnings (CI sets RUSTFLAGS=-D warnings)
cargo fmt --check             # CI runs cargo fmt --all --check
cargo doc --workspace --no-deps  # must stay clean (RUSTDOCFLAGS=-D warnings, deny(missing_docs))
```

Miri is lib-only by design: `cargo +nightly miri test -p zcore --lib` (never the
`properties` integration test — 100-1000x slowdown guarantees a timeout).

## Dependency boundaries (do not add deps)

- `zcore`: zero deps, `#![no_std]` + `#![forbid(unsafe_code)]`. Takes `now_ms: u64`,
  returns `Vec<Action>`. Needs I/O? Return an `Action`, never add a dep.
- `zconfig`: only `zcore`. Pure `&str -> Plan`, no file/clock I/O (caller supplies text).
- `zrt`: only `libc`. Sole syscall layer; every `unsafe` needs `// SAFETY:`,
  `#![deny(unsafe_op_in_unsafe_fn)]`.
- `zservice`: only `zcore+zconfig+zrt+libc`. No async runtime, logger, or CLI parser.
- `zinit`: only place where all four meet (+`libc`). Hand-rolled args
  (`--init | --sup [<dir>] | --version`); do not add `clap`.
- `zctl`: zero deps. CLI shim only (formats one frame, prints one reply);
  the one crate exempt from the print ban (CI-exempted, not PID-1 path).
- Workspace lints `unsafe_code=forbid`; `zrt`/`zservice` override locally to `allow`
  with deny-unsafe-op discipline. `panic="abort"` in dev+release (test harness still unwinds).
- License `GPL-3.0-or-later` inherited via `license.workspace = true`;
  sole third-party dep `libc` is MIT/Apache-2.0. CI `licence` job fails otherwise.
- Edition 2024, MSRV 1.85 (`cargo build/test` on 1.85.0 in CI).

## Conventions CI enforces (purity job fails the build otherwise)

- No `println!/eprintln!/dbg!` outside `crates/zrt/src/report.rs`.
  Announce via `zrt::report::announce_degradation` (stderr — stdout is the console
  a `console` service takes over).
- No `unwrap/panic!/expect` outside tests in `zrt`/`zservice`/`zinit-sup`.
  Return `io::Result` with the real errno; a supervisor panic is a reboot loop,
  a child-path panic across a C frame is UB.
- `zcore`/`zconfig` must keep `#![no_std]` + `forbid(unsafe_code)` (checked by grep, not by build).
- No foreign-init coupling in Rust code: `systemd|runit|s6|openrc|dinit|dbus`
  outside `//` comments fails CI. Prose (DESIGN.md, doc comments) may name them
  as audit history. Only two grandfathered code literals exist, both proving
  rejection: `zconfig/desc.rs` reserved-name `"dbus"`, `zconfig/diagnostic.rs`
  `"dinit-check bug"` string. New code mentions go in an external plugin, never here.
- Size budget: `target/release/zinit` ≤ 1 MiB (the gate's historical value;
  measured ~650 KiB with the current feature set). CI `size` job also
  asserts `cargo tree -p zinit` is workspace crates + `libc` only.

## Quirks that caused real bugs

- `zcore` model: `Desired{Up,Down}` vs `State{Stopped,Starting,Running,Stopping}` —
  no `Restarting` state; crash = `(Stopped,Up)` + reconciler + budget.
  `Event` in / `Action` out, always per-`Idx`, never global.
- `type = oneshot`: clean exit = `completed` (no respawn, deps satisfied, only
  `kick`/`start` re-arms); completion beats `restart = always`, failure obeys policy.
- `type = forking`: launcher exit held, daemon adopted from `pid-file`
  (`AdoptedPid`); adopted pids signalled by pid, never group; subreaper on Linux
  (`prctl_child_subreaper`, announced), `kill(pid,0)` liveness poll elsewhere.
- Watchdog (`watchdog-sec` + `ready = notify` only): `WATCHDOG=1` lines feed,
  never ready; expiry stops (TERM, stop-timeout escalates, reconciler restarts).
  Notify pipe stays open for life on watchdog services; other lines/partial bytes
  keep the legacy any-byte rule.
- Socket activation: `listen = tcp:<port>[:<name>] | unix:<path>` (repeatable);
  loopback-only TCP bind held across restarts, fds from 3 + `$LISTEN_FDS` /
  `$LISTEN_PID` (child-patched) / `$LISTEN_FDNAMES`, notify moves past them.
  No demand-start trigger yet: services start per plan, sockets pre-bound.
- Sandboxing (Linux, fail-closed): `drop-capabilities` (irreversible bounding-set
  drop after the uid drop), `syscall-filter = enforce|errno` (hand-rolled cBPF,
  arch-validated, `bpf`/`seccomp` never allow-listable; implicit baseline covers
  libc-init/threads/sockets). Non-Linux + configured = spawn refusal, never silent.
  Mount namespaces (`unshare`/`pivot_root`) deliberately deferred: untested mount
  plumbing in PID-1-adjacent code bricks boots; that phase needs hardware CI first.
- Post-fork child path (`zservice/spawn.rs`, DESIGN §9.1): everything prepared
  before `fork`; child is one `#[inline(never)]` fn exiting via `execve` or
  errno-pipe + `_exit(127)`. Order: sigmask-clear → `setsid` → fds → `chdir(/)` →
  rlimits → cgroup → ids → no-new-privs → ctty → `execve`.
- `setsid` fails `EPERM` when the parent is a group leader (always under `cargo test`);
  fallback is `setpgid(0,0)` — never treat EPERM as fatal.
- `type = script` runs `/bin/sh -c <command>` with **no** parent-side `$VAR` expansion;
  `process`/`console` expand in the parent against the supervisor env. Expanding
  scripts in the parent steals the shell's own `env` block (observed bug).
- Never `kill(-pgid)` until the exec pipe proves `ExecOk` (`group_is_signallable`);
  before that the group may not exist and the signal is silently lost. Supervisor
  must `children.track(pid)` before returning to the loop or a fast death becomes
  an untracked zombie. `ESRCH` on signal is the normal exited-between-decision race.
- Readiness timeout never blocks boot (degrades to `Running` + warning).
  Poll `poll_ready` only while `Starting` with a handshake `ready`; otherwise a
  perpetual `Ready` cancels `start-timeout` early.
- No `/proc` dependency anywhere; runtime `capdetect` + explicit degradation announce.
  Missing capability ⇒ announce + continue, never silent divergence.
- Supervisor load (`crates/zinit/src/sup.rs::load_layers`): layers low→high
  (`/usr/lib`, `/etc`, `/run` + generated; `$ZINIT_CONFIG_DIRS` or explicit
  `--sup <dir>` replace them). Higher `<name>.conf` replaces the file below;
  `<name>.d/*.conf` drop-ins merge in layer order via `ServiceDesc::overlay_onto`
  (only explicitly-set fields win; `env`/`depends`/`rlimits` accumulate).
  Bad file, orphan drop-in, cycle, or unresolved `user =` is fatal; missing
  layer dirs are skipped. Descriptions realign to plan indices — never assume
  `read_dir` order matches `plan.order_up`.
- Generators (contract C2): executables in `$ZINIT_GENERATORS` else
  `/etc/zinit/generators.d`, 10 s budget each, stdout becomes top-layer
  `<stem>.conf` iff it parses. Never fatal: skip + announce. Non-executables ignored.
- `wants/<target>/<svc>` entries (contract C3) become soft (`optional`) edges.
  Optional edges never order and never block — that is their contract.
- Control socket (`crates/zinit/src/ctl.rs`, contract C4): `AF_UNIX`+`SOCK_STREAM`,
  `\n`-delimited `C {id} <cmd>` → `R {id} 0 …` / `E {id} <code> …` (DESIGN §7, frozen).
  Verbs: `status/list/start/stop/restart/kick/reload-all/version/help`.
  Path `$ZINIT_SOCKET` else `/run/zinit/ctl`; state dir `$ZINIT_STATE_DIR` else
  `/run/zinit/state` (one `cat`-able file per service, rewritten on change only).
  Bind failure degrades to "no control socket", never a fatal boot.
- `reload-all` rebuilds the plan without disturbing live pids: survivors pair by
  name (`adopt_names`, never positionally) + `ManagedService::reindex`; newcomers
  start fresh; deleted-with-live-pid retire (one TERM, reaped, dropped). Any load
  failure keeps the old plan.
- `SIGTERM/SIGINT/SIGHUP` to `--sup` still means "stop every service, stay up idle".
  There is no poweroff/reboot path — don't add one beside the socket.
