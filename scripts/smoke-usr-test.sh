#!/bin/sh
# Boot with /usr on a separate ext4: stage 1 must mount it from fstab,
# pivot_root into the real root and run stage 2 from /usr/sbin/jiminit.
set -eu
cd "$(dirname "$0")/.."

OUT=out
LOG=$OUT/smoke-usr.log
FIFO=$OUT/smoke-usr.fifo
KERNEL=${KERNEL:-/boot/vmlinuz-linux}

./scripts/mk-usr-image.sh >/dev/null
./scripts/mk-initramfs.sh >/dev/null

rm -f "$LOG" "$FIFO"
mkfifo "$FIFO"
exec 9<>"$FIFO"

qemu-system-x86_64 -m 512M -smp 2 \
    -kernel "$KERNEL" \
    -initrd "$OUT/initramfs.cpio.gz" \
    -drive file="$OUT/rootfs-usr.ext4",format=raw,if=virtio \
    -drive file="$OUT/usr.ext4",format=raw,if=virtio \
    -append "console=ttyS0 rdinit=/init root=/dev/vda rootfstype=ext4 rw" \
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

wait_for "login:" 90 || fail "no login prompt"
printf 'root\n' >&9
wait_for "Password:" 30 || fail "login did not ask for a password"
sleep 1
printf 'jiminit\n' >&9
sleep 1
printf 'readlink /proc/1/exe\n' >&9
wait_for "/usr/sbin/jiminit" 30 || fail "PID 1 is not running from /usr"
printf 'echo USR-SHELL-OK\n' >&9
wait_for "USR-SHELL-OK" 30 || fail "login shell did not run"
printf 'kill -TERM 1\n' >&9

i=0
while [ "$i" -lt 180 ]; do
    kill -0 "$QEMU_PID" 2>/dev/null || break
    sleep 0.5
    i=$((i + 1))
done

grep -q 'usr: /usr comes from a separate filesystem' "$LOG" ||
    fail "/usr was not recognized as a separate mount"
grep -q "fstab: mounted .* on /newroot/usr" "$LOG" ||
    fail "/usr was not mounted from fstab"
grep -q 'pivot_root: switched to the new root' "$LOG" || fail "pivot_root did not run"
grep -q 'stage2: starting as pid 1' "$LOG" || fail "stage2 did not start"
grep -q 'USR-SHELL-OK' "$LOG" || fail "shell output missing"
grep -q 'reboot: Power down' "$LOG" || fail "poweroff did not happen"
if kill -0 "$QEMU_PID" 2>/dev/null; then
    fail "QEMU did not exit after poweroff"
fi

echo "PASS: /usr mounted from fstab, pivot_root, PID 1 from /usr, poweroff all work"
