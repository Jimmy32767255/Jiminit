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

# Packaging for an encrypted (and optionally LVM) root: the real cryptsetup,
# lvm and modprobe binaries plus their libraries, the device-mapper kernel
# modules and the crypttab. This keeps the default image small.
if [ "${LUKS:-0}" = "1" ]; then
    for program in /usr/bin/cryptsetup /usr/bin/lvm; do
        copy_with_libs "$program"
    done
    ln -sf /usr/bin/cryptsetup "$ROOT/sbin/cryptsetup"
    ln -sf /usr/bin/lvm "$ROOT/sbin/lvm"

    # kmod's modprobe understands the modules.dep index, unlike staged
    # busybox modprobe variants; stage 1 needs it to load dm_mod/dm_crypt.
    KMOD_MODPROBE=${KMOD_MODPROBE:-$(command -v modprobe)}
    copy_with_libs "$KMOD_MODPROBE"
    if [ "$KMOD_MODPROBE" != /usr/bin/modprobe ]; then
        mkdir -p "$ROOT/usr/bin"
        cp -L "$KMOD_MODPROBE" "$ROOT/usr/bin/modprobe"
    fi

    mkdir -p "$ROOT/etc/lvm/archive" "$ROOT/etc/lvm/backup" "$ROOT/run" \
        "$ROOT/etc/lvm"
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

    kmod_dir="/lib/modules/$(uname -r)"
    {
        modprobe --show-depends dm_mod
        modprobe --show-depends dm_crypt
    } | awk '$1 == "insmod" {print $2}' | sort -u > "$OUT/kmods.tmp"
    while read -r module; do
        mkdir -p "$ROOT$(dirname "$module")"
        cp "$module" "$ROOT$module"
    done < "$OUT/kmods.tmp"
    rm -f "$OUT/kmods.tmp"
    for name in modules.dep modules.dep.bin modules.builtin modules.builtin.bin \
        modules.alias modules.alias.bin; do
        if [ -f "$kmod_dir/$name" ]; then
            mkdir -p "$ROOT$kmod_dir"
            cp "$kmod_dir/$name" "$ROOT$kmod_dir/$name"
        fi
    done

    mkdir -p "$ROOT/etc"
    printf '# device-mapper and dm-crypt for LUKS/LVM\ndm_mod\ndm_crypt\n' \
        > "$ROOT/etc/modules"

    if [ -n "${CRYPTAB:-}" ]; then
        install -m644 "$CRYPTAB" "$ROOT/etc/crypttab"
    fi
fi

# Write the cpio archive first so a cpio failure is not hidden by gzip's
# exit status (POSIX sh has no pipefail).
(cd "$ROOT" && find . -print0 | cpio --null -o --format=newc) > "$OUT/initramfs.cpio"
gzip -9 -c "$OUT/initramfs.cpio" > "$OUT/initramfs.cpio.gz"
rm -f "$OUT/initramfs.cpio"

ls -lh "$OUT/initramfs.cpio.gz"
