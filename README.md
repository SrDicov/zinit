# zinit

> Un init con **un solo concepto** — *estado deseado* — y un reconciliador que lo cumple.
> Sin shell para la lógica, sin grafos mutables en runtime, sin acuses que falten.

**Estado**: núcleo, configuración y servicio cerrados. **504 tests, 0 warnings de clippy,
`cargo doc` limpio.**

| Crate | Qué es | Estado | Tests |
|---|---|---|---|
| `zcore` | Máquina de estados pura. `no_std`, sin E/S, **cero dependencias**. | ✅ | 47 + 10 props |
| `zconfig` | Parser de descripciones, grafo, plan congelado. | ✅ | 286 |
| `zrt` | `libc`: reactor ×3, señales, seguimiento de hijos, detección de capacidades. | ✅ | 51 |
| `zservice` | Ciclo de vida, readiness, identidad, rotación de logs. | ✅ | 73 |
| `zinit` | Binario: `--init` (PID 1) y `--sup` (supervisor) | ✅ | 20 + 7 integración |
| `zctl` | CLI sobre el socket de control (una trama dentro, una línea fuera) | ✅ | 4 |
| `zcheck` | `zinit check` — valida sin arrancar | ✅ | 6 |

Los binarios son `zinit` y `zctl` (los únicos `[[bin]]` del árbol). El *size
budget* de CI mide `target/release/zinit` (≤ 1 MiB); `zctl` no entra en el
presupuesto porque nunca corre como PID 1.

El supervisor (`zinit --sup`) ya conecta el reactor de `zrt` con el
reconciliador de `zcore`, sirve el socket de control y vuelca el estado visible.

## Deploy para testing en hardware/VMs

```sh
# 1. Build + package (local)
./scripts/zinit-deploy.sh build

# 2a. Deploy via SSH/SCP a un host o VM
./scripts/zinit-deploy.sh deploy <host> [user]

# 2b. O lanzar directamente en QEMU con zinit como PID 1
./scripts/zinit-deploy.sh qemu <disk.img> [kernel]
```

El binario estático musl también se produce en CI (job `release-musl`) y queda
disponible como artifact de 30 días en cada push.

## Bugs de producción que la integración destapó

Cuatro de ellos eran invisibles sin los tests de integración, y los cuatro eran reales:

- **`set_nonblocking` borraba `O_CLOEXEC`.** `F_SETFL` *reemplaza* la palabra de flags, así que
  pasar un `O_NONBLOCK` desnudo limpiaba todo lo demás. Todo descriptor marcado no-bloqueante
  quedaba heredable y se filtraba a cada `exec` posterior.
- **`setsid` con `EPERM` mataba el arranque.** Un `fork` recién hecho *es* líder de grupo cuando
  el supervisor arrancó desde una shell que se hizo líder — siempre, bajo `cargo test`. El hijo
  hacía `_exit(127)` y el servicio no arrancaba, intermitentemente, por una razón que ningún log
  explicaría. Ahora cae a `setpgid(0, 0)`.
- **Expansión `$VAR` en el padre para `type = script`.** Un script que usaba su propio bloque
  `env` tenía las variables consumidas por el supervisor, donde no están definidas, antes de que
  `/bin/sh` las viera. Los scripts ahora entregan la orden al shell tal cual.
- **`pgid` predicho reportado como hecho.** Entre el `fork` y el `setsid` del hijo el grupo no
  existe, así que un `kill(-pgid)` inmediato era un no-op silencioso: un servicio que se colgara
  ahí sobreviviría al `stop-timeout` y a cada `SIGKILL` posterior.

El diseño completo está en [`DESIGN.md`](DESIGN.md). Este README es el estado y el índice.

## La idea

El runtime guarda un único par por servicio:

```rust
desired: Up | Down     // lo que quiere el operador
state:   Stopped | Starting | Running | Stopping   // lo que es cierto
```

Todo lo demás es **convergencia**. No hay estado `Restarting`, ni cola de reinicios, ni grafo
mutable: un servicio que crashea simplemente ya no está `Running` mientras `Desired::Up` no ha
cambiado, y el reconciliador lo vuelve a levantar si el presupuesto lo permite.

El grafo de dependencias se **congela en un array ordenado topológicamente** al cargar, así que
"¿está mi dependencia listo?" es `order[dep] < order[i] && state[dep] == Running`. Sin recursión,
sin dos colas de propagación, sin contador de profundidad.

## El núcleo es puro, y eso es mecánico

```toml
[dependencies]
# INTENTIONALLY EMPTY. Ni siquiera libc.
```

`zcore` no puede abrir un fichero, llamar a un syscall, leer el reloj ni dormir. Recibe
`now_ms` como parámetro y devuelve `Vec<Action>`. Está forzado por `#![no_std]` +
`#![forbid(unsafe_code)]`, no por disciplina.

La razón es que los fallos de un supervisor son forma de carrera, y una carrera no se alcanza
escribiendo tests a mano: se alcanzan lanzándole interleavings aleatorios y comprobando que
nunca produce un estado imposible. Eso sólo tiene dónde correr si el núcleo no hace E/S.

## Arquitectura

```
PID 1: zinit --init      reaper incondicional, signal handling, vigilar al supervisor
  └─ zinit --sup         reactor, plan, reconciliador, socket de control
       └─ servicios      un proceso por cada uno
```

El supervisor es **hijo de PID 1**, no PID 1. Si revienta, PID 1 lo relanza y re-engancha a los
servicios vivos leyendo pidfiles — y re-enganchar es trivial *precisamente porque el plan es
inmutable*. Eso elimina por construcción la clase de fallo "un bug aquí y no arranca".

## Estructura

La tabla de arriba es el estado. Ésta es sólo el índice de módulos, y `zrt` no tiene
`cgroup`, `utmp` ni nada más: son `capdetect`, `childproc`, `clock`, `reactor`, `report`,
`signals`, `sys`.

## Compilar y testear

```sh
mise use rust@stable        # toolchain user-local
cargo test                  # todo
cargo test -p zcore         # el núcleo
cargo clippy --all-targets
```

## Bugs que los tests encontraron

El valor de partir el núcleo de esta forma se mide en lo que los tests de propiedades atraparon
antes de que existiera una sola línea de runtime. Los cuatro eran reales:

1. **El `restart-delay` se aplicaba al primer intento.** Medía el retardo desde la creación del
   cubo en vez de desde el intento anterior, así que con el presupuesto por defecto de 250 ms un servicio
   no podía arrancar nunca tras un fallo.
2. **`Ready::Strict(..)` no contaba como handshake.** `is_handshake()` no lo incluía, así que
   nunca se le armaba el deadline: la readiness estricta era un no-op silencioso.
3. **`restart = never` relanzaba igual.** El reconciliador consultaba el presupuesto pero nunca
   la política, así que un servicio configurado como "no reiniciar" se reintentaba en bucle.
4. **Contabilidad de presupuesto desplazada una unidad.** El crash consumía un token *y* el
   spawn exigía otro, de modo que un presupuesto de 1 permitía **cero** reinicios.

Ninguno de los cuatro habría salido de un test escrito a mano. Es el argumento completo para el
`zcore` tal como está.

## Bugs que este diseño elimina por construcción

Del análisis de 32 000 líneas de runit y dinit (44 bugs confirmados):

| Bug ajeno | Por qué no puede ocurrir aquí |
|---|---|
| dinit: `std::min` donde iba `std::max` en 3 sitios ⇒ `MAX_DEP_DEPTH` inerte desde siempre | No hay cálculo de profundidad: el orden topológico ya lo dice todo |
| dinit: `outbuf_size += pkt.size()` tras `std::move` ⇒ conexiones mudas en PID 1 | Protocolo delimitado por `\n`; no hay longitud que validar |
| dinit: `validate_service_name` con `return true` prematuro ⇒ permite `../` | Sin validación de nombres: se indexa por `Idx`, no por cadena |
| dinit: `sleep(1)` dentro del event loop | `now_ms` es un parámetro; el núcleo no duerme |
| runit: `execve` fallido sale con código 0 ⇒ `/etc/runit/2` no ejecutable enciende el apagado | Toda llamada al sistema devuelve `Action` o `Result`; nunca se sale por error |
| runit: `chmod` sin comprobar el retorno | El runtime comprueba todo; el núcleo no puede fallar |
| runit: `sv ok foo` ejecuta `o` en silencio | El readiness se configura explícitamente y el timeout nunca bloquea |

## Licencia

GPL-3.0-or-later (`Cargo.toml`, `LICENSE`). La única dependencia, `libc`, es MIT — compatible,
y el job `licence` de CI falla el build si deja de serlo.

## Contribución

Nota: el proyecto upstream del que procede el diseño **prohíbe explícitamente** contribuciones
de código o documentación producidas por modelos de lenguaje. Ese proyecto es dinit. zinit es
independiente y no hereda esa norma, pero conviene tenerlo presente si se deriva código de él.
