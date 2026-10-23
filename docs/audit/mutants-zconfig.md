# Mutantes de `zconfig` — tasa completa (2026-10-23)

```
cargo mutants -p zconfig --timeout 30 --no-shuffle -j 2
776 mutants tested in 33m: 116 missed, 607 caught, 41 unviable, 12 timeouts
```

**Tasa de supervivencia: 116/776 = 14,9 %.** La tasa completa está medida; es
la primera mitad del criterio 3 de alpha y la única que se puede afirmar sin
haber matado nada todavía.

## Desglose por archivo

| archivo | MISSED |
|---|---|
| `crates/zconfig/src/value.rs` | 48 |
| `crates/zconfig/src/parser.rs` | 23 |
| `crates/zconfig/src/graph.rs` | 21 |
| `crates/zconfig/src/desc.rs` | 12 |
| `crates/zconfig/src/plan.rs` | 12 |

Los cinco son camino crítico: `value.rs` y `parser.rs` deciden qué significa
un fichero, `graph.rs` decide el orden, `desc.rs` y `plan.rs` deciden qué se
ejecuta. No hay cosméticos aquí.

## Timeouts (12) — sospechosos por definición

Un timeout con `--timeout 30` significa que el test **no termina** con el
mutante aplicado. Eso no es "el mutante es equivalente": es un test que espera
un resultado que el mutante impide. Hay que mirarlos uno a uno antes de
llamarlos flake.

```
crates/zconfig/src/graph.rs:603:5 — replace lookup -> Option<Idx> with None
crates/zconfig/src/graph.rs:603:5 — replace lookup -> Option<Idx> with Some(Default::default())
crates/zconfig/src/parser.rs:1212:32 — replace + with -
crates/zconfig/src/parser.rs:1212:32 — replace + with *
crates/zconfig/src/parser.rs:1223:32 — replace + with -
crates/zconfig/src/parser.rs:1223:32 — replace + with *
crates/zconfig/src/parser.rs:1271:36 — replace + with *
crates/zconfig/src/parser.rs:1272:23 — replace += with -=
```

## Lo que falta para cerrar el criterio 3 en este crate

1. Un test de regresión por MISSED en `value.rs` (48) y `parser.rs` (23).
2. Lo mismo en `graph.rs` (21), `desc.rs` (12), `plan.rs` (12).
3. Aquellos que resulten **equivalentes** (dos formas del mismo cálculo) van a
   exclusión en `Cargo.toml`/`.cargo-mutants.toml` con el motivo escrito.
4. Los 12 timeouts, resueltos o justificados con la traza.

Comando de trabajo, un archivo cada vez:

```sh
cargo mutants -p zconfig --file crates/zconfig/src/value.rs --timeout 30
```
