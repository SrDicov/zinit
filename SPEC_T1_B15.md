# SPEC T1-B15 — log.rs, primera mitad

## Objetivo
Matar ~17 de los `MISSED` de `crates/zservice/src/log.rs`.

## Contexto
`log.rs` tiene 34 missed en el run completo (`mut-zservice-full.log`). Es
formateo/parseo de líneas de log del servicio: funciones puras, sin fork ni
señales — el trabajo debería ser barato comparado con `ready.rs`.
Base: `cargo test -p zservice --lib` → 105 passed.

## Pasos
1. Medir la lista fresca:
   `cargo mutants -p zservice --file crates/zservice/src/log.rs --timeout 60 -j 2 > /tmp/opencode/mut-log.log 2>&1`
   `grep '^MISSED' /tmp/opencode/mut-log.log | head -17`
2. Leer las líneas mutadas y agrupar por función.
3. Tests de tabla (una función pura se prueba con casos frontera: vacío, `\n`
   final, byte no-UTF8, línea larga, prefijo parcial). Un test mata 2-3 mutantes.
4. Verificar por mutación cada grupo: copia a `/tmp/opencode/bak`, `python3
   str.replace`, correr filtro (debe fallar), restaurar. Nunca `git checkout`.
5. Verde + `cargo fmt --all` + `clippy -p zservice --all-targets` 0 warnings.
6. `git add crates/zservice/src/log.rs` (ruta explícita, `git status --short`
   antes) + `test(log): kill 17 mutants`.

## Equivalente
Sólo con razón verificada in-code: `// equivalent (cargo-mutants): <razón>`.
Un `Display` que cambia de formato sólo cuenta como equivalente si ningún
consumidor lee ese formato — demuéstralo con `grep`.

## Corte
90 minutos. Lo que quede pasa al B16 con su línea exacta.

## Prohibido
`git add -A`/`.`/`commit -a`; tocar otro archivo; cambiar producción.

## REPORT (`REPORT_T1_B15.md`, máx 25 líneas)
Status, matados con línea, supervivientes, `tail -n 1` del log, tests, SHA.
