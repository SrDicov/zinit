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
