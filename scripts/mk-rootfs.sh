#!/bin/sh
# Build a small ext4 root filesystem containing jiminit + busybox and the
# config file, so the initramfs stage has a real root to switch_root into.
set -eu
cd "$(dirname "$0")/.."

BUSYBOX=${BUSYBOX:-/usr/bin/busybox}
OUT=out
ROOT=$OUT/rootfs
IMAGE=$OUT/rootfs.ext4
SIZE=${SIZE:-64M}

if [ ! -x "$OUT/jiminit" ]; then
    echo "out/jiminit missing; run scripts/build.sh first" >&2
    exit 1
fi
if [ ! -x "$BUSYBOX" ]; then
    echo "no busybox at $BUSYBOX (set BUSYBOX=...)" >&2
    exit 1
fi

rm -rf "$ROOT"
mkdir -p "$ROOT/bin" "$ROOT/sbin" "$ROOT/etc" "$ROOT/root" "$ROOT/proc" \
    "$ROOT/sys" "$ROOT/dev" "$ROOT/tmp" "$ROOT/run"
install -m755 "$OUT/jiminit" "$ROOT/sbin/jiminit"
ln -sf sbin/jiminit "$ROOT/init"
install -m755 "$BUSYBOX" "$ROOT/bin/busybox"

for applet in sh mount umount cat ls echo sleep dmesg kill ps free uname \
    hostname date tail head grep wc mkdir rm mv ln sync poweroff reboot halt \
    ip ifconfig route clear vi less more stty id whoami; do
    ln -sf busybox "$ROOT/bin/$applet"
done

cat > "$ROOT/etc/jiminit.toml" <<'EOF'
# jiminit configuration (all keys optional)
hostname = "jiminit-qemu"
console = "/dev/console"
shell = "/bin/sh"
respawn = true
shutdown_timeout_ms = 3000
init_path = "/sbin/jiminit"
EOF

rm -f "$IMAGE"
mke2fs -q -F -t ext4 -L JIMROOT -d "$ROOT" "$IMAGE" "$SIZE"
ls -lh "$IMAGE"
