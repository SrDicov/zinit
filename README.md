# zinit

> Un init con **un solo concepto** — *estado deseado* — y un reconciliador que lo cumple.
> Sin shell para la lógica, sin grafos mutables en runtime, sin acuses que falten.

**Estado**: fase 0 y la capa de runtime cerradas. **358 tests, 0 warnings.**

| Crate | Qué es | Estado | Tests |
|---|---|---|---|
| `zcore` | Máquina de estados pura. `no_std`, sin E/S, **cero dependencias**. | ✅ | 49 |
| `zconfig` | Parser de descripciones, grafo, plan congelado. | ✅ | 272 |
| `zrt` | `libc`: reactor ×3, señales, seguimiento de hijos, detección de capacidades. | ✅ | 37 |
| `zservice` | Ciclo de vida de un servicio, readiness, rlimits | ⏳ fase 1 | |
| `zinit` | Binario: `--init` (PID 1) y `--sup` (supervisor) | ⏳ fase 1 | |
| `zctl` | CLI | ⏳ fase 3 | |
| `zcheck` | `zinit check` — valida sin arrancar | ⏳ fase 3 | |

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

| Crate | Qué es | Estado |
|---|---|---|
| `zcore` | Máquina de estados pura. Sin E/S, sin deps. | ✅ 49 tests |
| `zconfig` | Parser de descripciones, grafo, plan congelado. | 🚧 integración |
| `zrt` | `libc`: reactor ×3, señales, fork/exec, mount, cgroup, utmp | ⏳ fase 1 |
| `zservice` | Ciclo de vida de un servicio, readiness, rlimits | ⏳ fase 1 |
| `zctl` | CLI | ⏳ fase 3 |
| `zcheck` | `zinit check` — valida sin arrancar | ⏳ fase 3 |
| `zinit` | Binario: `--init` y `--sup` | ⏳ fase 1 |

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

Apache-2.0.

## Contribución

Nota: el proyecto upstream del que procede el diseño **prohíbe explícitamente** contribuciones
de código o documentación producidas por modelos de lenguaje. Ese proyecto es dinit. zinit es
independiente y no hereda esa norma, pero conviene tenerlo presente si se deriva código de él.
