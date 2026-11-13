#!/bin/sh
# Build out/luks.img: a LUKS2 container holding one LVM volume group with an
# ext4 root logical volume, plus out/crypttab for the initramfs.
#
# Needs root (device-mapper/loop devices), e.g.:
#   qt-sudo ./scripts/mk-luks-image.sh
set -eu
cd "$(dirname "$0")/.."

IMG=out/luks.img
PASS=out/luks.pass
CRYPTTAB=out/crypttab
IMG_MB=${IMG_MB:-256}
LV_MB=${LV_MB:-192}
VG=jiminitvg
NAME=jimcrypt
LV=root

if [ "$(id -u)" != 0 ]; then
    echo "run as root: qt-sudo $0" >&2
    exit 1
fi

for program in cryptsetup pvcreate vgcreate lvcreate mke2fs; do
    command -v "$program" >/dev/null || { echo "missing $program" >&2; exit 1; }
done

# Always leave the host's device-mapper namespace clean, even on failure.
cleanup() {
    vgchange -an "$VG" >/dev/null 2>&1 || true
    cryptsetup close "$NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Never touch a volume group we did not create: this machine has its own
# vg0 for the real system.
if vgs "$VG" >/dev/null 2>&1; then
    echo "volume group $VG already exists; remove it first" >&2
    exit 1
fi

cleanup
rm -f "$IMG" "$PASS"

# Build the root filesystem as the normal user first: running cargo as root
# would use root's rustup/cargo home and fail. Only the device-mapper work
# below needs privileges.
if [ ! -x out/jiminit ] || [ ! -d out/rootfs ]; then
    echo "run ./scripts/build.sh && ./scripts/mk-rootfs.sh as your user first" >&2
    exit 1
fi

umask 077
printf 'jiminit' > "$PASS"

truncate -s "${IMG_MB}M" "$IMG"
cryptsetup luksFormat --batch-mode --type luks2 --pbkdf pbkdf2 \
    --pbkdf-force-iterations 2000 --key-file "$PASS" "$IMG"
UUID=$(cryptsetup luksUUID "$IMG")
cryptsetup luksOpen --key-file "$PASS" "$IMG" "$NAME"

pvcreate -ff -y "/dev/mapper/$NAME"
vgcreate "$VG" "/dev/mapper/$NAME"
lvcreate -y -L "${LV_MB}M" -n "$LV" "$VG"
mke2fs -q -F -t ext4 -L jiminit-luks -d out/rootfs "/dev/mapper/$VG-$LV"

printf 'jimcrypt UUID=%s none luks\n' "$UUID" > "$CRYPTTAB"
sync

cleanup
trap - EXIT

# The image and the temporary tree were created as root; hand them back so a
# normal `./scripts/smoke-test.sh` can rebuild out/ later.
chown -R "$(stat -c %u:%g out)" out

echo "built $IMG (LUKS UUID $UUID)"
echo "initramfs crypttab: $CRYPTTAB"
