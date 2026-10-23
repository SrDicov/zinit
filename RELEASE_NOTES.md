# v0.2.0-alpha

Primera alpha pública de **zinit**: un init con un solo concepto — *estado
deseado* — y un reconciliador que lo cumple. Sin shell para la lógica, sin
grafos mutables en runtime.

## Estado

**Alpha.** Usable y con límites escritos, no con límites supuestos.

- **633 tests** en el workspace, 0 fallos, 1 ignorado (`child_step_drop_ids_as_root`,
  sólo con root real).
- Mutantes medidos, no estimados: `zcore` 15,2 % de supervivientes (31/204),
  `zconfig` 14,9 % (116/776), `zservice` con los baratos a cero y los caros
  documentados por archivo y con coste.
- Matriz de CI: 32/64 bits, x86/ARM/RISC-V, glibc/musl. `riscv32gc` queda fuera
  por falta de `std` en Rust (issue #19).
- Arrancado como PID 1 en VM y contenedores; handoff en vivo dinit→zinit
  verificado. **Soak en hardware real pendiente** — eso es beta, no alpha.

Lo que falta y lo que se sabe roto: [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md).
La evidencia del cierre: [`docs/ACTA_ALPHA.md`](docs/ACTA_ALPHA.md).

## Qué trae

- `depends = after:x` ordena sin bloquear (semántica dinit) y `on-fatal`
  (`ignore|warn|halt|reboot`) impide que un fallo terminal tumbe el arranque.
- seccomp con argumentos (no sólo números de syscall) y `capdrop` antes del
  cambio de uid.
- `private-tmp` con namespace de montaje en los tres directorios que lo piden.
- Señales BSD, "power verbs" autorizados por política, y el generador
  dinit→zinit (`z-dinit-generator`, repo aparte) con sus huecos documentados
  uno por uno en `docs/audit/generator-gaps.md`.
- Rotación de logs, readiness real (exec, sleep, tcp, notify) y socket de
  control de una trama por línea.

## Instalación

Desde el release: un `.tar.gz` estático musl por target (`x86_64`, `i686`,
`aarch64`, `armv7`, `riscv64`), con `install.sh` y `.sha256` al lado.

```sh
sha256sum -c zinit-v0.2.0-alpha-x86_64-unknown-linux-musl.tar.gz.sha256
tar xzf zinit-v0.2.0-alpha-x86_64-unknown-linux-musl.tar.gz
sudo ./install.sh
```

Desde fuente:

```sh
cargo build --release          # zinit + zctl
```

Límite del proyecto: `zinit` ≤ 1 MiB, medido por el CI.

## Migración

Desde dinit, runit o systemd: [`MIGRATING.md`](MIGRATING.md). Los nombres de
directivas que no existen se rechazan con diagnóstico y span, no se ignoran.

## Cómo reportar

Issues en <https://github.com/SrDicov/zinit/issues> con la salida de
`zinit check` y `zinit status`. Un fallo de arranque sin traza no es
reproducible; con `RUST_BACKTRACE=1` sí.
