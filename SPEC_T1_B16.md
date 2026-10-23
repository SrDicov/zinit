# SPEC T1-B16 — cerrar log.rs

## Objetivo
Dejar `crates/zservice/src/log.rs` a cero: 9 MISSED + 5 timeouts resueltos.

## Contexto
Tras `7b688d5`: `69 mutants tested: 9 missed, 53 caught, 2 unviable, 5 timeouts`.
Base: `cargo test -p zservice --lib` → 117 passed.

Supervivientes conocidos: `84:9` (Drop), `283:17` (`<=`), `301/308` (guardas
ENOENT), 3× `|→^` en flags, `360:11` (`<=`).

## Pasos
1. `grep '^MISSED\|^TIMEOUT' /tmp/opencode/mut-log.log` → 14 líneas.
2. Los 5 timeouts: identificar la condición de bloqueo (escritura a pipe/fd
   lleno sin lector, testigo de rotación) y añadir un watchdog de test como en
   `ready.rs` (`hang_sentinel`) para que un cuelgue sea un fallo, no un timeout.
   Un test colgado no vale: si no se puede forzar la rama en 20 min, `#[ignore
   = "<motivo>"]`.
3. Los 3 `|→^`: si los bits son disjuntos, es equivalente — demuéstralo con un
   assert de disjunción (como se hizo en `ready.rs`) y `// equivalent`.
4. El resto: tests de frontera (`Drop` con fd ya cerrado, `<=` exacto,
   ENOENT cuando el fichero desaparece entre `stat` y `open`).
5. Verificar por mutación cada uno (copia + `python3 str.replace` + filtro debe
   fallar + restaurar).
6. Re-medir: `cargo mutants -p zservice --file crates/zservice/src/log.rs --timeout 60 -j 2 > /tmp/opencode/mut-log2.log 2>&1`
   Criterio: **0 timeout**, y cada MISSED restante con `// equivalent` razonado.
7. Verde + `cargo fmt --all` + `clippy -p zservice --all-targets` 0 warnings.
8. `git add crates/zservice/src/log.rs` + `test(log): close the file`.

## Corte
90 minutos.

## Prohibido
`git add -A`/`.`/`commit -a`; tocar otros archivos; cambiar producción.

## REPORT (`REPORT_T1_B16.md`, máx 25 líneas)
Status, `tail -n 1` del re-run, timeouts resueltos y cómo, equivalentes nuevos,
tests, SHA.
