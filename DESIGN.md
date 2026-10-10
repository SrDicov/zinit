# zinit — diseño de un init

> Un init con **un solo concepto** — *estado deseado* — y un reconciliador que lo cumple.
> Sin shell para la lógica, sin grafos mutables en runtime, sin acuses que falten.

---

## 1. Por qué este diseño y no otro

Este diseño sale de haber auditado `runit` y `dinit` al dedillo (32 000 líneas de análisis,
44 bugs confirmados). No es un third init genérico: cada decisión está justificada por
un hallazgo concreto de esos dos árboles.

### 1.1 Lo que se queda

| De runit | Por qué |
|---|---|
| `supervise/ok`: `open(O_WRONLY\|O_NDELAY)` + `close` sin escribir; `ENXIO` ⇒ muerto | Sonda de vida **coste cero**, sin pidfiles ni pidfd. Sigue siendo la mejor idea del proyecto. |
| Truco del doble fd sobre la misma FIFO (`open_read`+`open_write`) | Garantiza que el escritor nunca ve EOF. `sv` distingue "cerrado" de "muerto" sin ambigüedad. |
| Aislamiento por servicio (un `runsv` por servicio) | **La intención es correcta**: un fallo de supervisión no debe tumbar el sistema. La ejecución (un proceso por servicio) se conserva. |
| Ficheros pequeños, una responsabilidad cada uno | Legibilidad como propiedad de diseño, no como accidente. |
| `check` como programa de disponibilidad | Concepto correcto (dinit lo llamar `ready = ping`). Se conserva como adaptador. |
| Sin stdio en la ruta crítica | Correcto: en PID 1 no hay dónde escribir errores. |

### 1.2 Lo que se tira

| De runit | Por qué |
|---|---|
| Shell como **único** lenguaje de configuración | Es la raíz de casi todos sus problemas. `sv -w196 up /service/*` es una carrera con paso fijo; nada la verifica. `benefits.md` lo llama "paralelo", pero lo que hay es *sin orden conocido*. |
| Protocolo de 1-3 bytes sin acuses ni versionado | No se puede detectar un cambio de formato. `supervise/status` es un `struct` en disco sin magic number. |
| Sin readiness | Sin readiness, `depends-on` es una apuesta: adivinar cuándo está el otro. Es el fallo de diseño central de runit. |
| `sv ok foo` ejecuta `o` | `sv.c:319` sólo lee el primer carácter ⇒ el idiom documentado **falla en silencio**. |
| `execve` fallido sale con código 0 | `runit.c:151,248,270`: un `/etc/runit/2` no ejecutable **encienda el apagado**. |
| `chmod` sin comprobar el retorno | `runit.c:283`: TOCTOU. |
| `taia`/`atto` | Precisión muerta: `atto` siempre 0, `taia_unpack` declarado sin `.c`. |
| ~30 declaraciones sin implementación | Podar el código sin podar los headers es deuda que ningún compilador denuncia. |

### 1.3 Lo que se queda de dinit

| De dinit | Por qué |
|---|---|
| Grafo de dependencias real | Es la diferencia cualitativa frente a runit. |
| Recuperación suave | Un servicio que reinicia no cae a `STOPPED` mientras sus dependientes viven. |
| Protocolo versionado + handles | La idea es correcta; la implementación (buffer circular + prefijo de longitud) es donde están los bugs. |
| Variantes **reales** de setuid (`setresuid`/`cap_setuid`, nunca `seteuid`) | `dinit` hace bien el drop de privilegios: sin ventana de escalada. Se conserva íntegro. |
| Operaciones por fd: `openat`+`O_NOFOLLOW`, `fchown`/`fchmod` sobre fd | Anti-TOCTOU correcto. `O_NOFOLLOW` protege el último componente, pero los intermedios se atraviesan. |
| cgroups antes del drop de uid | Correcto y a menudo overlooked. |
| `utmp`/`utmpx` | runit no escribe nada ⇒ `who`/`w` no funcionan. dinit lo hace. Se conserva. |
| `dinit-check` como binario aparte | Validar descripciones antes de cargar esFeatures que valen dinero. |
| Limitación de tasa de reinicios | Sin ella, un servicio que falla se convierte en un fork-bomb. |

### 1.4 Lo que se tira de dinit

| De dinit | Por qué |
|---|---|
| Grafo mutable en runtime (`dependents` invertidos, punteros crudos a `service_dep` de otros registros) | Fuente de use-after-free en `unload_service`. Congela el grafo en un array ordenado y se elimina la clase entera de bugs. |
| Prefijo de longitud + buffer circular de 1024 | Origen de D-1: `outbuf_size += pkt.size()` tras `std::move` ⇒ **la conexión queda muda en PID 1**. Formato delimitado por `\n` lo hace estructuralmente imposible. |
| Dos colas (`prop_queue`/`stop_queue`) + `process_queues()` sin guarda de reentrancia, llamado desde 8 sitios | Complejidad innecesaria: el orden ya está en el índice topológico. |
| 51 directivas | Superficie enorme para documentar y mantener. Se baja a 11 + escape hatch a shell. |
| `sleep(1)` dentro del event loop (`kill_all_on_stop`) | Congela TODOS los servicios 1 s. Un segundo de latencia global por parada. |
| `name@arg` (servicios con argumentos) | Bonito, pero multiplica el espacio de nombres. Fuera de v1. |
| Log consumers estilo journald | Bueno, pero es unafeature separada. Fuera de v1. |

---

## 2. Los cinco principios

1. **Un solo concepto en runtime: `desired[i] ∈ {Up, Down}`.** Todo lo demás es convergencia.
   Los reinicios no son un caso especial: un servicio que crashea simplemente ya no está
   `Running` mientras `desired` sigue siendo `Up`, y el reconciliador lo vuelve a levantar
   si el presupuesto lo permite.
2. **El grafo se congela al cargar.** Orden topológico calculado una vez → array de enteros.
   En runtime, "está mi dependencia listo" es `order[dep] < order[i] && state[dep] == Running`.
   Sin ciclos, sin punteros, sin `dep_depth`, sin `std::min`/`std::max`.
3. **El núcleo no hace E/S.** La máquina de estados es `(Estado, Evento) → Vec<Acción>`,
   una función pura. Todo testeable sin fork, sin sockets, sin disco.
4. **Nunca se sale con código 0 por un error.** Cada syscall fallible tiene política explícita.
   Si el apagado falla, se grita y se para; no se espera en silencio para siempre.
5. **Ausencia de capacidad ⇒ aviso, nunca fallo silencioso.** Si algo no existe en la
   plataforma, se compila a no-op, se **anuncia en el arranque**, y se continúa. Nunca un
   `unimplemented!()` y nunca un comportamiento distinto sin que nadie lo sepa.

---

## 3. Arquitectura de procesos

```
┌─────────────────────────────────────────────────────────┐
│ PID 1:  zinit --init                                    │
│   · bloquear señales, sembrar /run, montar lo mínimo    │
│   · SIGCHLD → reaper INCONDICIONAL (obligación de PID 1)│
│   · arrancar /dev/console, getty de emergencia           │
│   · fork+exec  zinit --sup   ← el cerebro                │
│   · si el supervisor muere → relanzarlo y anotar         │
│   · señales dePower → pasar al supervisor, no ejecutarlas│
│   ~400 líneas. Sin grafo, sin parseo, sin io.            │
└──────────────────────────┬──────────────────────────────┘
                           │ vigila (pidfd / kqueue / waitpid)
┌──────────────────────────▼──────────────────────────────┐
│ zinit --sup   (el supervisor)                            │
│   reactor: epoll | kqueue | poll                         │
│   señales por signalfd | kqueue EVFILT_SIGNAL | selfpipe│
│   cargó el plan, abre el socket de control, reconcilia   │
│   ~2500 líneas. Si se rompe, PID 1 lo relanza y          │
│   re-engancha a los servicios vivos leyendo pidfiles.    │
└──────────┬─────────────┬─────────────┬──────────────────┘
           │             │             │
      ┌────▼────┐   ┌────▼────┐   ┌────▼────┐
      │ zinit:  │   │ zinit:  │   │ zinit:  │   un proceso por servicio
      │  mysqld │   │  sshd   │   │  cron   │   (aislamiento real)
      └─────────┘   └─────────┘   └─────────┘
```

### Por qué el supervisor es hijo y no el propio PID 1

Porque **D-1 mata el sistema**. Una línea mal puesta en `control.cc` dejó las conexiones de
control permanentemente mudas en PID 1. Si el supervisor es un hijo:

- Un panic, un OOM o un bug de protocolo **no deja la máquina sin governor**.
- PID 1 sólo hace 4 cosas y es trivialmente auditable.
- Re-enganchar es trivial **precisamente porque el plan es inmutable**: se releen pidfiles,
  se verifica liveness con pidfd/kqueue/`kill(0)`, y se reconcilia. Los servicios que estaban
  arriba siguen arriba; el supervisor no los toca.

El coste es un proceso más (~1 MB RSS compartido por fork). El beneficio es que la clase de
fallo "un bug aquí y el sistema no arranca" desaparece por construcción.

---

## 4. El núcleo puro (`zcore`)

Cero dependencias. Ni `libc`. Ni E/S. Compila a `no_std` si se quiere verificar que no se
coló nada. Este es el crate que importa que sea perfecto.

```rust
// ── Estado ──────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum State {
    #[default]
    Stopped,     // no hay proceso
    Starting,    // spawn emitido; esperando exec/readiness
    Running,     // proceso vivo y considerado listo
    Stopping,    // SIGTERM enviado; esperando exit o timeout
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Desired { Up, Down }
```

**No hay estado `Restarting`, y esa ausencia *es* el diseño.** Un servicio que crashea
pasa a `(Stopped, Up)`: `Desired` no ha cambiado, así que el reconciliador ve una
divergencia y converge — sujeto al presupuesto. Un reinicio no es un caso especial que
coordine, es la operación normal del reconciliador. No hay estado, ni cola, ni fase.

Hay un cuarto campo mutable por servicio que no es estado sino **memoria de por qué**:

```rust
restart_suppressed: bool   // el último caso no era reiniciable; esperar a `zctl kick`
```

`Desired` sigue diciendo `Up` a propósito: `zctl status` debe reportar la verdad —*lo
quiero arriba, no está arriba, y no se va a reintentar*— en vez de reescribir
silenciosamente la intención del operador. `restart_suppressed` sólo lo limpia una
acción explícita. Esa asimetría es deliberada: "este servicio no va a volver" es
siempre una decisión que alguien tomó, y que alguien puede deshacer.

`Event` y `Action` están definidos en `crates/zcore/src/event.rs` y `action.rs`: 15
eventos y 11 acciones, todos con el `Idx` del servicio al que conciernen. No hay
eventos globales: una señal que afecte a todos se reparte en un evento por servicio
antes de llegar al núcleo.

### 4.1 La máquina de estados completa

**Legalidad.** `State × Desired` = 4×2 = 8. Las combinaciones inválidas están prohibidas
por un aserto en el propio tipo (`fn new_state(s: State, d: Desired) -> Option<State>`),
no por un `if` desperdigado. `State` sin `Desired` no tiene invariantes que cumplir a
mano porque no existe solo.

**Transiciones** (`Estado` × `Desired` → `Evento` → `Acción`):

| Desde (State, Desired) | Evento | Invariante | → | Acción |
|---|---|---|---|---|
| `(Stopped, Up)` | muerte no reiniciable | — | `Stopped` | `restart_suppressed = true`, esperar a `zctl kick` |
| `(Stopped, Up)` | — | — | — | *idle; el reconciliador actúa* |
| `(Starting, Up)` | `ExecOk` | vivo, aún no ready | `Starting` | — |
| `(Starting, Up)` | `Ready` | listo | `Running` | `Notify(Up)`, cascade |
| `(Starting, Up)` | `ReadyTimedOut` | `start-timeout` | `Running` | `Log(warn)`, cascade |
| `(Starting, Up)` | `ProcessGone` | murió antes de ready | `Stopped` | contar en budget; `Notify(Down)` |
| `(Starting, Up)` | `StartTimeout` | no arrancó | `Stopping` | `Signal(KILL)`, `Log(error)` |
| `(Starting, Down)` | `ProcessGone` | cancelado | `Stopped` | — |
| `(Running, Up)` | `ProcessGone` | **crash** | `Restarting` | contar en budget; **cambiar a `Stopped` cuando el budget lo permita** |
| `(Running, Up)` | `DepFailed` | improbable | `Stopping` | `Signal(TERM)`, cascade |
| `(Running, Down)` | `StopTimeout` | no muere | `Stopping` | `Signal(KILL)`, `Log(error)` |
| `(Running, Down)` | `ProcessGone` | parada limpia | `Stopped` | cascade |
| `(Stopping, *)` | `ProcessGone` | terminó | `Stopped` | cascade |
| `(Restarting, *)` | `SpawnIssued` | — | `Starting` | — |
| `(Restarting, *)` | `BudgetExhausted` | — | `Stopped` | `Log(error)` |

**Invariantes que el reconciliador mantiene tras cada tick** (comprobables como
`debug_assert!` y como test de propiedades):

1. `order[i] < order[j]` ⇒ `j` no empieza antes que `i` esté `Running`.
2. `State::Running` ⇒ existe proceso vivo con el pid registrado.
3. `Desired::Up ∧ State::Stopped ∧ budget_agotado` ⇒ no se reintenta hasta el siguiente tick de presupuesto.
4. Ningún servicio arranca sin que sus `required` dependencias estén `Running`.
5. `Optional` no bloquea: si una dependencia opcional falla, `j` arranca igualmente.
6. Nunca se envían dos signals a un servicio en el mismo tick.

### 4.2 Reconciliación — el bucle completo, sin colas

```rust
fn reconcile(&mut self, plan: &Plan) -> Vec<Action> {
    let mut out = Vec::new();

    // ── 1. Top-down: arranque ────────────────────────────────
    // Se recorre EN ORDEN. Si j depende de i, order[i] < order[j], así que
    // cuando llegamos a j, i ya está resuelto. Una sola pasada.
    for &idx in &plan.order_up {
        let want_up = self.desired[idx] == Up
            && self.deps_ready(idx)
            && self.budget[idx].allow();
        match (want_up, self.state[idx]) {
            (true,  State::Stopped) | (true, State::Restarting)
                if !self.budget[idx].exhausted() =>
                out.push(Action::Spawn(idx)),
            (true,  State::Stopped) => /* agotado: no */,
            _ => {}
        }
    }

    // ── 2. Bottom-up: parada ─────────────────────────────────
    // Orden INVERSO: si j depende de i, i no puede morir antes que j.
    for &idx in &plan.order_down {
        if self.desired[idx] == Down
            && matches!(self.state[idx], Running | Starting) {
            out.push(Action::Signal(idx, Signal::TERM));
        }
    }
    out
}
```

**No hay `prop_queue`. No hay `stop_queue`. No hay recursión. No hay `dep_depth`.**
El orden topológico ya lo dice todo. Dos pasadas, O(n), sin asignaciones intermedias
(usa `order_up`/`order_down` precalculados al cargar).

**Consecuencia directa:** el bug D-3 (`std::min` donde iba `std::max` en el cálculo de
profundidad) **no puede existir**, porque no hay cálculo de profundidad que hacer.
Ni D-5, ni D-4, ni D-2 en runtime: el grafo se validó al cargar y es inmutable desde entonces.

### 4.3 Presupuesto de reinicios (en unidades humanas)

dinit tiene `restart-delay = 0.2` + "3 reinicios en 10 s" repartidos por dos directivas.
Eso es un token bucket con los números escritos en dos sitios distintos.

zinit lo expone como lo que es:

```ini
restart-budget = 5 restarts per 60s
restart-delay  = 250ms      # mínimo entre reintentos
```

Internamente un token bucket. Al agotarse, el servicio pasa a `(Stopped, Up)` con
`budget_agotado`, **se queda quieto**, y `zctl kick <svc>` lo rearma. Registra en log.
Nunca hace spin.

---

## 5. Formato de configuración

Once directivas. Cualquier cosa más compleja → `type = script` y shell. Esa es la válvula
de escape que permite que el formato sea pequeño **y** que el conjunto sea completo.

```ini
# /etc/zinit/services.d/sshd.conf
command    = /usr/sbin/sshd -D -e
depends    = network.target
user       = root
restart    = on-failure          # never | on-failure | always
ready      = tcp:22              # none | notify | ping:<cmd> | tcp:<port>
stop-timeout = 10s
env        = PATH=/usr/bin:/bin SSH_LOG_LEVEL=INFO
```

| Directiva | Valores | Def. | Nota |
|---|---|---|---|
| `command` | línea de comando | — | **obligatoria** salvo en `type = target` |
| `type` | `process` \| `script` \| `target` \| `console` | `process` | `script` = shell; `target` = virtual; `console` = posee la tty |
| `depends` | lista separada por comas | — | sufijos: `.target`, `.optional` |
| `restart` | `never` \| `on-failure` \| `always` | `on-failure` | |
| `restart-budget` | `<n> restarts per <t>` | `5 restarts per 60s` | |
| `restart-delay` | duración | `250ms` | |
| `ready` | `none` \| `notify` \| `ping:<cmd>` \| `tcp:<port>` | `notify` | ver §6 |
| `ready-timeout` | duración | `30s` | al expirar ⇒ `Running` + warning |
| `stop-timeout` | duración | `10s` | ⇒ SIGKILL |
| `start-timeout` | duración | `60s` | |
| `user` | `user[:group]` | — | o `uid:gid` |
| `env` | `K=V`, repetible | — | soporta `${VAR:-def}` |
| `log` | `none` \| `file` \| `syslog` | `file` | destino `/var/log/zinit/<svc>.log` |
| `rlimit-nofile` \| `rlimit-nproc` \| `rlimit-as` | número | — | sólo donde exista |
| `cgroup` | ruta bajo `zinit.slice` | — | Linux; elsewhere ⇒ aviso |
| `type` += | `oneshot` \| `forking` | — | `oneshot` = éxito terminal; `forking` = adopta vía `pid-file` |
| `pid-file` | ruta | — | sólo `forking`; el pid real tras el doble fork |
| `watchdog-sec` | segundos ≥1 | — | sólo con `ready = notify`; `WATCHDOG=1` por el fd 3 |
| `listen` | `tcp:<puerto>[:<nombre>]` \| `unix:<ruta>`, repetible | — | pre-bind en 127.0.0.1; `$LISTEN_FDS/$LISTEN_PID/$LISTEN_FDNAMES` desde fd 3 |
| `drop-capabilities` | nombres (`sys_admin`…) | — | irreversible (`PR_CAPBSET_DROP`); Linux, si no ⇒ el spawn se rechaza |
| `syscall-filter` | `enforce` \| `errno` \| `off` | — | seccomp-bpf manual; Linux, si no ⇒ el spawn se rechaza |
| `syscall-allow` | nombres (`read`…) | — | sobre la base implícita; sin `syscall-filter` ⇒ error |

**Dos operadores, tres separadores, cero anidamiento.** `=`, `:` y `#`. Si el parser necesita
más de 300 líneas, se ha designing mal.

### 5.1 Los targets

```
/etc/zinit/services.d/network.target      # target vacío, no arranca nada
/etc/zinit/services.d/all.conf            # depends = *.target
/etc/zinit/services.d/boot.conf            # depends = all.target
```

Un `target` es un servicio virtual: no tiene proceso, es `Running` cuando todas sus
dependencias requeridas lo están. Ordena el arranque sin obligar a nadie a escribir una lista.

`all.target` — wildcard,dependencias de todo lo habilitado.
`boot.target` — lo mínimo para un login.

Esto cubre el runlevel 1/2/3 de runit **sin stages y sin pausar el sistema**, y sin el
concepto `target` que dinit tuvo y retiró.

---

## 6. Readiness — el punto donde se gana o se pierde

Es la diferencia entre un init usable y un init con carreras. runit no tiene ninguno;
dinit tiene `ready-notify` pero no ofrece `ping` ni `tcp`.

**La regla que hace que esto funcione en cualquier distro sin tocar un solo servicio:**

> El timeout de readiness **nunca bloquea el arranque**. Agotar `ready-timeout` produce
> `Running` con un warning en el log.

Un servicio que no implementa el handshake simplemente tarda `ready-timeout` y sigue.
Un script de shell existente funciona sin cambios. **Esa es la decisión de ergonomía
central del diseño**: la corrección es el valor por defecto, la espera es el opt-in.

| Adaptador | Mecanismo | Ejemplo típico |
|---|---|---|
| `notify` | zinit pasa fd 3 write-only, `ZINIT_NOTIFY_FD=3` en el entorno. El servicio escribe una línea. | nginx, postgres, systemd-style |
| `ping` | zinit ejecuta `ping:<cmd>` cada 250 ms hasta que salga 0 | runit `check`; **cualquier script shell** |
| `tcp` | `connect()` a 127.0.0.1:puerto hasta que acepte | servidores de red |
| `none` | `Running` en cuanto el fork tiene éxito | daemons que no se pasan a segundo plano |

También se honra `LISTEN_FDS`/`LISTEN_PID` (convención systemd) para socket activation, por
compatibilidad con servicios existentes.

---

## 7. Protocolo de control

**NDJSON sobre UNIX socket. Delimitado por `\n`. Sin prefijos de longitud. Sin buffer circular.**

```
→ C {id} status sshd
← R {id} 0 sshd running desired=up pid=421 up=4m12s restarts=2/5
→ C {id} stop sshd
← R {id} 0 down
→ C {id} reload-all
← R {id} 0 12 services
→ C {id} stop missing
← E {id} 3 unknown service `missing`
```

Congelado en v1 (`crates/zinit/src/ctl.rs`): `C {id} <verbo> [nombre]` por línea;
verbos `status | list | start | stop | restart | kick | reload-all | version | help`;
códigos `1` trama inválida, `2` comando/aridad desconocidos, `3` servicio
desconocido, `4` no aplicable, `5` recarga fallida. Sin apretón inicial
`zinit <n>`, sin `--wait`, sin `restart all`: un verbo por servicio, una línea
por trama. El resto de la lista de capacidades (`enable`, `console`,
`log-level`, `shutdown`, `catlog`, `wait`…) llega con su fase, no antes.

- **Texto**: conducible con `socat`/`nc`. Un bug de protocolo se depura sin herramientas.
- **Delimitado por `\n`**: no hay longitud que validar, ni `chklen`, ni desincronización
  de stream. **D-1, D-4 y D-5 son estructuralmente imposibles.**
- **Versionado**: primera línea `zinit <n>`. Sin él, no hay forma de detectar un cambio.
- **`{id}`** para multiplexar sin handles opacos ni tabla de ciclo de vida.

Capacidades: `status`, `start`, `stop`, `restart`, `reload`, `enable`, `disable`,
`list`, `console`, `log-level`, `shutdown`, `reboot`, `poweroff`, `halt`, `kill`, `kick`,
`catlog`, `wait`.

Sincronía: `zctl` espera por defecto; `--no-wait` devuelve tras el ACK. **Toda la
sincronía es del lado del cliente**, igual que en dinit, pero sin `PREACK` porque no hay
info-packets intercalados que filtrar.

**Autenticación**: `0600` en el socket, más `SO_PEERCRED`/`LOCAL_PEERCRED` **sí** (dinit
no lo hace en ninguna parte) para decidir entre socket de sistema y de usuario.

---

## 8. Portabilidad — la parte difícil

"Funciona en cualquier Linux y cualquier BSD" es una afirmación exigente. La diferencia
no está en el init, está en **cinco syscalls** que no existen igual en todas partes.

| Capacidad | Linux | FreeBSD/OpenBSD/NetBSD | Estrategia en zinit |
|---|---|---|---|
| Reactor | `epoll` | `kqueue` | Trait `Reactor` con 3 backends; `poll` como fallback universal |
| Seguimiento de hijos | `pidfd_open` (5.3+) | `kqueue` `EVFILT_PROC`/`NOTE_EXIT` | idem; fallback `waitpid(WNOHANG)` + `SIGCHLD` |
| Señalas como fds | `signalfd` | `kqueue` `EVFILT_SIGNAL` | **Las señales entran por el mismo reactor que todo lo demás.** Fallback: self-pipe |
| uid/gid | `setresuid`/`setresgid` | **no existen** | Detección en runtime: si hay `setresuid`, usarlo; si no, `setuid`+verificación explícita de `getuid()==geteuid()` |
| No-new-privs | `prctl(PR_SET_NO_NEW_PRIVS)` | — | Linux; no-op + aviso en BSD |
| ASLR off por servicio | `personality(ADDR_NO_RANDOMIZE)` | — | Linux; no-op + aviso |
| Capabilities | `libcap` (CGO) | — | Linux; en BSD el modelo es uid/gid, sin fallo |
| cgroups | v2 (unificada), v1 (legacy) | jails (v2) | Linux: v2 con fallback v1. BSD: no-op + aviso |
| `/proc` | sí | **no montado por defecto** | **No se depende de `/proc` en ninguna parte.** Cualquier cosa que lo necesite va detrás de `cfg.has_proc()` |
| Reinicio | `reboot(LINUX_REBOOT_CMD_*)` | `reboot(RB_*)` | **Constantes distintas.** Tabla por plataforma, no un `if`. |
| `/run` | tmpfs estándar | `/var/run`, a veces nada | Path configurable; se crea tmpfs si se puede, se acepta dir existente si no |
| Aislamiento | `unshare` | `jail(2)` (FreeBSD) | **Honesto: no hay portabilidad posible.** Se ofrece `type = script` con la herramienta nativa. |
| Entropía | `getrandom()` (3.17+) | `arc4random()` | Con fallback a `/dev/urandom`; sembrado antes de arrancar nada que la necesite |

### 8.1 El principio de la degradación explícita

```rust
pub struct Capabilities {
    pub reactor:        ReactorKind,   // Epoll | Kqueue | Poll
    pub child_tracking: ChildKind,     // Pidfd | KqueueProc | Waitpid
    pub signal_fd:      bool,          // signalfd o kqueue EVFILT_SIGNAL
    pub setresuid:      bool,
    pub no_new_privs:   bool,
    pub personality:    bool,
    pub capabilities:   bool,          // libcap
    pub cgroup:         Option<CgroupVer>,
    pub procfs:         bool,          // sondeado en runtime, no asumido
    pub reboot_style:   RebootStyle,   // LinuxReboot | BSDReboot
}
```

Se detecta en el arranque (no en tiempo de compilación, para que un mismo binario sea
correcto en cualquier sitio), se **imprime en el log** y en `zctl version`, y cada sitio que
la use dice qué hace cuando no está. Nada de `unimplemented!()`.

### 8.2 Sobre "cualquier distribución"

Un init no puede *garantizar* eso, y decirlo sería mentir. Lo que sí se garantiza, y es
lo que importa:

- **No depende de la distribución.** No lee `/etc/*release`, no asume systemd, no asume
  OpenRC, no asume que exista `/proc`, no asume `/run`, no asume `libcap`, no asume
  `personality`, no asume cgroups. Todo eso son `Capabilities`.
- **Cero dependencias de terceros** (sólo `libc`). Sin `pkg-config`, sin `.pc` que falten,
  sin versiones de runtime.
- **Detección en runtime, no de compilación.** Un binario para Linux y un binario para BSD,
  el mismo código, decisiones distintas en caliente.
- **CI desde el día 1** en al menos: Debian (glibc, i386 y amd64), Alpine (musl),
  Void (glibc), Fedora, Arch, macOS si se soporta, FreeBSD, OpenBSD, NetBSD.
  Y con `_GLIBCXX_DEBUG` en debug, igual que dinit.
- **El contrato de configuración es estable.** Un servicio que funciona con zinit
  funciona mañana aunque cambie el runtime. Las *distribuciones*van y vienen; el
  *formato* no.

---

## 9. Seguridad y drop de privilegios

Lo mejor que hace dinit, copiado sin excusas. El orden lo dicta la seguridad, no la comodidad.

```
en el SUPERVISOR (antes del fork)          en el HIJO (después del fork)
──────────────────────────────────────     ──────────────────────────────────────
preparar argv/env punteros                setgroups(0, NULL)     ← Linux y BSD
preparar buffers de /proc/cmdline         setresgid  (o setgid+verificar)
no asignar, no tomar locks                setresuid  (o setuid+seteuid+verificar)
                                           → verificar getuid()==geteuid()==uid
cgroup.create("zinit.slice/nombre")        → es el ÚLTIMO paso con privilegio
place(child_pid)                           
fijo: fds de log, fds de notify,           cerrar todo lo demás
       socket de control                   personality(ADDR_NO_RANDOMIZE)  [Linux]
                                           prctl(NO_NEW_PRIVS)           [Linux]
                                           fchown/fchmod sobre el fd     [no TOCTOU]
                                           cap_set_proc / cap_set        [Linux/libcap]
                                           setsid + TIOCSCTTY            [console]
                                           execvp
```

**Reglas duras, aprendidas de la auditoría:**

1. **Nunca `seteuid` solo.** Es la trampa clásica: deja el permitted-set de capabilities.
   dinit usa variantes reales; eso se conserva.
2. **Verificar después de soltar.** `setuid(n)` puede no haber hecho lo que crees en un
   sistema con peculiarities. `getuid()`/`geteuid()`/`getgid()` y abortar si no coinciden.
3. **cgroup antes del drop.** Requiere privilegio; si se hace después, falla en silencio.
4. **`fchown`/`fchmod` sobre el fd abierto, nunca por ruta.** TOCTOU.
5. **Cero stdio en PID 1 y en la ruta de apagado.** Los errores van al log, y si el log no
   existe, se intento una vez y se sigue.
6. **`RLIMIT_NOFILE` seteado en el padre antes del fork**, no en el hijo: es una de las pocas
   cosas que `RLIMIT_INFINITY` en el hijo no puede arreglar.

### 9.1 El problema real: `fork()` en Rust

Esto es **el** desafío técnico del proyecto y hay que decirlo claro.

Después de `fork()`, en el hijo, sólo se pueden llamar funciones async-signal-safe.
Rust no da garantías de eso automáticamente, y `panic = "abort"` ayuda pero no resuelve
todo: un `Vec::push` en el hijo puede abortar el proceso, y abortar un hijo de PID 1 que
está arrancando un servicio crítico es malo.

Mitigaciones, todas en el diseño:

1. **El hijo post-fork no asigna.** Todo (`argv`, `envp`, buffers) se prepara **antes** del
   fork y el hijo sólo las referencia.
2. **El camino post-fork es un `#[inline(never)] fn` propio, con una sola salida: `execvp`.**
   Si falla ⇒ `_exit(127)`. Nunca hay retorno al código normal.
3. **`panic = "abort"`** en el perfil de release, y ningún `unwrap()` en ese camino.
4. **Smoke test real**: `zinit --selftest-spawn` arranca 200 hijos bajo ASan/Valgrind y
   verifica que ninguno aborta.
5. **Límite de intents**: en el estado `Starting`, un fallo de spawn es un evento
   (`ProcessGone{code:127}`) que el reconciliador maneja como cualquier otro crash.
   Nunca una pérdida silenciosa.

### 9.2 Aislamiento: honestidad

`unshare(CLONE_NEWPID|CLONE_NEWNS|CLONE_NEWNET)` en Linux; `jail(2)` en FreeBSD; nada
equivalente en OpenBSD/NetBSD. **No existe una abstracción honesta aquí** y no la vamos
a inventar.

Lo que sí se ofrece, en todas partes:
- `type = script` con la herramienta nativa del sistema (`systemd-run`, `jail`, `chroot`).
- `cgroup` en Linux para límites de recursos.
- Drop de privilegios para el caso más común (servidor que no necesita root).
- `depends` para Secuencia **correcta**, que es el 90% del aislamiento que la gente
  realmente quiere cuando empieza.

---

## 10. Logging

**Decisión: hacer el 5% de svlogd, no el 100%.**

svlogd son 863 líneas con máquina de estados, `processor`, `pmatch` y tres formatos de
timestamp. La rotación es lo único que de verdad hace falta.

```rust
enum LogSink {
    None,
    File { path: PathBuf, max_bytes: u64, backups: u8 },
    Syslog,                    // unix socket a syslogd/journald
}
```

- Default: fichero en `/var/log/zinit/<svc>.log`.
- Rotación: cuando supera `max_bytes` (default 1 MiB), `rename` a `.1`, y al existir `.1`,
  lo oldest se descarta. **20 líneas, sin estado, sin tabla de estados.**
- Escritura: cada línea del hijo se escribe con un `write(2)` al fd. Sin buffer, sin
  `BufWriter` que pueda perder datos en un `SIGKILL`.
- `zctl catlog <svc>` — `tail -f` sobre el fichero. La integración con journald
  (`log-type = pipe` + `consumer-of`) es una **feature separada** para v2, no parte del núcleo.

---

## 11. Testing

dinit tiene 171 tests y **ninguno ejecuta un proceso real** (`test-run-child-proc.cc`
anula `run_child_proc`). Es un fallo de diseño de la suite, no un descuido: la clase de
bugs más importante de un init es la que ocurre entre `fork` y `exec`, y esa clase no la
toca nadie.

### 11.1 Tres capas

**a) `zcore` — nucleo puro.** Tests exhaustivos, sin I/O.
- Todas las transiciones de la tabla §4.1, como tabla de tests.
- **Property tests**: generador de secuencias arbitrarias de `Event` y.check de los 6
  invariantes tras cada paso. Esto encuentra bugs de máquina de estados que ningún test
  escrito a mano encuentra.
- **Property test clave**: *"para todo orden topológico y toda secuencia de eventos,
  el estado final es alcanzable"* — prueba que el reconciliador converge y no oscila.

**b) `zservice` — parser.**
- Fuzz target sobre el parser de configuración (formato plano, blando, fuzzible bien).
- Fuzz target sobre el parser del protocolo NDJSON.
- Tests de tabla: cada directiva, cada valor, cada error.
- Detección de ciclos con el algoritmo de Kahn, con casos patológicos (auto-dep,
  A→B→A, A→B→C→A, diamonds, 10 000 nodos).

**c) Integración — con procesos REALES.** Esto es lo que falta en dinit.
- `zinit` arranca en un namespace (`unshare -pfm` o `jail`) con un servicio de prueba real.
- Se verifica **el servicio real**: está en la tabla de procesos, responde al puerto,
  recibe SIGTERM, muere al SIGKILL tras `stop-timeout`.
- Tests de carrera: matar el supervisor y verificar que PID 1 lo relanza y re-engancha
  **sin tocar los servicios vivos**.
- Tests dePortabilidad **en la CI real**: los mismos tests se ejecutan en los 8 sistemas.

### 11.2 Regla

> Un test que no puede detectar un fallo en el arranque real de un proceso real no cuenta
> como test de init. Se mide así o no cuenta.

---

## 12. Layout de ficheros

```
/etc/zinit/
    zinit.conf              # socket, usuario por defecto, límites globales
    services.d/*.conf       # descripciones de servicio
    zinit.d/*.conf          # drop-in que sobrescribe services.d
    scripts.d/              # scripts de shell equivalentes a los stages 1/2/3
/etc/default/zinit          # knob de paquete (sysv, por compatibilidad)

/run/zinit/                 # tmpfs: socket, pidfiles, estado
/var/log/zinit/*.log
/usr/lib/tmpfiles.d/zinit.conf
```

**Compatibilidad sysv**: `zctl` y un shim `sv` que traduce los argumentos de `sv` más usados
(`up`, `down`, `status`, `restart`, `term`) a `zctl`. Coste: 60 líneas de shell. Beneficio:
los scripts de runit existentes se leen sin reescribir.

Y al revés, opcional: `zinit import-runit /etc/service > /etc/zinit/services.d/`
convierte un árbol de runit. Convierte `depends` heurísticas en `.target` documentadas a mano,
no mágicas — la translator **avisa** de lo que no puede convertir.

---

## 13. Crates

```
zinit/
├── Cargo.toml                 # workspace, resolver = 2, MSRV documentado
├── crates/
│   ├── zcore/                 # SIN dependencias, SIN I/S. La máquina de estados.
│   ├── zconfig/               # parser de descripciones, el grafo, el plan, el validador
│   ├── zrt/                   # libc: reactor×3, señales, fork/exec, mount, cgroup, utmp
│   ├── zservice/              # un servicio: ciclo de vida, readiness, rlimits, spawn
│   ├── zctl/                  # CLI: una trama dentro, una línea fuera (implementado)
│   ├── zcheck/                # zinit check — valida sin arrancar (mismo parser, no puede divergir)
│   └── zinit/                 # binario: `--init` (PID 1) y `--sup` (supervisor)
└── docs/
    ├── DESIGN.md              # este documento
    ├── PORTABILITY.md         # tabla syscall×plataforma, con el razonamiento
    └── MIGRATING.md           # desde runit, desde dinit, desde systemd
```

**Dependencias totales: `libc`. Nada más.**

Cero `tokio`, cero `async`, cero `clap`, cero `serde`. El event loop son 200 líneas de
`epoll_wait`/`kqueue`, y el parser son 300 líneas. Las dependencias son deuda de
mantenimiento y un init no puede permitírselas: se compila en el hueco de
`/etc/runit/1` mientras arranca el sistema.

---

## 14. Fases

| Fase | Entregable | Condición de salida |
|---|---|---|
| **0** | `zcore` + `zconfig` + tests de propiedades | 6 invariantes pasan sobre 10⁶ secuencias aleatorias; parser fuzzed 1 h sin crashes |
| **1** | Reactor Linux (`epoll`+`pidfd`+`signalfd`), `--init`+`--sup`, un `type = process` | arranca un servicio real y lo supervisa; el test de integración pasa |
| **2** | `kqueue` + BSD | los tests de integración pasan en FreeBSD, OpenBSD y NetBSD |
| **3** | `zctl`, protocolo, `zcheck`, readiness completo | 100% de los tests de §11 verdes en los 8 sistemas |
| **4** | consola, targets, scripts.d, `sysv`, `import-runit` | un sistema real arranca y se apaga con login en tty |
| **5** | endurecimiento: fuzzing largo, audit de seguridad, docs | sin hallazgos abiertos de severidad media+ |

**Regla de la fase 0**: si la máquina de estados pura no se puede hacer impecable,
no se empieza el runtime. Todo lo demás es Easier que acertar en el modelo.

---

## 15. Riesgos, con honestidad

| Riesgo | Gravedad | Mitigación |
|---|---|---|
| **`fork()` en Rust post-fork no es async-signal-safe** | Alta | §9.1: cero asignación, un solo camino, `panic=abort`, smoke test con ASan |
| **"Cualquier BSD" es más largo de lo que parece** | Alta | 3 kqueue backends, no 1. CI en BSD desde la fase 1, no al final |
| **Detección de capacidades mal hecha ⇒ comportamiento distinto en silencio** | Alta | §8.1: se imprime al arrancar, `zctl version` lo expone, todo uso tiene rama explícita |
| **Modelo declarativo sorprende** (`zctl stop` y luego `desired` vuelve a `Up`) | Media | `zctl stop` es un *statement*, no un cambio de desired; los targets explícitos hacen el arranque determinista |
| **Ecosistema: nadie conoce otro init** | Media | shim `sv`, `import-runit`, docs de migración, y por encima de todo: `zcheck` que explica el error con el fichero y la línea |
| **Scope: consola, cgroups, jail, sysv** | Media | Todo eso es post-fase-3 y cada pieza es independiente. Un init v1 sin cgroups es útil |
| **Un init más en el mundo** | — | La contribución no es otro init, es la **semántica de reconciliación** aplicada a init, que es la parte que ni runit ni dinit han hecho |

---

## 16. La decisión, en dos líneas

runit es un esqueleto de 200 líneas al que le falta el 40% (dependencias, readiness,
verificación). dinit es un sistema completo con un modelo de datos correcto y 20 bugs
en el runtime. **zinit toma el modelo de datos correcto de dinit, lo reduce a un
reconciliador sin grafo mutable, toma la robustez de proceso de runit pero con un
supervisor reiniciable, usa un protocolo que no puede desincronizarse, y no depende de
nada que la plataforma pueda no tener.**

La frase que resume el diseño:

> **No un grafo que se muta en runtime, sino un plan ordenado inmutable y un reconciliador
> que converge al estado deseado.**

---

*Basado en el análisis de 32 000 líneas de runit (2.3.1+) y dinit (0.23.0pre) con 44 bugs
confirmados. Referencias concretas a cada decisión en las secciones 1.1-1.4.*
