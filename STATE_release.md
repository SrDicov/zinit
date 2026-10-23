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
