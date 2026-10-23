# STATE — cierre de alpha (para sesión nueva)

Leer esto y `STATE_alpha.md` antes de tocar nada. No re-explorar.

## Punto de partida verificado

```sh
cargo test -p zservice --lib                 # 93 passed
grep '^MISSED' /tmp/opencode/mut-ready2.log | wc -l  # 25
git log --oneline -1                          # 7f2ebba o posterior
git status --short                            # limpio
```

| archivo | missed | notas |
|---|---|---|
| `crates/zservice/src/ready.rs` | **0 libres** | cerrado en `6585c32`: 0 timeout, 12 MISSED con `// equivalent` razonado |
| `crates/zservice/src/log.rs` | **0 libres** | cerrado en `aa31e1e`: 0 timeout, 5 MISSED equivalentes razonados. OJO: el subagente necesitó `RUST_TEST_THREADS=1` (flake de fd 0 en paralelo, 4/15) — ver SPEC_T1_B18 |
| `crates/zservice/src/identity.rs` | **11, todos documentados** | `9a8322a`: 1 matado, 10 ERANGE + 1 carrera sin camino de test (ver doc) |
| **baratos** | **0** | ready 0, log 0, identity 11 (todos con causa verificada). T1 CERRADO |
| `crates/zconfig/*` | 116 | documentar, no matar |
| `crates/zservice/src/spawn.rs` | 152 | root-gated + documentar |
| `crates/zservice/src/service.rs` | 27 | documentar |

## Criterio de tag (no es 0 % missed)

Tag **sólo** si: 0 baratos libres, 268 caros
documentados con causa, los 9 criterios de `STATE_alpha.md` verdes, CI verde
salvo `riscv32gc`, y el release de GitHub creado. **Tag sin release = no tag.**

## T1 — los 75, en batches de 10-12

Ritmo medido: 15 muertos por 3 comandos. Plantilla por batch:

1. `cargo mutants -p zservice --file crates/zservice/src/<f>.rs --list`
2. Leer las líneas de cada mutante, escribir un test por frontera
   (un test suele matar 2-3 mutantes del mismo `&&`/`<`/`+=`).
3. Verificar por mutación — no hay `--mutant` en esta versión:

```sh
F=crates/zservice/src/ready.rs; cp $F /tmp/opencode/bak
python3 -c "import sys;p='$F';t=open(p).read();open(p,'w').write(t.replace(sys.argv[1],sys.argv[2],1))" 'OLD' 'NEW'
cargo test -q -p zservice --lib <filtro>   # DEBE fallar
cp /tmp/opencode/bak $F
```

4. Commit: `test(<archivo>): kill N mutants in <archivo>`.

Pendientes concretos ya vistos en `ready.rs`: `drain_notify` 234/238/239/262/265
(timeouts: `EINTR`/`WouldBlock` piden forzar la señal; si no sale en 30 min,
`#[ignore]` con motivo), `poll_readable` 438/440/445, `probe_tcp_connect`
565/567 (`SO_ERROR`), `run_check_child` 356/357/361 (fd leaks: contar
`/proc/self/fd` antes y después del fork).

## T2 — documentar, no matar

- `docs/audit/mutants-zconfig.md`: por archivo, cada MISSED con causa
  (parsing de valores, grafo, plan) y coste post-alpha.
- `docs/audit/mutants-zservice.md`: `spawn.rs` (152) con la cobertura userns ya
  probada + lo que pide root; `service.rs` (27) clasificado.
- Test root-gated del paso de ids:

```rust
#[test]
#[ignore = "setgroups is denied inside a user namespace; needs real root"]
fn child_step_drop_ids_as_root() { /* user = nobody + drop-capabilities = all */ }
```

## T3-T6 — tag, merge, release

```sh
# 1. versión
grep '^version' Cargo.toml            # 0.1.0 -> 0.2.0-alpha en los 6 crates + workspace

# 2. tag (tras CI verde)
git tag -a v0.2.0-alpha -m "v0.2.0-alpha: usable con limitaciones documentadas"
git push origin testing --tags

# 3. merge a main (--no-ff, historia intacta)
git checkout main && git pull --ff-only && git merge --no-ff testing -m "release: v0.2.0-alpha"
git push origin main

# 4. binarios
cargo build --release                       # nativo; musl si hay target instalado
cargo build --release --target x86_64-unknown-linux-musl
sha256sum target/release/zinit target/release/zctl > SHA256SUMS
stat -c '%s %n' target/release/zinit        # ≤ 1 MiB, presupuesto del proyecto

# 5. release
gh release create v0.2.0-alpha --title "v0.2.0-alpha" \
  --notes-file RELEASE_NOTES.md \
  target/release/zinit target/release/zctl SHA256SUMS
```

`RELEASE_NOTES.md`: qué es, estado alpha con enlace a `KNOWN_ISSUES.md`,
instalación, migración (`MIGRATING.md`), "verificado en VM y contenedores;
hardware real pendiente de soak", y cómo reportar.

## T7 — docs

`README.md` (badge + estado alpha + instalación), `CHANGELOG.md` (sección
`v0.2.0-alpha`: seccomp con argumentos, power verbs autorizados, señales BSD,
capdrop antes del uid, private-tmp en tres dirs, generador de capabilities,
mutantes zcore 15,2 % y 17 baratos muertos), `HANDOFF.md` (post-alpha, beta
espera hardware).

## Lo que NO se hace aquí

- 0 % de missed: es release, no alpha.
- Tocar `spawn.rs` sin root real (el test userns ya cubre capdrop y filtro).
- `git add -A`, `git add .`, `git commit -a`.

## Bitácora (modo dispatcher, sesión de cierre)

| SPEC | estado | commit | resultado |
|---|---|---|---|
| T1-B12 | SUCCESS | `10ea488` | 5 timeouts de `drain_notify` muertos (sentinel + EINTR/WouldBlock reales) |
| T1-B13 | SUCCESS | `1470377` | 11 de 24 matados; 12 equivalentes razonados |
| T1-B14 | SUCCESS | `6585c32` | `ready.rs` cerrado: 0 timeout, 12 MISSED con `// equivalent` |
| T1-B15 | SUCCESS | `7b688d5` | 22 de 31 matados en `log.rs` |
| T1-B16 | SUCCESS | `aa31e1e` | `log.rs` cerrado: 0 timeout, 5 equivalentes |
| (flaky) | FIX | `0796e6b` | fd-0: mutex no bastaba (3/20 fallos) → fork aislado, **0/40** |
| T1-B17 | SUCCESS | `9a8322a` | identity: 1 matado, 10 documentados (ERANGE + carrera `AlreadyExists`) |

Base actual: `cargo test -p zservice --lib` → **124 passed**, clippy 0, fmt limpio.

Siguiente: **T2** — documentar los caros (zconfig 116, spawn 152, service 27) en
`docs/audit/`. Después T3 (acta), y sólo entonces T4-T6 seguidos.

T1 cerrado: 0 baratos libres. Los 11 de identity tienen causa escrita, no
"pendiente".
