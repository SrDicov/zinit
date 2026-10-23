# HANDOFF — lo que la siguiente sesión necesita saber

## Un commit con cuatro cambios: `573a9e8`

`573a9e8` ("fix(ctl): a zctl where a zctl is installed…") contiene **cuatro
cambios lógicos**, porque el árbol ya estaba preparado al commitear y el
mensaje describe sólo el primero:

1. `authorize()` exige que el ejecutable del cliente esté en un directorio de
   instalación (o en el del propio supervisor).
2. `peer_exe = None` en Linux pasa a ser rechazo para los verbos de potencia
   (antes concedía: fail-open).
3. `MAX_PROGRAM` en seccomp: el límite de `build` y el de `install` salen de
   la misma expresión (antes un `install` con 206 instrucciones se rechazaba).
4. Tres tests reforzados (`topological_order`, adopción por nombre,
   `degradation_line`) y las correcciones de documentación.

**Auditorías futuras: leer el diff, no el mensaje.** Lo mismo pasó antes con
`7f47ec7` (fix de `authorize` + reescritura del test de parada).

## Lección de proceso

Si al terminar hay más de un cambio lógico sin commitear, **separar antes de
push**, aunque cueste: `git bisect` no lee mensajes, y un commit que miente
sobre su alcance envenena cualquier bisección futura. No se reescribe historia
para arreglarlo; se anota aquí y se corrige el hábito.

## Estado

- Código verificado por CI (matriz completa) y localmente.
- Beta pendiente **sólo** de hardware: `scripts/hw-verification.sh` +
  `docs/HW-VERIFICATION.md`. Sin máquina no hay soak, y sin soak no hay beta.
- devmon sigue en `dinit-base` a propósito (ver `KNOWN_ISSUES.md`).
- El generador (repo aparte) ya no pierde capabilities: `capabilities =` vacío
  → `drop-capabilities = all`; una lista no vacía deja el fichero con la
  directiva verbatim para que `zinit check` lo rechace (E004) en vez de
  convertir en silencio un servicio con más privilegio.

## Regla dura: nada de `git add -A`

Tres commits de esta sesión llevaron ficheros que no eran suyos (`573a9e8`
con cuatro cambios, `7f47ec7` con dos, `f8ac365` con `mutants.out/` dentro).
La regla, desde ya:

- `git add <archivo>` con nombre y apellido. Nunca `-A`, nunca `.`, nunca
  `git commit -a`.
- Un directorio se añade con `git add <dir>/*.rs`, no con el directorio.
- Antes de cada commit, `git status --short` y leer la lista: si hay algo que
  no se pretendía, se saca del índice antes de commitear.
- Si el commit ya salió con basura, se arregla en un commit de limpieza (no se
  reescribe historia) y se anota aquí.
