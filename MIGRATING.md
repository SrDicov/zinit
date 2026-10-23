# Migrating to zinit

Three roads in, one model out. In every case the honest subset converts
mechanically and the rest needs a human: this page says which is which.

## From runit: automatic

```sh
./scripts/runsv-import.sh /etc/sv ./services.d
zinit check ./services.d
```

| runit | zinit | notes |
|---|---|---|
| `run` (executable) | `type = script` + `command = cd '<dir>' && exec ./run` | cwd is the service dir, as runsv runs it |
| `finish` | folded into `command` (`./finish $code 0`) | no equivalent when killed by a signal — documented in the script header |
| `check` | `ready = ping:<dir>/check` | re-run until exit 0, like runsv |
| `log/run` (`svlogd <dir>`) | `log = file:<dir>/<name>.log` | single file, not svlogd rotation |
| `log/run` (`vlogger …`) | `log = syslog` | `-t`/`-p` tag options dropped (warned) |
| `down` | `enabled = no` | |
| `conf` (sourced env) | — (warned, skipped) | sourcing shell into declarative config is refused |
| supervision itself | `restart = always` | budget/delay stay default; no invented dependencies |

## From systemd: by hand, line by line

There is no importer: unit files carry an execution model (cgroups,
slices, `Type=` subtleties) that a converter would have to guess at, and
guessing is what this project refuses to do. The mapping is small enough
to do by hand:

| unit | zinit |
|---|---|
| `ExecStart=` | `command =` (`Type=oneshot` → `type = oneshot`) |
| `Type=forking` + `PIDFile=` | `type = forking` + `pid-file =` |
| `Restart=` | `restart =` (+ `restart-budget`/`restart-delay`) |
| `WantedBy=` / `After=` | `depends =` (+ targets for boot sets) |
| `User=` / `Group=` | `user =` |
| `Environment=` | `env =` (`${VAR}` expansion matches shell defaults) |
| `StandardOutput=journal` | `log = syslog` |
| `StandardOutput=file:` | `log = file:` |
| `WatchdogSec=` | `watchdog-sec =` (needs `ready = notify`) |
| `.socket` units | `listen =` (+ `on-demand = yes` for socket activation) |
| `WantedBy=multi-user.target` + enablement | `enabled =` (`yes` default; `no` stays down until `zctl start`) |

No equivalent, by design: `PrivateTmp=` beyond `private-tmp = yes` is
not modelled (no mount vocabulary beyond `/tmp`), slice hierarchies
flatten to one `cgroup`, and calendar timers do not exist (a oneshot +
an external cron stays cron).

## From sysv init scripts

`scripts/service` maps `service <name> <start|stop|restart|status>` onto
`zctl`. For the scripts themselves: the `start)` case usually contains
the daemon line (→ `command`), the `stop)` case the pidfile handling
(→ `pid-file` + `type = forking` if it double-forks, else plain
`process`). `chkconfig`/`update-rc.d` runlevels become `enabled =`
plus a boot target.

## After converting

1. `zinit check ./services.d` — green means a boot would accept it.
2. Boot it in a container first (see `scripts/zinit-deploy.sh`): leftover
   assumptions (`chpst`, hardcoded `/var/log` layouts, `ulimit` calls
   inside `run`) show up as crash loops with honest errors, not as
   silent divergence.
3. Wire boot order with `depends =` — converters never invent edges.

## From dinit: live conversion (verified on Artix, P1 2026-10-16)

With `z-dinit-generator`, que vive en su **propio repo**, no en este árbol
(`../z-dinit-generator`, el binario `zdg`). dinit stays PID 1 throughout; the zinit
supervisor runs beside it until the window. Evidence: P1 ran 65/65
dinit services covered by 94 zinit services, 0 missing.

```sh
# 1. Deploy, convert, check. Old confs + new sup is the safe direction
#    (new confs brick old sups with E008 — never ship confs alone).
zdg start            # convert + compatd.conf + dinit-watch.conf
zinit check          # must be green before anything starts
# 2. Supervise alongside. dinit keeps its ports; zinit's colliding
#    services stay stopped (0 failed is the invariant, not "all running").
#    dinitctl now answers via compatd (drop-in by design) — compare against
#    a dinitctl list captured BEFORE this step, not live dinitctl.
zinit --sup /etc/zinit/services.d &
# 3. Parity: every name in dinit's list exists in zctl's list.
# 4. Soak 30 min: dinit keeps serving (ssh stays on dinit's sshd),
#    zinit holds steady, 0 failed, 0 zombies.
# 5. THE WINDOW: remove dinit, then reboot onto zinit. Between the two
#    there is no dinit control plane and no rollback that keeps state:
#    - pacman -R dinit   (files go; PID 1 keeps running, processes stay)
#    - point the bootloader at init=/sbin/zinit --init BEFORE rebooting
#    - reboot promptly; verify PID 1, services, ssh, then zctl down-order
```

Do NOT reboot before the bootloader points at zinit — the machine
would come back on whatever fallback init exists. Do NOT expect
`zctl`'s colliding services to run before step 5 frees their ports.
