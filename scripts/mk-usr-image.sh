#!/bin/sh
# Build a root image whose /usr lives on a separate ext4 filesystem and whose
# init binary is /usr/sbin/jiminit (referenced from /etc/fstab by UUID).
set -eu
cd "$(dirname "$0")/.."

USR_UUID=22222222-2222-2222-2222-222222222222
OUT=out

./scripts/build.sh
./scripts/mk-rootfs.sh

# The /usr tree: only what stage 2 needs from it for this test.
rm -rf "$OUT/usr"
mkdir -p "$OUT/usr/sbin" "$OUT/usr/bin"
install -m755 "$OUT/jiminit" "$OUT/usr/sbin/jiminit"

mkdir -p "$OUT/rootfs/usr"

# Point the real root at the separate /usr and make sure the init binary is
# not reachable from the root filesystem.
rm -f "$OUT/rootfs/sbin/jiminit" "$OUT/rootfs/init"
sed -i 's|^init_path = .*|init_path = "/usr/sbin/jiminit"|' \
    "$OUT/rootfs/etc/jiminit.toml"
grep -q "UUID=$USR_UUID /usr " "$OUT/rootfs/etc/fstab" ||
    echo "UUID=$USR_UUID /usr ext4 rw 0 2" >> "$OUT/rootfs/etc/fstab"

mke2fs -q -F -t ext4 -U "$USR_UUID" -L JIMINIT-USR -d "$OUT/usr" \
    "$OUT/usr.ext4" 32M
mke2fs -q -F -t ext4 -L JIMINIT-ROOT -d "$OUT/rootfs" \
    "$OUT/rootfs-usr.ext4" 64M

echo "built $OUT/rootfs-usr.ext4 and $OUT/usr.ext4 (/usr/sbin/jiminit)"
