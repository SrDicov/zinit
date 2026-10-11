#!/bin/sh
# setup-zinit.sh — reproduce the zinit benchmark cage.
# Base: Alpine minirootfs + the static zinit binary under test
# (ZINIT_BIN defaults to the cage's pinned build; point it at a CI
# artifact to measure another revision). Workload mirrors the other
# cages: `a` = supervised sleep writing pid, `b` = oneshot.
set -eu
CAGE=${1:-/home/dicov/inits/zinit}
TEMPLATE=${TEMPLATE:-/home/dicov/inits/template}
ZINIT_BIN=${ZINIT_BIN:-/home/dicov/inits/zinit/sbin/zinit}
rm -rf "$CAGE"
cp -a "$TEMPLATE" "$CAGE"
mkdir -p "$CAGE/etc/zinit/svc" "$CAGE/tmp"
cp "$ZINIT_BIN" "$CAGE/sbin/zinit"
cat > "$CAGE/etc/zinit/svc/a.conf" <<'EOF'
type = script
command = echo $$ > /tmp/a.pid; exec /bin/sleep 300
ready = none
log = none
restart = always
EOF
cat > "$CAGE/etc/zinit/svc/b.conf" <<'EOF'
type = script
command = echo ok > /tmp/b.done
ready = none
log = none
restart = never
EOF
# Launch: ZINIT_CONFIG_DIRS=/etc/zinit/svc chroot $CAGE /sbin/zinit --init,
# marker $CAGE/tmp/a.pid
echo ZINIT-CAGE-READY
