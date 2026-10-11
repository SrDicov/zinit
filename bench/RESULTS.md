# Benchmarks: zinit vs runit, s6, dinit, sysvinit, openrc

Uniform cages, uniform workload, uniform host-side measurement. No init
cooperation: everything is observed through `/proc` and marker files from
outside the cage, so the numbers are comparable across inits.

## Workload

Two services per cage, identical behaviour everywhere:

| service | behaviour |
|---|---|
| `a` | long-running `/bin/sleep 300`, writes a boot marker on (re)start |
| `b` | oneshot writing `b.done`, then exits |

Per-init encodings (see `setup-<init>.sh`): runit/s6 `run` scripts,
dinit `type = process`, busybox `inittab` (`respawn`/`once`), openrc
runscripts in the `default` runlevel, zinit `a.conf`/`b.conf`.
Base: Alpine minirootfs 3.24 + one package (`runit`, `s6`, `dinit`,
`openrc`; sysvinit is busybox `init`, zinit is the static binary under
test). Launch: `unshare --map-root-user --mount --pid --fork
--mount-proc chroot <cage> <init>`, frozen per run (`bench-run.sh`).

## Base results (`results/<init>.txt`)

Two full series with the frozen harness; respawn re-measured with the
mtime method (below). Base series 2 + respawn series:

| init | boot_ms | rss_kb | idle_ticks_60s | respawn_ms | cage_du_kb |
|---|---|---|---|---|---|
| runit | 86 | 2720 | 16 | 15 | 13124 |
| s6 | 70 | 3656 | 16 | 1046 | 14700 |
| dinit | 154 | 4004 | 0 | 24 | 16276 |
| sysvinit | 69 | 1936 | 0 | 1048 | 9864 |
| openrc | 500 | 3032 | 0 | TIMEOUT | 14820 |
| **zinit** | **78** | **2472** | **0** | **14** | **10628** |

Reading:

- **boot**: sysvinit 69 ≈ s6 70 < zinit 85 ≈ runit 86 < dinit 154 <
  openrc 500. Sub-200 ms everywhere except openrc (shell-script
  runlevels, stable 497–500 across series). Includes ~50–100 ms of
  constant container setup (`unshare`+`chroot`), identical for all.
- **rss** (whole tree: supervisor + helpers + daemons): sysvinit 1936 <
  zinit 2368 < runit 2720 < openrc 3032 < s6 3656 < dinit 4004.
  Per-process dissection: sysvinit = init 956 + sleep 1048; zinit =
  reaper 640 + sup 728 + sleep 872. The sup is dominated by binary
  text (766 KB static-pie, profile already maxed: lto/strip/opt-s/
  codegen-units-1); retained heap is second-order. Honest ceiling:
  dieting closes toward ~2100, but #1 stays out of reach without
  cutting the two-process architecture or the feature set (control
  plane, readiness, sandboxing, generators) that the 432 KB pays for.
- **idle** (CPU ticks consumed in 60 s): dinit/openrc/sysvinit 0,
  zinit 6, runit/s6 16. runit/s6 churn includes their restart of the
  oneshot `b` (hot loop by design); dinit stops cycling `b` after
  "restarting too quickly". zinit decomposed by direct measurement:
  P1 took the supervisor loop to **0 wakeups / 0 ticks over 60 s**,
  and F2 (blocking `waitpid` reaper, `testing`) took PID 1 to
  **0 ticks / 0 wakeups over 60 s** with instant supervisor relaunch
  (kill -9 sup → services back, marker rewritten in 108 ms end to
  end). Full-cage idle is 0, matching the leaders — confirmed by the
  v0.0.1 release binary in the harness (table row: 0).
- **respawn** (`kill -9` the sleep → marker rewritten): runit 15 ≈
  zinit 14 ≈ dinit 24 ≪ s6 1046 ≈ sysvinit 1048 ≪ openrc TIMEOUT.
  Triplicate (3 extra rounds, mtime method): runit 14–15 rock-stable,
  zinit 14–24, dinit 15–24. zinit ties runit's best case; the ~9 ms
  single-series gap is variance, not a systematic deficit, so no
  reconciler fast-path: skipping the topological passes would trade
  ordering guarantees for noise-level milliseconds. Rejected by data.
  openrc does not supervise (`openrc-run` without `supervise-daemon`):
  TIMEOUT is the honest answer, not a harness failure. s6/sysvinit
  pace restarts at ~1 Hz by their own cadence.
- **disk** (whole cage, rootfs-dominated): sysvinit 9864 < zinit 10624
  < runit 13124 < s6 14700 < openrc 14820 < dinit 16276. Binaries:
  runsvdir 22K + runsv 26K, s6-svscan/supervise 26K each,
  dinit 224K, busybox 804K, zinit 766K static-pie (zero runtime deps).

zinit holds #1 (tied) in idle and respawn, top-group in boot, and #2
in RSS and disk — and never worst anywhere.

## Storm results (`results/storm-<init>.txt`)

Crashing service (`echo x >> /tmp/storm.count; exit 1`), lines counted
host-side in a fixed 10 s window (`bench-storm.sh`). Every supervisor
survived (`supervisor_alive=yes` everywhere):

| init | lines/10s | policy read |
|---|---|---|
| runit | 11 | ~1 Hz pacing, unbounded |
| s6 | 11 | ~1 Hz pacing, unbounded |
| sysvinit | 10 | ~1 Hz (init cadence), unbounded |
| dinit | 4 | gives up ("restarting too quickly; stopping") — service stays DOWN |
| openrc | 1 | no supervision — runs once |
| zinit (`restart = always`) | 41 | delay-spaced 4 Hz (250 ms), sustained — bare `always` carries `UNLIMITED_CAPACITY` by design (`value.rs`): imposing a budget the operator did not name would invent a requirement |
| zinit (`always` + `restart-budget = 5 restarts per 60s`) | 6 | 1 first run + 5 budgeted, then exact suppression (`BudgetExhausted`, announced once, `kick` to retry) |

## Method notes (what bit us)

- Series 1 boots (s6 1774, sysvinit 1273, zinit 1305 ms) were host
  load, not inits — series 2 under a quiet host reads 70/69/85.
  Boot gaps under ~100 ms are noise on this machine (eMMC/BTRFS);
  only larger gaps are signal.
- The harness poll once watched a hardcoded `a.pid` instead of
  `$marker` (openrc uses `a.up`) — every openrc run before the fix is
  void. The `sleep`-tracking respawn walked all of `/proc` per poll
  and billed its own forks inside the latency window (runit 4367 ms
  for a ~15 ms restart). Respawn now keys on the marker's mtime
  (`stat -c %.9Y`, one syscall per poll); victims are found once,
  before the window opens.
- Stale openrc pidfiles collide with immortal host kernel pids across
  the pid namespace (`started` check fooled) — each cage may ship an
  executable `.bench-reset` the harness runs pre-launch.
- `SKIP_IDLE=1` skips the 90 s settle+idle window for fast
  respawn-only runs (RSS/idle read `SKIPPED`, results land wherever
  the harness runs — keep them out of `results/` and merge by hand).

## Reproduce

1. `bench/setup-<init>.sh [cage-dir]` rebuilds each workload layer on
   the template rootfs (`TEMPLATE=` overrides, `ZINIT_BIN=` pins the
   binary under test for zinit).
2. Copy the harness before launching; never edit mid-flight
   (`/tmp` here is noexec — invoke via `sh bench/bench-run.sh`).
3. Storm: add the crashing service per `bench-storm.sh` header notes
   (or re-derive from this file's policy table), run, then remove —
   storm services pollute base re-measurement (runit/s6 pick them up
   live).
