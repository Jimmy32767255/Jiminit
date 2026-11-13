# Jiminit

一个个人实验性的 GNU/Linux init 系统实现方案：用 Rust 写的、单二进制的 PID 1。

当前功能：作为 initramfs 的 `/init` 挂载真实根并 `switch_root`，也能直接作为系统根上的
`init=` 使用；挂载伪文件系统、设置 hostname、按依赖启动并看护 `/etc/jiminit/services.d`
里的服务、在 console 上提供 root shell、回收孤儿进程，并在收到信号时有序关机/重启。

## 目录结构

```
src/
  main.rs     入口：判定 stage1/stage2，panic 兜底进入 rescue
  stage1.rs   initramfs 阶段：解析 root=、挂载真实根、switch_root
  stage2.rs   真实根阶段：PID 1 主循环（服务、shell、收尸、信号、关机）
  service.rs  服务监督：TOML 解析、依赖拓扑、重启退避、日志转发、停机
  rootdev.rs  根设备解析：/dev 路径、UUID=、PARTUUID=
  config.rs   /etc/jiminit.toml 配置
  cmdline.rs  /proc/cmdline 解析
  mount.rs    挂载辅助
  log.rs      同时写 stderr 和 /dev/kmsg 的日志
scripts/
  build.sh         静态 release 构建
  mk-initramfs.sh  生成 initramfs（/init + busybox）
  mk-rootfs.sh     生成 ext4 根镜像（jiminit + busybox + 配置 + 示例服务）
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
2. `sethostname`，接管 console（`setsid` + `TIOCSCTTY` + dup2 到 0/1/2）；
3. 阻塞信号并创建 `signalfd`，主循环用 `poll` 等待信号、服务输出或 100ms 超时；
4. 加载 `/etc/jiminit/services.d/*.toml`，按依赖关系启动服务；
5. fork 登录 shell（argv0 带 `-` 前缀），shell 退出时按配置 respawn（否则关机）；
6. 每次唤醒回收所有子进程：服务退出交给监督器处理，其他孤儿只记录；
7. `SIGINT`（ctrl-alt-del）：重启；`SIGTERM`：关机；`SIGUSR1`：软重启；
8. 关机流程：SIGHUP 结束 shell → 按启动逆序给服务发 SIGTERM → 等待
   `shutdown_timeout_ms` → 剩余进程 SIGKILL → `sync` → 根重新挂载为只读 →
   `reboot(RB_POWER_OFF | RB_AUTOBOOT)`；
9. 软重启复用同一段停机流程，但在 `sync` 之后：给 `/run`、`/tmp` 各挂一个新的
   tmpfs（先 `MNT_DETACH` 原有挂载）→ `execve` 自己（`/proc/self/exe --stage2`，
   环境里带 `JIMINIT_SOFT_REBOOTS=n+1`）→ 新 stage2 重新读配置、起服务、起 shell。
   因为不经过内核，PID 仍为 1；exec 失败则进入 rescue shell，而不是让机器卡死。

任何阶段出错或 panic 都不会让 PID 1 退出，而是进入 rescue shell 循环。

## 服务（`/etc/jiminit/services.d`）

每个 `*.toml` 是一个服务，文件名（去掉 `.toml`）就是服务名。所有键都可省略，但
`exec` 必填：

```toml
exec = "/bin/sh"            # 可执行文件；写成带空格的一行时自动拆成 argv
args = ["-c", "echo hi"]    # 显式参数（优先于上面的自动拆分）
depends_on = ["network"]    # 依赖的服务名
restart = "on-failure"      # always | on-failure | never（默认 on-failure）
oneshot = false             # true 表示跑完就结束、不重启
working_dir = "/"           # 工作目录，默认 /
env = { FOO = "bar" }       # 额外环境变量
```

启动语义：

- 按 `depends_on` 建依赖图，无依赖关系的服务并行拉起（启动完成 = fork+exec 成功，
  没有就绪探测协议）；依赖缺失或成环的服务会被跳过并告警；
- 服务在独立 session/进程组中运行，stdin 是 `/dev/null`，stdout/stderr 经管道转发到
  `/dev/kmsg`，每行带 `svc/<name>: ` 前缀；
- 退出后按策略重启：连续失败从 1s 开始指数退避（2s、4s…上限 60s），连续 5 次失败后
  放弃并告警；连续运行满 10s 会重置失败计数；
- 关机时按启动逆序发 SIGTERM，超时后 SIGKILL。

## 配置文件

`/etc/jiminit.toml`，所有键都可省略：

```toml
hostname = "jiminit"
console  = "/dev/console"
shell    = "/bin/sh"
respawn  = true                    # root shell 退出后是否重新拉起
shutdown_timeout_ms = 3000         # 关机时等待 SIGTERM 的时长
init_path = "/sbin/jiminit"        # stage2 可执行文件路径
services_dir = "/etc/jiminit/services.d"
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
- root shell 可交互，`tty` 为 `/dev/console`；shell `exit` 后自动 respawn；
- 服务：`ticker` 每秒输出并被转发到 kmsg（`svc/ticker: tick`）；
  `failer` 退出码 1 后按 1s→2s→4s 退避重启；关机时 `ticker` 收到 SIGTERM 退出；
- `kill -TERM 1` → `reboot: Power down`，QEMU 正常退出（shell 用 SIGHUP 结束，
  不再等满超时）；
- `kill -INT 1` → `reboot: Restarting system`；
- `kill -USR1 1` → `SIGUSR1: soft reboot` → 停机 → `fresh tmpfs on /run`、
  `/tmp` → `soft reboot #1: exec /sbin/jiminit --stage2` → 再次以 PID 1 启动、
  服务与 shell 全部重新拉起（日志里 `svc/ticker: started` 出现两次）；
  之后再 `kill -TERM 1` 正常 `Power down`；
- `root=UUID=<fs-uuid>`、`root=PARTUUID=<gpt-partition-guid>` 均能定位设备
  （GPT 分区测试得到 `/dev/vda1`）。

### 已知限制

- 不加载内核模块，因此只能用内建的文件系统和磁盘驱动（真实 initramfs 需要在这里加模块加载）；
- 不做 fsck，不处理 `/etc/fstab`；
- 没有 getty/login，root shell 是唯一交互入口；没有运行级（runlevel）概念；
- 服务没有就绪探测（没有 sd_notify 之类），依赖只保证启动顺序；
- 服务没有 cgroup 隔离，也不切换用户/组运行；
- 只支持 x86_64 上验证过的路径，其他架构未测试；
- `root=` 只支持 `/dev` 路径、UUID、PARTUUID，不支持 LABEL；
- 软重启只重新执行 stage2：initramfs 已经不在，内核也不会重新引导，所以
  `root=`/initramfs/内核参数相关的改动必须走真正的重启；
- 软重启用 tmpfs 顶替 `/run`、`/tmp`，旧内容若原本在根文件系统上仍会占用空间
  （真重启才会回收）。

## 设计取舍

依赖只引入 `nix` + `serde`/`toml` + `anyhow`，其余（cmdline、UUID/GPT 解析、
switch_root、signalfd 主循环、服务监督）都自己实现，便于后续替换或精简。
PID 1 主循环用 `signalfd` + `poll` 而不是线程，避免在多线程进程里 fork 的风险；
服务日志通过管道回到主循环带前缀写入 kmsg，同样不需要额外线程。

具体决策点（QEMU 验证、TOML 配置、root shell、init 接管关机、UUID/PARTUUID、
服务格式/依赖启动/重启策略/日志去向）都是和需求方逐项确认后定下的。
