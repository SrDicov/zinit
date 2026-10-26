# HANDOFF — lo que la siguiente sesión necesita saber

## Post-release: `v0.2.0-alpha` (2026-10-23)

- El release lo publica el workflow `release` al empujar el tag (5 targets musl
  estáticos + `install.sh` + `.sha256`); no se crea a mano. Las notas humanas
  se ponen después con `gh release edit v0.2.0-alpha --notes-file RELEASE_NOTES.md`.
- `main` lleva el merge `--no-ff` de `testing`; el árbol de `main` y `testing`
  es el mismo.
- Trabajo post-alpha, por coste (el detalle está en `docs/audit/`):
  1. `zconfig`: **cerrado**. `value.rs`, `desc.rs`, `graph.rs`, `plan.rs` y
     `parser.rs` no aportan missed; los 12 que quedan son equivalentes con la
     prueba escrita en el código (15 timeouts son bucles que el harness corta).
  2. `zservice/identity.rs`: extraer `grow_until_cap` — 45 min, mata 10.
  3. `zservice/service.rs` (3-4 h) y `spawn.rs` (4-6 h con job privilegiado).
- Beta **no** espera a los mutantes: espera hardware real
  (`scripts/hw-verification.sh`, soak 24 h, `docs/HW-VERIFICATION.md`).

## Un commit con cuatro cambios: `573a9e8`

`573a9e8` ("fix(ctl): a zctl where a zctl is installed…") contiene **cuatro
cambios lógicos**, porque el árbol ya estaba preparado al commitear y el
mensaje describe sólo el primero:

1. `authorize()` exige que el ejecutable del cliente esté en un directorio de
   instalación (o en el del propio supervisor).
2. `peer_exe = None` en Linux pasa a ser rechazo para los verbos de potencia
   (antes concedía: fail-open).
3. `MAX_PROGRAM` en seccomp: el límite de `build` y el de `install` salen de
   la misma expresión (antes un `install` con 206 instrucciones se rechazaba).
4. Tres tests reforzados (`topological_order`, adopción por nombre,
   `degradation_line`) y las correcciones de documentación.

**Auditorías futuras: leer el diff, no el mensaje.** Lo mismo pasó antes con
`7f47ec7` (fix de `authorize` + reescritura del test de parada).

## Lección de proceso

Si al terminar hay más de un cambio lógico sin commitear, **separar antes de
push**, aunque cueste: `git bisect` no lee mensajes, y un commit que miente
sobre su alcance envenena cualquier bisección futura. No se reescribe historia
para arreglarlo; se anota aquí y se corrige el hábito.

## Estado

- Código verificado por CI (matriz completa) y localmente.
- Beta pendiente **sólo** de hardware: `scripts/hw-verification.sh` +
  `docs/HW-VERIFICATION.md`. Sin máquina no hay soak, y sin soak no hay beta.
- devmon sigue en `dinit-base` a propósito (ver `KNOWN_ISSUES.md`).
- El generador (repo aparte) ya no pierde capabilities: `capabilities =` vacío
  → `drop-capabilities = all`; una lista no vacía deja el fichero con la
  directiva verbatim para que `zinit check` lo rechace (E004) en vez de
  convertir en silencio un servicio con más privilegio.

## Regla dura: nada de `git add -A`

Tres commits de esta sesión llevaron ficheros que no eran suyos (`573a9e8`
con cuatro cambios, `7f47ec7` con dos, `f8ac365` con `mutants.out/` dentro).
La regla, desde ya:

- `git add <archivo>` con nombre y apellido. Nunca `-A`, nunca `.`, nunca
  `git commit -a`.
- Un directorio se añade con `git add <dir>/*.rs`, no con el directorio.
- Antes de cada commit, `git status --short` y leer la lista: si hay algo que
  no se pretendía, se saca del índice antes de commitear.
- Si el commit ya salió con basura, se arregla en un commit de limpieza (no se
  reescribe historia) y se anota aquí.

## Estado 2026-10-26 — `main` = `3c57181`, todo verde

- PR #25 (`96906d2`), #26 (`c22c0c3`), #27 (`a1704ab`) y #28 (`3c57181`)
  mergeados. `ci` y `distro` (7/7) en success sobre `main`. El cuelgue de
  45 min de `distro` era **tests sin deadline**, no caché ni lentitud.
- `ci.yml` tiene el job **`privileged`**: el suite como root en un contenedor
  `--privileged` con `--include-ignored`; medido 658 tests, 0 fallos, frente a
  656 + 1 ignorado en un runner. Cgroup, sandbox, private-tmp y PID 1 en un
  namespace dejan de saltarse.
- Mutantes de `zconfig`: 116 → **12**, todos equivalentes con prueba escrita
  (`value.rs`, `desc.rs`, `graph.rs`, `plan.rs`, `parser.rs`). El resto está en
  `docs/audit/mutants-costs.md` con coste y prioridad.
- Trampa de CI que costó una ronda: `rust-toolchain.toml` fija `stable` y
  **gana a `rustup default`**; el job de fuzz necesita `RUSTUP_TOOLCHAIN=nightly`
  en el job, no basta con `dtolnay/rust-toolchain@nightly`.
- Trampa de test: `fcntl(fd, F_GETFD)` después de `drop` es una carrera con
  otros hilos del suite que reutilizan el número. Los tests de fd van en
  `in_isolated_fd_table` (fork), nunca en el proceso multihilo.
- Trampa del job `privileged`: `--include-ignored` corre los tests en el
  **mismo** proceso. Un test que cambia ids/capabilities/grupo del proceso
  envenena a todos los demás; `child_step_drop_ids_as_root` bajaba el euid a
  43210 y dejaba `kill` en EPERM, con un `waitpid` bloqueado 30 min hasta
  cancelar el job. Ese test ahora hace el drop en un hijo bifurcado.
- Beta pendiente **solo** de hardware: `scripts/hw-verification.sh full` +
  `after-reboot`, y el tag según `docs/BETA.md`.
