#!/bin/sh
# Install an example jiminit boot path on this machine (needs root).
#
# Everything this script installs is a starting point: it never overwrites an
# existing /etc/jiminit.toml, /etc/jiminit/crypttab or
# /etc/mkinitcpio-jiminit.conf, because those carry machine-specific values.
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HERE="$ROOT/contrib/example"

install -Dm755 "$ROOT/out/jiminit" /sbin/jiminit

if [ ! -f /etc/jiminit.toml ]; then
    install -Dm644 "$HERE/jiminit.toml" /etc/jiminit.toml
else
    echo "keeping existing /etc/jiminit.toml" >&2
fi

# crypttab holds LUKS UUIDs, so only seed it when the machine has none yet.
if [ ! -f /etc/jiminit/crypttab ]; then
    install -Dm600 "$HERE/crypttab.example" /etc/jiminit/crypttab
    echo "warning: edit /etc/jiminit/crypttab and replace the placeholder UUIDs" >&2
fi

install -d /etc/jiminit/services.d
for svc in "$HERE"/services/*.toml; do
    install -Dm644 "$svc" "/etc/jiminit/services.d/$(basename "$svc")"
done

install -d /etc/initcpio/install /etc/initcpio/hooks
install -Dm755 "$ROOT/contrib/mkinitcpio/install/jiminit" /etc/initcpio/install/jiminit
install -Dm644 "$ROOT/contrib/mkinitcpio/hooks/jiminit" /etc/initcpio/hooks/jiminit

# Keep an existing config: it may carry host-specific tweaks (extra modules,
# different compression) that this script must not overwrite.
if [ ! -f /etc/mkinitcpio-jiminit.conf ]; then
    sed -e 's|^MODULES=.*|MODULES=(dm_crypt dm_mod ext4 vfat)|' \
        -e 's|^HOOKS=.*|HOOKS=(base udev autodetect microcode modconf kms keyboard keymap consolefont block lvm2 jiminit)|' \
        /etc/mkinitcpio.conf > /etc/mkinitcpio-jiminit.conf
fi

# One initramfs per installed kernel: mkinitcpio bakes modules for exactly one
# kernel version, so a single image cannot boot every installed kernel.
for pkgbase in linux linux-lts linux-zen; do
    modver=
    for p in /usr/lib/modules/*/pkgbase; do
        if [ "$(cat "$p")" = "$pkgbase" ]; then
            modver=$(basename "$(dirname "$p")")
            break
        fi
    done
    if [ -z "$modver" ]; then
        echo "warning: kernel package $pkgbase is not installed, skipping" >&2
        continue
    fi
    mkinitcpio -c /etc/mkinitcpio-jiminit.conf -k "$modver" \
        -g "/boot/initramfs-jiminit-$pkgbase.img"
done

# The GRUB entry contains LUKS/disk UUIDs, so it is opt-in.
if [ "${INSTALL_GRUB:-0}" = 1 ]; then
    install -Dm755 "$HERE/41_jiminit.example" /etc/grub.d/41_jiminit
    grub-mkconfig -o /boot/grub/grub.cfg
    echo "installed /etc/grub.d/41_jiminit" >&2
else
    echo "skipped GRUB entry; set INSTALL_GRUB=1 to install it" >&2
fi

"$ROOT/out/jiminit" check
