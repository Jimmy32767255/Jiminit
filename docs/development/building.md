# 构建与镜像

所有脚本都从仓库根目录运行，产物统一放在 `out/`（已 gitignore）。

## 静态构建

```sh
./scripts/build.sh
```

内部是：

```sh
cargo rustc --release -- -C target-feature=+crt-static
install -m755 target/release/jiminit out/jiminit
```

为什么用 `cargo rustc ... -- -C ...` 而不是 `RUSTFLAGS`：`RUSTFLAGS` 会同时作用于
proc-macro（`serde_derive`），而 proc-macro 不能静态链接，构建会直接失败。
产物是 static-pie ELF，没有 `PT_INTERP`，可以塞进 initramfs 直接跑。

交叉构建：

```sh
TARGET=aarch64-unknown-linux-gnu ./scripts/build.sh
```

脚本把 `--target` 传给 cargo，产物路径相应变化。需要目标工具链与交叉 linker，
配置方法见[安装到真实系统](../usage/installation.md#交叉构建aarch64)。

## 产物一览

| 脚本 | 产物 | 说明 |
| --- | --- | --- |
| `build.sh` | `out/jiminit` | 静态二进制 |
| `mk-initramfs.sh` | `out/initramfs.cpio.gz` | `/init` + busybox（+ 可选组件） |
| `mk-rootfs.sh` | `out/rootfs.ext4` | 64 MiB ext4，jiminit + busybox + 测试配置 |
| `mk-usr-image.sh` | `out/rootfs-usr.ext4`、`out/usr.ext4` | 独立 `/usr` 布局 |
| `mk-luks-image.sh` | `out/luks.img`、`out/crypttab`、`out/luks.pass` | LUKS2 + LVM 根，需要 root |

## initramfs

默认镜像最小：`/init`（jiminit）、静态 busybox 及少量 applet 软链、空目录
`/proc /sys /dev /etc /newroot /tmp`。约 1.6 MB。

可选组件用环境变量打开：

```sh
FSCK=1 ./scripts/mk-initramfs.sh
COLDPLUG=1 ./scripts/mk-initramfs.sh
LUKS=1 CRYPTAB=out/crypttab ./scripts/mk-initramfs.sh
```

- `FSCK=1`：把 `e2fsck` 及其库打进去，并软链成 `/sbin/fsck`、`fsck.ext2/3/4`。
  只带 e2fsprogs，xfs/btrfs 检查工具不打包。
- `COLDPLUG=1`：打包 kmod `modprobe`、`/lib/modules/$(uname -r)` 的索引，以及
  `COLDPLUG_MODULES`（默认常见的 ahci/nvme/usb/virtio 存储驱动和 ext4/xfs/btrfs/vfat）
  及其依赖；`/etc/modules` 写成文件系统模块，存储驱动靠 stage1 扫 modalias 自动加载。
  与 `LUKS=1` 互斥（LUKS 自己打包 dm 模块）。
- `LUKS=1`：打包真实的 `cryptsetup`、`lvm`、`modprobe` 和 `dm_mod`/`dm_crypt`，
  生成精简 `/etc/lvm/lvm.conf`（关 udev 同步），并把 `CRYPTAB` 指定的文件装成
  `/etc/crypttab`。约 11 MB。

测试专用 stub：

```sh
FSCK_STUB=1 ./scripts/mk-initramfs.sh        # 假的 /sbin/fsck
MODPROBE_STUB=1 ./scripts/mk-initramfs.sh    # 假 modprobe + 测试用 /etc/modules、modprobe.d
MODPROBE_STUB_FAIL=1 MODPROBE_STUB=1 ./scripts/mk-initramfs.sh  # 让 stub 失败，验证 rescue
```

## 根文件系统

```sh
./scripts/mk-rootfs.sh
```

生成 64 MiB ext4（`SIZE=` 可改），内容：

- `/sbin/jiminit` 与 `/init` 软链；
- 静态 busybox 及 `sh mount umount cat ls ... poweroff reboot` 等 applet；
- `getty`/`login`/`passwd`/`sulogin`（busybox）；
- `/etc/passwd`、`/etc/group`、`/etc/shadow`（root 密码 `jiminit`）、`/etc/securetty`；
- `/etc/jiminit.toml`（hostname `jiminit-qemu`）与 `/etc/fstab`（`tmpfs /data`）；
- `/etc/jiminit/services.d/` 一组测试服务：`ticker`、`failer`、`cg`、`user`、
  `notifier`、`after`、`limited`、`hog`、`watcher`、`watchdog-dead`、`broken-dep`、
  `broken-child`、`parent`、`child2`。

这些服务是冒烟测试的 fixture，不是推荐配置；真实系统里删掉换成自己的。

## 独立 /usr

```sh
./scripts/mk-usr-image.sh
```

需要先 `build.sh` + `mk-rootfs.sh`。它把 init 放到 `out/usr/sbin/jiminit`，
根镜像不再带 `/sbin/jiminit`，配置改成 `init_path = "/usr/sbin/jiminit"`，
fstab 加一条按 UUID 挂载的 `/usr`（`USR_UUID` 固定为
`22222222-2222-2222-2222-222222222222`），最后生成 `out/rootfs-usr.ext4` 和
`out/usr.ext4`。

## LUKS + LVM 根

先以普通用户构建，再用 root 造盘（root 下 rustup 没有默认 toolchain，cargo 会失败）：

```sh
./scripts/build.sh && ./scripts/mk-rootfs.sh
qt-sudo ./scripts/mk-luks-image.sh
./scripts/smoke-luks-test.sh
```

`mk-luks-image.sh` 创建 256 MiB `out/luks.img`，LUKS2 + pbkdf2（2000 轮，TCG 下解锁
才不至于太慢），卷组 `jiminitvg`、逻辑卷 `root`，格式化 ext4 并写入测试 rootfs，
生成 `out/crypttab`。结束时把 `out/` 的属主交还给调用者。

注意：脚本使用固定名字 `jiminitvg`/`jimcrypt`，启动时如果发现同名卷组会拒绝执行；
不要改回通用名 `vg0`/`cryptroot`——在开发机上曾与宿主真实卷组撞名。

## 运行

```sh
./scripts/run-qemu.sh
```

默认借宿主机内核 `/boot/vmlinuz-linux`、512 MiB 内存、2 CPU、virtio 磁盘、串口
console。额外参数会透传给 QEMU。`ARCH=aarch64 KERNEL=/boot/Image` 可切到 aarch64
（需要 `qemu-system-aarch64` 与对应内核）。
