#!/bin/sh
# Boot a LUKS2 + LVM root in QEMU: crypttab unlocks the container, LVM gets
# activated on demand, then the normal login/shutdown flow runs.
#
# Build the disk first (needs root): qt-sudo ./scripts/mk-luks-image.sh
set -eu
cd "$(dirname "$0")/.."

OUT=out
LOG=$OUT/smoke-luks.log
FIFO=$OUT/smoke-luks.fifo
KERNEL=${KERNEL:-/boot/vmlinuz-linux}

if [ ! -f "$OUT/luks.img" ] || [ ! -f "$OUT/crypttab" ]; then
    echo "out/luks.img or out/crypttab missing; run: qt-sudo ./scripts/mk-luks-image.sh" >&2
    exit 1
fi

./scripts/build.sh
LUKS=1 CRYPTAB="$OUT/crypttab" ./scripts/mk-initramfs.sh

rm -f "$LOG" "$FIFO"
mkfifo "$FIFO"
exec 9<>"$FIFO"

qemu-system-x86_64 -m 512M -smp 2 \
    -kernel "$KERNEL" \
    -initrd "$OUT/initramfs.cpio.gz" \
    -drive file="$OUT/luks.img",format=raw,if=virtio \
    -append "console=ttyS0 rdinit=/init root=/dev/mapper/jiminitvg-root rootfstype=ext4 lvm=1 rw" \
    -nographic -no-reboot <"$FIFO" >"$LOG" 2>&1 &
QEMU_PID=$!

fail() {
    echo "FAIL: $*" >&2
    tail -60 "$LOG" >&2
    kill "$QEMU_PID" 2>/dev/null || true
    exit 1
}

wait_for() {
    pattern=$1
    timeout=${2:-60}
    i=0
    while [ "$i" -lt $((timeout * 2)) ]; do
        if grep -q "$pattern" "$LOG"; then
            return 0
        fi
        sleep 0.5
        i=$((i + 1))
    done
    return 1
}

wait_for "Enter passphrase" 90 || fail "cryptsetup never prompted for a passphrase"
sleep 1
printf 'jiminit\n' >&9

wait_for "crypttab: jimcrypt is ready" 60 || fail "LUKS container was not unlocked"
wait_for "lvm: volume groups activated" 60 || fail "LVM volume groups were not activated"
wait_for "login:" 90 || fail "no login prompt on the LUKS root"
wait_for "jiminit-qemu login:" 30 || true

printf 'root\n' >&9
wait_for "Password:" 30 || fail "login did not ask for a password"
sleep 1
printf 'jiminit\n' >&9
sleep 1
printf 'echo LUKS-SHELL-OK\n' >&9
wait_for "LUKS-SHELL-OK" 30 || fail "login on the encrypted root failed"

printf 'kill -TERM 1\n' >&9

i=0
while [ "$i" -lt 180 ]; do
    kill -0 "$QEMU_PID" 2>/dev/null || break
    sleep 0.5
    i=$((i + 1))
done

grep -q 'crypttab: jimcrypt is ready' "$LOG" || fail "unlock log missing"
grep -q 'lvm: activating volume groups' "$LOG" || fail "LVM activation log missing"
grep -q 'LUKS-SHELL-OK' "$LOG" || fail "shell output missing"
grep -q 'reboot: Power down' "$LOG" || fail "poweroff did not happen"

if kill -0 "$QEMU_PID" 2>/dev/null; then
    fail "QEMU did not exit after poweroff"
fi

echo "PASS: LUKS unlock, LVM activation, shell and poweroff all work"
