# Fatal policy: what a dead service means for the machine

Default: **tolerate**. A service that fails terminally holds (only an
operator `kick` re-arms it) and the boot continues. dinit tolerates a
dead service and boots the rest; zinit does too. Evidence: a dangling
`depends` edge (`agetty-service@ttyS0`, added at runtime by a
protocol client) used to fatal the whole boot while dinit would have
started 73/74. That is rigidity, not rigor.

## `on-fatal` (per service)

| Value    | Meaning |
|----------|---------|
| `ignore` | Log at info, hold. Routine noise. |
| `warn`   | Log loudly, hold. **The default.** Visible, never fatal. |
| `halt`   | Stop every service, stay idle (same landing as SIGTERM). |
| `reboot` | Reboot the machine. |

Only an explicit `halt`/`reboot` marking moves the machine. Nothing
else — not a crash loop, not a failed oneshot, not a dangling
dependency — halts or reboots.

## Dangling required dependencies

A `depends` name that resolves to nothing holds its service (never
spawns) with a loud `W103` warning at load naming the ghost. The
service reads as stopped; its dependents see unmet requirements and
hold as well. The boot is never failed over it.

## Power verbs (landed)

`on-fatal = reboot` stops everything, then reboots through the power
epilogue: orderly stop-all (slowest `stop-timeout` + 10s grace), breadcrumb
in `/var/log/zinit-power.log`, `sync`, then the syscall. Same path as an
operator `zctl reboot`. SIGTERM stays stop-all-stay-idle: power is explicit
verbs, never signals.
