# KNOWN ISSUES (honest list)

Everything here was observed in a VM, not reasoned about. Each entry
says what breaks, what does not, and what replaces the missing piece.

## No demand-start (runtime edges to disabled services hold)

A required edge to a `desired=down` service never fires: the
requirer holds, it never pulls. dinit starts dependencies on demand;
zinit starts per plan only. Observed 2026-10-16 on Artix: live
`dep_add` edges `agetty → getty@tty1..6` (console management, added
by dinit-protocol clients after convert) held `agetty`, which held
`system` (`after:agetty`), which held `boot` — while dinit runs all
of them (dinit-ref: `agetty`/`boot`/`system`/`getty@tty1..6` STARTED).
Manual `zctl start getty@ttyN` converges to full parity (`agetty`/
`system`/`boot` complete), so the reconciler is correct and only the
automation is missing. Console gettys on a tty-less VM are
functionally null (serial+ssh unaffected); the scar is protocol-
visible (`boot` reads stopped, dinit says started). Follow-up, not
a boot blocker. `enabled = no` outside the boot closure remains the
convert policy; a required edge added later does not re-enable.

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

## Soak scope

PSS/FD/zombie numbers are the `zinit --sup` process alone (PID 1 is
a separate process, children excluded). The 2026-10-16 soak runs a
pre-P0 binary (PSS baseline 948 kB fresh); P0 touched only
transition arms, but the number belongs to the old build until the
soak restarts on the release binary.
