# Changelog

## [Unreleased]
### runit import + enable + service shim (Fase C)
- `scripts/runit-import.sh` (POSIX sh, dash/ash-clean): `runsvdir` →
  `<name>.conf`. `run` (ejecutable, cwd=dir) + `finish` plegado
  (`./finish $code 0`; sin equivalencia ante señales, documentado),
  `check` → `ready = ping:`, `log/run` (dir de svlogd) →
  `log = file:<dir>/<name>.log`, `down` → `enabled = no`, `conf`
  avisado y omitido. Siempre `restart = always`, sin dependencias
  inventadas. Exit 0 aun con omisiones (avisos a stderr), 1 E/S, 2 uso.
  Test de integración con fixtures (`import_runit.rs`): salidas exactas
  byte a byte y re-parseadas por `zconfig`.
- `scripts/service`: `service <name> <start|stop|restart|status>` como
  frente sysv de `zctl` (mismo exit, `ZINIT_SOCKET` del entorno).
- `enabled = yes|no` (defecto `yes`): `no` deja el deseo abajo en el
  arranque hasta `zctl start`; nunca bloquea al operador ni cambia un
  deseo vivo en `reload-all`. Viaja de descripción a plan (`ServicePlan`).
### Consola e instancias (Fase B)
- `tty = /dev/ttyN` (absoluta o rechazo): solo `type = console` (rechazo en
  fichero y baranda fusionada en carga, igual que `pid-file`); el supervisor
  la entrega al hijo, que hace `TIOCSCTTY` (fallo de `ioctl` degradado,
  como documentado). Consola sin `tty`: aviso y corre como proceso.
- `%i`: `nombre@instancia` expande en `command`/`tty`/`pid-file`
  (`%%` colapsa, `%` solo literal, sin `@` no hay expansión, `@` doble o
  vacía se rechaza). Cada fichero se expande con su propio nombre;
  dependencias por nombre exacto.
### `zinit check`
- Offline validator: `zinit check [<dir>]` loads exactly what a boot (or
  `reload-all`) would load — layers, drop-ins, generators, merged
  cross-checks, `user =` resolution, graph order — freezes the plan and
  throws it away. Silent success (exit 0, the Unix answer), file-qualified
  refusal on stderr (exit 1), usage errors exit 2. No new crate: one module
  in the `zinit` binary sharing `resolve_sources`/`load_all`, so the
  validator cannot diverge from the boot. Binary and log-sink checks are
  deliberately out of scope: they depend on the supervisor's runtime
  environment, which an operator shell cannot judge.
### Control client
- `zctl` crate: zero-dep CLI over the C4 protocol (one frame in, one line out,
  exit 0/1, `--socket/--id` flags, `ZINIT_SOCKET` env). Pass-through verbs, no
  service semantics. Exempt from the print ban by CI allowlist (CLI, not PID 1).
### Confinement + socket activation (F10–F17)
- `listen =` (repeatable): loopback TCP + unix binds held across restarts,
  `$LISTEN_FDS/$LISTEN_PID/$LISTEN_FDNAMES` from fd 3, notify fd relocated.
- `drop-capabilities` (verified 41/41 table, irreversible) + `syscall-filter`
  (hand-rolled cBPF, arch-gated, x86_64 table verified 188/188 vs kernel).
  Both fail closed; both refuse loudly off Linux. No demand-start trigger and
  no mount namespaces yet — recorded above with reasons.
### Lifecycle: oneshot, forking, watchdog (F6–F9)
- `type = oneshot|forking` end to end (parser → plan → core → runtime).
  Oneshot: clean exit completes (no respawn, satisfies dependents, `kick` re-arms).
  Forking: launcher exit held, daemon adopted from `pid-file`, pid-only signals,
  subreaper on Linux with liveness-poll fallback elsewhere.
- Watchdog: `watchdog-sec` (needs `ready = notify`, refuses 0/targets);
  `WATCHDOG=1` multiplexed on the notify fd without ever readying; expiry stops
  and restarts via the normal escalation. Pipe kept open for life on watchdogs.
- `zconfig` layering: `explicit` presence tracking + `overlay_onto` (drop-ins),
  layered dirs, `wants/` soft edges, startup generators (10 s, parse-gated).
### Plano de control (C4/C6) + recarga en caliente
- Socket de control `AF_UNIX`/`SOCK_STREAM` (`crates/zinit/src/ctl.rs`, `zrt::sys::{unix_listener, unix_accept, unix_connect}`): protocolo congelado `C {id} <verbo>` → `R {id} 0 …` / `E {id} <código> …` (`DESIGN.md` §7). Verbos v1: `status/list/start/stop/restart/kick/reload-all/version/help`. Ruta por `$ZINIT_SOCKET` o `/run/zinit/ctl` (`0600`); el fallo al enlazar degrada a "sin socket", nunca es fatal.
- Estado visible (C6): un fichero `cat`-able por servicio en `$ZINIT_STATE_DIR` o `/run/zinit/state`, reescrito solo al cambiar; el primer fallo de disco desactiva los volcados sin tocar la supervisión.
- `reload-all`: reconstruye el plan sin perturbar pids vivos (adopción por nombre + `ManagedService::reindex`, altas frescas, bajas con proceso vivo jubiladas con un único `SIGTERM`). Un `load` fallido conserva el plan anterior.
- Tests: parser/protocolo unitario (`ctl.rs`), sockets reales (`zrt::sys`), adopción por nombre (`sup.rs`), e integración contra el binario real (`crates/zinit/tests/control_socket.rs`: stop/start y recarga con pids vivos).
- CI: job `size` (binario `zinit` ≤ 1 MiB — medido ~650 KiB; el plan aspiraba a
  500 KiB pero el presupuesto histórico del gate manda — + árbol solo crates+`libc`) y paso `purity` que prohíbe acoplar el código a inits foráneos (prosa de auditoría excluida, 2 literales de test en allowlist).

### Fase 1 (cierre)
- Reactor verificado: `epoll`/`kqueue`/`poll` level-triggered, `HUP`/`EV_EOF` siempre `readable = true` (`crates/zrt/src/reactor.rs`, tests `closed_pipe_end_is_readable_because_a_hangup_is_a_read`, `pipe_write_end_is_reported_readable`). Sin cambios de código.
- Regression test `group_leader_parent_still_spawns_with_own_group`: cubre el fallback `setsid` EPERM → `setpgid(0, 0)` (`child_step_session`) bajo harness de test líder de grupo.
- Limpieza `ReadyWait::for_service` (identidad muerta).
