# STATE — mutantes (T1/T2)

## Qué se está midiendo

Dos runs en segundo plano, con `--no-shuffle` y `--timeout 60`:

```sh
cargo mutants -p zconfig --timeout 60 --no-shuffle > /tmp/opencode/mut-zconfig.log 2>&1
cargo mutants -p zservice --timeout 60 --no-shuffle > /tmp/opencode/mut-zservice.log 2>&1
```

## Cómo retomar

1. `grep -E "^[0-9]+ mutants tested" /tmp/opencode/mut-*.log`
2. `grep "^MISSED" /tmp/opencode/mut-zconfig.log | sed 's/ in .*//'`
3. Por cada MISSED: test o exclusión justificada; documentar en
   `docs/audit/mutants-zconfig.md` / `mutants-zservice.md`.
4. `cargo mutants -p zconfig --list` da el inventario sin ejecutar.

## Estado al escribir esto

- `zcore`: 31/204 viables (15,2 %), 15 muertos esta sesión, doc en
  `docs/audit/mutants-zcore.md`.
- `zconfig`: 22 MISSED contados en un run anterior interrumpido; este
  run lo completa.
- `zservice`: sin medida previa.

## Criterio de éxito de alpha

Número real medido y documentado para los dos crates. No hace falta
llegar a <10 %: hace falta no fingir que se midió.

## Actualización 2026-10-23 (sesión 2)

Los runs lanzados con `timeout 5400` murieron con el reinicio del servidor;
`zconfig` no llegó a escribir nada. Números vistos antes del corte:

- `zservice`: **89 MISSED de 99 evaluados** (657 mutantes totales según
  `--list`). El grueso es `spawn.rs`, camino del hijo.
- `zconfig`: 0 líneas útiles; el run anterior sí contó **22 MISSED** antes
  de cortarse. 776 mutantes totales.
- `zcore`: 31/204 = 15,2 % (cerrado, `docs/audit/mutants-zcore.md`).

Test nuevo que ya cubre parte de `spawn.rs` sin root:
`crates/zinit/tests/supervises_real_service.rs::
the_child_path_runs_with_privileges_inside_a_user_namespace`
(re-exec bajo `unshare -U -r -p -f --mount-proc`; comprueba `CapBnd` a cero
y `unshare -m` rechazado). Los runs de mutantes **no** lo incluyen porque
arrancaron antes.

### Retomar

```sh
cd /home/dicov/zinit
git checkout agent/overnight-20261022-0332
nohup cargo mutants -p zconfig  --timeout 60 --no-shuffle > /tmp/opencode/mut-zconfig-full.log 2>&1 &
nohup cargo mutants -p zservice --timeout 60 --no-shuffle > /tmp/opencode/mut-zservice-full.log 2>&1 &
```

`nohup` + `&` sobrevive al cierre del shell; el reinicio del servidor mata
igual el proceso, así que relanzar y comprobar con
`grep -c '^MISSED'` cada vez que se retome la sesión.

Criterio de tag alpha: tasa completa de los dos crates medida y los
mutantes críticos de `spawn.rs` muertos. Los no críticos, documentados.
