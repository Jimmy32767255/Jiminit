#!/bin/sh
# Build a minimal initramfs: jiminit as /init plus a static busybox for
# debugging. Called by QEMU as `rdinit=/init`.
set -eu
cd "$(dirname "$0")/.."

BUSYBOX=${BUSYBOX:-/usr/bin/busybox}
OUT=out
ROOT=$OUT/initramfs

if [ ! -x "$OUT/jiminit" ]; then
    echo "out/jiminit missing; run scripts/build.sh first" >&2
    exit 1
fi
if [ ! -x "$BUSYBOX" ]; then
    echo "no busybox at $BUSYBOX (set BUSYBOX=...)" >&2
    exit 1
fi

rm -rf "$ROOT"
mkdir -p "$ROOT/bin" "$ROOT/sbin" "$ROOT/proc" "$ROOT/sys" "$ROOT/dev" \
    "$ROOT/etc" "$ROOT/newroot" "$ROOT/tmp"
install -m755 "$OUT/jiminit" "$ROOT/init"
install -m755 "$BUSYBOX" "$ROOT/bin/busybox"

for applet in sh mount umount cat ls echo sleep dmesg; do
    ln -sf busybox "$ROOT/bin/$applet"
done

# The smoke test uses a stub fsck to exercise the "fsck available" path
# without shipping a full e2fsprogs in the initramfs.
if [ "${FSCK_STUB:-0}" = "1" ]; then
    cat > "$ROOT/sbin/fsck" <<'EOF'
#!/bin/sh
echo "stub-fsck: $*"
exit 0
EOF
    chmod 755 "$ROOT/sbin/fsck"
fi

# Likewise a stub modprobe plus a small /etc/modules and modprobe.d fixture
# lets the smoke test exercise the module-loading path.
if [ "${MODPROBE_STUB:-0}" = "1" ]; then
    mkdir -p "$ROOT/etc/modprobe.d"
    cat > "$ROOT/etc/modules" <<'EOF'
# Modules loaded before the root device is resolved.
virtio_blk
foo irq=5
blacklisted_mod
baz
mynet
blocked_alias
EOF
    cat > "$ROOT/etc/modprobe.d/test.conf" <<'EOF'
blacklist blacklisted_mod
blacklist hidden_mod
options foo irq=9 timeout=2
install baz echo "stub-install: baz"
alias mynet foo
alias blocked_alias hidden_mod
EOF
    cat > "$ROOT/sbin/modprobe" <<'EOF'
#!/bin/sh
echo "stub-modprobe: $*"
EOF
    if [ "${MODPROBE_STUB_FAIL:-0}" = "1" ]; then
        echo "exit 1" >> "$ROOT/sbin/modprobe"
    else
        echo "exit 0" >> "$ROOT/sbin/modprobe"
    fi
    chmod 755 "$ROOT/sbin/modprobe"
fi

# Write the cpio archive first so a cpio failure is not hidden by gzip's
# exit status (POSIX sh has no pipefail).
(cd "$ROOT" && find . -print0 | cpio --null -o --format=newc) > "$OUT/initramfs.cpio"
gzip -9 -c "$OUT/initramfs.cpio" > "$OUT/initramfs.cpio.gz"
rm -f "$OUT/initramfs.cpio"

ls -lh "$OUT/initramfs.cpio.gz"
