#!/bin/sh
# setup-runit.sh — reproduce the runit benchmark cage.
# Base: Alpine minirootfs + `runit` package. Workload: service `a`
# (long-running sleep writing its pid) + service `b` (oneshot).
# Supervision note: runsvdir restarts EVERYTHING that exits, including
# the oneshot `b` — that hot loop is the measured behaviour, not a bug.
set -eu
CAGE=${1:-/home/dicov/inits/runit}
TEMPLATE=${TEMPLATE:-/home/dicov/inits/template}
rm -rf "$CAGE"
cp -a "$TEMPLATE" "$CAGE"
chroot "$CAGE" apk add runit
mkdir -p "$CAGE/service/a" "$CAGE/service/b" "$CAGE/tmp"
printf '#!/bin/sh\necho $$ > /tmp/a.pid\nexec /bin/sleep 300\n' > "$CAGE/service/a/run"
printf '#!/bin/sh\necho ok > /tmp/b.done\n' > "$CAGE/service/b/run"
chmod +x "$CAGE/service/a/run" "$CAGE/service/b/run"
# Launch (see bench/RESULTS.md): runsvdir -P /service, marker $CAGE/tmp/a.pid
echo RUNIT-CAGE-READY
