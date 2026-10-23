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
