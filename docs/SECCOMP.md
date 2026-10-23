# Filtro de syscalls: qué se filtra, con qué se compara, y qué queda fuera

`syscall-filter = enforce | errno` instala un programa cBPF clásico. Se
inspecciona **el número de syscall** y, desde esta ronda, **dos argumentos
concretos**. Nada más.

## Lo que se compara

| Syscall | Argumento | Regla | Comprobado por |
|---|---|---|---|
| `clone` | `args[0]` (flags) | `CLONE_NEWUSER \| CLONE_NEWNS \| CLONE_NEWPID \| CLONE_NEWNET` ⇒ violación | test de comportamiento real |
| `ioctl` | `args[1]` (request) | `TIOCSTI` ⇒ violación | test de comportamiento real |
| todas las demás | — | sólo el número, contra la tabla implícita + `syscall-allow` | programa + simulador |

"Violación" es la acción del filtro: `KILL_PROCESS` en `enforce`,
`EPERM` en `errno`.

El test (`a_namespace_clone_and_a_terminal_injection_are_refused`) instala
el filtro en un hijo forkeado y comprueba tres cosas, no una:

1. `clone(CLONE_NEWUSER|SIGCHLD)` falla con `EPERM`.
2. `clone(SIGCHLD)` **funciona** — la regla es específica, no "clone
   prohibido", que rompería `fork()` y `pthread_create()`.
3. `ioctl(0, TIOCSTI, …)` falla con `EPERM`.

Un filtro que "parece correcto" leyendo el programa y nunca dispara es el
fallo que ese test existe para atrapar.

## Lo que queda fuera, y por qué

- **`clone3`**: sus flags viven detrás de un puntero (`struct clone_args`),
  y un programa cBPF no puede desreferenciar memoria del usuario. Un servicio
  filtrado **puede** seguir creando un user namespace por `clone3`. Prohibir
  `clone3` entero se descartó: glibc lo usa para `pthread_create` en kernels
  actuales, así que el filtro rompería cualquier servicio con hilos. Cerrarlo
  pide `seccomp` con notificación (SECCOMP_RET_USER_NOTIF) o un supervisor en
  espacio de usuario; es otra feature, no un ajuste.
- **Syscalls con punteros** (`openat2` con `RESOLVE_*`, `mount` con flags):
  mismo motivo. Filtrar el puntero no dice nada del contenido.
- **`ioctl` distinto de `TIOCSTI`**: se permite. La lista de requests
  peligrosos es larga y específica de cada driver; filtrar sólo el que tiene
  un CVE de inyección de terminal es una decisión, no una cobertura.

## Números

La tabla de números de syscall está verificada **sólo para x86_64 Linux**. En
otra arquitectura, un `syscall-filter` configurado es rechazo de spawn
(fail-closed), nunca un filtro aproximado.
