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

# Copy a dynamically linked program and every shared library ldd reports.
copy_with_libs() {
    src=$1
    [ -e "$src" ] || { echo "missing $src" >&2; exit 1; }
    mkdir -p "$ROOT$(dirname "$src")"
    cp -L "$src" "$ROOT$src"
    ldd "$src" > "$OUT/ldd.tmp"
    while read -r line; do
        case "$line" in
            *"=>"*) lib=$(printf '%s\n' "$line" | awk '{print $3}') ;;
            /*)     lib=$(printf '%s\n' "$line" | awk '{print $1}') ;;
            *)      continue ;;
        esac
        [ -n "$lib" ] && [ -e "$lib" ] || continue
        mkdir -p "$ROOT$(dirname "$lib")"
        cp -L "$lib" "$ROOT$lib"
    done < "$OUT/ldd.tmp"
    rm -f "$OUT/ldd.tmp"
    # ldd reports the loader under its resolved path, but the binary asks for
    # the path recorded in PT_INTERP (e.g. /lib64/ld-linux-x86-64.so.2).
    interp=$(readelf -l "$src" 2>/dev/null |
        sed -n 's/.*Requesting program interpreter: \(.*\)\]/\1/p')
    if [ -n "$interp" ] && [ -e "$interp" ]; then
        mkdir -p "$ROOT$(dirname "$interp")"
        cp -L "$interp" "$ROOT$interp"
    fi
}

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

# --- optional packaging hooks -----------------------------------------
# The default image stays small; these are opt-in.
package_kmod() {
    KMOD_MODPROBE=${KMOD_MODPROBE:-$(command -v modprobe)}
    copy_with_libs "$KMOD_MODPROBE"
    if [ "$KMOD_MODPROBE" != /usr/bin/modprobe ]; then
        mkdir -p "$ROOT/usr/bin"
        cp -L "$KMOD_MODPROBE" "$ROOT/usr/bin/modprobe"
    fi
    # The kernel's request_module() helper looks for /sbin/modprobe.
    ln -sf /usr/bin/modprobe "$ROOT/sbin/modprobe"

    kmod_dir="/lib/modules/$(uname -r)"
    for name in modules.dep modules.dep.bin modules.builtin modules.builtin.bin \
        modules.alias modules.alias.bin; do
        if [ -f "$kmod_dir/$name" ]; then
            mkdir -p "$ROOT$kmod_dir"
            cp "$kmod_dir/$name" "$ROOT$kmod_dir/$name"
        fi
    done
}

# Copy the listed modules and all their dependencies into the image.
package_modules() {
    : > "$OUT/kmods.tmp"
    for module in $1; do
        modprobe --show-depends "$module" 2>/dev/null |
            awk '$1 == "insmod" {print $2}' >> "$OUT/kmods.tmp"
    done
    sort -u "$OUT/kmods.tmp" -o "$OUT/kmods.tmp"
    while read -r ko; do
        mkdir -p "$ROOT$(dirname "$ko")"
        cp "$ko" "$ROOT$ko"
    done < "$OUT/kmods.tmp"
    rm -f "$OUT/kmods.tmp"
}

# Packaging for an encrypted (and optionally LVM) root: the real cryptsetup,
# lvm and modprobe binaries, the device-mapper modules and the crypttab.
if [ "${LUKS:-0}" = "1" ]; then
    for program in /usr/bin/cryptsetup /usr/bin/lvm; do
        copy_with_libs "$program"
    done
    ln -sf /usr/bin/cryptsetup "$ROOT/sbin/cryptsetup"
    ln -sf /usr/bin/lvm "$ROOT/sbin/lvm"

    mkdir -p "$ROOT/etc/lvm/archive" "$ROOT/etc/lvm/backup" "$ROOT/run"
    cat > "$ROOT/etc/lvm/lvm.conf" <<'EOF'
# Initramfs LVM: no udev, no devices file.
devices {
    use_devicesfile = 0
    obtain_device_list_from_udev = 0
}
activation {
    udev_sync = 0
    udev_rules = 0
}
EOF

    package_kmod
    package_modules "dm_mod dm_crypt"

    mkdir -p "$ROOT/etc"
    printf '# device-mapper and dm-crypt for LUKS/LVM\ndm_mod\ndm_crypt\n' \
        > "$ROOT/etc/modules"

    if [ -n "${CRYPTAB:-}" ]; then
        install -m644 "$CRYPTAB" "$ROOT/etc/crypttab"
    fi
fi

# Real hardware: storage drivers are autoloaded from their modalias, while
# filesystem modules have no modalias and must be listed explicitly (mount(2)
# asks request_module for them, which needs /sbin/modprobe from package_kmod).
if [ "${COLDPLUG:-0}" = "1" ] && [ "${LUKS:-0}" != "1" ]; then
    package_kmod
    package_modules "${COLDPLUG_MODULES:-ahci sd_mod virtio_blk virtio_scsi nvme usb_storage xhci_pci xhci_hcd ext4 xfs btrfs vfat}"
    cat > "$ROOT/etc/modules" <<'EOF'
# Filesystems needed before the root mount; storage drivers come from the
# modalias coldplug in stage 1.
ext4
xfs
btrfs
vfat
EOF
fi

# FAT support for ESP-style fstab entries (short XXXX-XXXX UUIDs). With the
# stub modprobe in place, only calls for these modules are delegated to kmod.
if [ "${FAT:-0}" = "1" ]; then
    package_kmod
    package_modules "fat vfat"
    printf 'vfat\n' >> "$ROOT/etc/modules"
    if [ "${MODPROBE_STUB:-0}" = "1" ]; then
        # package_kmod left an absolute symlink; replace it, do not follow it.
        rm -f "$ROOT/sbin/modprobe"
        cat > "$ROOT/sbin/modprobe" <<'EOF'
#!/bin/sh
case "$1" in
fat | vfat | msdos | fs-fat | fs-vfat) exec /usr/bin/modprobe "$@" ;;
esac
echo "stub-modprobe: $*"
exit 0
EOF
        chmod 755 "$ROOT/sbin/modprobe"
    fi
fi

# Real fsck: symlink it as /sbin/fsck so stage 1's plain `fsck -p DEVICE`
# works without the util-linux dispatcher or an /etc/fstab.
if [ "${FSCK:-0}" = "1" ]; then
    for program in /sbin/e2fsck /sbin/fsck.ext4; do
        copy_with_libs "$program"
    done
    ln -sf e2fsck "$ROOT/sbin/fsck"
    ln -sf e2fsck "$ROOT/sbin/fsck.ext2"
    ln -sf e2fsck "$ROOT/sbin/fsck.ext3"
    ln -sf e2fsck "$ROOT/sbin/fsck.ext4"
fi

# Write the cpio archive first so a cpio failure is not hidden by gzip's
# exit status (POSIX sh has no pipefail).
(cd "$ROOT" && find . -print0 | cpio --null -o --format=newc) > "$OUT/initramfs.cpio"
gzip -9 -c "$OUT/initramfs.cpio" > "$OUT/initramfs.cpio.gz"
rm -f "$OUT/initramfs.cpio"

ls -lh "$OUT/initramfs.cpio.gz"
