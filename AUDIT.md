# AUDIT — zinit (noche 2026-10-22 03:32)

Contrato de la noche. Cada ítem termina con commit atómico + gate
(build + lint + tests) y queda marcado aquí. Nada fuera de esta lista.

Contexto heredado de las dos auditorías ya hechas
(`ses_e9df8c412ffeNPkSoFyuPDs4Ih` y `ses_e9df8c410ffe3D3V5BYPtigMkQ`) y del
trabajo del día. Lo que ya está arreglado y verificado **no** se repite.

## Estado de partida

- Rama base: `2f449e3` (sobre `testing`).
- CI: verde salvo `check riscv32gc-unknown-linux-gnu` (error `SYS_futex`
  dentro del std de nightly, no es código nuestro — se documenta, no se
  arregla aquí).
- Caja Chimera: **apagada** (el operador la apagó por accidente con un `kill` al
  supervisor). Todo lo que necesita la máquina real está
  bloqueado por eso.

## Bloqueados (necesitan la caja encendida)

- [ ] **B1** Desplegar el binario nuevo (artefacto musl de CI) en
  `/usr/bin/zinit` + `/sbin/zinit` y verificar md5.
- [ ] **B2** Soak 24 h con el binario final (P4 del encargo) y
  `soak-verdict.md`.
- [ ] **B3** Verificar en vivo el apagado por señal (`kill -TERM 1` → poweroff)
  con el binario nuevo.
- [x] **B4** Comprobar si la distribución trae `cron`/`crond`: **sí**.
  Evidencia en `z-dinit-generator/evidence/`: `crond` y `cronie` arrancan y
  se reinician en la propia caja (`iter-closure-obs.md`: "zctl start crond →
  Running en 5 s"), y `HANDOFF.md` los lista entre los servicios vivos. Los
  timers de zinit duplicarían una función que ya está: **descartados** (ver
  FASE 3 abajo).

## Ítems de código (orden por impacto ÷ tamaño)

- [x] **C1** `zctl emergency`: verbo que pone una shell en la consola con
  el plan sano. *(hecho: commit `zctl emergency`)*
- [x] **C2** `suspend` responde **después** de despertarse. El comentario del
  código promete lo contrario ("la respuesta ya está en el cable cuando el
  kernel duerme"): el orden está invertido y el cliente espera en el socket
  mientras la máquina duerme. Arreglo: renderizar y vaciar la respuesta
  antes de dormir.
- [x] **C3** El socket de control no tiene tope de clientes: un proceso
  local puede abrir miles de conexiones y el supervisor crece sin límite.
  Tope (64) y se cierra la conexión de más.
- [x] **C4** **El supervisor re-adopta los servicios vivos al arrancar.**
  Hoy, si el supervisor muere, al volver a arrancar arranca **copias** de
  todo lo que sigue vivo: la máquina queda con dos daemons de cada cosa
  peleándose por los puertos (pasó en vivo el 2026-10-16). El estado
  visible (`$ZINIT_STATE_DIR`) ya dice qué pid tenía cada servicio: basta
  con leerlo al arrancar, comprobar que el pid sigue vivo y **adoptarlo**
  en vez de arrancar otro.
- [x] **C5** `AUDIT.md`/`KNOWN_ISSUES.md` al día con el estado real
  (incluido el fallo de `riscv32` y la desviación del código de error 6).
- [x] **C6** Escala medida: 500 servicios convergen en 622 ms con 4,0 MB de
  RSS, 1008 descriptores, 1 hilo y 0 zombis (`docs/audit/baseline.md`). El
  techo es `RLIMIT_NOFILE / 2`; se documenta, no se construye un ayudante.
- [x] **C7** Dos `stop` simultáneos sobre el mismo servicio: ambos
  responden, el servicio muere una vez y el estado queda `Stopped`/`Down`
  (test `two_stops_at_once_stop_the_service_once`).
- [x] **C8** El grupo de procesos muere con el servicio **cuando se está
  parando** (`7b990e3`). Bug real, reproducido en contenedor Ubuntu (dash
  bifurca; el bash local ejecuta y por eso no salía aquí): el hijo directo
  moría al primer `SIGTERM`, el supervisor daba el servicio por parado y
  los nietos seguían vivos. La prueba de CI lo llevaba fallando desde el
  commit del guard.

## Descartado con motivo

- **Timers de calendario (FASE 3): descartados con evidencia.** La caja trae
  `cronie`/`crond` (B4), así que un segundo planificador sería la misma
  función dos veces, y el cálculo de calendario (zonas, DST) es justo donde
  una implementación pequeña se vuelve incorrecta en silencio. Si algún día
  se necesita un timer *dentro* del grafo de dependencias (arrancar algo N
  segundos después de otro servicio, sin cron), el alcance mínimo es
  `on-boot-sec` + `on-unit-active-sec`; `on-calendar` no.
- **Gramática systemd, journal, D-Bus, `sd_notify` completo, tamperlog**:
  prohibidos por el encargo y sin beneficio en esta máquina.
- **Tocar el sandbox "sin evidencia"**: la evidencia llegó, y era mala. El
  filtro no filtraba: `SECCOMP_MODE_FILTER` valía 1 (`STRICT`), NNP no se
  ponía sin cambio de identidad y la base implícita no cubría un shell. Ver
  C9.

## Reglas de esta noche

- Un ítem, un commit, un gate.
- Nada de refactors de consistencia ni de docs no pedidos.
- Cero dependencias nuevas.
- Nada de `#[allow]` para callar un warning.
- Evidencia real pegada en el informe final; "verde en CI" no es
  comportamiento en vivo.
- 3 fallos seguidos en el mismo bug → hard stop, `STATUS.md`, salir limpio.

- [x] **C9** `syscall-filter` estaba roto de raíz (modo STRICT, NNP no puesto
  sin drop de identidad, base implícita sin `getpgrp`/`getresuid`/
  `getresgid`/`wait4`/`waitid`/`vfork`). Arreglado con tres commits
  (`375641d`, `94a533f`, `46e0d5f`), test en CI para x86_64 y test de sandbox
  como root verificado en contenedor. Limitación declarada: tabla sólo
  x86_64.
- [x] **C10** `SIGINT` a PID 1 (lo que produce Ctrl-Alt-Del) pedía
  `poweroff`; el contrato del kernel es **reiniciar**. Ahora el handler
  recuerda qué señal llegó (la primera gana) y el init pide `reboot` para
  `SIGINT` y `poweroff` para `SIGTERM`, con test de ambos verbos (`f1e5968`).
- [x] **C11** `syscall-allow` probado de extremo a extremo: mismo comando sin
  filtro (0), filtrado (rechazado) y filtrado con `syscall-allow = kill` (0)
  (`476b10a`).
- [x] **C12** Un log que no se puede abrir es rechazo ruidoso, no servicio
  sin logs: test con `ENOTDIR` y aserción de que el servicio no corre y el
  supervisor sobrevive.

---

# AUDIT — noche 2026-10-24 (post-alpha, pre-beta)

Contrato de esta sesión: todo lo que se puede cerrar **sin hardware**. Rama
`agent/overnight-20261024-0050`, base `fb6ca1c` (`main` == `testing`). Nada de
beta sin soak + power-test: eso es hardware, no código.

## Estado de partida

- `ci` de `main` verde salvo `riscv32gc` (run `40026936480`).
- `distro` cancelado por timeout en los 7 legs (run `40026936449`, job alpine
  `120665476036`): **no es lentitud de caché**. A los 3 minutos ya había tests
  corriendo y 4 de ellos se colgaron para siempre.

## Ítems

- [x] **A1** Tests de `crates/zinit/tests/` colgados sin deadline:
  `connect()` bloqueante con el backlog lleno y `zctl()` con
  `Command::output()` sin timeout; el `Mutex` serial arrastraba a 3 tests más.
  Fix: `connect_deadline` (socket no bloqueante + `poll`) y `zctl` con
  deadline de 10 s. Evidencia: log del job alpine, «running for over 60
  seconds» a los 3 min, cancelación a los 45.
- [x] **A6** `a_root_service_keeps_setuid_working` era un flaky real:
  `wait_until(... is_some())` aceptaba el fichero vacío que `>` crea antes de
  que `grep` escriba. Raíz: `Scratch::read` ahora devuelve `None` para un
  fichero vacío.
- [x] **A2** `a_private_tmp_covers_the_scratch_directories` FAILED en Alpine.
- [x] **A3** `a_service_with_a_cgroup_lands_in_it` FAILED en Alpine (contenedor
  sin cgroup escribible): decidir skip con razón o fix real.
- [x] **A4** Workflow: quitar el `cargo build --all-targets` redundante,
  ajustar timeout y decidir matriz con números, no por intuición. Documentar
  en `docs/CI.md` por qué **no** se cachea (una sola dependencia: `libc`).
- [x] **A7** `the_control_socket_refuses_a_mutation_from_an_unknown_peer` leía
  `""` en Alpine: el socket es `0600` y el peer `nobody` ni conectaba. El test
  abre el modo (`0666`, solo durante el test) y un `connect` fallido escribe su
  errno en vez de silencio. 4/4 en podman Alpine.
- [ ] **A5** `distro` verde en `main` o cada leg rojo documentado como externo.
  (Bloqueado por el push: el fix está probado en Alpine, falta el run.)

- [x] **B1** `docs/audit/mutants-costs.md`: tabla Archivo | Missed | Tipo |
  Coste | Prioridad, con los números ya medidos.
- [x] **B2** Los dos «pre-beta» de `zconfig` cerrados a 0, medidos por archivo:
  `value.rs` 48 → 0 (208 mutantes) y `desc.rs` 12 → 0 (77). `zconfig` baja de
  116 a 56. El resto (parser, graph, plan) sigue con coste en la tabla.

- [x] **C1** `KNOWN_ISSUES.md`: el flaky del state congelado en el tag, y que
  los binarios no llevan tests.
- [x] **C2** `docs/BETA.md`: los 5 pre-requisitos y el proceso de tag.

- [x] **D1** `scripts/hw-verification.sh full` (deploy → soak → report →
  kill-term-1), `power-test` por el verbo del operador, `kill-term-1` por
  señal, `after-reboot` y `dinit-removal-check`; `--help` y `--dry-run`.
- [x] **D2** `docs/HW-VERIFICATION.md` al día con el comando único.

- [x] **E1** Punto de retorno documentado en `STATE_release.md` (sección
  «Sesión 2026-10-24»). `AGENTS.md` prohíbe un STATE nuevo: el único STATE es
  `STATE_release.md` (+ `alpha`/`mutants`), así que no se creó `STATE_final.md`.
- [x] **E2** README / CHANGELOG / HANDOFF coherentes con alpha publicado.
- [ ] **E3** Gate final: build + clippy + fmt + tests + `git diff main` revisado.

## Reglas de la noche

- Un ítem, un commit, un gate. Nada de `git add -A`.
- Si un ítem destapa otro, se añade aquí; no se persigue en caliente.
- 3 intentos fallidos en el mismo bug → hard stop + `STATUS.md`.
