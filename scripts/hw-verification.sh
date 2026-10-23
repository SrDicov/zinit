#!/bin/sh
# Hardware verification for a real machine: deploy, verify, soak, power.
#
# Everything that needs the box is one command here, because every one of
# these steps has already been done wrong once by hand.
#
#   hw-verification.sh deploy <artifact-dir> [--sha256 HASH]
#   hw-verification.sh verify
#   hw-verification.sh soak [hours]
#   hw-verification.sh soak-report
#   hw-verification.sh power-test
#   hw-verification.sh verify-power
#
# `deploy` and `verify` are safe. `soak` leaves a background loop running.
# `power-test` POWERS THE MACHINE OFF: that is what the test is.
set -eu

ZINIT_BIN=${ZINIT_BIN:-/usr/bin/zinit}
ZCTL=${ZCTL:-/usr/bin/zctl}
SOAK_TSV=${SOAK_TSV:-/var/log/zinit-soak.tsv}
SOAK_LOG=${SOAK_LOG:-/var/log/zinit-soak.log}
POWER_MARK=${POWER_MARK:-/var/log/zinit-power.log}
SOCKET=${ZINIT_SOCKET:-/run/zinit/ctl}

die() { printf 'hw-verification: %s\n' "$*" >&2; exit 1; }
say() { printf '== %s\n' "$*"; }

# The supervisor must be reachable; everything below assumes it.
need_socket() {
    [ -S "$SOCKET" ] || die "no control socket at $SOCKET: is zinit running?"
    "$ZCTL" version >/dev/null 2>&1 || die "$ZCTL cannot talk to $SOCKET"
}

# Live services, failed services, zombies, and the supervisor's own weight.
snapshot() {
    pid=$(pgrep -x zinit | head -n 1 || true)
    [ -n "$pid" ] || die "no zinit process"
    running=$("$ZCTL" list 2>/dev/null | grep -c ' running' || true)
    failed=$("$ZCTL" list 2>/dev/null | grep -ci 'failed' || true)
    zombies=$(ps -eo stat= | grep -c '^Z' || true)
    rss=$(ps -o rss= -p "$pid" | tr -d ' ')
    fds=$(ls "/proc/$pid/fd" 2>/dev/null | wc -l | tr -d ' ')
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$running" "$failed" "$zombies" "$rss" "$fds"
}

# Pick a hash for the artifact: an explicit one wins, then a checksum file
# next to the binary, then nothing (and then we refuse).
artifact_hash() {
    dir=$1; want=$2
    if [ -n "$want" ]; then printf '%s\n' "$want"; return; fi
    for f in "$dir"/*.sha256 "$dir"/SHA256SUMS; do
        [ -f "$f" ] || continue
        base=$(basename "$dir"/zinit)
        grep -E "[ *]$base\$" "$f" | head -n 1 | awk '{print $1}'
        return
    done
}

cmd_deploy() {
    dir=${1:-}; want=${2:-}
    [ -n "$dir" ] || die "usage: deploy <artifact-dir> [--sha256 HASH]"
    [ -f "$dir/zinit" ] || die "$dir/zinit not found (download the CI artifact first)"
    [ -f "$dir/zctl" ] || die "$dir/zctl not found: the client and the daemon must match"

    say "hash check"
    if hash=$(artifact_hash "$dir" "$want") && [ -n "$hash" ]; then
        got=$(sha256sum "$dir/zinit" | awk '{print $1}')
        [ "$got" = "$hash" ] || die "sha256 mismatch: want $hash, got $got"
        printf '   %s ok\n' "$got"
    else
        die "no expected hash: pass --sha256 HASH so a truncated download cannot be installed"
    fi

    say "installing (rename, never overwrite a running binary)"
    $SUDO install -m 0755 "$dir/zinit" "$ZINIT_BIN.new"
    $SUDO install -m 0755 "$dir/zctl" "$ZCTL.new"
    $SUDO mv "$ZINIT_BIN.new" "$ZINIT_BIN"
    $SUDO mv "$ZCTL.new" "$ZCTL"
    printf '   %s %s\n' "$ZINIT_BIN" "$(sha256sum "$ZINIT_BIN" | awk '{print $1}')"

    say "runtime is still the old image; restart decides"
    if [ "$(cat /proc/1/comm 2>/dev/null)" = "zinit" ]; then
        printf '   zinit is PID 1: no in-place restart. Reboot, or run --sup elsewhere.\n'
    else
        p=$(pgrep -f 'zinit --sup' | head -n 1 || true)
        [ -n "$p" ] || die "no 'zinit --sup' process: nothing to restart"
        $SUDO kill -TERM "$p"
        i=0
        while [ $i -lt 50 ]; do
            sleep 0.1
            newp=$(pgrep -f 'zinit --sup' | head -n 1 || true)
            if [ -n "$newp" ] && [ "$newp" != "$p" ]; then
                printf '   supervisor %s -> %s in %sms\n' "$p" "$newp" $((i * 100))
                unset p
                break
            fi
            i=$((i + 1))
        done
        [ -z "${p:-}" ] || die "supervisor did not come back (its launcher is not restarting it)"
    fi
    cmd_verify
}

cmd_verify() {
    need_socket
    say "services"
    "$ZCTL" list || die "list failed"
    line=$(snapshot)
    failed=$(printf '%s' "$line" | cut -f3)
    zombies=$(printf '%s' "$line" | cut -f4)
    [ "$failed" = "0" ] || die "$failed services are not healthy"
    [ "$zombies" = "0" ] || die "$zombies zombies: the reap path is broken"
    printf '   running=%s failed=0 zombies=0 rss=%sKB fds=%s\n' \
        "$(printf '%s' "$line" | cut -f2)" "$(printf '%s' "$line" | cut -f5)" "$(printf '%s' "$line" | cut -f6)"
    say "power breadcrumb from a previous SIGTERM test: ${POWER_MARK}"
    [ -f "$POWER_MARK" ] && tail -n 3 "$POWER_MARK" || printf '   (none: power path not exercised on this boot)\n'
}

cmd_soak() {
    hours=${1:-24}
    need_socket
    say "soak: $hours h, sample every 60s, random restart every 5 min, reload-all every 30 min"
    printf 'ts\trunning\tfailed\tzombies\trss_kb\tfds\n' > "$SOAK_TSV"
    start=$(date +%s)
    end=$((start + hours * 3600))
    n=0
    while [ "$(date +%s)" -lt "$end" ]; do
        snapshot >> "$SOAK_TSV"
        n=$((n + 1))
        if [ $((n % 5)) -eq 0 ]; then
            svc=$("$ZCTL" list 2>/dev/null | awk '$2 == "running" {print $1}' | shuf -n 1 || true)
            [ -n "$svc" ] && { "$ZCTL" restart "$svc" >/dev/null 2>&1 || true; }
        fi
        if [ $((n % 30)) -eq 0 ]; then
            "$ZCTL" reload-all >/dev/null 2>&1 || true
        fi
        sleep 60
    done
    cmd_soak_report
}

cmd_soak_report() {
    [ -f "$SOAK_TSV" ] || die "no $SOAK_TSV: soak never ran"
    samples=$(($(wc -l < "$SOAK_TSV") - 1))
    [ "$samples" -gt 0 ] || die "no samples"
    first=$(sed -n '2p' "$SOAK_TSV"); last=$(tail -n 1 "$SOAK_TSV")
    rss0=$(printf '%s' "$first" | cut -f5); rss1=$(printf '%s' "$last" | cut -f5)
    fd0=$(printf '%s' "$first" | cut -f6); fd1=$(printf '%s' "$last" | cut -f6)
    bad=$(awk -F'\t' 'NR > 1 && ($3 != 0 || $4 != 0)' "$SOAK_TSV" | wc -l | tr -d ' ')
    say "soak report"
    printf '   samples=%s  rss=%sKB -> %sKB  fds=%s -> %s\n' "$samples" "$rss0" "$rss1" "$fd0" "$fd1"
    printf '   samples with failed services or zombies: %s\n' "$bad"
    printf '   verdict: '
    if [ "$bad" = "0" ] && [ $((rss1 - rss0)) -lt 2048 ]; then
        printf 'PASS (no failures, no zombies, RSS grew < 2 MB)\n'
    else
        printf 'FAIL (see %s)\n' "$SOAK_TSV"
        exit 1
    fi
    say "writing $SOAK_LOG"
    { printf 'soak report\n'; cat "$SOAK_TSV"; } >> "$SOAK_LOG"
}

cmd_power_test() {
    [ "$(cat /proc/1/comm 2>/dev/null)" = "zinit" ] || die "PID 1 is not zinit: this test is about the init path"
    printf '\n'; printf '  SIGTERM to PID 1 powers the machine OFF. Everything unsaved is lost.\n'
    printf '  Type the word poweroff to continue: '
    read -r answer
    [ "$answer" = "poweroff" ] || die "not confirmed"
    # Written and synced *before* the signal: nothing after it is guaranteed.
    printf '%s SIGTERM to PID 1 about to be sent\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" | $SUDO tee -a "$POWER_MARK" >/dev/null
    $SUDO sync
    say "sending SIGTERM to PID 1; the machine should go down in an orderly way"
    $SUDO kill -TERM 1
    sleep 5
    say "still up after 5s: the power path did not fire (that is the bug)"
    exit 1
}

cmd_verify_power() {
    [ -f "$POWER_MARK" ] || die "no breadcrumb at $POWER_MARK: the power path never ran"
    say "last power attempt"
    tail -n 5 "$POWER_MARK"
    if "$ZCTL" version >/dev/null 2>&1; then
        say "the supervisor answered after the reboot: PASS"
    else
        die "the supervisor is not answering after the reboot"
    fi
    say "logind/init state: $(cat /proc/1/comm 2>/dev/null)"
}

SUDO=${SUDO:-}
[ "$(id -u)" = "0" ] || SUDO=${SUDO:-sudo}

case ${1:-} in
deploy)
    shift
    sha=""
    dir=""
    while [ $# -gt 0 ]; do
        case $1 in
        --sha256) sha=${2:-}; shift 2 ;;
        *) dir=$1; shift ;;
        esac
    done
    cmd_deploy "$dir" "$sha"
    ;;
verify) cmd_verify ;;
soak) shift; cmd_soak "${1:-24}" ;;
soak-report) cmd_soak_report ;;
power-test) cmd_power_test ;;
verify-power) cmd_verify_power ;;
*)
    sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
