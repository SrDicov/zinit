# STATUS — 2026-10-22 (noche)

Rama: `agent/overnight-20261022-0332` (fast-forward a `testing` en cada
commit). Base: `2f449e3`.

## Hecho (con evidencia)

| # | Ítem | Commit | Evidencia |
|---|---|---|---|
| C1 | `zctl emergency`: shell en la consola con el plan sano | `d228feb` | test de regresión del verbo + camino de código |
| C2 | `suspend` responde antes de dormirse | `56fea77` | test que falla con el código viejo (`E ... suspend failed` en vez de `R 1 0 suspending`) |
| C3 | Tope de clientes en el socket de control | `7cd13f1` | test que abre más conexiones que el tope y exige que el supervisor siga vivo |
| C4 | El supervisor re-adopta los servicios vivos | `a9dc307` | test que mata el supervisor y exige que el nuevo NO arranque una copia; falla con el código viejo (pid distinto) |
| C5 | Estado visible honesto (atómico + poda) | `3597c57` (previo) | test: el fichero de un servicio borrado desaparece tras el reload |
| — | `kill -TERM 1` apaga la máquina | `62b8397`…`20c91d4` | test de la conversión señal→verbo `poweroff` |
| — | Modo emergencia (config rota → shell + socket) | `a2b3a64` | test: config rota → el supervisor no muere, el socket responde, `reload-all` recoge el plan real |
| — | Cerrojo de instancia única (flock) | `b816d19` | test: el segundo supervisor se aparta con el código 11; otro: un supervisor muerto deja el socket libre |
| — | devtmpfs, PATH heredado, `RLIMIT_NOFILE`, estado atómico | varios | Gate local: build + clippy + suite verdes |
| — | Test de PID 1 real en namespace | `2d1517d` | `unshare --user --map-root-user --pid --fork`: PID 1 es zinit, el servicio arranca, el namespace se limpia |
| — | Test del sandbox (seccomp + capdrop) | `f7b6a93` | la misma operación con y sin confinamiento: sin él pasa, con él falla |
| C6 | Escala: 500 servicios | `6030459` | 622 ms hasta converger, 4,0 MB RSS, 1008 fds, 1 hilo, 0 zombis (`docs/audit/baseline.md`) |
| C7 | Dos `stop` simultáneos | `a578607` | ambos responden, el servicio muere una vez, estado `Stopped`/`Down` |
| C8 | Un servicio que se para se lleva su grupo | `7b990e3` | reproducido en contenedor Ubuntu (dash): el hijo directo moría al primer `SIGTERM` y los nietos quedaban vivos con el servicio dado por parado. Era el fallo que llevaba la suite de CI en rojo desde el guard de parada |
| C9 | `syscall-filter` no filtraba | `375641d` + `94a533f` + `46e0d5f` | `SECCOMP_MODE_FILTER` valía 1 (`STRICT`): todo servicio filtrado moría con `SIGKILL` en el `execve`. Además NNP no se ponía sin cambio de identidad y la base implícita no cubría un shell (faltaban `getpgrp` 111, `getresuid` 118, `getresgid` 120, `wait4`, `waitid`, `vfork` 58). Test en CI para x86_64 + sandbox como root verificado en contenedor |
| C10 | Ctrl-Alt-Del reinicia | `f1e5968` | `SIGINT` a PID 1 pedía `poweroff`; el contrato del kernel es reiniciar. Test de los dos verbos por el socket falso |
| C11 | `syscall-allow` de extremo a extremo | `476b10a` | mismo comando: sin filtro 0, filtrado rechazado, filtrado con `syscall-allow = kill` 0 |
| C12 | Log que no se puede abrir = rechazo | `ce967ab` | `ENOTDIR` en el camino del log: el servicio no corre y el supervisor sobrevive |
| C13 | En BSD/macOS el supervisor no oía señales | `22ddfde` | `EVFILT_SIGNAL` leía `fflags` en vez de `ident`: `SIGTERM` se ignoraba en silencio. Evidencia: el log del job macOS sin una sola línea de parada y con el servicio `Running`; tras el arreglo, macOS converge |
| C14 | El test de parada y el de credenciales son honestos | `7f47ec7` + `9bf8717` | la parada espera **convergencia** (no un pid concreto) y el rechazo de `poweroff` se afirma sólo donde hay `/proc` para probarlo; la plataforma sin `/proc` se anuncia al arrancar |
| C15 | La suite dice por qué falla un cliente | `ef9e3cd` | el supervisor escribe su log en `<scratch>/sup.log` y el test de reload falla citando sus últimas líneas (antes: `WouldBlock` a secas) |
| — | Un reload no reinicia lo que ya dijo READY | `5c474cc` | `ready = notify` de extremo a extremo (nunca se había probado): tras `reload-all` el pid es el mismo y sigue `Running` pasado el `start-timeout`. Cierra el sospechoso del bucle de chronyd |

Suite: verificada **sólo por CI** desde la orden de no compilar en este PC
(último run: `39890406294`, `success` en toda la matriz Linux glibc/musl ×
x86_64/i686/aarch64, macOS aarch64, los `check` de BSD, msrv 1.85, purity,
size y los drivers). El único job rojo es `check
riscv32gc-unknown-linux-gnu` (std de nightly, `SYS_futex`): ajeno y
documentado. `zinit` release = **676 KiB** (presupuesto 1 MiB).

## Bloqueado (necesita la caja)

- **B1** Desplegar el binario nuevo (artefacto musl de CI) en la caja y
  verificar md5 + salud.
- **B2** Soak de 24 h con el binario final y `soak-verdict.md`.
- **B3** Verificar en vivo `kill -TERM 1` → apagado real (el camino está
  implementado y con test; el syscall final sólo se prueba en la máquina).
- **B4** ~~¿Trae la distribución `cron`?~~ **Resuelto sin la caja**: sí,
  `cronie`/`crond` (evidencia en `z-dinit-generator/evidence/`), así que los
  timers de zinit serían la misma función dos veces. Descartados.

La caja Chimera está **apagada** (sin red, sin SSH). Todo lo anterior
espera a que vuelva.

## Decisión necesaria (del usuario)

1. **Encender la caja** para B1-B4 y el soak.
2. **Timers**: ya no hay decisión que tomar — la máquina trae cron y se
   descartan con evidencia (B4).
3. **Grub**: el default sigue siendo dinit. El cambio a zinit es decisión
   del operador y espera al soak verde.

## Siguiente (cuando la caja vuelva)

```sh
# 1. artefacto musl del último CI verde
gh run download <run> --name zinit-musl-x86_64 --dir /tmp/zinit-musl
scp /tmp/zinit-musl/zinit dicov@100.88.252.35:/tmp/zinit-new
# 2. instalar (rename: el binario en uso no se puede sobrescribir)
ssh ... 'doas cp /tmp/zinit-new /sbin/zinit.new && doas mv /sbin/zinit.new /sbin/zinit \
          && doas cp /tmp/zinit-new /usr/bin/zinit.new && doas mv /usr/bin/zinit.new /usr/bin/zinit'
# 3. reiniciar por one-shot de grub (nunca `kill` al supervisor: es un
#    "stop everything" y deja la máquina sin red)
ssh ... 'doas grub-reboot gnulinux-zinit-44a208a1-e985-43c1-a5f8-0cec92d13414 && doas reboot'
# 4. verificar
ssh ... 'cat /proc/1/comm; md5sum /sbin/zinit; doas /sbin/zctl list'
```

## Deuda remanente

- `// ponytail:` en `crates/zcore/src/types.rs` (bucket por ventana) y en
  `crates/zinit/src/sup.rs` (`index_of` O(servicios²) en el epílogo de
  potencia): ambos están anotados y son aceptables a esta escala.
- El job `check riscv32gc-unknown-linux-gnu` falla por el std de nightly
  (`SYS_futex`): no es código nuestro, se documentó y no se tocó.
- La remoción completa de dinit (fase siguiente): falta el reemplazo de
  `devmon` (activación por dispositivo, que hoy viene del paquete de
  dinit) y congelar los conf convertidos para dejar de depender de
  `/usr/lib/dinit.d`.
