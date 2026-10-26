# STATE — cierre de alpha (`v0.2.0-alpha`)

Actualizado: 2026-10-23, sesión de cierre. Rama de trabajo:
`agent/overnight-20261022-0332`; `testing` remoto = `9f3d017`.

## Hecho en esta sesión

| paso | commit | qué |
|---|---|---|
| T2 | `79a3fca` | `child_step_drop_ids_as_root` (`#[ignore]`, root real) |
| T2 | `852d3ec` | `docs/audit/mutants-{zconfig,zservice}.md`: los 268, por archivo, con coste |
| CI | `3d308bd` | `tgkill` por `syscall(SYS_tgkill)` (musl no lo declara en el crate) |
| CI | `beb1898` | `mut` innecesario que `-D warnings` rechazaba en toda la matriz |
| T4 | `9f3d017` | versión `0.2.0-alpha` (workspace + `zinit` + lock) |
| T3 | sin commit | `docs/ACTA_ALPHA.md` (criterio 1 pendiente del CI) |
| T7 | sin commit | `README.md` (badge + estado), `CHANGELOG.md` (`v0.2.0-alpha`), `RELEASE_NOTES.md` |

Local: `cargo test --workspace` 573 tests, 0 fallos, 1 ignorado;
`clippy -D warnings` y `fmt --check` limpios; `check --target musl` OK.

## Punto exacto

Esperando el run `39989519802` (`ci`) de `9f3d017`. **12 jobs en failure**
(msrv, aarch64-glibc, macos, x86_64-{glibc,musl}, i686-musl, aarch64-musl,
nightly, drivers, riscv32gc) y el run sigue en progreso: `gh` no da los logs
hasta que termina. `alpine-e2e`, `fuzz`, `property-tests` y
`security-advisories`: **success**.

Si el fallo es real y compartido por todos los que ejecutan tests, hay que
leer el log de `x86_64-musl` y arreglarlo antes de nada. Si es de
infraestructura (drivers/cross), anotarlo y no taggear hasta verde.

## Después, en orden

1. Leer logs del run `39989519802`; arreglar lo real; esperar verde.
2. Cerrar el criterio 1 del acta (run id + estado).
3. Tag `v0.2.0-alpha` + push **del tag** → el workflow `release` publica
   5 targets musl estáticos + `install.sh` + `.sha256`. **No** `gh release
   create` a mano: el workflow ya lo crea con `--generate-notes`; después
   `gh release edit v0.2.0-alpha --notes-file RELEASE_NOTES.md`.
4. Merge `--no-ff` a `main` + push; verificar CI de `main`.
5. `HANDOFF.md` post-release y `STATE_final.md`.
6. T8: `git diff main testing --stat` vacío, `gh release view`, runs de `main`.

## Trampas ya pagadas

- `git push origin testing` empuja la **ref local `testing`**, no HEAD: con
  la sesión en otra rama, el push "OK" no movió nada. Usar
  `git branch -f testing HEAD && git push origin testing`.
- El CI compila tests con `-D warnings`: un `mut` de más tumba 20 jobs.
- El release no se crea a mano si el workflow `release` ya corre con el tag.

## Cierre alpha — 2026-10-23 (sesión final)

- `d230fbb`: los 4 fallos de macOS eran 3 causas de plataforma, no un cuelgue
  (`/bin/true` inexistente en Darwin, `listen` exacto vs `backlog+1`, y
  `maxdgram` 2048). Arreglado/gateado con razón; CI `40002462937` verde salvo
  `riscv32gc`.
- `32c92d1`: acta con el run verde. Tag `v0.2.0-alpha` → ese commit.
- `840f688`: merge `--no-ff` a `main`, pusheado. `git diff main testing` vacío.
- Release `v0.2.0-rc1` marcado como pre-release (superseded); no se borró.
- Workflow `release` corriendo sobre el tag (5 targets musl + install.sh +
  sha256). Al terminar: `gh release edit v0.2.0-alpha --notes-file RELEASE_NOTES.md`.

Punto de retorno si algo falla: `main` = `840f688`, tag = `32c92d1`.

## CERRADO — 2026-10-23

- `main` = `2781c72` (merge + 3 fixes de CI + docs). CI `40015410128` verde
  salvo `riscv32gc`. `distro` se verifica aparte. `git diff main testing` = 0.
- Tag `v0.2.0-alpha` → `32c92d1`. Release publicado por el workflow `release`:
  5 targets musl + `install.sh` en cada tarball + `.sha256` (10 assets).
  Notas editadas con `--notes-file RELEASE_NOTES.md`.
- `v0.2.0-rc1` queda como **pre-release** histórico, no se borró.
- Flaky real encontrado y arreglado: `the_visible_state_follows_the_plan`
  leía el state file antes de que el supervisor publicara `state=Running`
  (perdido en aarch64). Ahora usa `wait_until`.
- `driver clang` falló una vez y pasó al relanzar: no se reprodujo, no se
  tocó. Si vuelve, mirar el step `Test` completo, no `--log-failed`.

### Abierto (post-alpha, por coste)

1. Hardware real: `scripts/hw-verification.sh deploy|soak 24|power-test`.
2. `identity.rs`: `grow_until_cap` (~45 min, mata 10).
3. `zconfig` 116 (2-3 h + corpus), `spawn.rs` 152 (4-6 h, job privilegiado),
   `service.rs` 27 (~2 h, hijo/reloj inyectable).
4. `riscv32gc`: bloqueado por upstream (sin `std`).
5. El árbol del tag dice "573 tests"; el real es 633. El tag no se reescribe;
   `main` y las notas del release llevan 633.

- CI del HEAD documental `24d5bcc`: verde salvo `riscv32gc` (run `40023312149`).
- El tag `32c92d1` incluye el flaky de `the_visible_state_follows_the_plan`;
  los binarios no contienen tests y las notas del release lo dicen.
- `distro` no está verde en `main` todavía: el 2026-10-23 los 7 legs agotaron
  `timeout-minutes: 45`. **La causa no era caché ni lentitud**: cuatro tests se
  colgaban para siempre y el `Mutex` serial arrastraba a tres más. Arreglado en
  la rama de cierre; los 7 legs están verdes en `main` (ver «Cierre
  2026-10-26»).

## Sesión 2026-10-24 — cierre pre-beta (sin hardware)

Rama `agent/overnight-20261024-0050`, base `fb6ca1c` (`main` == `testing`).
Contrato en `AUDIT.md` (sección final).

| commit | qué |
|---|---|
| `3dc2966` | `connect_deadline` + `zctl` con deadline + `Scratch::read` ignora fichero vacío (flake real) |
| `2040769` | skips con razón (mount ns, cgroup RO) y peer `nobody` capaz de conectar |
| `1ec8a47` | `distro`: 25 min, `cargo build --workspace` (no `--all-targets`), `docs/CI.md` |
| `795fb4e` | `KNOWN_ISSUES.md`: el flaky del tag y el contrato de deadlines |
| `3032877` | `docs/audit/mutants-costs.md` |
| `b9f05c9` | `hw-verification.sh full` / `kill-term-1` / `after-reboot` / `dinit-removal-check` |
| `16746be` | `docs/BETA.md` + `docs/HW-VERIFICATION.md` al día |

Evidencia local: `cargo test --workspace` **641 passed, 0 failed, 1 ignored**;
`cargo build --workspace --release`, clippy `-D warnings`, `fmt --check` y
`cargo doc` con `-D warnings` limpios. En Alpine (podman) los cuatro tests que
fallaban pasan (tres como skip con razón).

Mutantes cerrados esta noche, medidos por archivo con
`cargo mutants --file`:

| archivo | antes | después | mide |
|---|---|---|---|
| `zconfig/src/value.rs` | 48 | **0** | 208 mutantes: 200 caídos, 8 no viables |
| `zconfig/src/desc.rs` | 12 | **0** | 77 mutantes, 77 caídos |
| `zconfig/src/plan.rs` | 12 | **7** | heap a 0; el cursor de `find_cycle` sigue sin forma de grafo que lo distinga |

`zconfig` queda en **51 missed** (parser 23, graph 21, plan 7). La trampa que
costó una vuelta: un fixture `5 restarts per 1m` **es** el `Default`, así que
`From<RestartSpec>` mutado a `Default::default()` pasaba; hay que usar valores
que no coincidan con el default.

Tres fallos de CI que solo aparecieron al correr los legs de verdad (no en el
contenedor local): `macos-aarch64` no tiene `SOCK_NONBLOCK` en `libc` (connect
portable ahora), los legs corren como root, donde un `chmod 0555` no niega la
escritura (el test de rotación pregunta si el modo tomó efecto antes de
afirmar), y `drop_closes_the_file_descriptor` leía un fd **reutilizado** por
otro hilo de test entre `drop` y `fcntl` (ahora corre en una tabla de fds
aislada con `fork`, como los tests de fd 0).

### Lo único que falta para beta

1. Push de esta rama + run verde de `distro` en `main` (el fix es de tests).
2. Hardware: `sudo ./scripts/hw-verification.sh full <artefacto> --sha256 <hash>`
   y, tras el arranque, `after-reboot`. Eso cubre soak 24 h, los dos caminos de
   apagado y el chequeo de dinit.
3. Tag `v0.2.0-beta` según `docs/BETA.md`. **Nada de tag antes del paso 2.**

### Trampas nuevas

- `cargo test --workspace` **no** construye el binario `zctl`; sin
  `cargo build --workspace` previo, los tests de socket se saltan solos.
- Un test que falla porque el contenedor no tiene `CAP_SYS_ADMIN` o el cgroup
  es de solo lectura no prueba nada: skip con la razón escrita, nunca un
  `#[allow]` ni un `unwrap` defensivo.
- El socket es `0600`: para probar el chequeo de credenciales el test abre el
  modo (`0666`) antes de que el peer `nobody` conecte. Es test-only.

## Cierre 2026-10-26 — mergeado y verde

- PR #25 → merge `96906d2`: el cuelgue de `distro` (deadlines en los tests),
  los skips de contenedor, `docs/BETA.md`, `docs/CI.md`, `hw-verification.sh`
  y los mutantes de `zconfig` (`value.rs` 48→0, `desc.rs` 12→0, `plan.rs`
  12→7; crate 116→51).
- PR #26 → merge `c22c0c3`: el job nocturno de libFuzzer corría sobre **stable**
  porque `rust-toolchain.toml` fija `channel = "stable"` y rustup deja que el
  fichero gane a `rustup default nightly`; `RUSTUP_TOOLCHAIN: nightly` en el
  job es el override que sí gana. Verificado con `workflow_dispatch`.
- Flake real encontrado por CI y arreglado: `drop_closes_the_file_descriptor`
  leía un fd **reutilizado** por otro hilo de test entre `drop` y `fcntl`
  (ahora en tabla de fds aislada con `fork`, como los tests de fd 0).

Evidencia sobre `main` = `c22c0c3`:

| run | resultado |
|---|---|
| `ci` `40286289942` | **success** (todos los jobs, `riscv32gc` incluido) |
| `distro` `40286289917` | **7/7 success** |
| `fuzz` (dispatch sobre la rama) | mutation + libFuzzer **success** |

Local: 641 tests, 0 fallos, 1 ignorado; clippy 0; fmt limpio; `cargo doc
-D warnings` limpio; `cargo build --release` limpio.

### Lo que queda (todo hardware o post-beta)

1. **Beta**: soak 24 h, `zctl poweroff`, `SIGTERM` a PID 1 y retirada de dinit
   en la máquina real (`scripts/hw-verification.sh full` + `after-reboot`).
   Después, tag según `docs/BETA.md`. **Sin hardware no hay tag.**
2. Mutantes post-beta con coste medido: `spawn.rs` 152, `service.rs` 27,
   `graph.rs` 17 (+3 timeouts), `parser.rs` 23, `identity.rs` 10, `plan.rs` 7,
   `zcore` 31.
3. `riscv32gc`: upstream sin `std` para ese target (hoy pasó por suerte del
   toolchain; no se depende de ello).
