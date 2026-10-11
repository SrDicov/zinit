#!/bin/sh
# setup-dinit.sh — reproduce the dinit benchmark cage.
# Base: Alpine minirootfs + `dinit` package. Workload: `a` (sleep writing
# pid) + `b` (oneshot), as process services started explicitly.
set -eu
CAGE=${1:-/home/dicov/inits/dinit}
TEMPLATE=${TEMPLATE:-/home/dicov/inits/template}
rm -rf "$CAGE"
cp -a "$TEMPLATE" "$CAGE"
chroot "$CAGE" apk add dinit
mkdir -p "$CAGE/etc/dinit.d" "$CAGE/tmp"
printf '#!/bin/sh\necho $$ > /tmp/a.pid\nexec /bin/sleep 300\n' > "$CAGE/a-run.sh"
printf '#!/bin/sh\necho ok > /tmp/b.done\n' > "$CAGE/b-run.sh"
chmod +x "$CAGE/a-run.sh" "$CAGE/b-run.sh"
printf 'type = process\ncommand = /a-run.sh\n' > "$CAGE/etc/dinit.d/a"
printf 'type = process\ncommand = /b-run.sh\n' > "$CAGE/etc/dinit.d/b"
# Launch: dinit --container -d /etc/dinit.d -t a -t b, marker $CAGE/tmp/a.pid
echo DINIT-CAGE-READY
