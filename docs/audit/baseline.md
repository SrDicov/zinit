# Línea base — 2026-10-22

Números medidos, no estimados. La fuente es el árbol de trabajo de este
commit (`3ec683b`); el binario es el `release` local con el perfil del
workspace (`panic=abort`, `lto`, `codegen-units=1`, `opt-level="s"`,
`strip`).

## Líneas de código

| crate | src | tests | total | qué es |
|---|---:|---:|---:|---|
| `zcore` | 3 491 | 714 | 4 205 | núcleo puro: `no_std`, `forbid(unsafe_code)`, 0 deps |
| `zconfig` | 11 701 | 0 | 11 701 | parser + grafo + plan (`&str -> Plan`) |
| `zrt` | 7 340 | 0 | 7 340 | la única capa de syscalls (solo `libc`) |
| `zservice` | 5 813 | 0 | 5 813 | spawn, readiness, logs, sandbox |
| `zinit` | 4 712 | 1 702 | 6 414 | el binario: `--init`, `--sup`, `check` |
| `zctl` | 216 | 0 | 216 | el cliente de control |
| **total** | **33 273** | **2 416** | **35 689** | |

## Tests

| crate | `#[test]` |
|---|---:|
| `zcore` | 66 (incluye 10 de propiedades) |
| `zconfig` | 304 |
| `zrt` | 54 |
| `zservice` | 79 |
| `zinit` | 52 |
| `zctl` | 4 |
| **total** | **559** |

Además: `crates/zcore/tests/properties.rs` (propiedades del núcleo),
`crates/zinit/tests/{control_socket,supervises_real_service,import_runsv}.rs`
(integración con procesos reales), `fuzz/fuzz_targets/parse_service.rs`
(libFuzzer sobre el parser).

## Binario

| artefacto | bytes | perfil |
|---|---:|---|
| `zinit` (release local) | 685 256 | glibc, `strip` |
| `zctl` (release local) | 294 448 | glibc, `strip` |
| `zinit` (artefacto musl de CI) | 774 832 | musl estático, `strip` |

El presupuesto de CI es 1 MiB para el `zinit` estático: 774 832 B deja
un margen de 20 %. El `zctl` no está en el presupuesto (es un shim de
216 líneas).

## Dependencias

`cargo tree -p zinit` → los crates del workspace + `libc`. Nada más. El
job `purity` de CI falla el build si aparece una tercera parte.

## Matriz de CI

| workflow | qué cubre |
|---|---|
| `ci` | glibc/musl × x86_64/i686/aarch64 + macOS, clippy `-D warnings`, `fmt`, purity, licence, size budget, msrv 1.85, docs, nightly, miri (`zcore` sólo) |
| `property-tests` | las propiedades del núcleo en release |
| `fuzz` | `no_panic` del parser + libFuzzer 10 min |
| `alpine-e2e` | musl real: busybox, `setsid` EPERM, reactor |
| `distro` | void (glibc+musl), alpine, arch, eweos, gentoo, chimera |
| `release` | matriz + `zigbuild` (riscv64 musl) + publicación |

## Tiempos medidos (4 núcleos, local)

| paso | tiempo |
|---|---:|
| `cargo build --workspace --release` (incremental) | 13 s |
| `cargo test --workspace --release` | 61 s |
| `cargo clippy --workspace --all-targets` | 4 s |

El veredicto de build/test/lint es de GitHub Actions; estos números son
solo para saber cuánto tarda un ciclo local.
