# Contratos de interfaz — congelados en v0.0.1

Siete fronteras hablan con el mundo exterior. Todo lo demás es
detalle interno y puede cambiar. **Esto no.**

Regla de congelación: cualquier cambio aquí debe ser 100%
retrocompatible (aceptar todo lo que aceptaba v0.0.1 y responder
igual). Un cambio incompatible exige versión mayor + ruta de
migración documentada en `MIGRATING.md`. El incumplimiento lo cazan
los tests de integración, no la memoria.

## C1 — Sintaxis de descripciones y capas

Un fichero `<nombre>.conf` con directivas `clave = valor` (`DESIGN.md`
§5), capas de menor a mayor precedencia (`/usr/lib`, `/etc`, `/run` +
generado; `$ZINIT_CONFIG_DIRS` o `--sup <dir>` las sustituyen),
drop-ins `<nombre>.d/*.conf` que solo ganan lo explícito.
`zinit check [<dir>]` valida exactamente lo que un arranque leería y
su veredicto es el contrato legible por máquina de C1.

## C2 — Generadores (`sup.rs`, `GENERATORS_ENV`)

Ejecutables en `$ZINIT_GENERATORS` o `/etc/zinit/generators.d`: 10 s
de presupuesto cada uno, su stdout se vuelve `<stem>.conf` de la capa
superior solo si parsea. Nunca fatales (omitir + avisar). Los no
ejecutables se ignoran.

## C3 — `wants/` (`sup.rs`)

Entradas `wants/<target>/<svc>` que se vuelven aristas blandas
(`optional`): nunca ordenan y nunca bloquean. Ese es su contrato.

## C4 — Socket de control (`ctl.rs`, `DESIGN.md` §7, congelado en código)

`AF_UNIX` + `SOCK_STREAM`, una línea por trama con `\n`:

```text
client →  C {id} <command> [args...]\n
server →  R {id} 0 [payload]\n        on success
server →  E {id} <code> <message>\n   on failure
```

Sin prefijo de longitud ni negociación. Códigos: `1` trama mala,
`2` comando desconocido, `3` servicio desconocido, `4` no aplicable,
`5` reload fallido. Ruta `$ZINIT_SOCKET` o `/run/zinit/ctl`; verbos
`status/list/start/stop/restart/kick/reload-all/version/help`.

## C5 — Entorno del lado del hijo (`zservice/spawn.rs`, `DESIGN.md` §6)

Sockets en fds desde el 3 + `$LISTEN_FDS` / `$LISTEN_PID` (parcheado
por el hijo) / `$LISTEN_FDNAMES`; el notify se mueve tras ellos.
Tubería de readiness abierta de por vida en servicios con watchdog
(líneas `WATCHDOG=1` alimentan, nunca declaran ready); cualquier otro
byte mantiene la regla legada. Listo = `none|notify|ping|tcp`
según descripción, nunca inventado.

## C6 — Estado visible (`sup.rs`)

Un fichero `cat`-able por servicio en `$ZINIT_STATE_DIR` o
`/run/zinit/state`, reescrito solo al cambiar. La supervisión no
depende de la observabilidad: sin disco hay silencio, no fallo.

## C7 — Superficie del operador (`zctl`, `check`)

`zctl` formatea una trama `C {id} …`, imprime la línea de respuesta
y sale 0 ante `R` / 1 ante `E`, sin parsear ni embellecer; los verbos
pasan textuales y `--socket` refleja el default del supervisor
(`/run/zinit/ctl`, `$ZINIT_SOCKET` en tests). `zinit check` no toca
nada: valida y su código de salida es el veredicto.
