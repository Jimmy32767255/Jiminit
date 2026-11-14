# 安装到真实系统

先构建静态二进制：

```sh
./scripts/build.sh
```

## 本地安装

```sh
./scripts/install.sh                      # /sbin/jiminit + /etc/jiminit.toml
./scripts/install.sh --prefix /usr        # 二进制装到 /usr/sbin，配置仍在 /etc
./scripts/install.sh --destdir out/stage  # 打包用（DESTDIR），不碰宿主 /etc
```

行为：

- 二进制 → `$PREFIX/sbin/jiminit`（默认 `/sbin`）；
- README → `/usr/share/doc/jiminit/README.md`；
- `/etc/jiminit.toml` 只在不存在时写入，已有配置不会被覆盖；
- 创建 `/etc/jiminit/services.d/`，并放一个 `example.toml.disabled`（去掉后缀即启用）。

安装后编辑 `/etc/jiminit.toml` 和服务目录。用 `systemctl` 之类管理不了它：
jiminit 自身就是 PID 1，控制走 `jiminit --status` 等命令。

## 直接启动（不用 initramfs）

如果根设备驱动已内建、没有 LUKS/LVM、`/usr` 不是独立分区，可以跳过 stage1：
内核命令行写

```text
init=/sbin/jiminit
```

jiminit 会判定自己为 stage2，直接作为 PID 1 运行。配置里的 `init_path` 仍然要指向
真实路径（stage1 不走时它只用于软重启的兜底）。

## Arch/Artix 打包

```sh
makepkg -si
```

PKGBUILD 从当前 checkout 构建，安装内容与 `install.sh` 一致，并附带 mkinitcpio
hook。`pkgver` 目前固定为项目版本，从 git 构建时不会自动跟随提交。

## mkinitcpio 集成

把 hook 放到 mkinitcpio 目录（PKGBUILD 会自动做；手动安装时）：

```sh
install -Dm755 contrib/mkinitcpio/install/jiminit /usr/lib/initcpio/install/jiminit
install -Dm755 contrib/mkinitcpio/hooks/jiminit   /usr/lib/initcpio/hooks/jiminit
```

`/etc/mkinitcpio.conf` 里把 `jiminit` 放到 **最后**，并移除 `fsck`（jiminit 自己跑
`fsck -p`）：

```
HOOKS=(base udev autodetect modconf block filesystems keyboard jiminit)
```

然后：

```sh
mkinitcpio -P
```

hook 会把 `/sbin/jiminit`、`modprobe`、可选的 `fsck.ext4`/`cryptsetup`/`lvm`，
以及宿主上的 `/etc/crypttab`、`/etc/lvm/lvm.conf` 打进 initramfs。内核命令行仍需
写 `root=`/`rootfstype=` 等。

注意：`/etc/crypttab` 里的**所有**条目都会被解锁，目前没有按 `root=` 过滤。

## 交叉构建（aarch64）

代码没有 x86_64 特有假设，但只在 x86_64 上实测过。

```sh
rustup target add aarch64-unknown-linux-gnu
# 需要交叉 linker（如 aarch64-linux-gnu-gcc），并在 .cargo/config.toml 里配置
TARGET=aarch64-unknown-linux-gnu ./scripts/build.sh
ARCH=aarch64 KERNEL=/boot/Image ./scripts/run-qemu.sh
```

静态 glibc 在非 x86_64 上可能需要内核支持 static PIE；遇到问题可改用
`aarch64-unknown-linux-musl`（musl 静态链接更省心，但这条路径尚未验证）。

## 卸载

没有卸载脚本。删除 `/sbin/jiminit`、`/etc/jiminit.toml`、`/etc/jiminit/`，
并把 mkinitcpio 的 `jiminit` hook 从 `HOOKS` 里去掉即可。
