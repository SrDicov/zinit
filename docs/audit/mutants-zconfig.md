# Mutantes de `zconfig` — los 116, documentados por archivo

```
cargo mutants -p zconfig --timeout 30 --no-shuffle -j 2
776 mutants tested in 33m: 116 missed, 607 caught, 41 unviable, 12 timeouts
```

**Tasa: 116/776 = 14,9 %.** Medida completa, no una muestra.

**Post-alpha (2026-10-24): `value.rs` y `desc.rs` cerrados a 0.** Medido por
archivo: `value.rs` 208 mutantes, 200 caídos, 8 no viables, **0 missed**;
`desc.rs` 77 mutantes, **77 caídos**. Los dos eran «pre-beta, unitario puro» en
`mutants-costs.md` y costaron una sesión, no las 1,5 h estimadas: el techo no
era el test, era darse cuenta de que el fixture tenía que usar valores
distintos de los `Default` (un `5 restarts per 1m` es exactamente el default y
dejaba vivo `From<RestartSpec> for Budget`). `zconfig` queda en 56 missed
(parser 23, graph 21, plan 12).

| archivo | missed | qué muta | razón para no matarlo en alpha | post-alpha |
|---|---|---|---|---|
| `value.rs` | 48 | 34 brazos de `parse_capability` (nombre → bit), texto de `ValueError`, dos `From<RestartSpec>` con `Default` | es una tabla de datos: el test que la cubre recorre los 41 nombres, no 41 casos sueltos | **cerrado: 0** |
| `parser.rs` | 23 (+8 timeouts) | límites de longitud y línea en `parse_service_with_diagnostics`, spans de `split_list_spans`, booleanos de `parse_depends`, índices de `quote`, `unknown_key_error` | grammar y spans: matarlos exige un corpus de ficheros inválidos con span esperado y casos de borde de índice | 2-3 h |
| `graph.rs` | 21 (+2 timeouts) | `Display`/`span` de `GraphError`, texto de `render_cycle`, `is_empty`, umbrales de `suggest` | diagnósticos (comparar strings exactos) y heurística de sugerencia (tabla de nombres parecidos) | 1 h |
| `desc.rs` | 12 (histórico; **cerrado: 0**) | 10 en `overlay_onto` (precedencia "lo explícito gana"), 2 en `budget`/`restart_policy` devolviendo `Default` | unitario puro, pero exige pares base+drop-in construidos a mano | 30-45 min |
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

## Cierre alpha — 116 missed, documentados por archivo

Medido en `mut-zconfig-full.log` (776 mutantes, 33 min). Ninguno se mata en
alpha: son trabajo de tabla, no de diseño. El criterio de alpha es "medido y
con causa escrita", no 0 %.

| Archivo | Missed | Tipo | Razón | Post-alpha |
|---|---|---|---|---|
| `value.rs` | 48 | parseo de magnitudes (duración, tamaño, porcentaje, listas) | cada mutante cambia un comparador o una multiplicación dentro del parser numérico; matarlo pide el string de entrada exacto que lo atraviesa. Sin riesgo: el parser ya rechaza lo inválido | 2-3 h de tabla de casos |
| `parser.rs` | 23 | gramática línea a línea (`clave = valor`, comillas, comentarios, secciones) | los casos que faltan salen de `.conf` reales, no de sintéticos; el corpus vive en la caja Chimera | 2 h con corpus real |
| `graph.rs` | 21 | orden topológico, ciclos, aristas blandas | pide fabricar `Plan`s sintéticos con la forma exacta del mutante. Incluye el timeout medido (×69 de coste, no cuelgue — ver `timeouts.md`) | 1 h |
| `plan.rs` | 12 | orden de arranque/parada, realineado de índices tras `reload-all` | pide planes fabricados y comparar la secuencia completa | 1 h |
| `desc.rs` | 12 (histórico; **cerrado: 0**) | defaults de campo y overlay de drop-ins | la semántica de overlay ya está cubierta; los mutantes tocan defaults que ningún test compara porque ningún servicio real los usa | 30 min |
