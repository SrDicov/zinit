# Mutantes de `zservice` — medición parcial y plan

Run: `cargo mutants -p zservice --timeout 60 --no-shuffle` (657 mutantes según
`--list`). Interrumpido con **89 MISSED de 99 evaluados**: la tasa completa
sigue sin medirse, y 89/99 no se extrapola a 657.

## Desglose por archivo (lo evaluado)

| archivo | MISSED |
|---|---|
| `crates/zservice/src/ready.rs` | 46 |
| `crates/zservice/src/log.rs` | 31 |
| `crates/zservice/src/identity.rs` | 11 |
| resto | 1 |

**Corrección.** Una nota anterior atribuía el grueso a `spawn.rs`. El desglose
del log dice otra cosa: `spawn.rs` no aparece en la lista. `ready.rs` y `log.rs`
son funciones con entrada y salida normales — se testean sin root, sin fork y
sin supervisor. El diagnóstico cómodo era el equivocado.

## `spawn.rs`: lo que sí tiene red

`crates/zinit/tests/supervises_real_service.rs::
the_child_path_runs_with_privileges_inside_a_user_namespace` re-ejecuta el
test bajo `unshare -U -r -p -f --mount-proc` y verifica dos efectos
observables: `CapBnd` a cero tras `drop-capabilities = all`, y `unshare -m`
rechazado por el filtro. Probado por mutación: anulando `child_step_capdrop`
el test falla con `CapBnd: 000001ffffffffff`.

## Lo que no es testeable sin root, con evidencia

El paso de ids (`setgroups`) y el orden `capdrop` antes de `ids` exigen root
real. Tres invocaciones probadas el 2026-10-23:

| invocación | uid/gid dentro | `setgroups` |
|---|---|---|
| `-U -r --map-groups=0:0:1` | el kernel rechaza el rango | n/a |
| `-U --map-users=0:1000:1 --map-groups=0:1000:1` | 0/0 | **-1 (EPERM)** |
| `-U --map-groups=0:1000:1` | 65534/0 | **-1 (EPERM)** |

`unshare -r` deja `/proc/self/setgroups` en `deny`; habilitarlo exige
`CAP_SETGID` en el namespace *padre*, que es lo que un runner no tiene. No es
una limitación de zinit.

Plan: test root-gated (patrón de `private-tmp`) con `user = nobody` +
`drop-capabilities = all`, que exige `Running` con `CapBnd=0` y uid 65534 —
invertir el orden lo mata con EPERM —, cableado al job que corre como root
(podman `--privileged`), nunca al job no-root. Hasta entonces no se declara
cubierto: un test que no se puede ejecutar aquí no es evidencia.

## Siguiente paso, en orden de coste

1. `ready.rs` (46): funciones puras; un test por rama. El más barato de los tres.
2. `log.rs` (31): rotación/line-splitting; unitario.
3. `identity.rs` (11): comparación de credenciales; unitario.
4. `spawn.rs`: sólo con root, como queda dicho.
