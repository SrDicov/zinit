# REPORT_T1_B16 — cerrar log.rs

**Status:** DONE — 0 timeouts.

## Re-run (criterio)
`cargo mutants -p zservice --file crates/zservice/src/log.rs --timeout 60 -j 2` con
`RUST_TEST_THREADS=1` (flakes del paralelismo fd-0, repro 4/15, desaparecen en single-thread).
`tail -n 1 /tmp/opencode/mut-log4.log`: `69 mutants tested in 4m: 5 missed, 62 caught, 2 unviable`
(previo: 9 missed, 5 timeouts).

## Timeouts resueltos (5 → 0)
- Causa verificada empíricamente: los no-op de `write_line`/`write_datagram`/`write_all` dejan los sockets de test en `recvfrom` **bloqueante** sin datos; el binario entero cuelga (`datagram_carries_boundary_line` era el test en el cuelgue).
- Fix: `bound_recv` (`SO_RCVTIMEO` 3 s) en ambos sockets daemon de test → el hang es un fallo en ~3 s.
- `log.rs:309 i -= 1 → i /= 1`: bucle de shift genera spin infinito (renames ENOENT); el primer test con rotación `backups>1` cuelga el binario antes que un watchdog tardío. Fix: sentinel de hang (`abort()` a los 5 s, patrón de `ready.rs`) en el **primer test alfabético**, `a_rotation_terminates_before_the_suite_spins`. Verificado por mutación: SIGABRT a ~5 s → CAUGHT.

## Kills (5): 84:9 Drop (`drop_closes_the_file_descriptor`, F_GETFD→EBADF), 301:23 EISDIR≠ENOENT (dir en `.2`), 308:19 fichero vivo borrado entre `fstat` y `rename`, 360:11 fd-0 (`open_devnull_accepts_fd_zero`), 309 (sentinel).
## Equivalentes verificados (5): 283:17 `<=` (invariante local comentada: `is_err()` ⟹ `fd == -1`, fijada por `rotation_failure_reopens_the_live_fd`); 3× 354 + 370:72 `|→^` (bits disjuntos: `annotated_or_flags_are_disjoint_bitsets` + `// equivalent (cargo-mutants)` en las 2 líneas). Cada uno verificado por mutación individual (copia+str.replace+test+restore).

## Verde
`cargo test -p zservice --lib` → `test result: ok. 123 passed; 0 failed` (última) · `cargo fmt --check` ✓ · `clippy -p zservice --all-targets` 0 warnings ✓
Commit: **`aa31e1e`** (`test(log): close the file`), sólo `crates/zservice/src/log.rs`.
