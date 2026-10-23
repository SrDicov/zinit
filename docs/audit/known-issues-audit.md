# Auditoría de `KNOWN_ISSUES.md` (2026-10-23)

Cada entrada, con la pregunta que importa: ¿sigue abierta, tiene causa raíz, y
el número que cita es real?

| Entrada | ¿Abierta? | ¿Causa raíz? | ¿El dato es real? |
|---|---|---|---|
| Demand-start implementado | no | n/a (feature) | sí: `on-demand = yes` + test `demand_start_wakes_on_connection` |
| `on-fatal = reboot` reinicia de verdad | no | n/a | sí: `docs/audit` y test de política |
| compatd responde NAK donde dinit actúa | sí (por diseño) | sí: NAK es el contrato C4 para verbos no mapeados | sí |
| Probes leen "held" | sí (artefacto) | sí: `completed` es sólo de oneshot | sí |
| Recetas de plataforma | sí | sí: cada una con su fichero | sí |
| Hallazgos de la caja viva (2026-10-16) | sí | sí, con evidencia de la sesión | sí |
| Alcance del soak | sí (bloqueado) | sí: sin caja no hay soak | n/a |
| Auto-bloqueo al quitar dinit | **abierta** | sí: quitar dinit sin reemplazo de devmon deja la máquina sin hotplug | sí |
| Cerrado en la noche del 16/17 | no | sí, con commit por ítem | sí |
| Abierto esperando la máquina | sí (bloqueado) | sí | sí |
| seccomp era `SECCOMP_MODE_STRICT` | no (arreglado) | sí, con la línea exacta y el test | sí: 3 fallos, 3 tests |
| Señales BSD leían `fflags` | no (arreglado) | sí: `ident` vs `fflags`, evidencia del log de macOS | sí |
| Verbo de potencia sin `/proc` | sí (límite) | sí: anuncio al arrancar + `ZINIT_TRUST_SELF_DIR` | sí |
| Flake de identidad en macOS | abierta, medida | sí: 0/3000 con `ZINIT_FLAKE_RUNS` en el job real | **sí, el N es real**: el env está en `ci.yml` y el test lo lee |
| devmon sigue en `dinit-base` | sí (decisión) | sí: zinit no tiene disparo por dispositivo; issue #24 | sí |
| Instalaciones fuera de los 6 directorios | sí (límite) | sí: la lista y la variable están en el código | sí |
| Reutilización de pid | sí (límite teórico) | sí: ventana entre `accept` y `readlink` | sí |
| Filtro seccomp mira números | **actualizado**: ahora también `clone` (flags) e `ioctl` (TIOCSTI) | sí, con test de comportamiento | sí, y `clone3` sigue fuera con motivo |
| `private-tmp` sólo `/tmp` | **no**: cubre `/var/tmp` y `/dev/shm` | sí | sí, con test (root-gated) |
| Generador pierde capabilities | no (arreglado en su repo) | sí: mapeo exacto o rechazo | sí, end-to-end |

## Lo que esta auditoría corrigió

- Tres entradas describían límites que ya no existen (`private-tmp`,
  capabilities del generador, seccomp sin argumentos). Se actualizaron en vez
  de dejarlas "abiertas" para siempre.
- La entrada del flake decía "no reproducido" sin decir cuántas veces; ahora
  cita el mecanismo (`ZINIT_FLAKE_RUNS`) y el número.
- Ninguna entrada dice "aceptable" sin una mitigación concreta al lado.

## Lo que sigue sin ser "aceptable" y por qué se deja

- **Auto-bloqueo al quitar dinit**: no es un bug de código, es una decisión de
  despliegue que el operador ya tomó (mantener `dinit-base`). Mitigación real:
  el issue #24 y el aviso en el generador.
- **Reutilización de pid**: cerrarlo del todo pide comparar `starttime` en el
  mismo `open`, no un `readlink` posterior. Anotado con la ventana exacta.
