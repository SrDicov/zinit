# Mutantes de `zservice` — tasa completa (2026-10-23)

```
cargo mutants -p zservice --timeout 30 --no-shuffle -j 2
657 mutants tested in 16m: 270 missed, 343 caught, 18 unviable, 24 timeouts
```

**Tasa de supervivencia: 270/657 = 41,1 %.** Medida completa. Es alta, y el
desglose dice por qué.

## Desglose por archivo

| archivo | MISSED |
|---|---|
| `crates/zservice/src/spawn.rs` | 152 |
| `crates/zservice/src/ready.rs` | 47 |
| `crates/zservice/src/log.rs` | 34 |
| `crates/zservice/src/service.rs` | 27 |
| `crates/zservice/src/identity.rs` | 10 |

**Corrección de una nota anterior.** Con la muestra parcial (99 evaluados) el
desglose no mostraba `spawn.rs` y escribí que el problema era `ready.rs` y
`log.rs`. Con el run completo `spawn.rs` es **152 de 270 (56 %)**: la muestra
no era representativa porque los mutantes de `spawn.rs` tardan más y quedaron
al final. Ambas correcciones, en direcciones opuestas, tienen la misma lección:
una muestra no es una tasa.

## Qué hay ya con red, y qué no

`spawn.rs` (152): el camino del hijo corre tras `fork` y no vuelve al
llamante. Tiene red parcial real —
`crates/zinit/tests/supervises_real_service.rs::
the_child_path_runs_with_privileges_inside_a_user_namespace` ejecuta bajo
`unshare -U -r -p -f --mount-proc` y comprueba `CapBnd` a cero y el rechazo de
`unshare -m`; probado por mutación (con `child_step_capdrop` anulado el test
falla con `CapBnd: 000001ffffffffff`). El resto pide root real: `setgroups`
devuelve EPERM dentro del userns en las tres variantes de `unshare` probadas,
porque `unshare -r` deja `/proc/self/setgroups` en `deny`.

`ready.rs` (47) y `log.rs` (34): funciones puras. **Testables aquí y ahora**,
sin root, sin fork, sin supervisor. Es el trabajo barato y el que más baja la
tasa.

`service.rs` (27): máquina de estados del servicio, alcanzable con los tests
de integración del supervisor.

`identity.rs` (10): comparación de credenciales, unitario.

## Timeouts (24)

Casi todos en `log.rs` (rotación). Un timeout con el mutante aplicado es un
test que espera algo que el mutante rompió: hay que leer la traza antes de
llamarlo flake.

```
crates/zservice/src/log.rs:304:11 — replace -= with +=
crates/zservice/src/log.rs:304:11 — replace -= with /=
crates/zservice/src/log.rs:360:11 — replace < with <=
crates/zservice/src/log.rs:372:5 — replace write_all -> io::Result<()> with Ok(())
crates/zservice/src/log.rs:372:11 — delete !
crates/zservice/src/ready.rs:238:23 — replace match guard e.kind() == io::ErrorKind::Interrupted with true
```

## Orden de ataque (coste creciente)

1. `ready.rs` 47 y `log.rs` 34 — unitarios, sin privilegios.
2. `identity.rs` 10 — unitario.
3. `service.rs` 27 — integración del supervisor.
4. `spawn.rs` 152 — sólo con root (job privilegiado), salvo lo ya cubierto.

Comando por archivo:

```sh
cargo mutants -p zservice --file crates/zservice/src/ready.rs --timeout 30
```

## identity.rs — 11 missed, medidos (`mut-identity.log`, 38 mutantes)

`cargo mutants -p zservice --file crates/zservice/src/identity.rs --timeout 60 -j 2`
→ `38 tested: 11 missed, 26 caught, 1 unviable`, 66 s, **0 timeouts**.
Matado en esta pasada: `319:5` (`ensure_cgroup -> Ok(Default::default())`), con
`ensure_cgroup_refuses_a_directory_without_cgroup_procs` (verificado por
mutación: el mutante la rompe).

Los 11 restantes son **una sola causa, no once**:

| líneas | mutante | por qué no muere en un unit test |
|---|---|---|
| `97:26`, `97:34`×2, `103:34`×2 | `resolve_user`: el `>= 1024*1024` y el `buf.len()*2` del reintento `ERANGE` | sólo se ejecutan si `getpwnam_r` devuelve `ERANGE`, es decir si la entrada de `/etc/passwd` no cabe en el buffer inicial. No hay forma de fabricar esa entrada desde un test sin escribir en `/etc/passwd` ni montar NSS falso |
| `147:26`, `147:34`×2, `153:34`×2 | idéntico en `resolve_group` (`getgrnam_r`) | ídem con `/etc/group` |
| `342:25` | `ensure_cgroup`: `!= AlreadyExists` → `==` | `create_dir_all` devuelve `Ok` cuando el directorio ya existe; `AlreadyExists` sólo aparece en una carrera real entre dos supervisores. No reproducible a voluntad |

**Camino de salida (post-alpha, ~45 min):** extraer el bucle de reintento a un
helper genérico `fn grow_until_cap(len: usize, cap: usize) -> Option<usize>` y
probar sus tres ramas (`*2`, `>= cap`, `None` al pasarse) sin invocar NSS. Eso
mata 10 de los 11. El `342:25` es equivalente de facto: exige dos supervisores
corriendo a la vez sobre el mismo slice, y en ese caso el resultado correcto
(seguir adelante) es el mismo que produce el mutante.
