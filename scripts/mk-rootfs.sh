#!/bin/sh
# Build a small ext4 root filesystem containing jiminit + busybox and the
# config file, so the initramfs stage has a real root to switch_root into.
set -eu
cd "$(dirname "$0")/.."

BUSYBOX=${BUSYBOX:-/usr/bin/busybox}
OUT=out
ROOT=$OUT/rootfs
IMAGE=$OUT/rootfs.ext4
SIZE=${SIZE:-64M}

if [ ! -x "$OUT/jiminit" ]; then
    echo "out/jiminit missing; run scripts/build.sh first" >&2
    exit 1
fi
if [ ! -x "$BUSYBOX" ]; then
    echo "no busybox at $BUSYBOX (set BUSYBOX=...)" >&2
    exit 1
fi

rm -rf "$ROOT"
mkdir -p "$ROOT/bin" "$ROOT/sbin" "$ROOT/etc" "$ROOT/root" "$ROOT/proc" \
    "$ROOT/sys" "$ROOT/dev" "$ROOT/tmp" "$ROOT/run" "$ROOT/data" \
    "$ROOT/var/run" "$ROOT/var/log" "$ROOT/etc/jiminit/services.d"
install -m755 "$OUT/jiminit" "$ROOT/sbin/jiminit"
ln -sf sbin/jiminit "$ROOT/init"
install -m755 "$BUSYBOX" "$ROOT/bin/busybox"

for applet in sh mount umount cat ls echo sleep dmesg kill ps free uname \
    hostname date tail head grep wc mkdir rm mv ln sync poweroff reboot halt \
    ip ifconfig route clear vi less more stty id whoami;
do
    ln -sf busybox "$ROOT/bin/$applet"
done
# getty/login keep the authentication outside jiminit itself.
for applet in login passwd sulogin; do
    ln -sf busybox "$ROOT/bin/$applet"
done
ln -sf ../bin/busybox "$ROOT/sbin/getty"
: > "$ROOT/var/run/utmp"

cat > "$ROOT/etc/passwd" <<'EOF'
root:x:0:0:root:/root:/bin/sh
nobody:x:65534:65534:nobody:/nonexistent:/sbin/nologin
EOF

cat > "$ROOT/etc/group" <<'EOF'
root:x:0:
nogroup:x:65534:
EOF

# Password for root is "jiminit" (SHA-512, salt "jiminit").
cat > "$ROOT/etc/shadow" <<'EOF'
root:$6$jiminit$j3JzVbIoJD6GphNRFtqvFj6YmQNmJrc9zO2bt65rgUMHc.dh9c72FY5W2.39vtNz8L3Weey9nYgw1jlPC8FLu0:19000:0:99999:7:::
EOF
chmod 600 "$ROOT/etc/shadow"

cat > "$ROOT/etc/securetty" <<'EOF'
console
tty1
tty2
tty3
tty4
tty5
tty6
ttyS0
EOF

cat > "$ROOT/etc/jiminit.toml" <<'EOF'
# jiminit configuration (all keys optional)
hostname = "jiminit-qemu"
console = "/dev/console"
shell = "/bin/sh"
respawn = true
shutdown_timeout_ms = 3000
init_path = "/sbin/jiminit"
default_runlevel = "multi-user"
getty = "/sbin/getty"
getty_baud = "38400"
serial_getty_baud = "115200"
ttys = ["/dev/tty1", "/dev/tty2", "/dev/tty3", "/dev/tty4", "/dev/tty5", "/dev/tty6"]
serial_ttys = ["/dev/ttyS0"]
cgroup = true
ready_timeout_ms = 3000
EOF

cat > "$ROOT/etc/fstab" <<'EOF'
# Extra filesystems mounted by stage 1 from the initramfs.
tmpfs /data tmpfs nosuid,nodev,nofail 0 0
EOF

# Optional ESP-style vfat disk, to exercise short (XXXX-XXXX) filesystem UUIDs.
if [ "${ESP:-0}" = "1" ]; then
    mkdir -p "$ROOT/boot"
    truncate -s 16M "$OUT/esp.img"
    # The volume id is what blkid reports as UUID 1234-5678.
    mkfs.vfat -F 32 -i 0x12345678 -n ESP "$OUT/esp.img" >/dev/null 2>&1
    echo 'UUID=1234-5678 /boot vfat ro,nofail 0 0' >> "$ROOT/etc/fstab"
fi

cat > "$ROOT/etc/jiminit/services.d/ticker.toml" <<'EOF'
# Example service: prints one line per second. Exercised by the smoke test.
exec = "/bin/sh"
args = ["-c", "while true; do echo tick; sleep 1; done"]
restart = "always"
EOF

cat > "$ROOT/etc/jiminit/services.d/failer.toml" <<'EOF'
# Example service: always fails, to exercise the backoff restart logic.
exec = "/bin/sh"
args = ["-c", "echo failing; exit 1"]
restart = "on-failure"
EOF

cat > "$ROOT/etc/jiminit/services.d/cg.toml" <<'EOF'
# Example: reports the cgroup v2 path it was placed in.
exec = "/bin/sh"
args = ["-c", "cat /proc/self/cgroup; sleep 600"]
restart = "never"
EOF

cat > "$ROOT/etc/jiminit/services.d/limited.toml" <<'EOF'
# Example: reports the limits written into its cgroup.
exec = "/bin/sh"
args = ["-c", "cat /sys/fs/cgroup/jiminit/limited/memory.max; cat /sys/fs/cgroup/jiminit/limited/pids.max; sleep 600"]
restart = "never"

[limits]
memory_max = "8M"
pids_max = 16
EOF

cat > "$ROOT/etc/jiminit/services.d/hog.toml" <<'EOF'
# Example: allocates more than memory_max and gets OOM-killed.
exec = "/bin/sh"
args = ["-c", "x=$(head -c 20000000 /dev/zero | tr '\\000' x); echo allocated; sleep 600"]
restart = "never"

[limits]
memory_max = "4M"
EOF

cat > "$ROOT/etc/jiminit/services.d/user.toml" <<'EOF'
# Example: drops privileges to nobody.
exec = "/bin/sh"
args = ["-c", "id -u; id -g; sleep 600"]
user = "nobody"
restart = "never"
EOF

cat > "$ROOT/etc/jiminit/services.d/notifier.toml" <<'EOF'
# Example: announces readiness through sd_notify after one second.
exec = "/bin/sh"
args = ["-c", "sleep 1; /sbin/jiminit --notify-send \"$NOTIFY_SOCKET\" READY=1; sleep 600"]
notify = true
restart = "never"
EOF

cat > "$ROOT/etc/jiminit/services.d/after.toml" <<'EOF'
# Example: depends on notifier and must only start once it is ready.
exec = "/bin/sh"
args = ["-c", "echo AFTER-STARTED; sleep 600"]
depends_on = ["notifier"]
restart = "never"
EOF

cat > "$ROOT/etc/jiminit/services.d/watcher.toml" <<'EOF'
# Example: keeps a 2s watchdog alive by pinging every second.
exec = "/bin/sh"
args = ["-c", "while true; do /sbin/jiminit --notify-send \"$NOTIFY_SOCKET\" WATCHDOG=1; sleep 1; done"]
watchdog = "2s"
restart = "never"
EOF

cat > "$ROOT/etc/jiminit/services.d/watchdog-dead.toml" <<'EOF'
# Example: never pings, so it must be killed by the watchdog.
exec = "/bin/sh"
args = ["-c", "sleep 600"]
watchdog = "1s"
restart = "never"
EOF

cat > "$ROOT/etc/jiminit/services.d/broken-dep.toml" <<'EOF'
# Example: a oneshot dependency that fails, so its dependent never starts.
exec = "/bin/sh"
args = ["-c", "echo broken dependency; exit 1"]
oneshot = true
restart = "never"
EOF

cat > "$ROOT/etc/jiminit/services.d/broken-child.toml" <<'EOF'
# Example: must never run because broken-dep fails.
exec = "/bin/sh"
args = ["-c", "echo BROKEN-CHILD-RAN; sleep 600"]
depends_on = ["broken-dep"]
restart = "never"
EOF

cat > "$ROOT/etc/jiminit/services.d/parent.toml" <<'EOF'
# Example: child2 depends on this; stopping it must cascade.
exec = "/bin/sh"
args = ["-c", "sleep 600"]
restart = "never"
EOF

cat > "$ROOT/etc/jiminit/services.d/child2.toml" <<'EOF'
# Example: stopped when its dependency parent is stopped.
exec = "/bin/sh"
args = ["-c", "echo CHILD2-RAN; sleep 600"]
depends_on = ["parent"]
restart = "never"
EOF

rm -f "$IMAGE"
mke2fs -q -F -t ext4 -L JIMROOT -d "$ROOT" "$IMAGE" "$SIZE"
ls -lh "$IMAGE"
