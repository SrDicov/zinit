#!/bin/sh
# install.sh — install zinit + zctl from a release tarball directory.
#
# Usage: ./install.sh [--prefix /usr/local] [--destdir /]
#
# Copies the binaries, seeds /etc/zinit/services.d with one example
# service (never overwrites), and prints the three commands that prove
# the install: check, supervise, control. Needs root for system paths.
# No init-system integration: enabling anything is the operator's job.

set -eu

PREFIX=/usr/local
DESTDIR=
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX=${2:?--prefix needs a value}; shift 2 ;;
        --destdir) DESTDIR=${2:?--destdir needs a value}; shift 2 ;;
        -h | --help) sed -n '2,9p' "$0"; exit 0 ;;
        *) printf 'install.sh: unknown argument %s\n' "$1" >&2; exit 2 ;;
    esac
done

for bin in zinit zctl; do
    if [ ! -x "./$bin" ]; then
        printf 'install.sh: ./%s missing or not executable\n' "$bin" >&2
        exit 1
    fi
done

SBIN="$DESTDIR$PREFIX/sbin"
BIN="$DESTDIR$PREFIX/bin"
SVC="$DESTDIR/etc/zinit/services.d"
install -d "$SBIN" "$BIN" "$SVC"
install -m 0755 ./zinit "$SBIN/zinit"
install -m 0755 ./zctl "$BIN/zctl"

if [ ! -f "$SVC/hello.conf" ]; then
    cat > "$SVC/hello.conf" <<'EOF'
type = script
command = echo "zinit is alive"; exec /bin/sleep 30
ready = none
log = none
restart = never
EOF
fi

printf 'installed:\n  %s\n  %s\n  %s/hello.conf\n' \
    "$SBIN/zinit" "$BIN/zctl" "$SVC"
printf 'prove it:\n'
printf '  %s check %s\n' "$SBIN/zinit" "$SVC"
printf '  %s --sup %s   # then: %s status hello\n' \
    "$SBIN/zinit" "$SVC" "$BIN/zctl"
printf 'as PID 1 (needs the prefix on the rootfs): init=%s --init\n' \
    "$SBIN/zinit"
