#!/bin/sh
# setup-sysvinit.sh — reproduce the sysvinit benchmark cage.
# Base: Alpine minirootfs, no extra package: PID 1 is busybox `init`
# driven by /etc/inittab. `a` is a respawned sleep, `b` a one-shot.
set -eu
CAGE=${1:-/home/dicov/inits/sysvinit}
TEMPLATE=${TEMPLATE:-/home/dicov/inits/template}
rm -rf "$CAGE"
cp -a "$TEMPLATE" "$CAGE"
mkdir -p "$CAGE/tmp"
cat > "$CAGE/etc/inittab" <<'EOF'
::sysinit:/bin/echo sysv-boot
::once:/bin/sh -c "echo ok > /tmp/b.done"
::respawn:/bin/sh -c "echo $$ > /tmp/a.pid; exec /bin/sleep 300"
EOF
# Launch: /sbin/init (busybox), marker $CAGE/tmp/a.pid
echo SYSVINIT-CAGE-READY
