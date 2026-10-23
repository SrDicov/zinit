# Los 36 timeouts: qué son y qué no (2026-10-23)

`zservice`: 24 · `zconfig`: 12. Listados en `docs/audit/mutants-*.md`.
Todos, sin excepción, son mutantes de **contadores, índices o búsquedas**
(`+=`→`*=`/`-=`, `-=`→`+=`, `<`→`<=`, `lookup`→`None`, `pgid`→`Some(1)`).

## Cómo se reproduce un mutante suelto

`cargo mutants` de esta versión **no tiene `--mutant`** (el `--help` ofrece
`--Zmutate-file`, que es interno). Se aplica el cambio a mano, se mide, se
revierte:

```sh
cp crates/zconfig/src/graph.rs /tmp/graph.ok
# aplicar el cambio del log (lookup -> None)
cargo test -q -p zconfig --lib
cp /tmp/graph.ok crates/zconfig/src/graph.rs
```

## Lo medido: `graph.rs:603 lookup -> None`

| | pared | resultado |
|---|---|---|
| normal | **1,1 s** | 305 passed |
| mutante | **75,8 s** | falla, 4 tests en rojo |

El mutante **no cuelga**: termina y suspende. Es ×69 de coste, porque sin
`lookup` la resolución de nombres no encuentra nada y el trabajo por nodo deja
de ser lineal (los tests de 1000 nodos son los que se disparan).

Con `--timeout 30` eso se reporta como `TIMEOUT`; con `--timeout 180` sería
`CAUGHT`. **No es un bug del producto ni un test que esconde el mutante**: es
el límite cortando un mutante lento.

## Lo que no se puede afirmar todavía

De los 36, **uno** está medido. Los otros 35 comparten forma (contadores de
bucle), pero eso es una hipótesis, no una medición, y esta sesión ya ha
corregido dos diagnósticos por fiarse de una muestra. No se clasifican hasta
re-medirlos.

## Acción concreta

Re-correr sólo los archivos con timeouts, con límite holgado y **en serie**
(`-j 1`, sin el otro run compitiendo por los 4 cores):

```sh
cargo mutants -p zconfig  --file crates/zconfig/src/graph.rs  --file crates/zconfig/src/parser.rs --file crates/zconfig/src/plan.rs  --timeout 180 --no-shuffle -j 1
cargo mutants -p zservice --file crates/zservice/src/log.rs    --file crates/zservice/src/ready.rs  --file crates/zservice/src/service.rs --file crates/zservice/src/spawn.rs --timeout 180 --no-shuffle -j 1
```

Lo que salga `CAUGHT` con 180 s era un artefacto del límite. Lo que siga
`TIMEOUT` a los 180 s sí merece traza (`strace`), y entonces sería un bucle de
verdad o un test que espera algo que el mutante rompió.
