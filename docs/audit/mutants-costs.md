# Coste de los mutantes que quedan — una tabla, no 305 entradas

Medido con `cargo mutants --timeout 30 --no-shuffle -j 2`. Este fichero no
repite el detalle (está en `mutants-zconfig.md` y `mutants-zservice.md`): sólo
responde a «¿cuánto cuesta matarlo y cuándo se paga?».

| archivo | missed | qué es | coste | prioridad |
|---|---|---|---|---|
| `zservice/src/spawn.rs` | 152 | camino del hijo post-`fork`: fds, cgroup, ids, mounts, ctty, orden de llamadas | 4-6 h + job privilegiado | **post-beta** (lo cubierto por el test de userns ya está probado; el resto pide root real) |
| `zservice/src/service.rs` | 27 | máquina de estados: listenfds heredados, carrera exec/ready, adopción forking, señales de stop | 3,5-4,5 h (4 grupos de 0,5-1 h) | post-beta (integración con procesos vivos) |
| `zconfig/src/parser.rs` | 23 (+8 timeouts) | límites de longitud, spans, booleanos, índices de `quote` | 2-3 h + corpus de ficheros inválidos | post-beta |
| `zconfig/src/graph.rs` | **3** (+3 timeouts) | tres equivalentes con prueba escrita (dos `+=` del merge y un `<` de `suggest`) | 0 | cerrado |
| `zconfig/src/plan.rs` | **6** (+3 timeouts) | seis equivalentes con prueba escrita (cursor de `find_cycle`, `<=` con valores iguales, brazo `1` del pop) | 0 | cerrado |
| `zservice/src/identity.rs` | 10 | `ERANGE` de `getpwuid_r`/`getgrgid_r` sin entrada mayor que el buffer + carrera `AlreadyExists` | 45 min (`grow_until_cap`) | post-beta |
| `zcore` (varios) | 31 | ya al 15,2 %, disperso y barato | ~2 h | post-beta |

## Lo que sí se cerró (no vuelve a la tabla)

- `zconfig/src/value.rs`: 48 → **0** (208 mutantes: 200 caídos, 8 no viables).
  Tabla de 41 capabilities completa, `help()` con sus nueve frases, los dos
  `From<RestartSpec>` con fixtures no-default y el borde `tcp:65535`.
- `zconfig/src/desc.rs`: 12 → **0** (77 mutantes, 77 caídos). Un fixture por
  overlay silencioso con todos los campos, más el dedup de las directivas que
  acumulan y los tres accesos de aristas.
- `zconfig/src/graph.rs`: 21 → **3**, y los 3 son equivalentes con la prueba
  escrita en el código (`// equivalent (cargo-mutants)`): dos avances de
  cursor que el siguiente brazo Less/Greater corrige (listas ordenadas y sin
  duplicados) y un `<` que solo compara valores iguales. Los tests nuevos
  fijan `span()` variante a variante, `render_cycle`, los umbrales de
  `suggest` (presupuesto y holgura de longitud, en las dos direcciones), el
  desempate de nombres y el W101 con dos aristas dobles.
- `zconfig/src/plan.rs`: 12 → **6**, los 6 equivalentes con prueba escrita
  (el cursor de `find_cycle` no puede agotar dependencias antes de cerrar el
  ciclo; `<=` solo intercambia valores iguales; el brazo `1` del pop hace lo
  mismo que el general). El heap queda a 0 de verdad: invariante estructural
  tras cada push y la secuencia mínima que rompe un índice de padre erróneo.

- `zservice/src/ready.rs`: 47 → **0 libres**, 0 timeouts.
- `zservice/src/log.rs`: 34 → **0 libres**, 0 timeouts.
- Flake de fd 0: `in_isolated_fd_table` (fork), 0/40.

## Cómo se lee el coste

- **pre-beta**: unitario puro o tabla de datos; no necesita hardware ni root y
  cabe en una sesión. `value.rs` y `desc.rs` suman ~1,5 h y bajan el missed de
  `zconfig` de 116 a 56.
- **post-beta**: pide procesos vivos, root o un corpus que todavía no existe.
  El techo no es el test, es el entorno.
- Los timeouts no son flake: son bucles de índice que, mutados, dejan de
  avanzar. Se pagan con su archivo.

## Regla

Ningún mutante se declara equivalente sin una prueba escrita en el código
(`// equivalent: <por qué>`). Un «equivalente» sin prueba es una excusa, y ya
costó una ronda de auditoría en alpha.
