# Acta de alpha — `v0.2.0-alpha`

Fecha: 2026-10-23. Base verificada: `d230fbb` (`testing`); el tag apunta al
commit de este acta, cuyo padre es esa base.

Este acta no repite lo que dicen otros documentos: cada fila apunta a la
evidencia y dice qué se comprobó **en esta sesión**.

| # | criterio | estado | evidencia |
|---|---|---|---|
| 1 | CI verde, salvo `riscv32gc` | ✅ (run `40002462937`) | ver §1 |
| 2 | mutantes `zcore` ≤ 20 % | ✅ 15,2 % | `docs/audit/mutants-zcore.md` (31/204) |
| 3 | `zconfig`/`zservice`: baratos muertos, caros documentados | ✅ | §3, `docs/audit/mutants-{zconfig,zservice}.md` |
| 4 | SA7: seccomp con argumentos | ✅ | `docs/SECCOMP.md` (`clone` con 7 namespaces, `ioctl(TIOCSTI)`) |
| 5 | SA9: deberes de PID 1 | ✅ | `docs/PID1-DUTIES.md` |
| 6 | SA10: re-auditoría de `KNOWN_ISSUES` | ✅ | `docs/audit/known-issues-audit.md` |
| 7 | gaps del generador documentados | ✅ | `docs/audit/generator-gaps.md` |
| 8 | cross-verificación | ✅ 2 rondas, 3 fallos reales | commits `4904202`, `bb36339` |
| 9 | acta con evidencia | ✅ | este fichero |

## 1. CI

El run `40002462937` (commit `d230fbb`) termina con **todos los jobs en
success salvo `check riscv32gc-unknown-linux-gnu`**, que queda excluido
porque Rust no tiene target con `std` (issue #19, `blocked-by-upstream`).
`macos-aarch64` pasa: el run anterior (`39991917913`) fallaba con cuatro
nombres de test y **tres** causas, todas de plataforma y todas arregladas en
`d230fbb`:

- `/bin/true` no existe en Darwin (`/usr/bin/true` sí): el modo `dueping`
  usaba la ruta literal en vez de `testutil::true_bin()`.
- Darwin encola exactamente `listen(backlog)`, no `backlog + 1`: el test de
  backlog lleno con dos conexiones sólo tiene sentido en Linux, y queda
  gateado con la razón escrita.
- `net.local.dgram.maxdgram` es 2048 en Darwin: el framing de 4096 bytes no
  es observable ahí, y el fallback a `/dev/log` disfrazaba el `EMSGSIZE` de
  `ENOENT`. Gateado a Linux igual que el de backlog.

Los dos fallos de compilación anteriores (`libc::tgkill` ausente en los
bindings musl, `let mut` que `-D warnings` rechaza) siguen arreglados en
`3d308bd` y `beb1898`.

Medido en local antes de taggear: `cargo test --workspace` verde (633 tests,
1 ignorado), `cargo clippy --workspace --all-targets -- -D warnings` limpio,
`cargo fmt --all --check` limpio.

`riscv32gc-unknown-linux-gnu` sigue excluido: Rust no tiene target con `std`
(issue #19, `blocked-by-upstream`).

## 3. Mutantes

| archivo | al medir | ahora | evidencia |
|---|---|---|---|
| `zservice/ready.rs` | 47 | **0 libres** | `6585c32` + `10ea488` + `1470377`: 12 `// equivalent` razonados |
| `zservice/log.rs` | 34 | **0 libres** | `7b688d5` + `aa31e1e`: 5 equivalentes razonados |
| `zservice/identity.rs` | 10 | **10 documentados** | una sola causa (reintento `ERANGE`), `9a8322a` |
| `zservice/service.rs` | 27 | documentado | `docs/audit/mutants-zservice.md` §service.rs |
| `zservice/spawn.rs` | 152 | documentado + test root-gated | `79a3fca`, `child_step_drop_ids_as_root` (`#[ignore]`) |
| `zconfig/*` | 116 | documentado por archivo | `docs/audit/mutants-zconfig.md` |

No se declara ningún mutante equivalente sin prueba. Los 12 timeouts de
`zconfig` y los 24 de `zservice` (al medir) están atribuidos: los de
`ready`/`log` quedaron resueltos en T1; el resto se lee con su archivo.

## Lo que este acta NO dice

- No dice "0 % de mutantes": la tasa de `zconfig` (14,9 %) y `zservice`
  (41,1 % al medir) se mantiene, con los caros documentados y con coste.
- No dice "hardware verificado": el soak en hardware real es post-alpha
  (`KNOWN_ISSUES.md`).
- No dice "release publicado": el release se crea con el tag y el workflow
  `release` (5 targets musl estáticos); este acta se firma antes.
