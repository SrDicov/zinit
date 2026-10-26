#!/bin/sh
# Hardware verification for a real machine: deploy, verify, soak, power.
#
# Everything that needs the box is one command here, because every one of
# these steps has already been done wrong once by hand.
#
#   hw-verification.sh full [hours] [<artifact-dir>] [--sha256 HASH] [--dry-run]
#   hw-verification.sh deploy <artifact-dir> [--sha256 HASH] [--dry-run]
#   hw-verification.sh verify
#   hw-verification.sh soak [hours]
#   hw-verification.sh soak-report
#   hw-verification.sh power-test
#   hw-verification.sh kill-term-1
#   hw-verification.sh verify-power
#   hw-verification.sh dinit-removal-check
#   hw-verification.sh after-reboot
#
# `deploy`, `verify`, `soak-report`, `dinit-removal-check` are safe.
# `soak` is a blocking loop. `power-test` and `kill-term-1` POWER THE MACHINE
# OFF: that is what they test. `full` chains deploy -> soak -> the shutdown
# test, so `after-reboot` is the second half, run once the machine is back.
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
    if [ "$DRY_RUN" = "1" ]; then
        printf '   dry run: would install %s and %s (hash ok, nothing touched)\n' "$ZINIT_BIN" "$ZCTL"
        return
    fi
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

# Both shutdown tests destroy the running system, so the confirmation is one
# function: the words are the operator's proof that the machine was meant to go
# down, and the breadcrumb is written and synced *before* the trigger.
confirm_power() {
    printf '\n'; printf '  %s\n' "$1"
    printf '  Everything unsaved is lost.\n'
    printf '  Type the word poweroff to continue: '
    read -r answer
    [ "$answer" = "poweroff" ] || die "not confirmed"
    printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$2" | $SUDO tee -a "$POWER_MARK" >/dev/null
    $SUDO sync
}

# Did the machine actually go down? If it is still here after the grace
# period, the trigger did not reach the power path: that is the bug.
still_up() {
    sleep 5
    say "still up after 5s: the $1 path did not fire (that is the bug)"
    exit 1
}

cmd_power_test() {
    [ "$(cat /proc/1/comm 2>/dev/null)" = "zinit" ] || die "PID 1 is not zinit: this test is about the init path"
    need_socket
    confirm_power "zctl poweroff asks PID 1 to power the machine off." "zctl poweroff about to be sent"
    say "sending the operator verb; the machine should go down in an orderly way"
    "$ZCTL" poweroff || die "zctl poweroff was refused"
    still_up "poweroff"
}

# The signal path, as opposed to the operator verb above: Ctrl-Alt-Del is
# SIGINT (reboot) and SIGTERM is poweroff, straight from the kernel. This is
# the one an operator cannot type, so it gets its own test.
cmd_kill_term_1() {
    [ "$(cat /proc/1/comm 2>/dev/null)" = "zinit" ] || die "PID 1 is not zinit: this test is about the init path"
    need_socket
    confirm_power "SIGTERM to PID 1 powers the machine OFF." "SIGTERM to PID 1 about to be sent"
    say "sending SIGTERM to PID 1; the machine should go down in an orderly way"
    $SUDO kill -TERM 1
    still_up "SIGTERM"
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

# Nothing here removes anything: it only says what still owns dinit and what
# would break first (devmon is the known one). Read-only on purpose.
cmd_dinit_check() {
    say "dinit removal check (read-only)"
    found=0
    for b in dinit dinitctl; do
        p=$(command -v "$b" 2>/dev/null || true)
        if [ -n "$p" ]; then printf '   binary: %s\n' "$p"; found=1; fi
    done
    for d in /etc/dinit.d /usr/lib/dinit.d; do
        if [ -d "$d" ]; then
            n=$(find "$d" -maxdepth 1 -type f 2>/dev/null | wc -l | tr -d ' ')
            printf '   config: %s (%s entries)\n' "$d" "$n"
            found=1
        fi
    done
    p=$(pgrep -x dinit 2>/dev/null | head -n 1 || true)
    if [ -n "$p" ]; then printf '   running: dinit pid %s\n' "$p"; found=1; fi
    if command -v devmon >/dev/null 2>&1; then
        printf '   devmon: %s (ships in dinit-base; what starts it?)\n' "$(command -v devmon)"
        found=1
    fi
    if [ "$found" = "0" ]; then
        say "PASS: no dinit binary, config, process or devmon on this machine"
        return
    fi
    say "not removable by this check alone:"
    printf '   - remove the package that owns the entries above (distro package manager)\n'
    printf '   - keep dinit-base while devmon is what automounts /run/media or /media\n'
    printf '   - then verify /sbin/init is zinit and reboot; the box must come back\n'
}

cmd_after_reboot() {
    cmd_verify_power
    cmd_dinit_check
}

# The night in one command, in the only order that is physically possible: the
# shutdown test ends the boot, so `after-reboot` is the second half. Nothing
# here is skipped on a failure: `set -e` stops the chain at the first red step.
cmd_full() {
    hours=${1:-24}; dir=${2:-}; sha=${3:-}
    # Checked here, not only in the shutdown step: 24 hours of soak followed by
    # "PID 1 is not zinit" is a night wasted on the wrong machine. A dry run
    # runs on a development box, so it only says what it would check.
    if [ "$DRY_RUN" = "1" ]; then
        say "dry run: not checking PID 1 (a real run ends in a power test)"
    else
        [ "$(cat /proc/1/comm 2>/dev/null)" = "zinit" ] || die "PID 1 is not zinit: this chain ends in a power test"
    fi
    if [ -n "$dir" ]; then cmd_deploy "$dir" "$sha"; else cmd_verify; fi
    if [ "$DRY_RUN" = "1" ]; then
        say "dry run: stopping before the soak and the shutdown test"
        return
    fi
    cmd_soak "$hours"
    say "soak is green; the machine powers off next, then: hw-verification.sh after-reboot"
    cmd_kill_term_1
}

SUDO=${SUDO:-}
[ "$(id -u)" = "0" ] || SUDO=${SUDO:-sudo}

# `--dry-run` is the one flag that belongs to no command: strip it first.
DRY_RUN=0
rest=""
for a in "$@"; do
    case $a in
    --dry-run) DRY_RUN=1 ;;
    *) rest="$rest $a" ;;
    esac
done
# shellcheck disable=SC2086 # the artifact path carries no spaces; POSIX has no arrays
set -- $rest

case ${1:-} in
help | -h | --help | '')
    sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
    [ -n "${1:-}" ] && exit 0
    exit 2
    ;;
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
kill-term-1) cmd_kill_term_1 ;;
verify-power) cmd_verify_power ;;
dinit-removal-check) cmd_dinit_check ;;
after-reboot) cmd_after_reboot ;;
full)
    shift
    hours=24
    dir=""
    sha=""
    while [ $# -gt 0 ]; do
        case $1 in
        --sha256) sha=${2:-}; shift 2 ;;
        --hours) hours=${2:-24}; shift 2 ;;
        -* ) die "unknown flag: $1" ;;
        *) dir=$1; shift ;;
        esac
    done
    cmd_full "$hours" "$dir" "$sha"
    ;;
*)
    die "unknown command: $1 (try --help)"
    ;;
esac

