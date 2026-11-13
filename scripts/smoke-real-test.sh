#!/bin/sh
# Boot with the real-hardware helpers: modalias coldplug with kmod's
# modprobe, storage/filesystem modules packaged, and e2fsprogs' e2fsck.
set -eu
cd "$(dirname "$0")/.."

OUT=out
LOG=$OUT/smoke-real.log
FIFO=$OUT/smoke-real.fifo
KERNEL=${KERNEL:-/boot/vmlinuz-linux}

./scripts/build.sh >/dev/null
./scripts/mk-rootfs.sh >/dev/null
FSCK=1 COLDPLUG=1 ./scripts/mk-initramfs.sh >/dev/null

rm -f "$LOG" "$FIFO"
mkfifo "$FIFO"
exec 9<>"$FIFO"

qemu-system-x86_64 -m 512M -smp 2 \
    -kernel "$KERNEL" \
    -initrd "$OUT/initramfs.cpio.gz" \
    -drive file="$OUT/rootfs.ext4",format=raw,if=virtio \
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

wait_for "coldplug: processed" 60 || fail "modalias coldplug did not run"
wait_for "login:" 90 || fail "no login prompt"
printf 'root\n' >&9
wait_for "Password:" 30 || fail "login did not ask for a password"
sleep 1
printf 'jiminit\n' >&9
sleep 1
printf 'echo REAL-SHELL-OK\n' >&9
wait_for "REAL-SHELL-OK" 30 || fail "login shell did not run"
printf 'kill -TERM 1\n' >&9

i=0
while [ "$i" -lt 180 ]; do
    kill -0 "$QEMU_PID" 2>/dev/null || break
    sleep 0.5
    i=$((i + 1))
done

grep -q 'coldplug: [0-9][0-9]* distinct modalias entries' "$LOG" ||
    fail "no modalias entries were collected"
grep -q 'coldplug: processed' "$LOG" || fail "coldplug did not call modprobe"
grep -q 'modules: modprobe ext4' "$LOG" || fail "filesystem modules were not loaded"
grep -q 'fsck: no fsck program' "$LOG" && fail "real fsck was not packaged"
grep -q 'fsck: exited with 0' "$LOG" || fail "real e2fsck did not run"
grep -q 'pivot_root: switched to the new root' "$LOG" || fail "pivot_root did not run"
grep -q 'REAL-SHELL-OK' "$LOG" || fail "shell output missing"
grep -q 'reboot: Power down' "$LOG" || fail "poweroff did not happen"
if kill -0 "$QEMU_PID" 2>/dev/null; then
    fail "QEMU did not exit after poweroff"
fi

echo "PASS: modalias coldplug, packaged e2fsck, shell and poweroff all work"
