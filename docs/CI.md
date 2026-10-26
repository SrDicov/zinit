# CI: qué es el gate y qué es la red

Dos workflows, dos contratos distintos. Confundirlos es cómo se acaba subiendo
un timeout para tapar un cuelgue.

| workflow | cuándo | qué decide |
|---|---|---|
| `ci.yml` | push a `main`, PR | **El gate.** build, clippy `-D warnings`, fmt, tests en la matriz de targets, fuzz/property/security, y un job `privileged` con los caminos que necesitan root. Un rojo aquí bloquea el merge. |
| `distro.yml` | push a `main`, PR, manual | Red de portabilidad: el mismo suite en 7 imágenes x86_64. Un rojo aquí **no** bloquea el merge, pero cada rojo es un hallazgo que se documenta o se arregla. |

## Los 7 legs y la superficie que cubre cada uno

| leg | imagen | qué aporta de distinto |
|---|---|---|
| `void-glibc` | `void-linux/void-glibc` | glibc + xbps + GNU coreutils: el entorno del autor |
| `void-musl` | `void-linux/void-musl` | mismo layout con musl: separa "musl" de "alpine" |
| `alpine` | `alpine:latest` | musl + busybox: el que más rompe (`ps`, `sh`, `nobody`) |
| `chimera` | `chimeralinux/chimera` | clang+musl **sin gcc**: toolchain distinta, `cc` inexistente |
| `arch` | `archlinux:latest` | glibc + pacman + coreutils GNU recientes |
| `eweos` | `ghcr.io/eweos/docker:master` | musl + pacman (Arch derivado): combina las dos anteriores |
| `gentoo` | `gentoo/stage3` | stage3 mínimo, sin `curl` ni `gcc` preinstalados |

Cada fila ha cazado algo real (el `/bin/true` de macOS fue el mismo tipo de
suposición en otra plataforma). Por eso la matriz no se recorta a 4 sin una
medición que lo justifique: los runners corren **en paralelo**, y el coste de
un leg extra es un runner, no tiempo de reloj.

## Por qué no hay `actions/cache`

- El workspace tiene **una** dependencia de terceros: `libc`. No hay sources
  que cachear.
- El `target/` compartido entre variantes de libc es veneno: objetos glibc
  enlazados en un binario musl. El propio workflow lo documenta.
- El toolchain se instala dentro del contenedor (`$HOME/.cargo` no es una ruta
  del host), y un toolchain glibc no ejecuta en musl. Lo que se cachearía no
  se puede reutilizar.
- Lo caro es la descarga del toolchain por imagen, y eso es precisamente lo
  que no se comparte.

Medido: un leg completo (prep + rustup + build + 633 tests) son minutos, no
decenas. El 2026-10-23 los 7 legs agotaron `timeout-minutes: 45` y **no era
lentitud**: cuatro tests se colgaban para siempre y el `Mutex` serial
arrastraba a tres más. Sin ese cuelgue, el suite entero en musl tarda ~65 s.

## Si un leg se cuelga

1. Buscar `running for over 60 seconds` en el log del job: nombra al culpable.
2. El culpable tiene una espera sin deadline: `connect(2)`, `Command::output()`
   o un `read_line` sobre un socket. El fix es un deadline en el test, con el
   estado observado en el mensaje.
3. Nunca subir `timeout-minutes` para "arreglarlo". El timeout solo decide
   cuánto tarda en verse el fallo.

## El `cargo build --workspace` no es redundante

`crates/zinit/tests/` ejecuta el `zctl` **real** desde el directorio de
target. `cargo test --workspace` no lo produce (el crate `zctl` no tiene
targets de test propios), así que sin un build previo las pruebas de socket se
saltan solas por falta de binario — y un skip silencioso en CI es peor que un
fallo. El build es de `--workspace` a secas: `--all-targets` compilaba los
harnesses dos veces para nada.

## El job `privileged`

Los caminos de confinamiento (crear un cgroup, montar un tmpfs privado,
`unshare -m`, instalar el filtro seccomp) se **saltan** en los legs de
`distro`: dentro de `docker run` sin `CAP_SYS_ADMIN` el `unshare -m` es EPERM
y el cgroup es de solo lectura. Eso es honesto, pero durante meses significó
que ese código no tenía ninguna evidencia en CI.

El job `privileged` de `ci.yml` corre el suite completo en un contenedor
`--privileged` como root, con `--include-ignored` para el test que pide root
real (`setgroups` dentro de un user namespace está denegado). Ahí sí se
ejecutan `a_service_with_a_cgroup_lands_in_it`, `the_sandbox_really_confines`,
`a_private_tmp_covers_the_scratch_directories` y
`zinit_boots_as_pid_one_in_a_namespace`; medido: **658 tests, 0 fallos** en ese
contenedor frente a 656 + 1 ignorado en un runner normal.

Dos límites siguen siendo del entorno, no del código: dentro de un user
namespace que no posee un network namespace no se puede montar `sysfs` (PID 1
lo anuncia y sigue), y el contenedor no es una máquina real — el soak y los
caminos de apagado siguen pidiendo hardware.

## Fallos de entorno, no de código

Dentro de `docker run` sin `--privileged` no hay `CAP_SYS_ADMIN` y el cgroup
del host puede ser de solo lectura. Los tests que lo necesitan escriben un
fichero `skipped` con la razón y pasan, igual que hacen con "needs root". Un
test que falla porque el contenedor no puede montar un tmpfs no prueba nada
sobre zinit: o se salta con razón, o se ejecuta en un job privilegiado.

El artefacto `zinit-alpine-debug` del leg `alpine` es el binario real que se
mide en una cage; el resto de legs solo compilan y corren tests.
