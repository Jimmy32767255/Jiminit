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

(cd "$ROOT" && find . -print0 | cpio --null -o --format=newc 2>/dev/null | gzip -9) \
    > "$OUT/initramfs.cpio.gz"

ls -lh "$OUT/initramfs.cpio.gz"
