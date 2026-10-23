# Generador dinit→zinit: huecos, causa y estado (2026-10-23)

Repo: `z-dinit-generator` (aparte). Cada fila: qué se pierde, por qué, y qué
hay hoy en su lugar.

| Hueco | Causa raíz | Estado |
|---|---|---|
| `capabilities` / `securebits` | zinit sólo puede **quitar** capacidades, nunca conceder; dinit restringe el conjunto permitido | **arreglado**: conjunto vacío → `drop-capabilities = all`; lista no vacía → el fichero conserva la directiva y `zinit check` lo rechaza (E004) para mapeo manual. `securebits` igual |
| `starts-on-console` (dinit 0.19+) | no estaba en el `match`, caía al aviso genérico | **arreglado**: alias de `runs-on-console`, con test |
| `kill-all-on-stop` | zinit no tiene "mata todo el sistema antes de parar este servicio"; su equivalente es el stop-all del epílogo de potencia | **documentado**: no se emite; el apagado ordenado lo cubre. Un servicio que dependa de matar el resto al pararse necesita revisión manual |
| `waits-for` → `pulls` | `waits-for` en dinit arranca el objetivo sin depender de su éxito; `pulls` en zinit además lo mete en la clausura de demanda | **documentado**: el borde `after:` es correcto; el `pulls` puede activar de más. Verificación con corpus real pendiente (no hay corpus en este árbol) |
| `restart` por defecto | dinit decide el default en **build-time** (`DEFAULT_AUTO_RESTART`), el generador emite `always` | **documentado**: se elige el default más común y se dice; un dinit compilado con `NEVER` divergiría |
| sockets de activación: 666 vs 0600 | dinit crea el socket con permisos configurable (default 666); zinit bindea 0600 | **documentado**: un cliente no-root pierde acceso; mitigación: `chmod` en un servicio oneshot previo, o `listen` + socket propio. Falta una directiva `listen-permissions` en zinit (post-alpha) |
| `start-timeout` | mismo nombre, semántica distinta: dinit manda SIGINT al grupo al vencer; zinit sólo aplica el plazo con `ready = notify` | **documentado**: un `ready = none` no tiene plazo que vencer. Mitigación: usar `ready = notify` o `tcp:` |
| `inittab-id` / `inittab-line` | contabilidad utmp; zinit no la tiene | **documentado**: las sesiones de getty no aparecen en `who`/`w`. Mitigación: el servicio de getty puede escribir utmp por su cuenta |
| 12 claves mapeadas sin test | tests escritos para el camino feliz, no para cada clave | **parcial**: `depends-ms`, `restart` explícito, `stop-command`, `chain-to`, `stop-timeout`, `start-timeout`, `run-as`, `logfile`, `pid-file`, `ready-notification`, `export-passwd-vars` siguen sin test dedicado |

## Por qué no se cerró todo aquí

Los cinco primeros son mecánicos; los tres últimos (`waits-for`, `restart`
build-time, permisos de socket) cambian **semántica del grafo o del socket** y
no se tocan sin un corpus real con el que comparar. El corpus de Chimera/Artix
no está en ninguno de los dos repos: vive en la máquina, que está apagada.

Eso es un bloqueo real, no una excusa: sin corpus no hay forma de afirmar
"equivalente" con evidencia, y afirmarlo sin ella es exactamente lo que esta
auditoría existe para impedir.
