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

## Sesión 3 — T0 cerrado con evidencia y runs paralelos

### ¿Reboot del host? No.

`uptime -s` → `2026-10-22 19:54:56`, sesión de trabajo a las 04:29 del 23
(8h34m de uptime). `last reboot` sólo muestra julio. **El host no se
reinició**: lo que murió fue el servidor de la sesión de opencode, y el run
colgaba de su árbol. tmux con `SID` propio lo resuelve y basta. No se usó
cron: no hay reboot que sobrevivir.

### Paralelo, no en serie

Dos sesiones tmux, cada una con su `CARGO_TARGET_DIR` (el lock de `target/`
era la única razón para serializar) y `-j 2`.

```sh
tmux new-session -d -s mut-zconfig  'cd /home/dicov/zinit && CARGO_TARGET_DIR=/tmp/mut-target-zconfig  cargo mutants -p zconfig  --timeout 30 --no-shuffle -j 2 > /tmp/opencode/mut-zconfig-full.log  2>&1; echo EXIT=$? >> /tmp/opencode/mut-zconfig-full.log'
tmux new-session -d -s mut-zservice 'cd /home/dicov/zinit && CARGO_TARGET_DIR=/tmp/mut-target-zservice cargo mutants -p zservice --timeout 30 --no-shuffle -j 2 > /tmp/opencode/mut-zservice-full.log 2>&1; echo EXIT=$? >> /tmp/opencode/mut-zservice-full.log'
```

Ritmo medido (zservice): **103 evaluados en ~6 min** → 657 en ~40 min.
Frente a las 11-13 h del run en serie con `--jobs` por defecto.

`pkill -f "cargo mutants"` **se mata a sí mismo** (su propia línea de órdenes
contiene el patrón) y tumbó el relanzamiento una vez. Para parar un run:
`tmux kill-session -t mut-zconfig`.

### Parseo y criterio

```sh
grep -m1 'mutants to test' /tmp/opencode/mut-<crate>-full.log
grep -cE '^(MISSED|CAUGHT|TIMEOUT|UNVIABLE)' /tmp/opencode/mut-<crate>-full.log
grep -c '^MISSED' /tmp/opencode/mut-<crate>-full.log
grep '^MISSED' /tmp/opencode/mut-<crate>-full.log | sed 's/ in .*//'
```

### Siguiente trabajo (T1) — `ready.rs`, 46 missed ya listados

Todos con `0s build + 0s test`: son funciones puras, se matan con `#[test]`
normales. Los grupos, por línea del log:

- `drain_notify` (238/229/234/253/262/265): guardas `Interrupted`/`WouldBlock`,
  cotas del buffer, `+=` y negaciones. Un test por rama con un pipe real
  (`pipe2`) y EOF/partial reads.
- `poll_readable` (428/430/438/440/445): `Ok(true)`/`Ok(false)` fijos, `|`
  contra `&`/`^` en las flags, `<` en el retorno de `poll`.
- `new_probe_socket` (480/484/494/495) y `probe_tcp_connect` (565/567):
  flags de socket y `SO_ERROR`.
- `ReadyWait::check` (189) y `run_check_child` (356/357/361): cota de tiempo y
  `waitpid` con `WNOHANG`.
- `PollNeed::is_idle` (79) y `EnvBlock::as_ptrs` (417): dos ramas triviales.

Criterio de T1: `cargo mutants -p zservice --file crates/zservice/src/ready.rs
--timeout 30` → 0 missed, o cada superviviente con razón de equivalencia en
`docs/audit/mutants-zservice.md`.
