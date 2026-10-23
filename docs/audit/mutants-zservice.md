# Mutantes de `zservice` — medición parcial y plan

Run: `cargo mutants -p zservice --timeout 60 --no-shuffle` (657 mutantes según
`--list`). El run se interrumpió con **88 de 99** evaluados: la tasa
completa sigue sin medirse, y con $MISS/99 no se puede extrapolar.

## Desglose por archivo (lo evaluado)

```
     46 crates/zservice/src/ready.rs
     31 crates/zservice/src/log.rs
     11 crates/zservice/src/identity.rs
```

## Lectura

Casi todo es `spawn.rs`, y no por falta de tests sino por el diseño: los
pasos viven en el hijo, después del `fork`, y no vuelven al llamante. Un
test unitario no puede alcanzarlos; sólo un supervisor real.

## Lo que ya tiene red

`crates/zinit/tests/supervises_real_service.rs::
the_child_path_runs_with_privileges_inside_a_user_namespace` re-ejecuta el
test bajo `unshare -U -r -p -f --mount-proc` (sin root real) y verifica dos
efectos observables: `CapBnd` a cero tras `drop-capabilities = all`, y
`unshare -m` rechazado por el filtro. Probado por mutación: anulando
`child_step_capdrop` el test falla con `CapBnd: 000001ffffffffff`.

## Lo que no es testeable así, con evidencia

El paso de ids (`setgroups`) y, por tanto, el orden `capdrop` antes de
`ids`, necesitan root real. Comprobado el 2026-10-23 con tres variantes de
`unshare`:

| invocación | uid/gid dentro | `setgroups` |
|---|---|---|
| `-U -r --map-groups=0:0:1` | el kernel rechaza el rango | n/a |
| `-U --map-users=0:1000:1 --map-groups=0:1000:1` | 0/0 | **-1 (EPERM)** |
| `-U --map-groups=0:1000:1` | 65534/0 | **-1 (EPERM)** |

`unshare -r` deja `/proc/self/setgroups` en `deny`; habilitarlo exige
`CAP_SETGID` en el namespace *padre*, que es justo lo que un runner no
tiene. No es una limitación de zinit.

## Plan para cerrarlo

1. Test root-gated (mismo patrón que `private-tmp`) con `user = nobody` +
   `drop-capabilities = all`: el hijo debe llegar a `Running` con
   `/proc/<pid>/status` → `CapBnd=0` y uid 65534. Con el orden invertido
   muere con EPERM en el capdrop, así que el test lo distingue.
2. Cablearlo al job que corre como root (podman `--privileged`), no al
   job no-root.
3. Hasta entonces: **no** se declara cubierto. Un test que no se puede
   ejecutar aquí no es evidencia.
