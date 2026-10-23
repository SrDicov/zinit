# SPEC T1-B12 — los 5 timeouts de drain_notify

## Objetivo
Matar o documentar los 5 mutantes que hoy dan timeout en
`crates/zservice/src/ready.rs` (run `/tmp/opencode/mut-ready2.log`:
101 mutantes, 25 missed, 5 timeouts).

## Contexto
`drain_notify` lee del pipe de notificación. Los timeouts son las ramas de
error de `read`/`write` en las líneas ~234/238/239/262/265: `EINTR` y
`WouldBlock`. Base: `cargo test -p zservice --lib` → 93 passed.

## Pasos
1. `grep -n "TIMEOUT" /tmp/opencode/mut-ready2.log` para ver qué mutante
   exacto cuelga. Si el log no basta, reproducir uno a mano.
2. Leer `ready.rs` alrededor de esas líneas. Anotar qué rama muta cada uno.
3. Escribir en el mismo archivo (módulo `tests`) tests que fuercen la rama:
   - `EINTR`: `signal(SIGALRM)`/`setitimer` sobre el fd bloqueante, o el
     syscall de test que devuelva EINTR de forma determinista.
   - `WouldBlock`: fd en modo `O_NONBLOCK` con el pipe vacío.
   Nombres: `drain_notify_retries_on_eintr`, `drain_notify_returns_wouldblock`.
4. Verificar POR MUTACIÓN cada test nuevo (no hay `--mutant` en esta versión):
   ```sh
   F=crates/zservice/src/ready.rs; cp $F /tmp/opencode/bak
   python3 -c "import sys;p='$F';t=open(p).read();open(p,'w').write(t.replace(sys.argv[1],sys.argv[2],1))" 'OLD' 'NEW'
   cargo test -q -p zservice --lib <filtro>   # DEBE fallar
   cp /tmp/opencode/bak $F
   ```
5. `cargo test -p zservice --lib 2>&1 | tail -1` → todo verde.
6. `cargo fmt --all && cargo clippy --all-targets` → 0 warnings.
7. Commit con ruta explícita: `git add crates/zservice/src/ready.rs` y
   `test(ready): kill the drain_notify timeouts`.

## Regla de corte
30 minutos de reloj. Si una rama no se puede forzar sin colgar el test:
`#[ignore = "<motivo exacto>"]`, documentarla en el propio test y seguir con
la siguiente. No entregar un test que cuelgue.

## Si algo se rompe
Revertir el archivo desde `/tmp/opencode/bak`, dejar la suite verde, y
reportar PARTIAL con la línea exacta que no se pudo forzar.

## Prohibido
- `git add -A`, `git add .`, `git commit -a`.
- Tocar `spawn.rs`, `seccomp.rs` o cualquier otro archivo.
- Cambiar comportamiento de producción para hacer el test pasable.

## REPORT (escribir `REPORT_T1_B12.md`, máx 25 líneas)
Status (SUCCESS/PARTIAL/FAILED/TIMEOUT), mutantes muertos (números de línea),
mutantes ignorados con motivo, tests añadidos, resultado de la suite, SHA.
