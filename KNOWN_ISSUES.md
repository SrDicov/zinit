# KNOWN ISSUES (honest list)

Everything here was observed in a VM, not reasoned about. Each entry
says what breaks, what does not, and what replaces the missing piece.

## Demand-start (implemented)

A required edge to a `desired=down` service now fires pull-on-demand:
when a service that declares such an edge is actually running on a
pass, every **disabled** (`enabled = false` at load) dependency that
edge points at becomes `Wanted::Up` and joins the same pass. Mirrors
dinit\'s `depends-on` activation. Operator-stopped services (desired=Down
on an `enabled=true` one) are never re-armed, so `zctl stop` still wins.

Observed cause (Artix): live `dep_add` edges `agetty → getty@tty1..6`
held `agetty`, `system` (`after:agetty`) and `boot`. With the
reconciler\'s pool rule in place (see `Test in
`crates/zcore/src/reconcile.rs::demand_start_single_pass`), the same
chain now converges without any manual `zctl start`: live proof on P1
(binary c11f5fee) where `getty@tty6.conf` carries `enabled = no` on disk
yet runs from boot, pulled by `agetty`'s live required edge. `enabled = no`
off the closure remains the convert policy (it decides the model, not
the run-time rule).

## `on-fatal = reboot` lands stop-all-idle

There is no reboot/power path in this build (same reason an
operator SIGTERM stays idle), so a `reboot`-marked terminal failure
stops every service and announces the gap loudly instead of
pretending. See `docs/fatal-policy.md`. A reboot-capable runtime
replaces that arm, not the core action.

## compatd answers NAK where dinit acts

`unload`, `SOFTREBOOT`, `KEXEC`, and shutdown verbs are refused
honestly. TERM to PID 1 is ignored by design (shutdown goes to
`--sup`: stop-all, stay idle, no poweroff path). Clients that
reboot via the bus will see the refusal, not a reboot.

## Probe-style scripts read "held", not "completed"

`type = script` + `restart = never` exiting 0 is held as suppressed
(the `completed` marker is oneshot-only), so self-test services like
`probe` show `stopped desired=up` after doing their job (`PROBE_DONE`
written, exit 0). Benign test artifact, not a failure. Audit rule:
`restart = never` + `restarts=0` + suppressed + no failure = done.

## Platform recipes, not bugs

- Void (dracut): the guest has no net service of its own, so boot
  with `ip=dhcp rd.neednet=1` — dracut leaves the interface up and
  ssh works. Without it the machine boots fine with no address.
- Chimera: serial-console login never worked (root password unknown,
  ssh-key only). Use ssh. Seasoning note: `sshd.conf` must not carry
  convert-stamped `enabled = no` — strip it at seasoning or ssh dies
  on the next reboot (observed 2026-10-16, fixed by stripping).
- Alpine: OpenRC → zinit conversion is future work. Supported claim
  is "zinit boots as PID 1" (5 services, serial+sshd), nothing more.
- Slirp: hostfwd delivers to the guest's `.15`. Static configs must
  target `10.0.2.15/24` via `10.0.2.2`.

## Live-Chimera findings (2026-10-16, coexistence, sup alongside dinit)

- Fast-dying child goes stale (single occurrence, NOT reproduced):
  synthesized `agetty-service@tty1` (pid 3899/4287/4379) was reported
  `Running` with dead pids, `restarts = 0/3`, no log. Since then:
  demand-pull + instant-death + instance + C4-wake + direct-exec were all
  reproduced live and in CI (`demand_pulled_instant_death_keeps_flapping`,
  green) and all flap correctly; getty instances synthesized later run
  healthy for 15+ min. A reap-path audit (Oct 2026) showed reap-before-
  track is mechanically impossible (SIGCHLD blocked, nothing reaps between
  fork and track) and pointed at the forking hold-window
  (`on_waitpid(forking, unadopted)` holds the death while `cleanup()` wipes
  state) or a lookup-miss discard. Needs the directed root-cause session,
  not more documentation.
- compatd v7 vs dinit 0.22.0 clients: CLOSED. The RC=124 hangs were
  head-of-line blocking (single-threaded accept + one silent holder),
  fixed by one-thread-per-connection with shared handles (generator
  d9a5c2a). Proven live twice: real `dinitctl is-started/status` answer
  instantly. Open minor: after the unexplained 12:44 reboot compatd was
  found stopped (desired=down, restarts=0) with the socket file present
  but unserved; manual `zctl start` restored it. Why it was down is
  unknown (operator action during the poweroff tests, or boot ordering).
- Runtime-activated services: HALF CLOSED by evidence. rtkit and
  power-profiles-daemon run healthy under zinit PID1 (restarts 0),
  pulled at runtime — the dbus-Exec + compatd path works. Only
  bluetoothd stays stopped until something requests org.bluez (same
  path, not yet observed firing). No provider to build; the remaining
  work is observing one activation, not implementing one.

## Soak scope

PSS/FD/zombie numbers are the `zinit --sup` process alone (PID 1 is
a separate process, children excluded). The 2026-10-16 soak runs a
pre-P0 binary (PSS baseline 948 kB fresh); P0 touched only
transition arms, but the number belongs to the old build until the
soak restarts on the release binary.
