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

KERNEL=${KERNEL:-/boot/vmlinuz-linux}
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

exec qemu-system-x86_64 \
    -m 512M -smp 2 \
    -kernel "$KERNEL" \
    -initrd out/initramfs.cpio.gz \
    -drive file=out/rootfs.ext4,format=raw,if=virtio \
    -append "console=ttyS0 rdinit=/init root=/dev/vda rootfstype=ext4 rw" \
    -nographic -no-reboot "$@"
