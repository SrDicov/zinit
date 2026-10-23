# Mutantes de `zconfig` — los 116, documentados por archivo

```
cargo mutants -p zconfig --timeout 30 --no-shuffle -j 2
776 mutants tested in 33m: 116 missed, 607 caught, 41 unviable, 12 timeouts
```

**Tasa: 116/776 = 14,9 %.** Medida completa, no una muestra.

| archivo | missed | qué muta | razón para no matarlo en alpha | post-alpha |
|---|---|---|---|---|
| `value.rs` | 48 | 34 brazos de `parse_capability` (nombre → bit), texto de `ValueError`, dos `From<RestartSpec>` con `Default` | es una tabla de datos: el test que la cubre recorre los 41 nombres, no 41 casos sueltos | 30-45 min |
| `parser.rs` | 23 (+8 timeouts) | límites de longitud y línea en `parse_service_with_diagnostics`, spans de `split_list_spans`, booleanos de `parse_depends`, índices de `quote`, `unknown_key_error` | grammar y spans: matarlos exige un corpus de ficheros inválidos con span esperado y casos de borde de índice | 2-3 h |
| `graph.rs` | 21 (+2 timeouts) | `Display`/`span` de `GraphError`, texto de `render_cycle`, `is_empty`, umbrales de `suggest` | diagnósticos (comparar strings exactos) y heurística de sugerencia (tabla de nombres parecidos) | 1 h |
| `desc.rs` | 12 | 10 en `overlay_onto` (precedencia "lo explícito gana"), 2 en `budget`/`restart_policy` devolviendo `Default` | unitario puro, pero exige pares base+drop-in construidos a mano | 30-45 min |
| `plan.rs` | 12 (+2 timeouts) | DFS de `find_cycle`, `check_order_acyclic`, `service_plan` | grafos con ciclo y orden explícito construidos en el test | 1 h |

Los cinco son camino crítico: `value.rs` y `parser.rs` deciden qué significa un
fichero, `graph.rs` el orden, `desc.rs` y `plan.rs` qué se ejecuta. El coste es
el único motivo para dejarlos en esta fase; el más barato (la tabla de
capabilities) es el primer candidato post-alpha.

## Timeouts (12)

Un timeout con el mutante aplicado es un test que espera algo que el mutante
rompió, no un flake. Los 12 caen en cuatro bucles de índice que, mutados, dejan
de avanzar: `graph.rs:603 lookup`, `parser.rs:1212/1223 substitute_instance`,
`parser.rs:1271/1272 comment_offset`, `plan.rs:465/466 check_order_acyclic`. Se
resuelven en el mismo lote que su archivo, no en un trabajo aparte.

## Criterio de alpha en este crate

Medido (hecho) + caros documentados con causa (arriba). No hay baratos aquí:
el mínimo son 30-45 min. Ningún mutante se declara equivalente sin prueba;
"equivalente" sin prueba es una excusa.
