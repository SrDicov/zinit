# Changelog

## [Unreleased]
### Cierre pre-beta (sin hardware)
- El `distro` de los 7 legs ya no se cuelga: cuatro tests esperaban sin
  deadline (`connect(2)` con el backlog lleno, `zctl` sin timeout) y el
  `Mutex` serial arrastraba a tres más. Ahora toda espera tiene deadline y el
  fallo dice el errno. Timeout del job 45 → 25 min.
- Tres fallos de contenedor pasan a skip con razón escrita (falta de mount
  namespace, cgroup de solo lectura) y el peer `nobody` puede conectar: el
  socket `0600` impedía probar el chequeo de credenciales.
- `scripts/hw-verification.sh full` + `after-reboot`: la noche entera en dos
  comandos, con `kill-term-1` separado del verbo del operador y
  `dinit-removal-check` de solo lectura.
- `docs/BETA.md` (5 pre-requisitos y proceso de tag), `docs/CI.md` (qué es
  gate y qué es red) y `docs/audit/mutants-costs.md` (coste por mutante).
- Job de CI `privileged`: el suite completo como root en un contenedor
  `--privileged`, con `--include-ignored`. Cgroup, sandbox, private-tmp y
  PID 1 en un namespace dejan de saltarse (648 tests, 0 fallos) y destaparon
  dos tests que nunca podían pasar.
- Mutantes de `zconfig` que eran «pre-beta, unitario puro», cerrados a 0:
  `value.rs` 48 → 0 (tabla completa de 41 capabilities, los nueve `help`
  fijados, los dos `From<RestartSpec>` con fixtures no-default, borde
  `tcp:65535`) y `desc.rs` 12 → 0 (overlay silencioso con todos los campos,
  dedup de directivas acumulativas, accesos de aristas), `graph.rs` 21 → 3 y
  `plan.rs` 12 → 6 (el heap a 0 de verdad: invariante estructural tras cada
  push y la secuencia mínima que rompe un índice de padre erróneo). Los 9 que
  quedan son equivalentes con la prueba escrita en el código. `zconfig` baja
  de 116 a 32 missed (23 de ellos `parser.rs`).

## [v0.2.0-alpha] - 2026-10-23
### Cierre de alpha
- `633 tests` en el workspace, 0 fallos; `clippy` y `fmt` limpios;
  `check` y `tests` compilan para musl (`x86_64`).
- Mutantes medidos, no estimados: `zcore` 15,2 % (31/204), `zconfig` 14,9 %
  (116/776). En `zservice` los baratos quedan a cero (`ready.rs` y `log.rs`
  sin supervivientes libres, con los equivalentes razonados en el código) y
  los caros (268) documentados por archivo y con coste en `docs/audit/`.
- Dos fallos reales de portabilidad que el CI destapó y se arreglaron:
  `libc::tgkill` no está en los bindings musl del crate `libc` (se usa
  `syscall(SYS_tgkill, ...)`) y un `mut` innecesario que `-D warnings`
  convertía en error en toda la matriz.
- Acta con los 9 criterios: [`docs/ACTA_ALPHA.md`](docs/ACTA_ALPHA.md).

### Demand-start (2026-10-16)
- A boot-disabled (`enabled = no`) dependency now becomes `Wanted::Up` the moment a
  service that declares it `required` is actually running: dinit's `depends-on`
  activation, inside the reconciler, one pass (`Reconcile::demand_start_single_pass`).
  Operator stops are preserved (only boot-disabled services participate).
- Release candidate `v0.2.0-rc1`: full matrix booted with PID1=zinit; live
  dinit→zinit handoff on Artix verified (30 min coexistence + 1h window,
  0 failed, 0 zombies). Soak 24h underway (baseline PSS 948 kB,
  so far 1297/1309/1209 kB/h, all <100 kB/h influctuation; verdict pending).
### Fatal policy (`on-fatal`, tolerate by default)
- New `depends = after:`-style knob `on-fatal = ignore|warn|halt|reboot`
  (default `warn`): terminal failures hold one service, never the boot.
- Unknown required deps hold their service with a loud `W103` warning
  instead of failing the whole load (the agetty ghost that bricked a
  boot). `GraphError::UnknownDependency` removed.
- `on-fatal = reboot` currently lands stop-all-idle with an explicit
  announcement: no reboot path exists yet (see `docs/fatal-policy.md`).
### Aristas solo-orden `after:`
- `depends = after:x` ordena sin bloquear: el servicio espera el primer
  intento de `x`, nunca su éxito (completado, fallido, suprimido o
  deshabilitado liberan). Es la semántica de `after`/`waits-for`/`before`
  de dinit; `opt:` sigue siendo la arista que ni ordena ni bloquea.
  Ciclos `after` son fatales al cargar, como los requeridos.
### Cierre v0.0.1: contratos congelados + BSD preparado
- `CONTRACTS.md`: C1–C7 congelados (sintaxis y capas, generadores,
  `wants/`, socket de control, entorno del hijo, estado visible,
  superficie del operador). Cambio incompatible = versión mayor.
- RV32-Linux cerrado como `blocked-by-upstream` (issue #19): sin
  target con `std` en Rust, sin hacks.
- `.cirrus.yml`: test en `--release` + tarea de artefacto FreeBSD en
  tags (requiere secreto `GITHUB_TOKEN` una vez en la web de Cirrus).

## [v0.0.1] - 2026-10-11
### Release
- Cinco artefactos musl estáticos (`x86_64`, `i686`, `aarch64`,
  `armv7`, `riscv64` + `zctl` + `install.sh` + `.sha256`) desde el
  workflow `release` en cada tag `v*`. Perfil ya optimizado
  (`lto`, `strip`, `opt-s`, `codegen-units = 1`, `panic = "abort"`).
### Reaper bloqueante (bench F2)
- `init` espera con `waitpid` bloqueante en vez de sondear cada 50 ms:
  0 ticks en reposo (medido: 6 en debug) y relanzo del supervisor
  inmediato ante su muerte, sin signalfd, hilo ni timer. El `sleep` solo
  queda para el caso sin hijos (`ECHILD`: reaper pelado o forks
  fallando), que antes giraba en vacío.
### Reposo sin despertares (bench P1)
- `arm()` no debe nada: sin sondas, watchdogs, demanda ni reinicios
  pendientes devuelve espera máxima en vez del tope de 1 s. Los fds
  siguen registrados, así que un evento despierta igual de rápido y el
  idle cae de ~8 ticks a ~0 por minuto (dinit/openrc/sysvinit leen 0).
  Test `arm_sleeps_forever_when_settled_and_wakes_for_a_start_owed`.
### libFuzzer real (Fase E)
- `fuzz/` (crate aislada, fuera del workspace: libfuzzer-sys nunca toca
  el gate de dependencias): target `parse_service` (cuerpo arbitrario,
  nombre fijo + instanciado) + seeds comprometidas. CI nocturno 10 min
  (`fuzz.yml`: job `cargo-fuzz` en nightly/manual; el proxy `no_panic`
  sigue en cada PR de parser).
### montajes privados (Fase D3)
- `private-tmp = yes|no` (defecto `no`): `unshare` de montajes + tmpfs
  `mode=1777` sobre `/tmp`, instalado en el hijo con privilegios aún
  (tras cgroup, antes de ids); si el tmpfs falla se rechaza el spawn
  (fail-closed; el detach de propagación es best-effort).
  Linux-only (rechazo en padre fuera de Linux); sin privilegios +
  activado = fallo ruidoso. Viaja a plan.
### demand-start por sockets (Fase D2)
- `on-demand = yes|no` (defecto `no`): un servicio parado y no deseado con
  sockets retenidos arranca con la primera conexión — igual que un `start`
  de operador (`Up` + `kick`, presupuesto respetado, sin bomba de forks).
  Sin `listen` es error de carga. Sondeo `poll(2)` sin aceptar (el servicio
  acepta tras nacer) + tope de 100 ms en `arm` (latencia acotada sin spin).
  Bind perezoso con un solo aviso ante puerto ocupado. Viaja a plan.
  Test de integración contra binario real (conexión TCP → `Running`).
### syslog por datagrama (Fase D)
- `log = syslog` cableado: socket `AF_UNIX` datagrama conectado a `/dev/log`
  como stdout/stderr del hijo (un `write` = un datagrama; bloqueante, como
  manda stdio POSIX). Sin formato `PRI`/`TAG` — la atribución viaja en
  `SCM_CREDENTIALS`. El supervisor comparte el socket del handle y reconecta
  una vez ante escritura fallida; sin daemon, degradación anunciada con el
  servicio a oscuras, nunca rechazo. Desaparece
  `SpawnError::SyslogNotWired` (y su test de rechazo).
### runit import + enable + service shim (Fase C)
- `scripts/runsv-import.sh` (POSIX sh, dash/ash-clean): `runsvdir` →
  `<name>.conf`. `run` (ejecutable, cwd=dir) + `finish` plegado
  (`./finish $code 0`; sin equivalencia ante señales, documentado),
  `check` → `ready = ping:`, `log/run` (dir de svlogd) →
  `log = file:<dir>/<name>.log` o `vlogger` → `log = syslog`, `down` →
  `enabled = no`, `conf` avisado y omitido. Siempre `restart = always`, sin dependencias
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
