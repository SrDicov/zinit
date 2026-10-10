# Unsafe audit

Every `unsafe` block in `zrt`, `zservice` and `zinit` (`zcore`/`zconfig`
forbid it entirely), reviewed as of `testing` @ Fase E. Status: **179/179
blocks carry a `// SAFETY:` comment**; the 21 missing ones were backfilled
in this pass (all `mem::zeroed` initialisers, three test-only `fork`s, one
`socket`, one `unmount`, one error-path `close`).

## Method

1. **Mechanical coverage scan**: every `unsafe {` must have `SAFETY` in the
   6 lines above. Re-run: scan `crates/{zrt,zservice,zinit}/src/*.rs` for
   `unsafe {` without a preceding `SAFETY`. (`zcore`/`zconfig` are
   `#![forbid(unsafe_code)]` — checked by grep, not by trust.)
2. **Targeted re-reads** of the blocks where memory safety is actually at
   stake (below). Trivial int-in/int-out syscalls were verified by shape,
   not line by line: `deny(unsafe_op_in_unsafe_fn)` plus `-D warnings`
   already force every raw operation into an `unsafe` block with a comment.
3. **Execution**: `cargo +nightly miri test -p zcore --lib`, the full matrix
   (including i686/i586/riscv/musl/macos), and real PID-1 boots in an
   Alpine container (user + mount namespaces).

## Verdicts by area

- **Post-fork child** (`spawn.rs` `child_main`, `ready.rs`
  `run_check_child`): single-exit (`execve` or errno-pipe + `_exit`),
  no allocation, no locks, `#[inline(never)]`. Proven by container runs
  (PID 1 + supervisor + services) and the 76 `zservice` tests. Sound.
- **Syscall wrappers** (`sys.rs`): ints in, checked returns out; buffers
  bounded (`sun_path` length checks with pre-kernel refusal tests,
  `pollfd` vec, 4 KiB stack line buffer). New code in this phase
  (`unix_dgram_connect`, `poll_readable`, `dup_high`) follows the same
  shape. Sound.
- **Reactor / child tracking / signals**: kernel-filled event arrays,
  fd lifecycle with EBADF-tolerant teardown (`resync`, `close_quietly`).
  Sound; exercised on epoll/kqueue/poll backends in CI.
- **Identity** (`identity.rs`): `getpwnam_r`/`getgrnam_r` with 16 KiB
  buffers grown once to a 1 MiB cap, result-null checked, `ERANGE`
  handled; `setgroups`/`setresuid` with verify-after. The three `fork`s
  are test-only and `_exit` with the verdict. Sound.
- **seccomp install** (`seccomp.rs`): verified number tables (x86_64
  188/188 vs `ausyscall`), `prctl` copies the program; the caller holds
  no-new-privs. Sound.
- **Capability probes** (`capdetect.rs`): read-only syscalls, no mutation
  (`PR_GET`, never `PR_SET`). Sound. Note: `prctl` is restricted in some
  containers, so the positive test fails there — test environment, not
  code (fails identically on unmodified `HEAD`).
- **Log fds** (`log.rs`): owned descriptors, best-effort close, rotation
  by `fstat` (never by path). Sound.
- **`sup.rs` `pid_alive`**: signal 0 is check-only. Sound.

## Residual risks (open, priced)

- No ASan/TSan/MSan runs anywhere; miri covers `zcore --lib` only.
- BSD executes nothing natively (cross-`check` from Linux only); the
  `kqueue` paths are reviewed and compiled, not run.
- Overlayfs write→exec races (`ETXTBSY`) get a bounded 3×5 ms retry in
  the generator path only; the service spawn path does not retry (a
  service binary under concurrent write is an operator error that
  should stay loud).
- Nested containers restrict mount propagation changes (`EINVAL`):
  `private-tmp` treats detach as best-effort and the tmpfs mount as
  fail-closed. Proven in the Alpine container, not on bare metal.
- libFuzzer (`fuzz/`, detached crate) covers `parse_service` nightly;
  the rest of the input surface (control frames, service files at load)
  relies on deterministic tests.
