#!/bin/sh
# setup-s6.sh — reproduce the s6 benchmark cage.
# Base: Alpine minirootfs + `s6` package. Workload: identical to runit
# (`a` = sleep writing pid, `b` = oneshot). Same note: s6-supervise
# restarts the oneshot `b` forever — measured behaviour.
set -eu
CAGE=${1:-/home/dicov/inits/s6}
TEMPLATE=${TEMPLATE:-/home/dicov/inits/template}
rm -rf "$CAGE"
cp -a "$TEMPLATE" "$CAGE"
chroot "$CAGE" apk add s6
mkdir -p "$CAGE/scan/a" "$CAGE/scan/b" "$CAGE/tmp"
printf '#!/bin/sh\necho $$ > /tmp/a.pid\nexec /bin/sleep 300\n' > "$CAGE/scan/a/run"
printf '#!/bin/sh\necho ok > /tmp/b.done\n' > "$CAGE/scan/b/run"
chmod +x "$CAGE/scan/a/run" "$CAGE/scan/b/run"
# Launch: s6-svscan /scan, marker $CAGE/tmp/a.pid
echo S6-CAGE-READY
