# Mutantes de `zservice` — estado tras T1

```
cargo mutants -p zservice --timeout 30 --no-shuffle -j 2
657 mutants tested in 16m: 270 missed, 343 caught, 18 unviable, 24 timeouts
```

Tasa inicial: 270/657 = 41,1 %. Lo que cambió tras la caza de T1 (`574eea5`,
más el fix de musl `3d308bd`):

| archivo | missed al medir | estado actual | dónde |
|---|---|---|---|
| `ready.rs` | 47 | **0 libres**, 0 timeouts | 12 `// equivalent` razonados en el código |
| `log.rs` | 34 | **0 libres**, 0 timeouts | 5 equivalentes razonados (`RUST_TEST_THREADS=1` por el fd 0) |
| `identity.rs` | 10 | **10 documentados** | sección al final de este fichero |
| `service.rs` | 27 | documentado | tabla de abajo |
| `spawn.rs` | 152 | documentado | párrafo siguiente |

## spawn.rs — 152 (camino del hijo, root-gated)

Lo probado por mutación: `supervises_real_service::the_child_path_runs_with_privileges_inside_a_user_namespace`
corre bajo `unshare -U -r -p -f --mount-proc` y comprueba `CapBnd` a cero y el
rechazo de `unshare -m`; anular `child_step_capdrop` lo rompe. Paso de ids: su
test existe ya, `child_step_drop_ids_as_root` (`#[ignore]`, root real,
`--test-threads=1`), porque `setgroups` devuelve EPERM dentro del userns.

Los 152, por paso medido: `child_step_fds` 40, `child_step_cgroup` 14,
`child_step_name` 12, `child_step_ids` 12, `spawn` 9, `child_step_mount` 9,
`child_step_ctty` 7 (103 de 152 en esos siete); el resto en entorno,
`pid_ascii`, `child_main` y capdrop. El orden de llamadas del hijo no se puede
verificar con unit tests: pide un job privilegiado que ejecute el camino
completo y compare fds, ids y mounts. Post-alpha, 4-6 h.

## service.rs — 27 (máquina de estados)

| grupo | missed | razón | post-alpha |
|---|---|---|---|
| `ensure_listeners` | 8 | reutilización de fds heredados: exige un supervisor real con listenfds | 1 h |
| `poll_exec` + `poll_ready` | 5 | carrera exec-error/`WouldBlock`: necesita el par padre-hijo vivo | 1 h |
| `poll_forking_adopt` + `poll_forking_unstick` + `on_waitpid` | 8 | adopción de huérfanos y `waitpid` reales | 1 h |
| `stop_signal`, `cleanup`, `adopt`, `pid`, `adopted`, `group_is_signallable` | 6 | estado tras `fork`/`kill`: sólo con procesos vivos | 30-45 min |

Es integración, nunca unitario puro: por eso no entró en el lote barato.
De los 24 timeouts iniciales, los de `ready.rs` y `log.rs` se resolvieron en
T1; los de `service.rs` y `spawn.rs` se leen con su archivo.

## identity.rs — 10 missed, una sola causa (medido)

`cargo mutants -p zservice --file crates/zservice/src/identity.rs --timeout 60 -j 2`
→ `38 tested: 11 missed, 26 caught, 1 unviable`, 66 s, **0 timeouts**.
Matado en T1: `319:5` (`ensure_cgroup -> Ok(Default::default())`), con
`ensure_cgroup_refuses_a_directory_without_cgroup_procs` (verificado por
mutación).

Los 10 restantes son **una causa, no diez**:

| líneas | mutante | por qué no muere en un unit test |
|---|---|---|
| `97:26`, `97:34`×2, `103:34`×2 | `resolve_user`: el `>= 1024*1024` y el `buf.len()*2` del reintento `ERANGE` | sólo se ejecutan si `getpwnam_r` devuelve `ERANGE`, es decir si la entrada de `/etc/passwd` no cabe en el buffer inicial. No hay forma de fabricar esa entrada desde un test sin escribir en `/etc/passwd` ni montar NSS falso |
| `147:26`, `147:34`×2, `153:34`×2 | idéntico en `resolve_group` (`getgrnam_r`) | ídem con `/etc/group` |

**Camino de salida (post-alpha, ~45 min):** extraer el bucle de reintento a un
helper genérico `fn grow_until_cap(len: usize, cap: usize) -> Option<usize>` y
probar sus tres ramas (`*2`, `>= cap`, `None` al pasarse) sin invocar NSS. Eso
mata los 10 de golpe.

## Timeouts: la regla

Un timeout con el mutante aplicado **no es un flake**: es un test que espera un
resultado que el mutante impide. Se lee la traza antes de llamarlo flake, y si
es un bucle que no avanza se añade el caso de borde que lo corta.
