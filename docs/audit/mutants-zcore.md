# Mutantes en `zcore` (cargo-mutants)

Comando: `cargo mutants -p zcore --timeout 60 --no-shuffle`

## Números

| Momento | Probados | Muertos | Sobreviven | No viables |
|---|---|---|---|---|
| Primera medida | 228 | 158 | **46** | 24 |
| Tras los tests de esta ronda | 228 | 173 | **31** | 24 |

Sobre mutantes viables (228 − 24 = 204): **46 → 15,2 %**. El objetivo (< 5 %)
**no está cumplido**.

## Qué se mató y por qué estaba vivo

Los 15 mutantes muertos eran todos del mismo tipo: `name()`, `describe()`,
`protocol()`, `is_up()`, `is_handshake()`, `is_empty()`, y el `Display` de
`Event`. `cargo mutants` los sustituía por `""`, `"xyzzy"`, `true`, `false` o
`default()` y la suite entera seguía verde: los tests comprobaban que un
string *existe*, no cuál es. Es exactamente el patrón de test theater que una
auditoría a ojo no ve.

Arreglado con `mod mutation_guards` en `types.rs`, `event.rs`, `runtime.rs` y
`reconcile.rs`: aserciones sobre el valor exacto de cada nombre y de cada
predicado, incluido el `Display` que acaba en el log.

## Los 31 que sobreviven

Casi todos son operadores booleanos y comparaciones dentro de la máquina de
estados, y cada uno pide un escenario que distinga los dos caminos:

- `reconcile.rs`: 17 (`&&`↔`||` en las condiciones de arranque/parada, `>` y
  `>=` en los plazos, `delete !`, `delete match arm State::Stopping`).
- `runtime.rs`: 9 (`is_empty` ya cubierto; quedan siete `&&`↔`||` en las
  condiciones de snapshot y dos en `is_empty` de colecciones internas).
- `transition.rs`: 3 (`is_empty`, `kick` con `||`, `arm_start_deadlines` con
  `()`), y `Bucket::refill` en `types.rs`.

Matarlos de verdad significa escribir un escenario por mutante en el que el
programa tenga que *decidir* por esa rama: p. ej. un servicio `Stopping` con
el plazo justo vencido y otro justo antes, que es lo que distingue `>` de
`>=`; o un `kick` con las dos condiciones dadas a la vez.

## Coste honesto

- 31 mutantes × ~10-15 min cada uno (escenario + aserción + comprobar que el
  mutante muere) ≈ **5-8 horas** de trabajo concentrado en `zcore`.
- No es imposible ni requiere hardware: es tiempo. No se ha hecho.
