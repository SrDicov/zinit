# STATE — mutantes (T0→T4)

## T0 — persistencia (resuelto y verificado)

El fallo no era el host: era que el run colgaba del árbol de la sesión de
opencode, y al reiniciarse el **servidor de la sesión** se llevó el proceso.
En este host no hay systemd (`systemctl` no existe), así que la opción es tmux
con sesión propia y loop de reintento:

```sh
tmux kill-session -t mutants 2>/dev/null
tmux new-session -d -s mutants 'cd /home/dicov/zinit && for p in zconfig zservice; do log=/tmp/opencode/mut-$p-full.log; n=0; while [ $n -lt 5 ]; do n=$((n+1)); cargo mutants -p $p --timeout 60 --no-shuffle > $log 2>&1; grep -q "mutants tested" $log && break; sleep 60; done; done; touch /tmp/opencode/mutants.done'
```

Verificación: el pane tiene `sid` propio (21198) distinto del shell que lo
lanzó (21193), y el servidor tmux se desacopla del padre. Comprobado el
2026-10-23 03:57 con `Found 776 mutants to test` y baseline `ok`.

Sobrevive a la muerte de la sesión de opencode, que es el fallo real y
repetido. **No** sobrevive a un reboot del kernel: en este host no hay
servicio de arranque donde engancharlo (sin systemd) y no se toca el crontab
del usuario. Si el host se reinicia, se relanza: el loop lo detecta porque
el log no termina en `mutants tested`.

Los dos crates van **en serie** a propósito: `cargo mutants` comparte
`target/` y dos runs a la vez se pelean por el lock.

## Parseo

```sh
grep -E '^[0-9]+ mutants tested' /tmp/opencode/mut-zconfig-full.log
grep '^MISSED' /tmp/opencode/mut-zconfig-full.log | sed 's/ in .*//'
grep -c '^TIMEOUT' /tmp/opencode/mut-zconfig-full.log
ls /tmp/opencode/mutants.done
```

## Lo medido hasta ahora

- `zcore`: 31/204 = 15,2 % (cerrado, `docs/audit/mutants-zcore.md`).
- `zservice`: 89 MISSED de 99 evaluados en el run cortado. El grueso es
  `spawn.rs`.
- `zconfig`: 22 MISSED del run anterior; el de esta sesión arrancó con 776.

## Cobertura de `spawn.rs` sin root

`crates/zinit/tests/supervises_real_service.rs::
the_child_path_runs_with_privileges_inside_a_user_namespace` re-ejecuta el
test bajo `unshare -U -r -p -f --mount-proc` y comprueba dos efectos
observables: `CapBnd` a cero tras `drop-capabilities = all` y `unshare -m`
rechazado por el filtro. Probado por mutación: con `child_step_capdrop`
anulado el test falla con `CapBnd: 000001ffffffffff`.

**El paso de ids no es testeable así, comprobado**: `setgroups` devuelve -1
dentro del userns en las tres variantes probadas
(`--map-groups=0:0:1` → el kernel rechaza el rango;
`--map-users=0:1000:1 --map-groups=0:1000:1` y `--map-groups=0:1000:1` →
`uid=0 gid=0` pero `setgroups -1`, porque `unshare -r` deja
`/proc/self/setgroups` en `deny`). Sin CAP_SETGID en el ns padre no hay
forma de habilitarlo, así que el orden `capdrop` antes de `ids` sólo se
puede verificar con root real.
