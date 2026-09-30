# AGENTS.md

Workspace: `crates/zcore`, `crates/zconfig`, `crates/zrt`, `crates/zservice`. No `zinit`/`zctl`/`zcheck` binaries yet (README phases 1/3 pending) — don't invent them.

## Commands
```sh
mise use rust@stable
cargo test                    # all (423 tests)
cargo test -p zcore           # single crate; same for zconfig/zrt/zservice
cargo test -p zcore <filter>  # single test
cargo clippy --all-targets    # must be 0 warnings (CI sets RUSTFLAGS=-D warnings)
cargo fmt --check
```
CI workflows in `.github/workflows/`: `ci.yml` (portability matrix + purity + license + size + msrv/docs/nightly/miri) plus `fuzz.yml`, `property-tests.yml`, `security-audit.yml`, `bench.yml`, `size.yml`, `release.yml`, `alpine-e2e.yml`. Toolchain `stable` + `clippy,rustfmt` per `rust-toolchain.toml`.

## Dependency boundaries (do not add deps)
- `zcore`: zero deps, `#![no_std]` + `#![forbid(unsafe_code)]`. Takes `now_ms: u64`, returns `Vec<Action>`. Needs I/O? Return an `Action`, never add a dep.
- `zconfig`: only `zcore`. Pure `&str -> Plan`, no file/clock I/O.
- `zrt`: only `libc`. Sole syscall layer; every `unsafe` needs `// SAFETY:`, `#![deny(unsafe_op_in_unsafe_fn)]`.
- `zservice`: only `zcore+zconfig+zrt+libc`. No async runtime, logger, or CLI parser.
- Workspace lints `unsafe_code=forbid`; `zrt`/`zservice` override locally to `allow` with deny-unsafe-op discipline. `panic="abort"` in dev+release (test harness still unwinds).

## Quirks that break builds
- Edition 2024, MSRV 1.85, license `GPL-3.0-or-later` (`Cargo.toml`; README's Apache line is stale).
- `zcore` model: `Desired{Up,Down}` vs `State{Stopped,Starting,Running,Stopping}` — no `Restarting` state; crash = `(Stopped,Up)` + reconciler + budget. `Event` in / `Action` out, always per-`Idx`, never global.
- Post-fork child path (`zservice/spawn.rs`, DESIGN §9.1): no alloc/locks, single exit via `execve` or `_exit(127)`, errno back through pipe.
- Readiness timeout never blocks boot (degrades to `Running` + warning). No `/proc` dependency; runtime `capdetect` + explicit degradation announce.
