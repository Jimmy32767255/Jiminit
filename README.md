# Jiminit

一个个人实验性的 GNU/Linux init：用 Rust 写的、单二进制的 PID 1。

Jiminit 从 initramfs 接管启动（模块、fsck、fstab、LUKS/LVM、`pivot_root`），在真实根上
作为 PID 1 管理 getty/login 会话与服务监督，并在收到信号时有序关机/重启。

## 能做什么

- **两阶段单二进制**：同一个 `/init` 既能跑 initramfs（stage1），也能作为真实根的
  `init=`/`init_path`（stage2）。
- **启动真实系统**：`root=` 支持路径、UUID、PARTUUID；`/etc/modules` + modalias
  coldplug、`fsck -p`、fstab 额外挂载、`pivot_root`（含独立 `/usr`）、crypttab
  LUKS 解锁与按需 LVM 激活。
- **服务监督**：`/etc/jiminit/services.d/*.toml`，依赖拓扑、重启退避、sd_notify
  就绪与看门狗、cgroup v2 资源限制与 OOM 报告、user/group 降权、控制 socket。
- **观测**：`jiminit --status` 查看每个服务的 pid/重启次数/就绪/内存/CPU/pids/io，
  `jiminit --metrics` 输出 Prometheus 文本。
- **会话**：为多个 tty 拉起 getty，认证交给外部 `login`；`single`/`multi-user`/
  `graphical` 运行级可运行时切换；`SIGUSR1` 软重启（exec 自己）。
- **可交付**：静态构建脚本、四种 QEMU 冒烟测试、install.sh、PKGBUILD、
  mkinitcpio hook、aarch64 构建路径。

## 快速开始

在 Arch/Artix 上（需要 Rust、`qemu-system-x86`、静态 `busybox`、`e2fsprogs`、`cpio`、
`gzip` 和 `/boot/vmlinuz-linux`）：

```sh
./scripts/build.sh
./scripts/mk-initramfs.sh
./scripts/mk-rootfs.sh
./scripts/run-qemu.sh
```

看到 `jiminit-qemu login:` 后用测试账号 `root` / `jiminit` 登录。退出 QEMU：
`Ctrl-a` 再 `x`。

自动验证：

```sh
./scripts/smoke-test.sh   # 约 1–2 分钟，PASS/FAIL + out/smoke.log
```

## 文档

完整文档在 [docs/](docs/README.md)，按使用 / 开发 / 参考分层：

| 层 | 入口 |
| --- | --- |
| 使用 | [上手](docs/usage/getting-started.md) · [配置](docs/usage/configuration.md) · [服务](docs/usage/services.md) · [会话与运行级](docs/usage/sessions-and-runlevels.md) · [安装](docs/usage/installation.md) |
| 开发 | [架构](docs/development/architecture.md) · [构建与镜像](docs/development/building.md) · [测试](docs/development/testing.md) |
| 参考 | [配置速查](docs/reference/configuration-reference.md) · [已知限制](docs/reference/limitations.md) · [设计取舍](docs/reference/design-notes.md) |

## 项目状态

- 77 个单元测试；`smoke-test.sh` 覆盖主路径，另有 LUKS/LVM、独立 `/usr`、
  coldplug+真实 fsck 三条冒烟路径各自 PASS。
- 只在 QEMU x86_64 上验证过；aarch64 只验证了构建路径，未实机测试。
- 这是个人实验项目，不追求 systemd 兼容，也不适合直接替换生产环境的 init。
  能力边界见[已知限制](docs/reference/limitations.md)。

## 仓库结构

```
src/        单二进制 init 的全部 Rust 代码
scripts/    构建、镜像、冒烟测试、安装
contrib/    配置模板、mkinitcpio hook
docs/       按使用/开发/参考分层的文档
PKGBUILD    Arch/Artix 打包
```

## 许可

GPL-3.0，见 [LICENSE](LICENSE)。
