# SPEC T1-B17 — identity.rs completo

## Objetivo
Dejar `crates/zservice/src/identity.rs` a cero MISSED (10) sin timeouts.

## Contexto
10 missed en el run completo. Funciones de identidad de usuario/grupo:
resolución de `user =` a uid/gid, parseo de nombres, comparaciones. Sin fork.
Base: `cargo test -p zservice --lib` → 123 passed.

## Pasos
1. `cargo mutants -p zservice --file crates/zservice/src/identity.rs --timeout 60 -j 2 > /tmp/opencode/mut-identity.log 2>&1`
   `grep '^MISSED\|^TIMEOUT' /tmp/opencode/mut-identity.log`
2. Tests de frontera por función (nombre vacío, desconocido, uid 0, nombre con
   dígitos, gid separado por `:`, overflow de parseo).
3. Verificar por mutación cada grupo (copia + `python3 str.replace` + filtro
   debe fallar + restaurar). Nunca `git checkout`.
4. Equivalente sólo con razón verificada in-code.
5. Re-medir → 0 timeout.
6. Verde + `cargo fmt --all` + `clippy -p zservice --all-targets` 0 warnings.
7. `git status --short`, `git add crates/zservice/src/identity.rs`, commit
   `test(identity): close the file`.

## Corte
60 minutos. Este archivo no justifica más: si queda algo, equivalente razonado
o PARTIAL con la línea exacta.

## Prohibido
`git add -A`/`.`/`commit -a`; tocar otros archivos; cambiar producción.

## REPORT (`REPORT_T1_B17.md`, máx 25 líneas)
Status, `tail -n 1`, matados con línea, equivalentes con razón, tests, SHA.
