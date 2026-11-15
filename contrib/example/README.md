# 示例：迁移 dinit 服务并安装实验性启动项

这个目录是 `jiminit` 在真实机器上跑起来所需配置的通用模板，**不含任何本机信息**。
本机实际在用的那份配置（主机名、LUKS/文件系统 UUID、用户名、LVM 快照名）只留在
本地，不进入仓库。

## 目录内容

| 路径 | 用途 |
| --- | --- |
| `jiminit.toml` | 桌面机器的示例配置（LUKS/LVM root、tty1-6 getty、graphical 级） |
| `services/*.toml` | 从 dinit 迁移到 jiminit 的服务示例：dbus、udev、elogind、NetworkManager、sddm、sshd、tor、libvirtd、zram 等 |
| `crypttab.example` | stage1 解锁 LUKS 的 crypttab 模板（UUID 为占位符） |
| `41_jiminit.example` | GRUB 子菜单模板，含 `cryptomount` 与每内核一份 initramfs |
| `install-example.sh` | 安装脚本：二进制、配置、服务、mkinitcpio hook 与每内核 initramfs |

## 使用

```sh
CARGO_NET_OFFLINE=true ./scripts/build.sh
sudo ./contrib/example/install-example.sh
```

脚本不会覆盖已有的 `/etc/jiminit.toml`、`/etc/jiminit/crypttab` 和
`/etc/mkinitcpio-jiminit.conf`；`crypttab` 只在不存在时从模板生成，并提示替换
占位 UUID。GRUB 启动项默认不安装，需要时用 `INSTALL_GRUB=1`：

```sh
sudo INSTALL_GRUB=1 ./contrib/example/install-example.sh
```

## 服务示例的注意点

- 多数服务把日志写到 `/var/log/jiminit/<name>.log`，超过 1 MiB 轮转为 `.1`。
- 需要 D-Bus 就绪的服务先用 `dbus-send ... ListNames` 轮询，再 `exec` 真正的进程。
- `ipfs.toml` 里的 `user`、`HOME`、`IPFS_PATH` 是占位值，改成你自己的用户。
- sddm 0.21 没有 `--no-daemon`：前台进程会自己后台化并以 0 退出，所以示例用
  `pgrep -x sddm` 等守护进程退出，再用 `restart = "always"` 重新拉起。
- 用 util-linux 的 agetty 时，`jiminit.toml` 需要 `getty_baud_first = false` 和
  `getty_tty_basename = true`（agetty 自己补 `/dev/`，参数顺序与 busybox 相反）。
- jiminit 不内置 udev：把 `udevd` 当普通服务拉起，再用 oneshot 的 `udev-trigger`
  重放 uevent；`elogind`、`sddm` 都依赖它，否则 Wayland 会话拿不到输入设备 ACL。

## 只验证配置

```sh
jiminit check
```
