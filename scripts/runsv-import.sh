#!/bin/sh
# runsv-import.sh — convert a runit runsvdir into zinit service descriptions.
#
# Usage: runsv-import.sh <runsvdir> <outdir>
#
# Reads every service directory in <runsvdir> and writes one `<name>.conf`
# per service into <outdir>. Text in, text out; nothing is started.
#
# Mapping (approximations are documented, not hidden):
#   run            required, executable. Becomes
#                    command = cd '<dir>' && exec ./run
#                  (runit runs ./run with the service directory as cwd).
#   finish         executable. Folded into the command:
#                    command = cd '<dir>' && ./run; code=$?; exec ./finish $code 0
#                  The signal argument is approximated as 0: a wrapper killed
#                  by a signal dies with it, so finish-on-signal has no
#                  equivalent here. A non-executable finish is ignored.
#   check          executable. Becomes `ready = ping:<dir>/check`
#                  (re-run until exit 0, like runit's check). Otherwise
#                  `ready = none`. A non-executable check is ignored.
#   log/run        executable. svlogd's target directory (last non-option
#                  argument) becomes `log = file:<dir>/<name>.log` — a single
#                  file, not svlogd's rotation scheme. A `vlogger` line
#                  becomes `log = syslog` (its `-t`/`-p` tag options are
#                  dropped with a warning: this sink does not frame).
#                  Anything else warns and logs nowhere (`log = none`).
#   down           any `down` file becomes `enabled = no`. Absent means yes
#                  (omitted, never spelled out).
#   conf           a sourced env file: warned about and skipped. Sourcing
#                  shell into a declarative format is exactly the coupling
#                  this converter refuses to invent.
#   restart        always `restart = always` (runit semantics); budget and
#                  delay stay default. Dependencies are always empty: runit
#                  has no dependency model, and inventing edges would be
#                  governing a guess. Users, cgroups and `chpst` lines stay
#                  inside ./run where they already work.
#
# Exit: 0 even when services are skipped (skips are warnings on stderr),
# 1 on fatal I/O, 2 on usage. Strictly POSIX sh: runs under dash and
# busybox ash, no arrays, no `local`, no pipefail.
#
# Example:
#   ./scripts/runsv-import.sh /etc/sv ./services.d
#   zinit check ./services.d && echo imports govern

set -eu

prog=runsv-import

warn() {
    printf '%s: %s\n' "$prog" "$*" >&2
}

die() {
    warn "$@"
    exit 1
}

usage() {
    printf 'usage: %s <runsvdir> <outdir>\n' "$prog" >&2
    exit 2
}

# Single-quote a path for embedding in a generated command line:
# `o'clock` becomes `'o'\''clock'`, the Worley-Thompson spelling.
squote() {
    escaped=$(printf '%s' "$1" | sed "s/'/'\\\\''/g")
    printf "'%s'" "$escaped"
}

# True when $1 is usable as a zinit service name: a single path-safe token.
# Mirrors the parser's rules loosely (the strict check runs at load);
# anything exotic is skipped with a warning rather than converted wrong.
usable_name() {
    case "$1" in
        *[!a-zA-Z0-9_.@+-]* | "" | .* | @* | *@ | *..* | *@*@*) return 1 ;;
    esac
    return 0
}

# Last non-option argument of the svlogd invocation in $1, or empty.
svlogd_dir() {
    awk '
        {
            for (i = 1; i <= NF; i++) {
                if ($i == "svlogd" || $i ~ /\/svlogd$/) {
                    j = i + 1
                    while (j <= NF && substr($j, 1, 1) == "-") { j++ }
                    if (j <= NF) { print $j; exit }
                }
            }
        }
    ' "$1" 2>/dev/null | head -n 1
}

convert_one() {
    svdir=$1
    outdir=$2
    name=$(basename "$svdir")

    if ! usable_name "$name"; then
        warn "$svdir: odd service name, skipped"
        return 0
    fi
    if [ ! -f "$svdir/run" ]; then
        warn "$name: no ./run, skipped"
        return 0
    fi
    if [ ! -x "$svdir/run" ]; then
        warn "$name: ./run is not executable, skipped"
        return 0
    fi

    qdir=$(squote "$svdir")
    if [ -x "$svdir/finish" ]; then
        command="cd $qdir && ./run; code=\$?; exec ./finish \$code 0"
    else
        if [ -e "$svdir/finish" ]; then
            warn "$name: ./finish is not executable, ignored"
        fi
        command="cd $qdir && exec ./run"
    fi

    if [ -x "$svdir/check" ]; then
        ready="ping:$svdir/check"
    else
        if [ -e "$svdir/check" ]; then
            warn "$name: ./check is not executable, ignored"
        fi
        ready="none"
    fi

    if [ -x "$svdir/log/run" ]; then
        logdir=$(svlogd_dir "$svdir/log/run")
        if [ -n "$logdir" ]; then
            log="file:$logdir/$name.log"
        elif grep -q vlogger "$svdir/log/run" 2>/dev/null; then
            log="syslog"
            warn "$name: vlogger mapped to syslog; -t/-p tag options are dropped"
        else
            warn "$name: log/run is neither svlogd nor vlogger; logging nowhere"
            log="none"
        fi
    else
        if [ -e "$svdir/log/run" ]; then
            warn "$name: log/run is not executable, ignored"
        fi
        log="none"
    fi

    {
        printf 'type = script\n'
        printf 'command = %s\n' "$command"
        printf 'ready = %s\n' "$ready"
        printf 'log = %s\n' "$log"
        printf 'restart = always\n'
        if [ -e "$svdir/down" ]; then
            printf 'enabled = no\n'
        fi
    } >"$outdir/$name.conf" || die "cannot write $outdir/$name.conf"

    if [ -e "$svdir/conf" ]; then
        warn "$name: ./conf is sourced shell, not imported"
    fi
}

main() {
    [ $# -eq 2 ] || usage
    runsvdir=$1
    outdir=$2
    [ -d "$runsvdir" ] || die "$runsvdir: not a directory"
    mkdir -p "$outdir" || die "cannot create $outdir"

    for svdir in "$runsvdir"/*/; do
        # An empty runsvdir leaves the glob unexpanded; a file is not a
        # service. Both are skipped, not errors.
        [ -e "$svdir" ] || continue
        [ -d "$svdir" ] || continue
        convert_one "${svdir%/}" "$outdir"
    done
}

main "$@"
