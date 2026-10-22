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
| — | devtmpfs, PATH heredado, `RLIMIT_NOFILE`, estado atómico | varios |Gate local: build + clippy + suite verdes |
| — | Test de PID 1 real en namespace | `2d1517d` | `unshare --user --map-root-user --pid --fork`: PID 1 es zinit, el servicio arranca, el namespace se limpia |
| — | Test del sandbox (seccomp + capdrop) | `f7b6a93` | la misma operación con y sin confinamiento: sin él pasa, con él falla |

Suite local: **567 tests, 0 fallos**; clippy `-D warnings` limpio; `fmt`
limpio. CI: los workflows corren en GitHub Actions (el veredicto de la
matriz está en el run del tip `fd4d76f`).

## Bloqueado (necesita la caja)

- **B1** Desplegar el binario nuevo (artefacto musl de CI) en la caja y
  verificar md5 + salud.
- **B2** Soak de 24 h con el binario final y `soak-verdict.md`.
- **B3** Verificar en vivo `kill -TERM 1` → apagado real (el camino está
  implementado y con test; el syscall final sólo se prueba en la máquina).
- **B4** ¿Trae la distribución `cron`? Si lo trae, los timers de zinit
  duplican una función que ya existe y no se implementan (YAGNI).

La caja Chimera está **apagada** (sin red, sin SSH). Todo lo anterior
espera a que vuelva.

## Decisión necesaria (del usuario)

1. **Encender la caja** para B1-B4 y el soak.
2. **Timers**: si la máquina trae cron, se descartan (mi recomendación:
   descartar). Si no lo trae, se implementan en la versión reducida
   (`on-boot-sec` + `on-calendar HH:MM:SS`).
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
