#!/bin/sh
# Automated smoke test: boot the VM, verify switch_root + stage2 + shell,
# then power off and check that QEMU exits. Prints PASS or FAIL.
set -eu
cd "$(dirname "$0")/.."

LOG=out/smoke.log
FIFO=out/smoke.fifo
TIMEOUT=90

fail() {
    echo "FAIL: $*" >&2
    if [ -f "$LOG" ]; then
        echo "--- last log lines ---" >&2
        tail -30 "$LOG" >&2
    fi
    exit 1
}

./scripts/build.sh >/dev/null
./scripts/mk-initramfs.sh >/dev/null
./scripts/mk-rootfs.sh >/dev/null

rm -f "$FIFO" "$LOG"
mkfifo "$FIFO"
# Hold the FIFO open read+write so QEMU's stdin never blocks at startup.
exec 9<>"$FIFO"

qemu-system-x86_64 \
    -m 512M -smp 2 \
    -kernel "${KERNEL:-/boot/vmlinuz-linux}" \
    -initrd out/initramfs.cpio.gz \
    -drive file=out/rootfs.ext4,format=raw,if=virtio \
    -append "console=ttyS0 rdinit=/init root=/dev/vda rootfstype=ext4 rw quiet" \
    -nographic -no-reboot <"$FIFO" >"$LOG" 2>&1 &
qpid=$!

sleep 10
if ! kill -0 "$qpid" 2>/dev/null; then
    fail "QEMU exited before the shell was reached"
fi

printf 'echo SMOKE-SHELL\nhostname\nkill -TERM 1\n' >"$FIFO"

waited=0
while kill -0 "$qpid" 2>/dev/null; do
    waited=$((waited + 1))
    if [ "$waited" -gt "$TIMEOUT" ]; then
        kill "$qpid" 2>/dev/null || true
        fail "timed out waiting for poweroff"
    fi
    sleep 1
done
wait "$qpid" || true

grep -q 'moved new root onto /' "$LOG" || fail "switch_root did not run"
grep -q 'stage2: starting as pid 1' "$LOG" || fail "stage2 did not start as PID 1"
grep -q 'hostname set to jiminit-qemu' "$LOG" || fail "config file was not applied"
grep -q 'SMOKE-SHELL' "$LOG" || fail "root shell did not run commands"
grep -q 'reboot: Power down' "$LOG" || fail "poweroff was not requested"

echo "PASS: switch_root, stage2 PID 1, shell and poweroff all work"
