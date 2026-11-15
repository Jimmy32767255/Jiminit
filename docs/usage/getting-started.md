# 上手

这篇只做一件事：在本机把 jiminit 跑进 QEMU，登录进去看看它作为 PID 1 的样子。

## 依赖

在 Arch/Artix 上：

```sh
pacman -S --needed rustup qemu-system-x86 busybox e2fsprogs cpio gzip
```

另外需要宿主内核镜像 `/boot/vmlinuz-linux`（测试脚本直接借宿主机内核启动）。
`busybox` 必须是静态链接的版本（`file /usr/bin/busybox` 应显示 `statically linked`）。

## 构建镜像

```sh
./scripts/build.sh          # 静态链接的 out/jiminit
./scripts/mk-initramfs.sh   # out/initramfs.cpio.gz（/init + busybox）
./scripts/mk-rootfs.sh      # out/rootfs.ext4（jiminit + busybox + 测试配置）
```

三个脚本都是幂等的，改了代码后重跑即可。注意：**改了 stage1 要重跑 mk-initramfs，
改了 stage2 要重跑 mk-rootfs**；只跑其中一个会看到旧二进制，曾经因此排查了很久。

## 启动

```sh
./scripts/run-qemu.sh
```

等价于：

```sh
qemu-system-x86_64 -m 512M -smp 2 \
  -kernel /boot/vmlinuz-linux \
  -initrd out/initramfs.cpio.gz \
  -drive file=out/rootfs.ext4,format=raw,if=virtio \
  -append "console=ttyS0 rdinit=/init root=/dev/vda rootfstype=ext4 rw" \
  -nographic -no-reboot
```

磁盘用 virtio 而不是 IDE，是因为测试宿主内核里 `virtio_blk` 内建、`ata_piix` 是模块。

串口上依次会看到 stage1 解析根设备、`pivot_root: switched to the new root`、
`stage2: starting as pid 1`，最后出现登录提示：

```text
jiminit-qemu login:
```

用测试镜像里的账号登录（仅测试镜像）：

```text
用户名：root
密码：  jiminit
```

退出 QEMU：`Ctrl-a` 然后 `x`。

## 进去之后

```sh
cat /proc/1/comm            # jiminit
tty                         # /dev/ttyS0
hostname                    # jiminit-qemu

jiminit status            # 运行级 + 每个服务一行状态
jiminit runlevel single   # 切单用户；当前登录会话会被结束
jiminit runlevel multi-user
jiminit poweroff            # 有序关机，QEMU 退出
jiminit reboot              # 重启
# 信号仍然是兜底：kill -TERM 1 关机，kill -INT 1 重启
```

`single` 级没有 getty，直接在 console 上给你一个 root shell；切回 `multi-user`
会重新拉起 getty。

## 自动化冒烟

```sh
./scripts/smoke-test.sh
```

它会重建镜像、后台启动 QEMU、自动登录并依次验证 switch_root、getty/login、
运行级切换、服务、软重启和关机，最后打印 `PASS: ...` 或 `FAIL: ...`。
完整输出在 `out/smoke.log`。没有 KVM 时（本机就是）用 TCG，一轮大约 1–2 分钟。

其他镜像路径各有自己的冒烟脚本，见[测试](../development/testing.md)。

## 下一步

- 想配置 hostname、tty、关机超时：[配置](configuration.md)
- 想加自己的守护进程：[服务](services.md)
- 想装到真实机器：[安装到真实系统](installation.md)
