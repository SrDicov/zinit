# STATE — criterios de alpha (2026-10-23)

| # | criterio | estado | evidencia |
|---|---|---|---|
| 1 | CI verde (excepto riscv32gc) | ✅ | run `bb36339` → success; `riscv32gc` falla por std propio (probado ajeno) |
| 2 | mutantes `zcore` ≤ 20 % | ✅ | 31/204 = 15,2 % (`docs/audit/mutants-zcore.md`) |
| 3 | `zconfig`/`zservice` **tasa real** | **medido, críticos sin matar** | zconfig 116/776 = 14,9 %; zservice 270/657 = 41,1 % (`docs/audit/mutants-{zconfig,zservice}.md`) |
| 4 | SA7 seccomp con argumentos | ✅ | `clone` (7 namespaces) e `ioctl(TIOCSTI)`; probado por mutación |
| 5 | SA9 deberes de PID 1 | ✅ | `docs/PID1-DUTIES.md` |
| 6 | SA10 re-auditoría | ✅ | `docs/audit/known-issues-audit.md` |
| 7 | gaps del generador | ✅ documentados; 3 bloqueados por corpus (máquina apagada) | `docs/audit/generator-gaps.md` |
| 8 | cross-verificación | ✅ 2 rondas, 3 fallos reales (máscara incompleta, test vacuo, `_exit` en el sondeo) | commits `4904202`, `bb36339` |
| 9 | acta con evidencia | ✅ | este fichero |

## Lo único que falta para taggear

Matar los mutantes **críticos** de `zconfig` y `zservice` (no hace falta 0 %).
Orden por coste, ya medido:

1. `zservice/ready.rs` 47 y `log.rs` 34 — unitarios, sin root. **Empezar aquí.**
2. `zservice/identity.rs` 10, `service.rs` 27 — unitario/integración.
3. `zconfig/value.rs` 48, `parser.rs` 23, `graph.rs` 21, `desc.rs` 12, `plan.rs` 12.
4. `zservice/spawn.rs` 152 — sólo con root (job privilegiado), salvo lo cubierto
   por el test userns.

Comando por archivo:

```sh
cargo mutants -p zservice --file crates/zservice/src/ready.rs --timeout 30
cargo mutants -p zconfig  --file crates/zconfig/src/value.rs  --timeout 30
```

## Cómo taggear cuando 1-9 estén verdes

```sh
git tag -a v0.2.0-alpha -m "alpha: usable, with documented limits"
git push origin v0.2.0-alpha
# release con binarios + sha256 (el script de hardware firma por hash)
```

## Lo que NO se declara

- Los 24 timeouts de `zservice` no son flakes hasta que se lea la traza: un
  timeout con el mutante aplicado es un test que espera algo que el mutante
  rompió.
- Los 12 timeouts de `zconfig`, igual.
- `spawn.rs` sin root: 152 supervivientes, la mayoría del camino del hijo.
  No se declara cubierto.
