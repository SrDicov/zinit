# SPEC T1-B14 — cerrar ready.rs

## Objetivo
Matar los 13 `MISSED` que quedan en `crates/zservice/src/ready.rs`.

## Contexto
`/tmp/opencode/mut-ready4.log`: 24 → 13 MISSED tras `1470377`. Base:
`cargo test -p zservice --lib` → 104 passed.

## Pasos
1. `grep '^MISSED' /tmp/opencode/mut-ready4.log` → 13 líneas.
2. Leer esas líneas en `ready.rs`, agrupar por función.
3. Un test por frontera; reutilizar los helpers que ya existen en `mod tests`
   (`hang_sentinel`, probe children con `process_group(0)`, SIGALRM/tgkill,
   backlog-stuffing con `listen(1)`).
4. Verificar por mutación cada grupo (plantilla `python3 str.replace`, revertir
   desde `/tmp/opencode/bak`).
5. Re-medir al final:
   `cargo mutants -p zservice --file crates/zservice/src/ready.rs --timeout 60 -j 2 > /tmp/opencode/mut-ready5.log 2>&1`
   `tail -n 1 /tmp/opencode/mut-ready5.log`
   Criterio de cierre: **0 timeout** y cada MISSED restante con
   `// equivalent (cargo-mutants): <razón>` en el código.
6. Verde + `fmt --check` + `clippy -p zservice --all-targets`.
7. `git add crates/zservice/src/ready.rs` + `test(ready): close the file`.
   Incluye `STATE_release.md` si lo tocas (no lo toques).

## Corte
90 minutos. Lo que sobreviva, documentado como equivalente con razón
verificada, no a ojo.

## Prohibido
`git add -A`/`.`/`commit -a`; tocar otros archivos; cambiar producción.

## REPORT (`REPORT_T1_B14.md`, máx 25 líneas)
Status, recuento final (`tail -n 1` del log), equivalentes nuevos con razón,
tests añadidos, SHA.
