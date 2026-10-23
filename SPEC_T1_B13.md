# SPEC T1-B13 — 14 mutantes de ready.rs

## Objetivo
Matar 14 de los 25 `MISSED` de `crates/zservice/src/ready.rs` en una pasada.

## Contexto
Base: `cargo test -p zservice --lib` → 94 passed (tras `10ea488`, que mató los
5 timeouts de `drain_notify`). No quedan timeouts conocidos.

## Pasos
1. Medir la lista fresca (3 min):
   `cargo mutants -p zservice --file crates/zservice/src/ready.rs --timeout 60 -j 2 > /tmp/opencode/mut-ready3.log 2>&1`
   `grep '^MISSED' /tmp/opencode/mut-ready3.log | head -14`
2. Leer `ready.rs` en las líneas de esos 14 mutantes y agruparlos por función.
3. Escribir un test por frontera (un test suele matar 2-3 mutantes del mismo
   operador). Reutilizar `hang_sentinel()` si alguna rama puede bloquear.
   Sólo se añaden tests al módulo de tests de `ready.rs`.
4. Verificar POR MUTACIÓN cada grupo:
   ```sh
   F=crates/zservice/src/ready.rs; cp $F /tmp/opencode/bak
   python3 -c "import sys;p='$F';t=open(p).read();open(p,'w').write(t.replace(sys.argv[1],sys.argv[2],1))" 'OLD' 'NEW'
   cargo test -q -p zservice --lib <filtro>   # DEBE fallar
   cp /tmp/opencode/bak $F
   ```
5. `cargo test -p zservice --lib 2>&1 | tail -1` verde, `cargo fmt --all`,
   `cargo clippy -p zservice --all-targets` 0 warnings.
6. `git add crates/zservice/src/ready.rs` (ruta explícita, `git status --short`
   antes) y commit `test(ready): kill 14 mutants`.

## Equivalente
Si uno de los 14 resulta equivalente (mismo comportamiento observable),
NO lo fuerces: `// equivalent: <razón>` en el código mutado y anótalo.

## Corte
2 horas. Si a los 90 min quedan >5 sin matar, corta: commit de lo conseguido y
PARTIAL con la línea exacta de cada superviviente.

## Prohibido
`git add -A`/`.`/`commit -a`; tocar otro archivo; cambiar producción.

## REPORT (`REPORT_T1_B13.md`, máx 25 líneas)
Status, líneas matadas, supervivientes con motivo, tests añadidos, resultado de
la suite, SHA.
