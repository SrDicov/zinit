# Contributing

Small diffs, verified live. This project replaces PID 1 — every
change must prove it boots.

## Rules

- Minimal diffs. No unrequested abstractions; deletion over addition.
- `zcore`: zero deps, `no_std`, `forbid(unsafe_code)`. Needs I/O?
  Return an `Action`, never add a dep.
- Every fix carries a regression test. Bug without a test is a rumor.
- Every behavior change documents its dinit divergence (or its
  parity evidence) in `KNOWN_ISSUES.md` or the crate docs.
- Never touch host PID 1. Verify in VMs (`matrix/`), never on iron
  without a rollback plan. Exact-PID kills only, `sync` before kill,
  one log file per boot.

## Cycle

Edit, then re-read the edited region, then commit, push, and read
the workflows. Verdicts live in GitHub Actions only — never
`cargo`/`rustc` locally (except `rustc` for the std-only
single-file generator, and read-only `rustfmt --check`).

```sh
cargo test                    # all (in CI; keep it green)
cargo clippy --all-targets    # must be 0 warnings (RUSTFLAGS=-D warnings)
cargo fmt --check
```

## Commits

Short subject, evidence in the body: boot logs, `zctl list`
excerpts, console captures, metrics. A claim without observed
output does not merge.
