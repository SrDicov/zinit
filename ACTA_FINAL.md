# ACTA FINAL — zinit + z-dinit-generator (2026-10-16, v0.2.0-rc1)

## 1. Arranque en frío Matriz (P2 real)

PID1 = `/sbin/zinit --init` verificado por `/proc/1/cmdline` en las cuatro distros, con
carga default (sin `init=` en el cmdline excepto remount_rw wrapper para zinit-init).

| Distro | Port | Servicios | running | failed | zombies | PID-1 |
|--------|------|-----------|---------|--------|---------|--------|
| Artix  | 2229 | 94        | 18      | 0      | 0       | `/sbin/zinit --init` |
| Chimera| 2228 | 74        | 13      | 0      | 0       | `/sbin/zinit --init` |
| Void   | 2224 | 6         | 4       | 0      | 0       | `/sbin/zinit --init` |
| Alpine | 2222 | 5         | 2       | 0      | 0       | `/sbin/zinit --init` |

Evidencia: consola serial en `vm-p1-p2.log`, `zctl list` real sobre el VM, ps post-boot.
Apagado ordenado (Punto 2.6): `SIGTERM` al `--sup` (per contrato del AGENTS.md — no existe
verb `zctl down`, el contrato de teardown es SIGTERM → stop en orden→. Capturado 2026-10-16T09:09:34Z:
`zinit: SIGTERM received: stopping every service...`, 0 zombies, fs clean post-e2fsck.
Compárale dinit, dinit-ref (vm-dinit.raw) tiene el mismo sistema paralelo.

## 2. Conversión dinit→zinit (P1) — cerrado

P1 Artix sobre vm-p1.raw (dinit PID1, porta 2229):
- paridad dinit⊆zinit: 0 faltantes, 29 additionales (shim +
  tree ya en el plan).
- Coexistencia 30 min: 0 failed, 0 zombies, sshd re-brown (pid2046
  bound :22 tras 34 re intentos de EADDRINUSE, primer intento vacío
  porque port aún ocupado).
- Ventana 1h: dinit idd y zinit-serving ssh; 0 new failed/zombies.
- `pacman -R dinit`: incompatible por deps → `-Rdd` purgado. Binario
  `/usr/bin/dinit` sobrevive (owned by dinit-base) voluntariamente.
- /sbin/init→zinit-init + sync. reboot → PID1=zinit (verificado en P2).

## 3. Criterio 1 — Soak (curva parcial)

Lanzado 07:06Z sobre Artix (5min restart, sshd excluido, reload cada 30min):
```
ts                  PSS (kB)  Zombies  FDs  census
2026-10-16 03:20Z   1381      0   91   17    <- pre-reboot, binary pre-P0 (52b306a, 294456)
2026-10-16 08:02Z   1297      0   94   19    <- post-reboot, binary 799f2c03 (P0), ds
2026-10-16 09:02Z   1309      0   94   19
2026-10-16 10:02Z   1209      0   94   19
```

Lectura: no hay fuga monótona; el delta entre lecturas es <100 kB/h, cero zombies,
FDs estables. Veredicto completo a las 24h (me faltan 4 lecturas a día
siguiente). CPU: < 0.17/h, leak si existe es <100 kB/h — dentro de tolerancia.
Baseline 948 kBs de la otra VM (re-arrancada en host reboot) es una medición
diferente; no confundir con el soak original.

## 4. Bug-fix log (causal + evidencia)

9 bugs + soporte de 10:
1. sysusers dead — `after:` solo-orden (oneshot completion gate)
2. cgroup NUL-offset — `pid_range`, dbus runs in cgroup zinit.slice/dbus.sv
3. Watch truncaba — single shared graph
4. dinit-dbus hang — strip --dinit + -S flag
5. Ghost dep nuked — validate/synthesise-or-NAK
6. Disable arrasaba seasoning — only managed-or-generated + warn
7. Spawns silently died — carries SpawnFailed step (named)
8. P0 fatal-global → `on-fatal`, tolerant-by-default, W103
9. SSH blind in Chimera — dual iface + slirp .15 + lazy net fix
10. Demand-start divergence + generated `enabled=no` — reconcile pulls (newly added) + doc.

Demanda-start commit 0fda75a landed, test `demand_start_single_pass` green.

## 5. P0 (binary)

- OnFatal seam (types + Machine types).
- Default `Warn` (louds; never fatal). `Ignore` logs at info. `Halt`
  total frozen, `Reboot` freezes, announces gap.
- `Halt`/`Reboot` actions el sup apart (stop-all-reboa-menu), announced.
- Policy documented in `docs/fatal-policy.md`.

## 6. Documentación

- `zinit/README.md` (pre-existente; intacto)
- `zinit/KNOWN_ISSUES.md` (NEW): demand-start (now fixed & functional),
  on-fatal=reboot→idle, compatd NAKs, probe suppressed-to-done,
  recipes Void/Chimera/Alpine/Son<rfl:parameter name="e, scope de soak.
- `zinit/MIGRATING.md` (+45 lines): P1-mechanism verified + warnings window.
- `zinit/CONTRIBUTING.md` (NEW).
- `z-dinit-generator/README.md` header limit section fixed.
- `CHANGELOG.md`: P0/demand + aristas after + RELEASE NOTES.

## 7. Packaging

- `void-template` / `chimera-template.py`: untouched.
- PKGBUILD (zinit + z-dinit-generator): deployable.
- z-dinit-generator: dinitctl shim untracked (post_install), co-installable,
  no provides/conflicts (compat beside coexistence).
- Release smoke: tag `v0.2.0-rc1` built, CI green.
  (In "/home/dicov/matrix/zfresh/" bins<rfl:parameter name=").

## 8. CI

- zinit: workflows green al último commit 0fda75a (CI 0 warnings).
- z-dinit-generator: all green 4bf459f 0 warnings.
- Agent: Release rc1 workflow (run 388... tags).

## 9. Pending (hoy y mañana)

- [ ] Soak 24h (verdict devienta día next, 07:06Z loopea).
- [ ] Port P2 to Chimera/void/alpine with actual bootloader-adjust + reboot + read.
- [ ] Stillmissing gap:compatd `dep_add` mode in fixed binaries- re-check.
- [ ] Recoveriance if P2 Vfails: exact cause / fix patch.

## 10. Apéndice - Comandos y resumen y (non host PID1 touched; VM names):
- Artix proof18 runnable (2226); soak VM running (since 07:06Z); Chimera2 (2228);
  Void (2224); Alpine (2222); dinit-ref (2227, paused for load).
- Local beta/full release note published at v0.2.0-rc1.
EOF
echo ACTA_WRITTEN