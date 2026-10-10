#!/usr/bin/env bash
# zinit-deploy.sh — package the static musl binary and deploy to a QEMU VM or bare-metal host.
#
# Usage:
#   ./zinit-deploy.sh build                    # build + package locally
#   ./zinit-deploy.sh deploy <host> [user]     # scp + ssh to target
#   ./zinit-deploy.sh qemu <disk.img>          # launch QEMU with zinit as PID 1
#
# Prerequisites: cargo, rustup target x86_64-unknown-linux-musl, qemu-system-x86_64 (for qemu mode)

set -euo pipefail

TARGET="x86_64-unknown-linux-musl"
PROFILE="release"
BIN="zinit"
TARBALL="zinit-deploy.tar.gz"
SERVICES_DIR="services.d"

log() { printf '\033[1;32m[zinit]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[zinit]\033[0m %s\n' "$*" >&2; exit 1; }

build() {
    log "Building static musl release binary..."
    rustup target add "$TARGET" 2>/dev/null || true
    cargo build --release --target "$TARGET" --workspace

    local bin="target/$TARGET/$PROFILE/$BIN"
    [ -f "$bin" ] || die "binary not found at $bin"

    # Verify static linking
    if command -v file &>/dev/null; then
        file "$bin" | grep -q "statically linked" \
            || die "binary is not statically linked"
    fi

    # Create a minimal services.d for first boot
    mkdir -p "$SERVICES_DIR"
    if [ ! -f "$SERVICES_DIR/hello.conf" ]; then
        cat > "$SERVICES_DIR/hello.conf" <<'EOF'
type = script
command = echo "zinit is alive"; exec /bin/sleep 30
ready = none
log = none
restart = never
EOF
        log "Created $SERVICES_DIR/hello.conf (edit before deploy)"
    fi

    log "Packaging $TARBALL..."
    tar czf "$TARBALL" \
        -C "target/$TARGET/$PROFILE" "$BIN" \
        -C "$OLDPWD" "$SERVICES_DIR"

    log "Done: $TARBALL ($(stat -c%s "$TARBALL") bytes)"
}

deploy() {
    local host="${1:?usage: deploy <host> [user]}"
    local user="${2:-root}"

    [ -f "$TARBALL" ] || die "$TARBALL not found — run 'build' first"

    log "Copying $TARBALL to $user@$host:/tmp/..."
    scp -o StrictHostKeyChecking=accept-new "$TARBALL" "$user@$host:/tmp/"

    log "Extracting and installing on $host..."
    ssh -o StrictHostKeyChecking=accept-new "$user@$host" bash -s <<'REMOTE'
        set -euo pipefail
        cd /tmp
        tar xzf zinit-deploy.tar.gz
        install -m 0755 zinit /usr/local/bin/zinit
        mkdir -p /etc/zinit/services.d
        cp -r services.d/* /etc/zinit/services.d/ 2>/dev/null || true
        echo "zinit installed to /usr/local/bin/zinit"
        echo "Services in /etc/zinit/services.d/"
        echo ""
        echo "To test as PID 1 in a VM:"
        echo "  qemu-system-x86_64 -kernel /boot/vmlinuz \\"
        echo "    -append 'init=/usr/local/bin/zinit console=ttyS0' \\"
        echo "    -nographic"
REMOTE

    log "Deploy complete on $host"
}

qemu() {
    local disk="${1:?usage: qemu <disk.img>}"
    local kernel="${2:-/boot/vmlinuz}"

    [ -f "$TARBALL" ] || die "$TARBALL not found — run 'build' first"
    [ -f "$kernel" ] || die "kernel not found at $kernel (pass as 2nd arg)"

    # Extract to a temp dir for the initramfs
    local tmp
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    tar xzf "$TARBALL" -C "$tmp"

    # Build a minimal initramfs with zinit + a service
    mkdir -p "$tmp/rootfs"/{bin,etc/zinit/services.d,proc,sys,dev,run}
    cp "$tmp/zinit" "$tmp/rootfs/bin/"
    cp "$tmp/services.d/"* "$tmp/rootfs/etc/zinit/services.d/" 2>/dev/null || true

    # Create init script that mounts and execs zinit
    cat > "$tmp/rootfs/init" <<'INIT'
#!/bin/sh
mount -t proc none /proc
mount -t sysfs none /sys
mount -t tmpfs none /run
exec /bin/zinit --init
INIT
    chmod +x "$tmp/rootfs/init"

    # Build initramfs
    (cd "$tmp/rootfs" && find . -print0 | cpio --null -o -H newc | gzip) > "$tmp/initramfs.cpio.gz"

    log "Launching QEMU with zinit as PID 1..."
    qemu-system-x86_64 \
        -kernel "$kernel" \
        -initrd "$tmp/initramfs.cpio.gz" \
        -append "init=/init console=ttyS0 panic=1" \
        -nographic \
        -m 512 \
        "$@"
}

main() {
    local cmd="${1:-help}"
    shift || true
    case "$cmd" in
        build)  build "$@" ;;
        deploy) deploy "$@" ;;
        qemu)   qemu "$@" ;;
        help|*) grep '^#' "$0" | head -12 ;;
    esac
}

main "$@"
