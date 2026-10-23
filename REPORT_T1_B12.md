# REPORT T1-B12 — los 5 timeouts de drain_notify

**Status: SUCCESS**

## Mutantes muertos (verificados por mutación manual, SPEC paso 4)
- 238:23 guard `Interrupted`→`true`: SIGABRT (sentinel, bucle en WouldBlock)
- 238:32 `==`→`!=`: SIGABRT (sentinel, bucle en WouldBlock)
- 262:15 `+=`→`*=`: SIGABRT (sentinel, bucle en el parse de líneas)
- 262:22 `+`→`-`: panic underflow en `ready.rs:262` (rápido, sin sentinel)
- 262:22 `+`→`*`: SIGABRT (sentinel, bucle en el parse de líneas)
Todos con `cargo test -q -p zservice --lib drain_notify` fallando bajo mutante,
exit 101 (5 de 5; no queda ningún timeout conocido del log mut-ready2.log).

## Mutantes ignorados
Ninguno. No hizo falta `#[ignore]`: el sentinel (`abort()` a los 5s si
`drain_notify` no vuelve) convierte el cuelgo en crash, que el mutador cuenta
como falla. Condicional: el mutador debe clasificar SIGABRT como caught; así
se observó exit≠0 en cada mutante reproducido a mano.

## Tests añadidos (crates/zservice/src/ready.rs, `mod tests`)
- `drain_notify_returns_wouldblock` — pipe nonblock vacío = "nada todavía".
- `drain_notify_retries_on_eintr` — `alarm(1)` + SIGALRM real (sigaction sin
  SA_RESTART) interrumpe el read bloqueante; el escritor escribe después;
  payload `\nREADY=1\n` (línea vacía delante cansa a los mutantes de `+=`).
- `hang_sentinel()` — watch-dog de 5s que aborta si el drain cuelga.
- Limpieza incidental: `#[test]` duplicado + doc huérfano sobre
  `notify_crlf_watchdog…` (preexistente, rompía clippy -D warnings).

## Suite
`cargo test -q -p zservice --lib` → ok. 94 passed; 0 failed (1.50s).
`cargo fmt --all --check` OK. `clippy -p zservice --all-targets` con
`RUSTFLAGS=-Dwarnings` → 0 warnings en zservice. NOTA: queda 1 warning
preexistente en `crates/zinit/tests/supervises_real_service.rs:2069`
(unused_mut), fuera del alcance de este SPEC.

## Commit
`10ea488` test(ready): kill the drain_notify timeouts (sólo ready.rs;
86 inserciones, 3 borrados; rama agent/overnight-20261022-0332)
