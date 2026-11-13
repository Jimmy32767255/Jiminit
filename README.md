# Jiminit

一个个人实验性的 GNU/Linux init 系统实现方案：用 Rust 写的、单二进制的 PID 1。

当前是第一版「极简骨架」：能作为 initramfs 的 `/init` 挂载真实根并 `switch_root`，
也能直接作为系统根上的 `init=` 使用；负责挂载伪文件系统、设置 hostname、启动
root shell、回收孤儿进程，并在收到信号时有序关机/重启。

## 目录结构

```
src/
  main.rs     入口：判定 stage1/stage2，panic 兜底进入 rescue
  stage1.rs   initramfs 阶段：解析 root=、挂载真实根、switch_root
  stage2.rs   真实根阶段：PID 1 主循环（shell、收尸、信号、关机）
  rootdev.rs  根设备解析：/dev 路径、UUID=、PARTUUID=
  config.rs   /etc/jiminit.toml 配置
  cmdline.rs  /proc/cmdline 解析
  mount.rs    挂载辅助
  log.rs      同时写 stderr 和 /dev/kmsg 的日志
scripts/
  build.sh         静态 release 构建
  mk-initramfs.sh  生成 initramfs（/init + busybox）
  mk-rootfs.sh     生成 ext4 根镜像（jiminit + busybox + 配置）
  run-qemu.sh      交互式启动测试
  smoke-test.sh    自动冒烟测试
```

## 工作方式

二进制通过两种方式判定自己的阶段：

1. 显式参数 `--stage1` / `--stage2`，或环境变量 `JIMINIT_STAGE=1|2`；
2. 否则自动判断：`/` 是 ramfs/tmpfs 且内核命令行有 `root=` 时为 stage1，
   否则为 stage2。

### stage1（initramfs）

1. 挂载 `devtmpfs` 到 `/dev`、`proc`、`sysfs`；
2. 解析内核命令行：
   - `root=`：`/dev/xxx`、`UUID=...`、`PARTUUID=...`（不支持 `LABEL=`）；
   - `rootfstype=`（可省略，省略时先让内核自动探测，失败再依次试 ext4/xfs/btrfs/vfat）；
   - `rootflags=` 透传给 mount；
   - `rw`：默认只读挂载，出现 `rw` 才读写；
3. 设备节点可能还没就绪，所以按 20 次 × 250ms 重试；
4. 挂载到 `/newroot`，校验 `/newroot/sbin/jiminit` 存在；
5. 把 `/dev`、`/proc`、`/sys` 的挂载点 MS_MOVE 进新根；
6. `chdir("/newroot")` → `mount(".", "/", MS_MOVE)` → `chroot(".")` → `chdir("/")`
   （与 busybox switch_root 相同的收尾，缺 `chroot(".")` 会导致后续 exec 仍看旧根）；
7. `execve("/sbin/jiminit", ["--stage2"])`。

### stage2（PID 1）

1. 挂载伪文件系统，读取 `/etc/jiminit.toml`（缺失则全部用默认值）；
2. `sethostname`；
3. `setsid` + 打开 `/dev/console` + `dup2` 到 0/1/2 + `TIOCSCTTY`；
4. 阻塞 `SIGCHLD/SIGINT/SIGTERM/SIGQUIT/SIGHUP`，用 `sigwaitinfo` 做主循环；
5. fork 子进程：`setsid` + 控制终端 + `execve` 登录 shell（argv0 带 `-` 前缀）；
6. `SIGCHLD`：回收所有子进程；shell 退出时按配置 respawn（否则关机）；
7. `SIGINT`（ctrl-alt-del）：重启；`SIGTERM`：关机；
8. 关机流程：`kill(-1, SIGTERM)` → 等待 `shutdown_timeout_ms` → `kill(-1, SIGKILL)`
   → `sync` → 根重新挂载为只读 → `reboot(RB_POWER_OFF | RB_AUTOBOOT)`。

任何阶段出错或 panic 都不会让 PID 1 退出，而是进入 rescue shell 循环。

## 配置文件

`/etc/jiminit.toml`，所有键都可省略：

```toml
hostname = "jiminit"
console  = "/dev/console"
shell    = "/bin/sh"
respawn  = true             # root shell 退出后是否重新拉起
shutdown_timeout_ms = 3000  # 关机时等待 SIGTERM 的时长
init_path = "/sbin/jiminit" # stage2 可执行文件路径
```

## 构建与测试

依赖：Rust、`qemu-system-x86`、`busybox`（静态链接）、`e2fsprogs`、`cpio`、`gzip`，
以及宿主内核 `/boot/vmlinuz-linux`（测试直接借宿主机内核）。

```sh
./scripts/build.sh           # 生成静态链接的 out/jiminit
./scripts/mk-initramfs.sh    # 生成 out/initramfs.cpio.gz
./scripts/mk-rootfs.sh       # 生成 out/rootfs.ext4
./scripts/run-qemu.sh        # 交互式启动（ctrl-a x 退出）
./scripts/smoke-test.sh      # 自动验证并输出 PASS/FAIL
```

QEMU 磁盘用 virtio（`/dev/vda`），因为宿主内核里 `ata_piix` 是模块而 virtio-blk
是内建。启动参数：

```
console=ttyS0 rdinit=/init root=/dev/vda rootfstype=ext4 rw
```

也可以换成 `root=UUID=...` 或 `root=PARTUUID=...`。

### 已验证

- initramfs → 挂载 `/dev/vda` → MS_MOVE 切换根 → stage2 以 PID 1 运行；
- 读取 `/etc/jiminit.toml`（hostname 生效）、`/proc/1/comm` 为 `jiminit`；
- root shell 可交互，`tty` 为 `/dev/console`；
- shell `exit` 后自动 respawn；
- `kill -TERM 1` → `reboot: Power down`，QEMU 正常退出；
- `kill -INT 1` → `reboot: Restarting system`；
- `root=UUID=<fs-uuid>`、`root=PARTUUID=<gpt-partition-guid>` 均能定位设备
  （GPT 分区测试得到 `/dev/vda1`）。

### 已知限制

- 不加载内核模块，因此只能用内建的文件系统和磁盘驱动（initramfs 真实项目需要在这里加模块加载）；
- 不做 fsck，不处理 `/etc/fstab`；
- 没有 getty/login、没有服务管理（systemd/dinit 那类），root shell 是唯一入口；
- 关机时交互式 busybox sh 会忽略 SIGTERM（非交互式正常退出），所以要等满超时后
  由 SIGKILL 收尾，这是预期行为；
- 只支持 x86_64 上验证过的路径，其他架构未测试；
- `root=` 只支持 `/dev` 路径、UUID、PARTUUID，不支持 LABEL。

## 设计取舍

第一版按「能启动、能切换根、能关机」的最小骨架来做，依赖只引入
`nix` + `serde`/`toml` + `anyhow`，其余（cmdline、UUID/GPT 解析、switch_root、信号循环）
都自己实现，便于后续替换或精简。具体决策点（QEMU 验证、TOML 配置、直接 root shell、
init 接管关机、支持 UUID/PARTUUID 等）都是和需求方逐项确认后定下的。
