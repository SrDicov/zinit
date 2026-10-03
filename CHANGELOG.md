# Changelog

## [Unreleased] — Fase 1 (cierre)
- Reactor verificado: `epoll`/`kqueue`/`poll` level-triggered, `HUP`/`EV_EOF` siempre `readable = true` (`crates/zrt/src/reactor.rs`, tests `closed_pipe_end_is_readable_because_a_hangup_is_a_read`, `pipe_write_end_is_reported_readable`). Sin cambios de código.
- Regression test `group_leader_parent_still_spawns_with_own_group`: cubre el fallback `setsid` EPERM → `setpgid(0, 0)` (`child_step_session`) bajo harness de test líder de grupo.
- Limpieza `ReadyWait::for_service` (identidad muerta).
