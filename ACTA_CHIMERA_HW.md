# ACTA — zinit como único init en PC real con Chimera Linux

Fecha: 2026-10-16. Máquina: `dicov` (x86_64, WiFi wlp2s0, btrfs subvol=@).
PID1 final: `/sbin/zinit --init`. Estado final: 24 corriendo / 72 parados,
0 failed, 0 zombies, WiFi+NTP+sshd+escritorio sanos.

## 0. Punto de partida

- Chimera Linux, kernel 7.2.2, dinit 0.22.0 como PID1, 76 servicios dinit.
- Red por WiFi (NetworkManager + iwd), sesión gráfica con labwc vía getty+`~/.profile` (sin display manager), `doas` sin password para dicov.

## 1. Actualización del sistema (pre-trabajo)

- `apk update && apk upgrade`: 10 paquetes, kernel 7.2.2 → 7.2.11 (initramfs + grub regenerados).
- Reboot verificado: kernel activo 7.2.11 en ~20s. Sin esto, todo lo demás
  se habría probado sobre un kernel viejo.

## 2. Despliegue inicial y conversión (convivencia P1)

- Binarios CI-verde rama `testing`: `zinit`+`zctl` musl estáticos
  (imprescindibles: Chimera es musl; el primer `zdg` compilado contra
  glibc ni siquiera ejecutaba — se detectó al vuelo, se recompiló con
  `--target x86_64-unknown-linux-musl`, estático).
- `z-dinit-generator start`: 76 nombres dinit → 90 confs en
  `/etc/zinit/services.d`. `zinit check` exit 0 (21 warnings W003, 0 errores).
- Supervisor en convivencia (`zinit --sup` como hijo de dinit): 8–9
  corriendo, 0 failed. Paridad calculada fichero a fichero contra
  `dinitctl list` capturado ANTES: 70/76 estáticos; los 6 restantes son
  instancias dinámicas `agetty-service@ttyN` (las crea el script
  `/usr/lib/dinit-agetty` en runtime, no existen como ficheros).

## 3. Bug real #1 — clausura boot leía el `waits-for.d` equivocado (z-dinit-generator 47e8949, CI verde)

- Síntoma: `sshd`, `networkmanager`, `tailscaled`, `agetty` salían con
  `enabled = no` pese a correr bajo dinit.
- Causa raíz: `boot` declara `waits-for.d: /etc/dinit.d/boot.d`, pero el
  conversor prefería `in_dir.join(basename)` → leía
  `/usr/lib/dinit.d/boot.d` (decoys: elogind, tmpfiles-clean, turnstiled,
  udevd). Reproducido en local con los dirs reales del PC.
- Fix mínimo: valor absoluto → path literal; relativo → resolución vieja.
  Test `absolute_dir_value_beats_same_named_subdir`. Diff 40/1.
- Prueba en vivo: reconversión dejó los tres sin `enabled = no`.

## 4. Bug real #2 — compatd monohilo: un holder silencioso colgaba todo dinitctl real (8c97ba3→46d33bf→d9a5c2a, CI verde)

- Síntoma: `dinitctl is-started/status` real (0.22.0) colgado (RC=124),
  cero bytes. El shim C4 respondía al instante.
- Descarte con evidencia: protocolo byte-exacto (subagente contra
  control-cmds.h de dinit 0.22.0: sin mismatch); strace al cliente mostró
  `write(QUERYVERSION)` + bloqueo en `read`.
- Causa raíz doble, ambas probadas en vivo:
  a) `cmd_compatd` servía conexiones estrictamente en serie; un
     `dinitctl add-dep` atascado (fd 4 abierto, wchan
     `unix_stream_read_generic`) aparcaba `serve_one` en lectura para
     siempre → todos los clientes posteriores al backlog.
  b) Primer intento de fix (mapa de handles por conexión) rompió
     semántica upstream que el e2e exige ("fresh socket, same handles":
     CATLOG colgó en keys). Corrección: hilos por conexión +
     `Arc<Mutex<Handles>>` global (contador + nombres compartidos).
- Tests: `silent_holder_does_not_wedge_next_client` (unit 62/62) + e2e
  `test-proto.py` 40/40. Prueba viva: `dinitctl is-started sshd` responde
  al instante (`STOPPED`, RC=1) y el script dinit-agetty sintetiza
  instancias que corren sanas (agetty tty6 15min, restarts 0).

## 5. Falsa pista documentada — `%i` y el heredoc sin comillas

- Un probe mostró `args=` vacío e inculpó la expansión `%i`. strace al
  sup demostró `execve(..., ["flap.sh", "tty1"])`: la expansión siempre
  funcionó; el probe estaba roto (heredoc sin comillar comió `$*` al
  escribir el fichero). Lección anotada, una hora perdida una vez.

## 6. Incidente de red (culpa mía, mecanismo entendido)

- Reiniciar el sup en convivencia con leaves habilitados levantó doble
  NetworkManager → pelea DHCP → ruta intermitente (.3↔.7) + dosKills de
  ssh a mitad de comando. Tailscale salvó la sesión.
- Regla impuesta desde entonces: pins `enabled = no # coex-pin` en todo
  lo que dinit sirve ANTES de arrancar el sup (respetados por
  reconversiones). `zctl stop` no basta (Desired se preserva; el fresco
  arranca igual).

## 7. Primer boot PID1: dos trampas antes del éxito

- Trampa A (busybox): `sed -i` sin sufijo consume el script como backup
  → mi `unpin.sh` no quitó pins. Reescrito en Python. Misma trampa había
  roto un debug anterior (`invalid command code z`).
- Trampa B: `reboot` habla con PID1 → compatd NAKea shutdown por diseño
  → reboot abortado. Handoff documentado y ejecutado: `zctl stop
  compatd` + `rm /run/dinitctl` + `kill -USR1 1` (dinit 0.22.0 reabre su
  socket; `dinitctl is-started sshd` = STARTED instantáneo como prueba)
  + `reboot` ordenado.
- Boot bueno en 15s por grub one-shot (default intacto en dinit; el primer
  intento ni siquiera llegó a bootear: el NAK abortó el reboot y la máquina
  se quedó en dinit con el one-shot armado).

## 8. Bug real #3 (crítico) — NNP incondicional mataba `doas` en todo el árbol (zinit 273886b, CI verde)

- Síntoma post-boot: `doas: not installed setuid` en TODAS las sesiones
  (ssh y consola), sin root por ningún canal remoto. SUID intacto en
  disco, mount sin nosuid.
- Causa raíz: `child_step_no_new_privs()` corría para TODOS los servicios
  (`NoNewPrivs: 1` heredado hasta las sesiones; el kernel ignora setuid).
- Fix mínimo: solo tras drop real de uid (`if p.drop_ids`), que es cuando
  la defensa tiene sentido. Un servicio root no tiene a qué escalar.
- Test `a_root_service_keeps_setuid_working` (falla en código viejo,
  pasa en nuevo) + docs (spawn.rs, DESIGN.md §8/§9.1).
- Probado en vivo: compatd NNP=0, sesiones NNP=0, `doas` OK bajo zinit PID1.

## 9. WiFi bajo zinit

- Causa: `iwd` con `enabled = no` (clausura) y nada lo pedía → `wlp2s0`
  DOWN. Un comando con root (`zctl start iwd`) + NM autoconecta.
- Fijado duradero: `enabled = yes # operator: laptop wifi backend` en
  `iwd.conf` (pin respetado por el conversor; se verificó que la clausura
  lo re-añadía sin pin). WiFi con .3 bajo zinit confirmado.

## 10. Bug real #4 — `pipefd:` de dos dígitos + busybox ash (generator 6595dee, CI verde)

- Síntoma: `chronyd` en loop (160+ restarts), único log: `exec: 11: not found`.
- Bisecado en vivo hasta trío mínimo: `log = file:` + `ready = notify` +
  redirect a fd fijo. El `/bin/sh` del PC (busybox ash) NO acepta fds de
  dos dígitos en redirecciones (`exec 11>&3` → `exec: 11: not found`;
  probado: fd 9 OK, fd 11 muere).
- El conversor emitía `exec N>&$ZINIT_NOTIFY_FD` para `pipefd:N`. Fix:
  si N viaja como argumento suelto (`-N 11`), se reescribe a
  `-N $ZINIT_NOTIFY_FD` y se salta el dup; un dígito sin cambios;
  fallback con warn ruidoso. Test
  `pipefd_two_digit_fd_passes_through_var`.
- Verificado en vivo ANTES de codificarlo: `chronyd -n … -N 3` corre,
  notifica, `chronyc tracking` Stratum 3. El generador ahora emite
  exactamente eso (comprobado byte a byte contra la conf viva).

## 11. Autologin + labwc directo

- Ya existía y funcionaba: `/etc/default/agetty-tty1`
  (`--autologin dicov`, leído por el wrapper `agetty-default`) y bloque
  de arranque en `~/.profile` (labwc 884 hijo de la shell de login).
- Añadidos: `--noclear` y guarda `pgrep` anti-doble-compositor.
  Sin display manager a propósito. Sesión actual intacta (nada reiniciado).

## 12. Apagado: estado honesto (no cerrado)

- `loginctl poweroff` se acepta (RC=0) pero la máquina sigue arriba 60s+:
  elogind lo agenda y se atasca en ejecución. Sin inhibidores de bloqueo,
  sin referencias a dinit en su binario, bus operativo.
- Funcionan hoy: SysRq+`o`/`b`, botón físico, y (tras el fix NNP) todo lo
  que sea userspace con root. `reboot`/`poweroff`/`halt` a secas NO
  funcionan (hablan con dinit ausente; NAK por diseño).
- Hibernar: imposible estructural (swap solo zram, sin `resume=`; igual
  con dinit). Suspender: sin probar.

## 13. Estado final y restos

- zinit PID1 único del sistema; `dinit --user` per-sesión y paquete
  dinit como proveedor de ficheros (intencionado).
- Brillo al máximo re-aplicado tras reboot; strace desinstalado (715
  pkgs); `/tmp` y sups de prueba eliminados.
- Una ocurrencia de `Running` con pid muerto (`agetty-service@tty1`,
  era pre-thread) nunca reprodujo en 4 réplicas + test CI en verde;
  queda documentada en KNOWN_ISSUES, no cerrada por repetición.
- Commits: zinit `273886b` (+docs), generator `1aff7f5`. Todo CI verde.

## 14. Lecciones de campo (para no pagarlas dos veces)

- busybox `sed -i` necesita sufijo o falla críptico; preferir Python.
- Heredocs sin comillar + ssh comen `$*`/`$$`; comillar siempre.
- `/usr/local/bin` no está en el PATH por defecto de Chimera (el shim
  `dinitctl` no sombrea; documentado, no forzado).
- `reload-all` preserva Desired: no sirve para bajar leaves; `stop` sí,
  y pins para arranques frescos.
- `grub-reboot` one-shot + default intacto = red de seguridad que se usó
  de verdad (el boot fallido volvió solo a dinit... nótese: el primer
  intento abortó antes por NAK, no por fallo de boot).
- tailscale como segundo camino salvó dos wedges de LAN.
