# Deberes de PID 1 — qué hace zinit, qué delega, y por qué

Referencia de `crates/zinit/src/init.rs` (PID 1) y `crates/zinit/src/sup.rs`
(el supervisor). Un init no es un programa: es un conjunto de deberes que
el kernel le delega porque no hay nadie más que los haga.

| deber | estado | decisión | por qué |
|---|---|---|---|
| Bloquear todas las señales antes del fork | sí (`zrt::signals::block_all`) | obligatorio | sin `SIGCHLD` bloqueado hay una ventana entre el fork y el registro del hijo donde una muerte se pierde |
| `SIGTERM`/`SIGINT` manejables | sí (`install_power_handler`) | obligatorio | una señal al init que no hace nada deja la máquina encendida para siempre; el kernel además manda `SIGINT` al init con Ctrl-Alt-Del |
| Montar `/proc` | sí | obligatorio | sin `/proc` no hay `cmdline`, ni `meminfo`, ni liveness por `/proc` |
| Montar `/sys` | sí | obligatorio | sin él no hay `/sys/fs/cgroup` |
| Montar `/run` (tmpfs) | sí | obligatorio | el estado y los sockets son de este boot, no del anterior |
| Montar `/dev` (devtmpfs) | sí, **sólo si no hay `/dev/null`** | obligatorio | sin `/dev/console` ni `/dev/tty1` no hay terminal; pero un `/dev` de initramfs con `/dev/pts` y `/dev/shm` no se tapa |
| Montar `cgroup2` | sí | obligatorio | sin él, `cgroup =` muere con un ENOENT profundo en el camino del hijo |
| Crear el cgroup del servicio | sí (`zservice/src/identity.rs`) | obligatorio | sin cgroup, un servicio no se puede agrupar ni limitar; el fallo es ruidoso, nunca silencioso |
| Reapear huérfanos | sí (`waitpid(-1)` en bucle) | obligatorio | un PID 1 que no reape deja zombis y el `init` del kernel los ignora |
| Subreaper (`PR_SET_CHILD_SUBREAPER`) | sí (en el supervisor) | obligatorio | el daemon de un `type = forking` es nieto: sin subreaper el kernel lo reparenta a PID 1 y el supervisor nunca lo puede reclamar |
| Lanzar el supervisor y relanzarlo si muere | sí | obligatorio | PID 1 sin supervisor es una máquina sin gobierno |
| No relanzar un supervisor que se aparta | sí (código de salida 11) | deseable | otro supervisor ya tiene el socket: pelear por él produce dos daemons de cada cosa |
| Un solo supervisor por socket | sí (`flock` en `<socket>.lock`) | deseable | dos supervisores sobre un socket es una carrera, no una configuración |
| Responder a una señal de potencia | sí (vía el socket de control) | obligatorio | el poder tiene un solo dueño: el supervisor (stop-all, breadcrumb, `sync`, syscall) |
| Plan que no carga | modo emergencia | obligatorio | un plan roto que mata al supervisor deja la máquina girando sin socket y sin servicios |
| `machine-id` | no | **deseable, pendiente** | lo necesita el resto del userspace, no zinit; escribirlo es del init, pero no es lo que rompe una máquina |
| `hostname` | no (`sethostname` existe en `zrt`, nadie lo llama) | **deseable, pendiente** | mismo caso: depende de la distribución, no del init |
| `/etc/mtab` | no | cosmético | glibc y musl leen `/proc/mounts` |
| Boot log a `/var/log/boot.log` | no | cosmético | el stderr del init ya va a la consola, que es lo que el operador tiene delante; duplicarlo es escribir dos veces |
| `/dev/kmsg` | no | cosmético | reescribir en kmsg lo que ya sale a la consola es redundante |
| Leer `/proc/cmdline` | no | **deseable, pendiente** |Sirve para arrancar en modo diagnóstico; sin sintaxis definida, no |
| Variable `PATH` en el entorno | sí (`DEFAULT_PATH` en el entorno fusionado) | deseable | el supervisor hereda del kernel un entorno sin `PATH`: `env = PATH=${PATH}:/opt/bin` perdía el prefijo |
| `TERM` | no | cosmético | lo fija el servicio de consola, no el init |
| `ulimit` de ficheros abiertos | sí (al tope duro) | deseable | 1024 descriptors no dan para cientos de servicios, y el fallo ("cannot spawn") no dice nada de la causa real |

## Lo que zinit delega a propósito

- **El poder**: el supervisor, siempre. PID 1 pide; el supervisor ejecuta.
  Un solo camino de apagado es más fácil de razonar que dos.
- **El plan**: el supervisor lo carga; PID 1 sólo lo relanza.
- **Los servicios**: el supervisor los gobierna. PID 1 es un reaper
  con un supervisor debajo, y esa forma es la que sobrevive a un crash
  del supervisor sin re-arrancar la máquina.
