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
EOF

cat > "$ROOT/etc/group" <<'EOF'
root:x:0:
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
EOF

cat > "$ROOT/etc/fstab" <<'EOF'
# Extra filesystems mounted by stage 1 from the initramfs.
tmpfs /data tmpfs nosuid,nodev,nofail 0 0
EOF

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

rm -f "$IMAGE"
mke2fs -q -F -t ext4 -L JIMROOT -d "$ROOT" "$IMAGE" "$SIZE"
ls -lh "$IMAGE"
