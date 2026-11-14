#!/bin/sh
# Automated smoke test: boot the VM, log in through getty/login, exercise the
# control socket (status + runlevel switching), soft reboot, log in again, then
# power off and check that QEMU exits. Prints PASS or FAIL.
set -eu
cd "$(dirname "$0")/.."

LOG=out/smoke.log
FIFO=out/smoke.fifo
TIMEOUT=120

fail() {
    echo "FAIL: $*" >&2
    if [ -f "$LOG" ]; then
        echo "--- last log lines ---" >&2
        tail -40 "$LOG" >&2
    fi
    exit 1
}

count() {
    # grep exits 1 when there are no matches (and prints nothing when $LOG
    # does not exist yet), so always yield a number.
    matches=$(grep -c "$1" "$LOG" 2>/dev/null || true)
    echo "${matches:-0}"
}

wait_for() {
    pattern=$1
    limit=$2
    waited=0
    while [ "$(count "$pattern")" -lt 1 ]; do
        waited=$((waited + 1))
        [ "$waited" -gt "$limit" ] && fail "timed out waiting for: $pattern"
        sleep 1
    done
}

line_of() {
    # First line number containing $1 (0 when absent).
    grep -n -m1 "$1" "$LOG" 2>/dev/null | cut -d: -f1 || true
}

wait_count() {
    pattern=$1
    want=$2
    limit=$3
    waited=0
    while [ "$(count "$pattern")" -lt "$want" ]; do
        waited=$((waited + 1))
        [ "$waited" -gt "$limit" ] && fail "timed out waiting for $want x: $pattern"
        sleep 1
    done
}

login() {
    attempt=$1
    wait_count 'login:' "$attempt" 60
    printf 'root\n' >"$FIFO"
    wait_count 'Password:' "$attempt" 20
    # login flushes typed-ahead input before reading the password.
    sleep 1
    printf 'jiminit\n' >"$FIFO"
    sleep 2
}

./scripts/build.sh >/dev/null
FSCK_STUB=1 MODPROBE_STUB=1 FAT=1 ./scripts/mk-initramfs.sh >/dev/null
ESP=1 ./scripts/mk-rootfs.sh >/dev/null

rm -f "$FIFO" "$LOG"
: >"$LOG"
mkfifo "$FIFO"
# Hold the FIFO open read+write so QEMU's stdin never blocks at startup.
exec 9<>"$FIFO"

qemu-system-x86_64 \
    -m 512M -smp 2 \
    -kernel "${KERNEL:-/boot/vmlinuz-linux}" \
    -initrd out/initramfs.cpio.gz \
    -drive file=out/rootfs.ext4,format=raw,if=virtio \
    -drive file=out/esp.img,format=raw,if=virtio \
    -append "console=ttyS0 rdinit=/init root=/dev/vda rootfstype=ext4 rw quiet" \
    -nographic -no-reboot <"$FIFO" >"$LOG" 2>&1 &
qpid=$!

# 1. Boot to a getty prompt and log in as root.
login 1
printf 'jiminit --status\n' >"$FIFO"
wait_for 'status multi-user' 20
wait_for 'ticker running pid=' 20
wait_for 'cg running pid=.* memory=[0-9]' 20
printf 'test -s /var/log/jiminit.log && echo LOGFILE-OK\n' >"$FIFO"
wait_for 'LOGFILE-OK' 20
printf 'jiminit --status\n' >"$FIFO"
wait_for 'ticker running pid=.* cpu=' 20
printf 'jiminit --metrics\n' >"$FIFO"
wait_for 'jiminit_build_info{version="0.1.0"} 1' 20
wait_for 'jiminit_runlevel_info{runlevel="multi-user"} 1' 20
wait_for 'jiminit_service_cpu_seconds_total{name="ticker"}' 20
wait_for 'jiminit_service_memory_bytes{name="cg"}' 20
printf 'id\n' >"$FIFO"
wait_for 'uid=0(root)' 20
printf 'ls -l /run/jiminit.sock\n' >"$FIFO"
wait_for 'srw-------' 20

# 2. Switch to single: the login session is stopped and a root shell takes
#    over the console. The client runs in that dying session, so verify the
#    switch through the kernel log and the new shell.
printf 'jiminit --runlevel single &\n' >"$FIFO"
wait_for 'now in runlevel single' 20
sleep 1
printf 'echo SINGLE-SHELL-OK\n' >"$FIFO"
wait_for 'SINGLE-SHELL-OK' 20

# 3. Switch back to multi-user: getty must come back.
printf 'jiminit --runlevel multi-user &\n' >"$FIFO"
wait_for 'now in runlevel multi-user' 20
wait_count 'login:' 2 30

# 4. Log in again and exercise per-service control over the socket.
login 2
printf 'jiminit --service after stop\n' >"$FIFO"
wait_for 'ok after stopping' 15
wait_for 'svc/after: stopping on request' 15
printf 'jiminit --service after start\n' >"$FIFO"
wait_for 'ok after started' 15
printf 'jiminit --service after restart\n' >"$FIFO"
wait_for 'ok after restarting' 15
wait_count 'svc/after: started' 2 20
printf 'jiminit --service notifier reload\n' >"$FIFO"
wait_for 'ok notifier reloaded' 15
printf 'jiminit --service parent stop\n' >"$FIFO"
wait_for 'ok parent stopping' 15
wait_for 'stopping because dependency' 15

# 5. Soft reboot: every session is stopped, stage 2 re-execs and getty comes
#    back.
printf 'kill -USR1 1\n' >"$FIFO"
wait_count 'running stage2 again' 1 30
wait_count 'login:' 3 40

# 6. Log in on the new stage 2 and power off.
login 3
printf 'kill -TERM 1\n' >"$FIFO"

waited=0
while kill -0 "$qpid" 2>/dev/null; do
    waited=$((waited + 1))
    if [ "$waited" -gt "$TIMEOUT" ]; then
        kill "$qpid" 2>/dev/null || true
        fail "timed out waiting for poweroff"
    fi
    sleep 1
done
qemu_rc=0
wait "$qpid" || qemu_rc=$?
if [ "$qemu_rc" -ne 0 ]; then
    fail "qemu exited with status $qemu_rc"
fi

grep -q 'pivot_root: switched to the new root' "$LOG" || fail "pivot_root did not run"
grep -q 'stub-modprobe: virtio_blk' "$LOG" || fail "modules were not loaded before the root was resolved"
grep -q 'stub-modprobe: foo irq=5' "$LOG" || fail "/etc/modules parameters were not passed to modprobe"
grep -q 'stub-modprobe: blacklisted_mod' "$LOG" || fail "an explicitly listed module was skipped because of a blacklist"
grep -q 'alias blocked_alias -> hidden_mod is blacklisted' "$LOG" || fail "modprobe.d blacklist was not applied to an alias"
grep -q 'stub-modprobe: foo irq=9 timeout=2' "$LOG" || fail "modprobe.d options/alias were not applied"
grep -q 'stub-install: baz' "$LOG" || fail "modprobe.d install command was not run"
grep -q 'stub-fsck: -p /dev/vda' "$LOG" || fail "fsck was not run before mounting the root"
grep -q 'fsck: exited with 0' "$LOG" || fail "fsck result was not checked"
grep -q 'fstab: mounted tmpfs on /newroot/data' "$LOG" || fail "fstab entry was not mounted"
grep -q 'fstab: mounted /dev/vdb on /newroot/boot' "$LOG" || fail "a vfat short UUID was not resolved"
grep -q 'stage2: starting as pid 1' "$LOG" || fail "stage2 did not start as PID 1"
grep -q 'hostname set to jiminit-qemu' "$LOG" || fail "config file was not applied"
grep -q 'started getty on /dev/ttyS0' "$LOG" || fail "getty did not start on the serial console"
grep -q 'status multi-user' "$LOG" || fail "--status did not report the runlevel"
grep -q 'switching runlevel multi-user -> single' "$LOG" || fail "runlevel switch to single did not happen"
grep -q 'now in runlevel single' "$LOG" || fail "init did not enter the single runlevel"
grep -q 'SINGLE-SHELL-OK' "$LOG" || fail "single-user shell did not run commands"
grep -q 'switching runlevel single -> multi-user' "$LOG" || fail "runlevel switch back did not happen"
grep -q 'now in runlevel multi-user' "$LOG" || fail "init did not return to multi-user"
grep -q 'uid=0(root)' "$LOG" || fail "login did not give a root shell"
grep -q 'srw-------' "$LOG" || fail "control socket is not root-only"
grep -q 'jiminit_service_info{name="ticker",state="running"' "$LOG" || fail "--metrics did not report a running service"
grep -q 'jiminit_service_memory_bytes{name="cg"}' "$LOG" || fail "--metrics did not report cgroup memory"
grep -q 'svc/ticker: tick' "$LOG" || fail "ticker service output was not forwarded"
grep -q 'svc/failer: restarting' "$LOG" || fail "failed service was not restarted"
grep -q 'SIGUSR1: soft reboot' "$LOG" || fail "SIGUSR1 did not trigger a soft reboot"
grep -q 'soft reboot #1: exec' "$LOG" || fail "stage 2 was not re-executed"
grep -q 'soft reboot: fresh tmpfs on /run' "$LOG" || fail "/run was not reset"
grep -q 'soft reboot: fresh tmpfs on /tmp' "$LOG" || fail "/tmp was not reset"
[ "$(count 'svc/ticker: started')" -ge 2 ] || fail "services did not restart after a runlevel switch/soft reboot"
grep -q 'svc/cg: 0::/jiminit/cg' "$LOG" || fail "service was not placed in its cgroup"
[ "$(count 'svc/user: 65534')" -ge 2 ] || fail "service did not drop privileges to nobody"
[ "$(count 'svc/limited: 8388608')" -ge 1 ] || fail "memory.max was not applied"
[ "$(count 'svc/limited: 16')" -ge 1 ] || fail "pids.max was not applied"
grep -q 'svc/hog: OOM-killed' "$LOG" || fail "OOM kill was not detected"
grep -q 'svc/notifier: waiting for READY=1' "$LOG" || fail "notify service did not get NOTIFY_SOCKET"
grep -q 'svc/notifier: ready' "$LOG" || fail "notify service never reported READY=1"
grep -q 'svc/watcher: watchdog every 2000 ms' "$LOG" || fail "watchdog interval was not passed to the service"
if grep -q 'svc/watcher: watchdog timeout' "$LOG"; then
    fail "a live watchdog service was killed"
fi
grep -q 'svc/watchdog-dead: watchdog timeout' "$LOG" || fail "watchdog timeout was not detected"
grep -q 'not started: dependency' "$LOG" || fail "a service started although its dependency failed"
if grep -q 'BROKEN-CHILD-RAN' "$LOG"; then
    fail "a dependent service ran after its dependency failed"
fi
grep -q 'stopping because dependency' "$LOG" || fail "stopping a dependency did not cascade"
if grep -q 'CHILD2-RAN' "$LOG"; then
    : # child2 ran before being stopped, which is fine; only the cascade matters
fi
ready_line=$(line_of 'svc/notifier: ready')
after_line=$(line_of 'svc/after: started')
[ -n "$ready_line" ] && [ -n "$after_line" ] && [ "$ready_line" -lt "$after_line" ] ||
    fail "dependent service started before its dependency was ready"
[ "$(count 'login:')" -ge 3 ] || fail "getty did not come back after the soft reboot"
grep -q 'reboot: Power down' "$LOG" || fail "poweroff was not requested"

echo "PASS: modules, fsck, fstab, switch_root, PID 1, getty/login, runlevels, dependencies, watchdog, soft reboot and poweroff all work"
