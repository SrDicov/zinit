#!/bin/sh
# bench-storm.sh — crash-loop policy per init cage, always the same way.
#
# Usage: bench-storm.sh <name> <cage-dir> -- <launch-cmd...>
#
# Assumes the cage runs a `storm` service that appends one line to
# /tmp/storm.count (== $cage/tmp/storm.count on the host) and exits 1.
# Measures lines written in a fixed 10 s window after the first line.
# Uniform: same crashing workload, same host-side counting, no init
# cooperation. What differs is policy: hot-loop (runit/s6), give up
# (dinit), budgeted backoff (zinit), throttle (sysvinit), run-once
# (openrc, no supervision).
#
# Exit 0 with results, 1 on missed boot deadline.

set -eu

DEADLINE_BOOT=60
POLL_S=0.05
STORM_SECS=10

die() { printf 'bench-storm: %s\n' "$*" >&2; exit 1; }

[ $# -ge 4 ] || die "usage: $0 <name> <cage-dir> -- <launch...>"
name=$1; cage=$2; shift 2
[ "$1" = "--" ] || die "missing -- before launch command"; shift
counter="$cage/tmp/storm.count"

cage_rooted() {
    for d in /proc/[0-9]*/root; do
        if [ "$(readlink "$d" 2>/dev/null)" = "$cage" ]; then
            pid=${d#/proc/}; printf '%s\n' "${pid%/root}"
        fi
    done | sort -un
}

# A storm service must start from zero: leftover lines would be counted.
rm -f "$counter"
if [ -x "$cage/.bench-reset" ]; then
    "$cage/.bench-reset" "$cage"
fi
rm -f "$counter"
leftovers=$(cage_rooted)
if [ -n "$leftovers" ]; then
    die "$name: stragglers already rooted in $cage: $leftovers"
fi
t0=$(date +%s%N)
# shellcheck disable=SC2093: intentional backgrounding of the cage
"$@" &
launcher=$!
initpid=""
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
        printf 'bench-storm: WARNING: survivors after cleanup: %s\n' "$leftovers" >&2
    fi
}
trap cleanup_cage EXIT
# Boot marker for storm: the counter's first line.
boot_iters=$((DEADLINE_BOOT * 20))
for i in $(seq 1 "$boot_iters"); do
    if [ -f "$counter" ]; then break; fi
    sleep $POLL_S
done
[ -f "$counter" ] || die "$name: storm counter never appeared"
t1=$(date +%s%N)
boot_ms=$(( (t1 - t0) / 1000000 ))
initpid=$(pgrep -P "$launcher" 2>/dev/null | head -n 1)
if [ -z "$initpid" ]; then
    initpid=$(ps -ef 2>/dev/null | awk -v c="$cage" '$0 ~ c && $0 !~ /awk/ {print $2}' | head -n 1)
fi
[ -n "$initpid" ] || die "$name: init pid not found"
sleep "$STORM_SECS"
lines=$(wc -l < "$counter" 2>/dev/null || echo 0)
# Supervisor survival: the init itself still alive and rooted.
alive=no
if kill -0 "$initpid" 2>/dev/null; then
    root=$(readlink "/proc/$initpid/root" 2>/dev/null || true)
    if [ "x$root" = "x$cage" ]; then alive=yes; fi
fi

mkdir -p results
{
    printf 'init=%s\n' "$name"
    printf 'storm_boot_ms=%s\n' "$boot_ms"
    printf 'storm_lines_10s=%s\n' "$lines"
    printf 'supervisor_alive=%s\n' "$alive"
} > "results/storm-$name.txt"
cat "results/storm-$name.txt"
