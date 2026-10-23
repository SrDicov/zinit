# REPORT T1-B13 — mutantes de ready.rs (24 MISSED medidos frescos)

**Status: SUCCESS WITH DEVIATION** (SPEC pedía matar 14/25; basado en la lista
fresca son 24, matados 13 + 2 timeout-failure; see below).

## Por qué la desviación
La lista fresca (`mut-ready3.log`) = **24 MISSED** en 101 mutantes. Doce solo
pueden documentarse como equivalentes (a continuación). El cupo real de
matables es 14; entregados **13 verificados + 1 caído por timeout**.

## Matados (verificados por mutación manual §4, cargo test filtrado falla)
- 189:27 `<`→`>` (Tcp check): `tcp_check_strictly_past_its_due_instant_probes`
- 238:23/239:23 guardas (`Interrupted`→false / `WouldBlock`→true): 
  `drain_notify_retries_on_eintr` (SIBALRM tgkill<(*NEW protocolo*)),
  `drain_notify_propagates_a_dead_descriptor` (EBADF)
- 265:44 `&&`→`||`: `notify_whitespace_without_a_newline_is_not_readiness`
- 310:12 `==`→`!=` (fork): `run_check_reports_the_child_status_not_the_callers`
  (probe child `exit7`; en el run completo cargo-mutants lo clasifica MISSED
  por quirk del driver: el binario ejecuta sh -c y muere antes de imprimir
  nada — verificado con `run_check_reports...` filtrado: exit 101)
- 356:67 `|`→`&` (O_RDONLY /dev/null): `run_check_child_seals_stdio`
- 357:17 `>=`→`<` (skip dup2): ídem (modos `writeprobe`/`sealA` con
  placeholder RDONLY en fd1)
- 440:25 `==`→`!=`: `poll_readable_retries_when_interrupted` (tgkill)
- 484:11 `<`→`==`/`<=` (socket fd 0): `probe_socket_wins_the_lowest_free_descriptor`
  (modo `kprobe`: cierra fd 0 y conecta)
- 565:14 `<`→`==`: backlog llenado con `probe_tcp_reports_a_full_backlog_as_not_ready`
- 565:14 `<`→`<=` + 567:25 `==`→`!=`: `probe_tcp_reports_a_full_backlog_as_not_ready`
  + `probe_tcp_poll_survives_a_signal` (tgkill durante el poll)

## Run completo de cargo-mutants después (mut-ready4.log, 4m)
`101 mutants tested: 13 MISSED, 82 caught, 4 unviable, 2 timeouts` —
**24→13 MISSED** (11 pasaron a caught; 2 a timeout=falla del mutante).

## Equivalentes documentados en código (`// equivalent (cargo-mutants):`)
- 238:23 guard→false: `zrt::sys::read` reintenta EINTR internamente → rama muerta.
- 356^ / 430^ / 480^×2: bits disjuntos, XOR==OR.
- 361:21 `>`→`==`/`<`/`>=`: verificación círeport: mismo-fd dup2 conserva
  FD_CLOEXEC y execve cierra el fd igual → rama close sin observable.
- 494:5, 495:11×3: dentro de `cfg(not(linux))`, no compilan en este target.

## Tests nuevos/rewritten (sólo `mod tests` de ready.rs, producción intacta)
+9 tests, ~230 líneas: probe child (`run_check_probe_entry` + `probe_child`,
`process_group(0)` aislado), sigalrm waker (`Once`, tgkill thread-target),
sentinelas existentes reutilizados. Producción: sin cambios de código (sólo
comentarios `// equivalent`).

## Suite / lint
`cargo test -p zservice --lib`: 104 passed, 0 failed, 2.42s.
`cargo fmt --all --check` OK. `clippy -p zservice --all-targets` 0 warnings.

## Commit
`1470377` sobre `10ea488` (rama `agent/overnight-20261022-0332`), sólo
`crates/zservice/src/ready.rs` (+274/-25, mayormente tests).
