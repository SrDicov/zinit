# AUDIT — zinit (noche 2026-10-22 03:32)

Contrato de la noche. Cada ítem termina con commit atómico + gate
(build + lint + tests) y queda marcado aquí. Nada fuera de esta lista.

Contexto heredado de las dos auditorías ya hechas
(`ses_e9df8c412ffeNPkSoFyuPDs4Ih` y `ses_e9df8c410ffe3D3V5BYPtigMkQ`) y del
trabajo del día. Lo que ya está arreglado y verificado **no** se repite.

## Estado de partida

- Rama base: `2f449e3` (sobre `testing`).
- CI: verde salvo `check riscv32gc-unknown-linux-gnu` (error `SYS_futex`
  dentro del std de nightly, no es código nuestro — se documenta, no se
  arregla aquí).
- Caja Chimera: **apagada** (el operador la apagó por accidente con un `kill` al
  supervisor). Todo lo que necesita la máquina real está
  bloqueado por eso.

## Bloqueados (necesitan la caja encendida)

- [ ] **B1** Desplegar el binario nuevo (artefacto musl de CI) en
  `/usr/bin/zinit` + `/sbin/zinit` y verificar md5.
- [ ] **B2** Soak 24 h con el binario final (P4 del encargo) y
  `soak-verdict.md`.
- [ ] **B3** Verificar en vivo el apagado por señal (`kill -TERM 1` → poweroff)
  con el binario nuevo.
- [ ] **B4** Comprobar si la distribución trae `cron`/`crond`: si lo trae,
  los timers de zinit son duplicados de función y no se implementan (YAGNI).

## Ítems de código (orden por impacto ÷ tamaño)

- [x] **C1** `zctl emergency`: verbo que pone una shell en la consola con
  el plan sano. *(hecho: commit `zctl emergency`)*
- [x] **C2** `suspend` responde **después** de despertarse. El comentario del
  código promete lo contrario ("la respuesta ya está en el cable cuando el
  kernel duerme"): el orden está invertido y el cliente espera en el socket
  mientras la máquina duerme. Arreglo: renderizar y vaciar la respuesta
  antes de dormir.
- [x] **C3** El socket de control no tiene tope de clientes: un proceso
  local puede abrir miles de conexiones y el supervisor crece sin límite.
  Tope (64) y se cierra la conexión de más.
- [ ] **C4** **El supervisor re-adopta los servicios vivos al arrancar.**
  Hoy, si el supervisor muere, al volver a arrancar arranca **copias** de
  todo lo que sigue vivo: la máquina queda con dos daemons de cada cosa
  peleándose por los puertos (pasó en vivo el 2026-10-16). El estado
  visible (`$ZINIT_STATE_DIR`) ya dice qué pid tenía cada servicio: basta
  con leerlo al arrancar, comprobar que el pid sigue vivo y **adoptarlo**
  en vez de arrancar otro.
- [ ] **C5** `AUDIT.md`/`KNOWN_ISSUES.md` al día con el estado real
  (incluido el fallo de `riscv32` y la desviación del código de error 6).

## Descartado con motivo

- **Timers de calendario**: dependen de B4. Si la distribución trae cron,
  un segundo planificador es la misma función dos veces.
- **Gramática systemd, journal, D-Bus, `sd_notify` completo, tamperlog**:
  prohibidos por el encargo y sin beneficio en esta máquina.
- **Tocar el sandbox**: no hay evidencia de que necesite cambios; el test
  nuevo (`the_sandbox_really_confines`) lo demuestra tal cual está.

## Reglas de esta noche

- Un ítem, un commit, un gate.
- Nada de refactors de consistencia ni de docs no pedidos.
- Cero dependencias nuevas.
- Nada de `#[allow]` para callar un warning.
- Evidencia real pegada en el informe final; "verde en CI" no es
  comportamiento en vivo.
- 3 fallos seguidos en el mismo bug → hard stop, `STATUS.md`, salir limpio.
