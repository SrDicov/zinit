#!/bin/sh
# setup-openrc.sh — reproduce the openrc benchmark cage.
# Base: Alpine minirootfs + `openrc` package. PID 1 is busybox `init`;
# openrc runs the sysinit/boot/default runlevels. `a` is a backgrounded
# sleep daemon (start_pre writes the boot marker), `b` a one-shot.
# Honest limits, also measured: openrc does NOT supervise — a killed `a`
# is never restarted (respawn reads TIMEOUT by design, not by accident).
set -eu
CAGE=${1:-/home/dicov/inits/openrc}
TEMPLATE=${TEMPLATE:-/home/dicov/inits/template}
rm -rf "$CAGE"
cp -a "$TEMPLATE" "$CAGE"
chroot "$CAGE" apk add openrc
mkdir -p "$CAGE/tmp"
cat > "$CAGE/etc/inittab" <<'EOF'
::sysinit:/bin/date +%s > /tmp/boot.t0
::sysinit:/sbin/openrc sysinit
::wait:/sbin/openrc boot
::wait:/sbin/openrc default
::respawn:/bin/sleep 86400
EOF
cat > "$CAGE/etc/init.d/a" <<'EOF'
#!/sbin/openrc-run
description="bench daemon"
command="/bin/sleep"
command_args="300"
command_background=true
pidfile="/run/a.pid"

start_pre() {
    date +%s > /tmp/a.t1
    echo "started" > /tmp/a.up
}
EOF
cat > "$CAGE/etc/init.d/b" <<'EOF'
#!/sbin/openrc-run
description="bench oneshot"

start() {
    echo ok > /tmp/b.done
}
EOF
chmod +x "$CAGE/etc/init.d/a" "$CAGE/etc/init.d/b"
ln -sf /etc/init.d/a "$CAGE/etc/runlevels/default/a"
ln -sf /etc/init.d/b "$CAGE/etc/runlevels/default/b"
# Launch: /sbin/init (busybox), marker $CAGE/tmp/a.up (NOT a.pid: openrc
# owns /run/a.pid itself). Needs .bench-reset between boots (stale pidfile
# collides with host kernel pids across namespaces); see bench/RESULTS.md.
echo OPENRC-CAGE-READY
