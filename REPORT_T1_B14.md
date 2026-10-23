# REPORT_T1_B14 — cerrar ready.rs

**Status:** DONE. `cargo mutants -p zservice --file crates/zservice/src/ready.rs --timeout 60 -j 2`
→ `101 mutants tested in 3m: 12 missed, 85 caught, 4 unviable` — **0 timeout**
(antes: 13 MISSED + 2 TIMEOUT; tras `1470377`).

**Recuento:** MISSED 13 → 12. Solo caen por caza: 313 (`pid == 0` en `run_check`).

**Equivalente nuevo con razón verificada — ninguno declarado de cero:** los 12 restantes
ya estaban anotados o quedaron anotados con razones demostrables in-code:
- `drain_notify` guard `Interrupted`: `zrt::sys::read` reintenta EINTR en su propio
  bucle (`crates/zrt/src/sys.rs::read`) — `Err(Interrupted)` no puede salir nunca.
- 5× `|`↔`^` (open/poll/socket): bits disjuntos; verificado en máquina por el test
  nuevo `annotated_or_flags_are_disjoint_bitsets` (5 asserts de disyunción).
- `null > 2` en `run_check_child` (==/</>=): `O_CLOEXEC` cierra el mismo fd en
  `execve` en todas las rutas; los `dup2` copian y no mueven el bit — tabla de fds
  del hijo idéntica; ventana close→exec inobservable. Anotación corregida (la
  anterior decía "redundant" a secas).
- Rama `cfg(not(linux/android))` (4 mutantes): compila fuera del target — la `cfg`
  lo prueba. Anotada en el formato exigido.
- Mutante probado para `<`: imposible matarlo por convergencia CLOEXEC (probe
  `sealB` construido y retirado: en clean falla porque fd 0 muere igual en exec).

**Hallazgo de medición (causaba el MISSED fantasma):** bajo el mutante `pid != 0`, el
binario de tests se `exec`ea con un comando que sale 0 y cargo-mutants lee "suite OK".
Arreglado: toda llamada a `run_check`/`check(Ping)` con fork va ahora en probe
children (`exit0/truefalse/dueping/growsup/nudge`) — el probe hijo muere con el
status real del comando y el test padre lo delata (verificado: exit 101).
`probe_tcp_reports_a_full_backlog...` y `...survives_a_signal` también van a probe
children (`stuff/alarm`): convierten los 2 TIMEOUT en CAUGHT ≤15 s (watchdog en
`probe_child` con reipería y status de señal).

**Tests añadidos:** 7 modos nuevos de probe (`exit0/truefalse/dueping/growsup/nudge/stuff/alarm`),
watchdog de 15 s en `probe_child`, test de disyunción de flags. Suite: 104 → 105, verde.
`cargo fmt --check` ✓, `cargo clippy -p zservice --all-targets` 0 warnings ✓.

**SHA:** `6585c32` `test(ready): close the file` (solo `crates/zservice/src/ready.rs`;
`STATE_release.md` no tocado). Log: `/tmp/opencode/mut-ready5.log`.
