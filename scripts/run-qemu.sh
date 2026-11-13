#!/bin/sh
# Boot the test VM. Boots the host kernel with our initramfs, which mounts
# the ext4 image as /, switches root and finally runs jiminit as PID 1.
#
#   rdinit=/init         -> stage 1 from the initramfs
#   root=/dev/vda        -> virtio-blk disk (ata_piix is a module here)
#
# The kernel comes from the host because we do not build one; this makes the
# test about jiminit, not about a distro. Exit the VM with ctrl-a x or by
# running `poweroff` inside it.
set -eu
cd "$(dirname "$0")/.."

ARCH=${ARCH:-$(uname -m)}
case "$ARCH" in
    x86_64)
        QEMU=qemu-system-x86_64
        MACHINE=
        APPEND="console=ttyS0 rdinit=/init root=/dev/vda rootfstype=ext4 rw"
        KERNEL=${KERNEL:-/boot/vmlinuz-linux}
        ;;
    aarch64|arm64)
        QEMU=qemu-system-aarch64
        MACHINE="-M virt -cpu cortex-a57"
        APPEND="console=ttyAMA0 rdinit=/init root=/dev/vda rootfstype=ext4 rw"
        KERNEL=${KERNEL:-/boot/Image}
        ;;
    *)
        echo "unsupported ARCH=$ARCH (use x86_64 or aarch64)" >&2
        exit 1
        ;;
esac
if [ ! -f "$KERNEL" ]; then
    echo "no kernel at $KERNEL (set KERNEL=...)" >&2
    exit 1
fi
for f in out/initramfs.cpio.gz out/rootfs.ext4; do
    if [ ! -f "$f" ]; then
        echo "missing $f; run scripts/build.sh && scripts/mk-initramfs.sh && scripts/mk-rootfs.sh" >&2
        exit 1
    fi
done

exec "$QEMU" \
    -m 512M -smp 2 $MACHINE \
    -kernel "$KERNEL" \
    -initrd out/initramfs.cpio.gz \
    -drive file=out/rootfs.ext4,format=raw,if=virtio \
    -append "$APPEND" \
    -nographic -no-reboot "$@"
