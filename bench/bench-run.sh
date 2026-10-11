#!/bin/sh
# bench-run.sh — measure one init cage, always the same way.
#
# Usage: bench-run.sh <name> <cage-dir> <marker-pid-file> -- <launch-cmd...>
#
# Measures, in order: boot time (spawn to marker), steady-state RSS,
# 60 s idle CPU, respawn latency (kill -9 the sleep, wait for the marker
# to be rewritten), then kills the cage. SKIP_IDLE=1 skips the 90 s
# settle+idle window for fast respawn-only runs (RSS/idle read SKIPPED). Results append to results/<name>.txt
# as KEY=VALUE lines. Text out, nothing else.
#
# Everything is observed from the host through /proc and the marker files
# the workload writes: no cooperation from the init, so the numbers are
# comparable across inits. Needs: /proc, pgrep-free (uses /bin/sh + awk).
#
# Honest limits, stated here rather than discovered later: boot, RSS,
# respawn and disk are exact. CPU sampling sums live processes, so a hot
# loop of short-lived children is undercounted (their ticks die with
# them); loop cost is measured exactly by the storm phase instead
# (counter file, restarts per 10 s).
#
# Exit 0 with results, 1 on any missed deadline (a hang is a failure).

set -eu

DEADLINE_BOOT=300
DEADLINE_RESPAWN=30
# Poll granularity is 50 ms: GNU sleep takes fractions, and marker files
# appear within milliseconds of the event. Coarser polling would quantize
# every latency to whole seconds (a 11 ms respawn reads as 1018 ms).
POLL_S=0.05
SETTLE=30
IDLE_SECS=60

die() { printf 'bench-run: %s\n' "$*" >&2; exit 1; }

[ $# -ge 4 ] || die "usage: $0 <name> <cage-dir> <marker> -- <launch...>"
name=$1; cage=$2; marker=$3; shift 3
[ "$1" = "--" ] || die "missing -- before launch command"; shift

# Pids rooted inside the cage (precise, no name matching: host processes
# have root=/). Used for preflight (no stragglers from previous boots —
# they would share ports, files and CPU and silently poison every number)
# and for verified cleanup.
cage_rooted() {
    for d in /proc/[0-9]*/root; do
        if [ "$(readlink "$d" 2>/dev/null)" = "$cage" ]; then
            pid=${d#/proc/}; printf '%s\n' "${pid%/root}"
        fi
    done | sort -un
}

# All pids in the cage: the init plus everything below it, to any depth
# (supervisor -> service -> helper -> ...). Breadth-first over /proc.
cage_pids() {
    printf '%s\n' "$1"
    seen=" $1 "
    frontier=$1
    while [ -n "$frontier" ]; do
        next=""
        for stat in /proc/[0-9]*/stat; do
            pid=${stat#/proc/}; pid=${pid%/stat}
            case "$seen" in *" $pid "*) continue;; esac
            line=$(cat "$stat" 2>/dev/null) || continue
            rest=${line##*)}
            set -- $rest
            ppid=${2:-0}
            for f in $frontier; do
                if [ "$ppid" = "$f" ]; then
                    printf '%s\n' "$pid"
                    seen="$seen$pid "
                    next="$next $pid"
                    break
                fi
            done
        done
        # `next` accumulates a leading space; word splitting copes.
        frontier=$next
    done | sort -un
}

# Sum VmRSS (kB) over a pid list.
rss_kb() {
    total=0
    for p in $1; do
        if [ -r "/proc/$p/status" ]; then
            v=$(awk '/^VmRSS:/ {print $2}' "/proc/$p/status" 2>/dev/null)
            total=$((total + ${v:-0}))
        fi
    done
    printf '%s\n' "$total"
}

# Sum utime+stime (ticks) over a pid list. Field positions after
# stripping `pid (comm)`: $1=state $2=ppid ... $12=utime $13=stime
# (Linux proc(5): utime/stime are overall fields 14/15).
cpu_ticks() {
    total=0
    for p in $1; do
        s=/proc/$p/stat
        [ -r "$s" ] || continue
        line=$(cat "$s" 2>/dev/null) || continue
        rest=${line##*)}
        set -- $rest
        # after comm: state ppid pgrp ... utime is field 11, stime 12
        # (overall stat fields 14/15 once pid+comm are stripped).
        total=$((total + ${12:-0} + ${13:-0}))
    done
    printf '%s\n' "$total"
}

rm -f "$marker"
# Cage-specific reset (optional executable `.bench-reset` inside the cage):
# state that must die between boots but the harness cannot know about —
# openrc's pidfiles (a stale pid collides with immortal host kernel pids
# across the namespace boundary, so openrc skips the start as "already
# running") and its runlevel state. Runs on the host, before launch.
if [ -x "$cage/.bench-reset" ]; then
    "$cage/.bench-reset" "$cage"
fi
# No stragglers from previous boots: they would share ports, files and
# CPU and silently poison every number below. Checked BEFORE launching —
# a launch below would be the contamination. The launcher itself runs
# with host root, so anything rooted inside the cage is, by
# construction, a leftover. Die loudly instead of measuring garbage.
leftovers=$(cage_rooted)
if [ -n "$leftovers" ]; then
    die "$name: stragglers already rooted in $cage: $leftovers"
fi
t0=$(date +%s%N)
# shellcheck disable=SC2093: intentional backgrounding of the cage
"$@" &
launcher=$!
# Always leave the machine as found: kill the cage init first — the
# kernel then tears down its whole pid namespace by itself — then sweep
# stragglers and the launcher, and warn on anything left. Fires on every
# exit path including die(), so a failed run cannot poison the next.
cleanup_cage() {
    if [ -n "${initpid:-}" ]; then
        kill -9 "$initpid" 2>/dev/null || true
    fi
    sleep 1
    leftovers=$(cage_rooted)
    if [ -n "$leftovers" ]; then
        # shellcheck disable=SC2046: word splitting intentional over pids
        for p in $leftovers; do
            kill -9 "$p" 2>/dev/null || true
        done
        sleep 1
        leftovers=$(cage_rooted)
    fi
    kill -9 "$launcher" 2>/dev/null || true
    wait "$launcher" 2>/dev/null || true
    if [ -n "$leftovers" ]; then
        printf 'bench-run: WARNING: survivors after cleanup: %s\n' "$leftovers" >&2
    fi
}
trap cleanup_cage EXIT
# The container init: youngest process whose command line matches.
# (Passed indirectly: find the unshare we just started below it.)
initpid=""
boot_iters=$((DEADLINE_BOOT * 20))
for i in $(seq 1 "$boot_iters"); do
    if [ -f "$marker" ]; then break; fi
    sleep $POLL_S
done
[ -f "$marker" ] || die "$name: boot marker never appeared"
t1=$(date +%s%N)
boot_ms=$(( (t1 - t0) / 1000000 ))

# Find the cage init: the process running the launch command. The caller
# knows it best; here: the child of our launcher still alive, else any
# process whose root matches. Fallback chain, newest match wins.
initpid=$(pgrep -P "$launcher" 2>/dev/null | head -n 1)
if [ -z "$initpid" ]; then
    initpid=$(ps -ef 2>/dev/null | awk -v c="$cage" '$0 ~ c && $0 !~ /awk/ {print $2}' | head -n 1)
fi
[ -n "$initpid" ] || die "$name: init pid not found"

# SKIP_IDLE=1 skips the 90 s settle+idle window (respawn-only runs):
# boot and respawn need seconds, RSS/idle need minutes.
if [ -n "${SKIP_IDLE:-}" ]; then
    rss=SKIPPED
    rss2=SKIPPED
    idle_ticks=SKIPPED
else
    sleep "$SETTLE"
    pids=$(cage_pids "$initpid")
    rss=$(rss_kb "$pids")
    t_cpu0=$(cpu_ticks "$pids")
    sleep "$IDLE_SECS"
    pids=$(cage_pids "$initpid")
    cpu1=$(cpu_ticks "$pids")
    idle_ticks=$((cpu1 - t_cpu0))
    rss2=$(rss_kb "$pids")
fi

# Respawn: kill every sleep in the cage, wait for the marker to be
# rewritten. The marker's mtime is the source of truth — every cage's `a`
# service rewrites it on (re)start — for two reasons. Pids in marker files
# lie across namespaces (container pid vs host pid), so pid comparison is
# meaningless; and a full /proc walk per poll costs more than the latency
# being measured (hundreds of fork+exec per iteration inside the timing
# window — it once reported 4367 ms for a ~50 ms restart). One walk finds
# the victims before the window opens; inside it, only cheap `stat` polls.
# `%.9Y` is a single word with ns resolution (no quoting pitfalls).
cage_sleeps() {
    for p in $(cage_pids "$initpid"); do
        if tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null | grep -q 'sleep 300'; then
            printf '%s\n' "$p"
        fi
    done
}
before=$(cage_sleeps | sort -un | tr '\n' ' ')
[ -n "$before" ] || die "$name: sleep victim not found"
mtime0=$(stat -c %.9Y "$marker" 2>/dev/null) || die "$name: cannot stat marker"
for p in $before; do
    kill -9 "$p" 2>/dev/null || true
done
t2=$(date +%s%N)
newmtime=""
respawn_iters=$((DEADLINE_RESPAWN * 20))
for i in $(seq 1 "$respawn_iters"); do
    mtime=$(stat -c %.9Y "$marker" 2>/dev/null || true)
    if [ -n "$mtime" ] && [ "$mtime" != "$mtime0" ]; then newmtime=$mtime; break; fi
    sleep $POLL_S
done
t3=$(date +%s%N)
if [ -n "$newmtime" ]; then
    respawn_ms=$(( (t3 - t2) / 1000000 ))
else
    respawn_ms="TIMEOUT"
fi

mkdir -p results
{
    printf 'init=%s\n' "$name"
    printf 'boot_ms=%s\n' "$boot_ms"
    printf 'rss_kb_steady=%s\n' "$rss"
    printf 'rss_kb_after_idle=%s\n' "$rss2"
    printf 'idle_ticks_60s=%s\n' "$idle_ticks"
    printf 'respawn_ms=%s\n' "$respawn_ms"
    printf 'cage_du_kb=%s\n' "$(du -sk "$cage" | awk '{print $1}')"
} > "results/$name.txt"
cat "results/$name.txt"
# The EXIT trap cleans the cage; reaching here is success.
