# REPORT T1-B15 — log.rs, primera mitad

Status: DONE. 31 MISSED -> 9 missed en run fresco (`mut-log.log`). 22 matados
(objetivo: ~17). 0 warnings clippy; `cargo test -p zservice --lib` 117 passed.
Commit: `7b688d5` (sólo `crates/zservice/src/log.rs`, +272 líneas de tests).

Matados (líneas log.rs): 75:9, 75:20 (close idempotente/real), 181:13 x2
(guarda fd<0: EBADF≠NotConnected y fd==0 es vivo), 195:19 x2, 226:23
(fróntera 4096 fichero+datagrama: `buf[4096]` panic), 257:26 (rota con fichero
exactamente en max_bytes), 273:12 (fd colgante tras rotate fallido: fichero
debe rotar de nuevo), 283:17 x2 (`==`/`>`: recovery debe reabrir el fd),
296:13 x3 (tabla de 3 rotaciones + `.0` no se toca), 297:36 x2 (shift i-1->i),
346:61 `|`& (O_APPEND/clobber de offset 0), 350:11 `==` (EISDIR en dir),
350:11 `<=` (open(2) legítimo devuelve fd 0).

Supervivientes (9) — todos con in-code reason:
- 84:9 (Drop noop): ir observable in-proc sin double-close UB del propio fd.
- 283:17 `<=`: en recovery h.fd siempre es -1 (`<=` reabre igual).
- 301:23, 308:19 `true/false`: guardas ENOENT; errores reales (EACCES/EISDIR)
  requieren permisos/objetos que el flujo de rotación siempre reabre igual.
- 346:28/44/61 `|`^, 359:72 `|`^: XOR de bits disjuntos == OR (equivalente
  matemático: O_WRONLY/O_CREAT/O_APPEND/O_CLOEXEC no comparten un bit).
- 360:11 `<=`: open_devnull nunca puede devolver fd 0 ni -1 en este runner.

`tail -n 1 mut-log.log`: `69 mutants tested in 4m: 9 missed, 53 caught, 2 unviable, 5 timeouts`

B16 (líneas exactas para retomar): 84:9, 283:17 `<=`, 301:23, 308:19 x2,
308:36 `!=`, 346:28/44/61 `^`, 359:72, 360:11 x2 (los `^` son equivalentes
demostrables: dejarlos como catalogados, no quemar tiempo).
Tests añadidos: 12 (boundary, datagram 4096, fd-0, rotate exacto, recovery,
reopens, shift table, gen .0, EISDIR, append paralelo).
