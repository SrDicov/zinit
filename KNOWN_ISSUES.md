# KNOWN ISSUES (honest list)

Everything here was observed in a VM, not reasoned about. Each entry
says what breaks, what does not, and what replaces the missing piece.

## Demand-start (implemented)

A required edge to a `desired=down` service now fires pull-on-demand:
when a service that declares such an edge is actually running on a
pass, every **disabled** (`enabled = false` at load) dependency that
edge points at becomes `Wanted::Up` and joins the same pass. Mirrors
dinit\'s `depends-on` activation. Operator-stopped services (desired=Down
on an `enabled=true` one) are never re-armed, so `zctl stop` still wins.

Observed cause (Artix): live `dep_add` edges `agetty → getty@tty1..6`
held `agetty`, `system` (`after:agetty`) and `boot`. With the
reconciler\'s pool rule in place (see `Test in
`crates/zcore/src/reconcile.rs::demand_start_single_pass`), the same
chain now converges without any manual `zctl start`: live proof on P1
(binary c11f5fee) where `getty@tty6.conf` carries `enabled = no` on disk
yet runs from boot, pulled by `agetty`'s live required edge. `enabled = no`
off the closure remains the convert policy (it decides the model, not
the run-time rule).

## `on-fatal = reboot` reboots for real now

Power verbs landed (`zctl poweroff|reboot|halt|suspend`, C4, orderly
stop-all + deadline + breadcrumb in `/var/log/zinit-power.log`), so a
`reboot`-marked terminal failure stops every service and reboots through
the same epilogue. `hibernate` stays refused (no resume target).
See `docs/fatal-policy.md`.

## compatd answers NAK where dinit acts

`unload`, `SOFTREBOOT`, `KEXEC` are refused honestly (`hibernate` joins
them: no resume target). Shutdown verbs now act: HALT/POWEROFF/REBOOT go
through the supervisor (orderly stop-all + deadline + breadcrumb), and
`reboot`/`poweroff`/`halt` binaries work again via the socket. TERM to
PID 1 stays stop-all-stay-idle by design (a Ctrl-C storm must never
power the machine off); power is an explicit verb, never a signal.

## Probe-style scripts read "held", not "completed"

`type = script` + `restart = never` exiting 0 is held as suppressed
(the `completed` marker is oneshot-only), so self-test services like
`probe` show `stopped desired=up` after doing their job (`PROBE_DONE`
written, exit 0). Benign test artifact, not a failure. Audit rule:
`restart = never` + `restarts=0` + suppressed + no failure = done.

## Platform recipes, not bugs

- Void (dracut): the guest has no net service of its own, so boot
  with `ip=dhcp rd.neednet=1` — dracut leaves the interface up and
  ssh works. Without it the machine boots fine with no address.
- Chimera: serial-console login never worked (root password unknown,
  ssh-key only). Use ssh. Seasoning note: `sshd.conf` must not carry
  convert-stamped `enabled = no` — strip it at seasoning or ssh dies
  on the next reboot (observed 2026-10-16, fixed by stripping).
- Alpine: OpenRC → zinit conversion is future work. Supported claim
  is "zinit boots as PID 1" (5 services, serial+sshd), nothing more.
- Slirp: hostfwd delivers to the guest's `.15`. Static configs must
  target `10.0.2.15/24` via `10.0.2.2`.

## Live-Chimera findings (2026-10-16, coexistence, sup alongside dinit)

- Fast-dying child goes stale (single occurrence, NOT reproduced):
  synthesized `agetty-service@tty1` (pid 3899/4287/4379) was reported
  `Running` with dead pids, `restarts = 0/3`, no log. Since then:
  demand-pull + instant-death + instance + C4-wake + direct-exec were all
  reproduced live and in CI (`demand_pulled_instant_death_keeps_flapping`,
  green) and all flap correctly; getty instances synthesized later run
  healthy for 15+ min. A reap-path audit (Oct 2026) showed reap-before-
  track is mechanically impossible (SIGCHLD blocked, nothing reaps between
  fork and track) and pointed at the forking hold-window
  (`on_waitpid(forking, unadopted)` holds the death while `cleanup()` wipes
  state) or a lookup-miss discard. Needs the directed root-cause session,
  not more documentation.
- compatd v7 vs dinit 0.22.0 clients: CLOSED. The RC=124 hangs were
  head-of-line blocking (single-threaded accept + one silent holder),
  fixed by one-thread-per-connection with shared handles (generator
  d9a5c2a). Proven live twice: real `dinitctl is-started/status` answer
  instantly. Open minor: after the unexplained 12:44 reboot compatd was
  found stopped (desired=down, restarts=0) with the socket file present
  but unserved; manual `zctl start` restored it. Why it was down is
  unknown (operator action during the poweroff tests, or boot ordering).
- Runtime-activated services: HALF CLOSED by evidence. rtkit and
  power-profiles-daemon run healthy under zinit PID1 (restarts 0),
  pulled at runtime — the dbus-Exec + compatd path works. Only
  bluetoothd stays stopped until something requests org.bluez (same
  path, not yet observed firing). No provider to build; the remaining
  work is observing one activation, not implementing one.

## Soak scope

PSS/FD/zombie numbers are the `zinit --sup` process alone (PID 1 is
a separate process, children excluded). The 2026-10-16 soak runs a
pre-P0 binary (PSS baseline 948 kB fresh); P0 touched only
transition arms, but the number belongs to the old build until the
soak restarts on the release binary.

## 2026-10-16 ~20:00Z — REMOVAL ATTEMPT SELF-LOCKOUT (open)
- The PID1-aware power shim was deployed to /sbin/poweroff (and friends),
  but the machine is OFFLINE and unresponsive 10+ min after `doas /sbin/reboot`.
  Root cause suspected: the shim's zinit branch calls `/sbin/zctl reboot`,
  which needs the control socket; under dinit PID 1 (the default boot)
  /sbin/zctl has no supervisor to talk to, so it exits 1 and the shim did
  nothing. The fallback to the original dinit applet was added too late
  (only after the reboot had already been issued).
- Status: box unreachable via SSH/tailscale/LAN; needs physical power cycle.
- Recovery on next boot (dinit default): SSH in, restore the original
  applets from /root/dinit-orig/sbin/shutdown.real, remove the
  compat-service pins, and only retry removal with a cold-boot-into-zinit
  verification before any power action.
- Lesson recorded: never replace the power path while the machine may still
  boot into dinit; test the shim under BOTH inits before relying on it.

## 2026-10-16/17 — Cerrado en esta noche

- **Un supervisor por socket (cerrado)**: dos supervisores sobre el mismo
  socket era una carrera, no una configuración; el segundo robaba el path y
  los clientes del primero acababan hablando con otro. Ahora hay un
  `flock` exclusivo sobre `<socket>.lock` y el segundo se aparta con el
  código de salida 11, que PID 1 lee para no relanzar (la máquina ya está
  gobernada por el otro). Un supervisor muerto libera el cerrojo: lo
  libera el kernel, no el código.
- **Un supervisor que vuelve arranca copias de todo (cerrado)**: al
  morir, sus hijos quedaban reparentados a PID 1, que no sabe nada de
  ellos, y el supervisor nuevo arrancaba un segundo copy de cada servicio
  (pasó en vivo el 2026-10-16: dos daemons de cada cosa peleándose por
  los puertos). Ahora el supervisor adopta los que siguen vivos: el estado
  visible dice qué pid tenía cada servicio, y un pid vivo de un servicio
  que el plan todavía nombra se adopta (vigilado con pidfd, señalado por
  pid, sin arrancar una copia).
- **`kill -TERM 1` no apagaba la máquina (cerrado)**: la señal se
  encolaba y se ignoraba. Ahora el handler sólo pone una bandera (en
  contexto de señal no se puede hacer nada más) y el bucle de PID 1 la
  convierte en un verbo `poweroff` por el socket de control: el poder
  sigue teniendo un solo dueño, el supervisor.
- **Configuración rota = máquina girando (cerrado)**: el supervisor
  moría, PID 1 lo relanzaba, moría otra vez, y la máquina se quedaba sin
  socket, sin servicios y sin manera de entrar. Ahora hay modo
  emergencia: una shell en `/dev/console` (escrita como una descripción
  normal y cargada por el mismo parser) y el socket de control en pie,
  para que `zctl reload-all` recoja el plan real cuando el fichero esté
  arreglado. `zctl emergency` la pone a demanda.
- **El estado visible mentía (cerrado)**: los ficheros de servicios
  borrados seguían ahí diciendo que estaban corriendo, y se escribían
  en sitio (un lector podía ver medio fichero nuevo y medio viejo). Ahora
  se escriben con rename (atómico) y los de servicios que ya no están en
  el plan se borran.
- **`suspend` respondía después de despertarse (cerrado)**: la escritura
  a `/sys/power/state` no vuelve hasta que la máquina despierta, así que
  el cliente esperaba dormido. Ahora la respuesta ("suspending") sale
  antes, y el error (si el kernel lo rechazó) llega después.
- **Doble superficie de autenticación (cerrado)**: el socket de control
  es 0600, pero cualquier proceso de ese usuario podía mandar `poweroff`.
  Ahora se capturan las credenciales del peer al aceptar (`SO_PEERCRED`):
  leer es para cualquiera, gestionar servicios es del dueño, y apagar la
  máquina es del dueño usando `zctl` (o de root). Código de error nuevo:
  `6`.

## Abierto (espera la máquina)

- **Soak de 24 h**: la caja está apagada; no se puede relanzar el
  muestreo ni medir la pendiente hasta que vuelva.
- **Verificación en vivo del apagado por señal** (`kill -TERM 1`): el
  camino está implementado y con test de regresión, pero el syscall
  final sólo se prueba en la máquina.
- **Timers**: resuelto por la vía de no hacerlos. La máquina trae
  `cronie`/`crond` (evidencia en `z-dinit-generator/evidence/`), así que un
  planificador dentro de zinit sería la misma función dos veces. 

## El filtro de syscalls no filtraba: era `SECCOMP_MODE_STRICT` (2026-10-22)

Lo que se creía una ventaja del proyecto — `syscall-filter = enforce|errno` —
no funcionaba, y no podía funcionar:

- `SECCOMP_MODE_FILTER` estaba escrito como `1`. El valor 1 es
  `SECCOMP_MODE_STRICT`: el kernel instala **su propio** filtro de cuatro
  syscalls y mata el proceso con `SIGKILL` en cuanto intenta `execve`. Todo
  servicio con filtro moría al arrancar; `strace` lo enseñaba como
  `prctl(PR_SET_SECCOMP, SECCOMP_MODE_STRICT)` y `killed by SIGKILL` una
  línea después. El valor correcto es `2`.
- Instalar un filtro sin `CAP_SYS_ADMIN` exige `no_new_privs`, y NNP sólo se
  ponía cuando el servicio cambiaba de identidad, así que el `prctl` fallaba
  con `EPERM` en todos los demás. Ahora NNP se pone siempre que hay filtro:
  un servicio filtrado que aún puede ganar privilegios por un binario setuid
  tiene una puerta que el filtro no ve.
- Faltaban en la tabla y en la base implícita `getpgrp` (111, syscall propia
  en x86_64, no un alias de `getpgid`), `getresuid` (118), `getresgid` (120),
  `wait4`, `waitid` y `vfork` (58). Sin ellas un shell no arranca, no puede
  esperar a un hijo y no puede hacer fork+exec: `dash` respondía
  `Cannot fork` y ningún `type = script` filtrado ejecutaba su comando.

Por qué nadie lo vio: el único test que lo cubría exigía root y se saltaba
en CI (que corre sin privilegios), y además **escribía las tres respuestas en
el mismo fichero** mientras las aserciones leían tres ficheros distintos: no
podía pasar ni como root. Los tres fallos están arreglados y con test que
corre en CI (`a_filtered_service_starts_and_does_its_work`) más el de sandbox
como root (`the_sandbox_really_confines`, verificado en contenedor
privilegiado). Evidencia del arreglo: servicio filtrado en `errno` y en
`enforce` que arranca, escribe su fichero y sale con 0; shell filtrado que
hace `unshare` (EPERM, como debe) y luego escribe su código de salida.

**Límite honesto**: la tabla de números está verificada sólo para
**x86_64 Linux**. En cualquier otra arquitectura un `syscall-filter`
configurado es rechazo de spawn (fail-closed), no un filtro aproximado.
Añadir la tabla de aarch64 es trabajo mecánico con riesgo real (un número
equivocado es un agujero, no una errata): se hará con la misma disciplina,
una tabla y sus números críticos verificados contra el kernel.
