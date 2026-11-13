# Jiminit

一个个人实验性的 GNU/Linux init 系统实现方案：用 Rust 写的、单二进制的 PID 1。

当前功能：作为 initramfs 的 `/init` 加载 `/etc/modules` 列出的内核模块、检查
（`fsck -p`）、挂载真实根、按 `/etc/fstab` 挂载额外文件系统并 `switch_root`，也能
直接作为系统根上的 `init=` 使用；挂载伪文件系统、设置 hostname、按依赖启动并看护 `/etc/jiminit/services.d`
里的服务、在多个 tty 上拉起 getty 交由外部 login 认证、支持运行级切换、回收孤儿进程，
并在收到信号时有序关机/重启。

## 目录结构

```
src/
  main.rs     入口：判定 stage1/stage2，panic 兜底进入 rescue
  stage1.rs   initramfs 阶段：解析 root=、挂载真实根、switch_root
  stage2.rs   真实根阶段：PID 1 主循环（服务、getty/login、收尸、信号、关机）
  runlevel.rs single / multi-user / graphical 运行级与数字别名
  service.rs  服务监督：TOML 解析、依赖拓扑、重启退避、日志转发、就绪探测、停机
  accounts.rs /etc/passwd、/etc/group 解析（服务降权，无 NSS）
  cgroup.rs   为每个服务建 cgroup v2 子组
  rootdev.rs  根设备解析：/dev 路径、UUID=、PARTUUID=
  modules.rs  /etc/modules + modprobe.d 规则，加载根设备所需的内核模块
  coldplug.rs 扫描 /sys 的 modalias，用 modprobe 自动加载已发现设备的驱动
  fstab.rs    /etc/fstab 解析（switch_root 前挂载额外文件系统）
  crypttab.rs /etc/crypttab 解析
  luks.rs     cryptsetup 解锁（重试 3 次）
  config.rs   /etc/jiminit.toml 配置
  cmdline.rs  /proc/cmdline 解析
  mount.rs    挂载辅助
  log.rs      同时写 stderr、/dev/kmsg 和可选日志文件（1 MiB 滚动）
scripts/
  build.sh         静态 release 构建（TARGET= 可交叉构建）
  install.sh       安装到 /sbin、/etc（支持 --prefix/--destdir）
  mk-initramfs.sh  生成 initramfs（/init + busybox）
  mk-rootfs.sh     生成 ext4 根镜像（jiminit + busybox + 配置 + 示例服务）
  run-qemu.sh      交互式启动测试
  smoke-test.sh    自动冒烟测试
  mk-usr-image.sh  /usr 独立分区的镜像
  smoke-usr-test.sh /usr 独立分区 + pivot_root 测试
  smoke-real-test.sh coldplug + 真实 e2fsck 测试
contrib/
  jiminit.toml         配置模板（install.sh 用它初始化 /etc/jiminit.toml）
  mkinitcpio/install/  mkinitcpio 构建脚本
  mkinitcpio/hooks/    mkinitcpio 运行时 hook（exec /sbin/jiminit --stage1）
PKGBUILD               Arch/Artix 包（从当前 checkout 构建）
  mk-luks-image.sh LUKS + LVM 根镜像（需要 root）
  smoke-luks-test.sh LUKS/LVM 自动冒烟测试
```

## 工作方式

二进制通过两种方式判定自己的阶段：

1. 显式参数 `--stage1` / `--stage2`，或环境变量 `JIMINIT_STAGE=1|2`；
2. 否则自动判断：`/` 是 ramfs/tmpfs 且内核命令行有 `root=` 时为 stage1，
   否则为 stage2。

### stage1（initramfs）

1. 挂载 `devtmpfs` 到 `/dev`、`proc`、`sysfs`；
2. 按 initramfs 里的 `/etc/modules` 逐行 `modprobe`（支持 `模块名 参数...`），并应用
   `/etc/modprobe.d/*.conf` 的 `options`/`blacklist`/`install`/`alias` 规则；找不到
   modprobe 或任一模块加载失败都会中止启动、进入 rescue。`blacklist` 与 modprobe(8)
   一致，只拦截 alias 解析——显式写在 `/etc/modules` 里的模块仍会加载（否则被
   blacklist 的存储驱动会让根设备直接不可达），单条 modprobe/install 命令超过 30s
   会被杀掉并判定失败；随后扫描 `/sys/bus/*/devices/*/modalias`，去重后逐个
   `modprobe -q <modalias>`，让内核已发现设备的驱动自动加载；这一步是尽力而为，
   modprobe 不存在或某个 alias 没有对应模块都只记日志、不阻塞启动；
3. 按 initramfs 里的 `/etc/crypttab` 逐个解锁 LUKS 容器（`name device keyfile
   options`，`keyfile` 为 `none`/`-` 时交互输入口令）。设备名支持 `/dev/*`、`UUID=`、
   `PARTUUID=`；每个容器最多尝试 3 次，仍失败则中止启动进入 rescue。`options` 只识别
   `discard`、`readonly`、`keyslot=N`、`header=PATH`（未知项告警忽略）；
4. 需要时激活 LVM：仅当命令行有 `lvm=1`/`lvm` 或 `root=/dev/mapper/...` 时才执行
   `lvm vgchange -ay`（先确认该 mapper 不存在），失败则进入 rescue；
5. 解析内核命令行：
   - `root=`：`/dev/xxx`、`UUID=...`、`PARTUUID=...`（不支持 `LABEL=`）；
   - `rootfstype=`（可省略，省略时先让内核自动探测，失败再依次试 ext4/xfs/btrfs/vfat）；
   - `rootflags=` 透传给 mount；
   - `rw`：默认只读挂载，出现 `rw` 才读写；
6. 设备节点可能还没就绪，所以按 20 次 × 250ms 重试；
7. 挂载前对根设备执行 `fsck -p`：initramfs 里找得到 `/sbin/fsck`（或 `/bin`、
   `/usr/sbin`、`/usr/bin`）才执行，`fsck=no` 可在内核命令行关闭；退出码 4/8/16/32
   等视为失败，拒绝挂载并进入 rescue（0/1/2 视为可用）；
8. 挂载到 `/newroot`，读取 `/newroot/etc/fstab`，把根以外的条目（跳过 swap、伪文件
   系统、`noauto` 和重复挂载点）挂到 `/newroot/<mountpoint>`；挂载点先规范化（折叠
   重复 `/` 和 `.`、解析 `..`、拒绝越过根），`/data/` 与 `/data` 视为同一条目；`nofail`
   的失败只告警，其余失败中止启动。`bind`/`rbind` 转成对应挂载标志，`_netdev`、`x-*`、`comment=`、
   `users` 等只属于 mount(8) 的选项会被丢弃而不是传给内核；其余选项作为文件系统
   参数透传；
9. 从 `/newroot/etc/jiminit.toml` 读 `init_path`（stage1 以真实根的配置为准），并校验
   该可执行文件存在；
10. 把 `/dev`、`/proc`、`/sys` 的挂载点 MS_MOVE 进新根；移动失败时若目标已经是挂载点
   则记 info，否则中止启动；
11. `chdir("/newroot")` → `pivot_root(".", ".")` → `umount2(".", MNT_DETACH)` →
   `chdir("/")`：新根的整棵挂载树（包括 fstab 挂进来的 `/usr` 等）成为 `/`，旧
   initramfs 根被摘掉。内核拒绝 pivot_root（例如挂载传播是 shared）时回退到
   `MS_MOVE` + `chroot(".")` 的老路径；
12. `execve("/sbin/jiminit", ["--stage2"])`（`init_path` 来自真实根的配置，可以指向
   `/usr/sbin/jiminit`；指向 `/usr/...` 但 `/usr` 没挂上时会给出明确错误）。

### stage2（PID 1）

1. 挂载伪文件系统，读取 `/etc/jiminit.toml`（缺失则全部用默认值）；
2. `sethostname`，把 console dup2 到 0/1/2；PID 1 自己不 `TIOCSCTTY`，控制终端留给
   各登录会话（getty/login/shell 各自 `setsid` + `TIOCSCTTY`）；
3. 阻塞信号并创建 `signalfd`，主循环用 `poll` 等待信号、服务输出、控制 socket 或
   100ms 超时；
4. 按 `default_runlevel` 选出属于该级的服务，加载 `/etc/jiminit/services.d/*.toml`
   并按依赖关系启动；
5. 非 single 级：为 `ttys` / `serial_ttys` 里每个存在的终端 fork 一个 getty，认证交给
   外部 `login`；getty 退出后 500ms 重新拉起。single 级：直接在 console 上 fork 一个
   root shell（argv0 带 `-` 前缀），退出后按 `respawn` 决定是否重新拉起；
6. 每次唤醒回收所有子进程：服务退出交给监督器，getty 退出触发重拉，其他孤儿只记录；
7. 监听 `/run/jiminit.sock`（权限 `0600`，仅 root 可控制），接受 `status` 与
   `runlevel <name>` 请求（见下文）；
8. `SIGINT`（ctrl-alt-del）：重启；`SIGTERM`：关机；`SIGUSR1`：软重启；
9. 关机流程：SIGHUP 结束所有 getty/单用户 shell → 按启动逆序给服务发 SIGTERM →
   向所有剩余进程（包括 login 的子进程等非直接子进程）发 SIGTERM → 等待
   `shutdown_timeout_ms`，直到没有用户进程还能写文件系统 → 仍存活则 SIGKILL 并再等
   2s → `sync` → 根重新挂载为只读 → `reboot(RB_POWER_OFF | RB_AUTOBOOT)`；
10. 软重启复用同一段停机流程，但在 `sync` 之后：给 `/run`、`/tmp` 各挂一个新的
    tmpfs（先 `MNT_DETACH` 原有挂载）→ `execve` 自己（`/proc/self/exe --stage2`，
    环境里带 `JIMINIT_SOFT_REBOOTS=n+1`）→ 新 stage2 重新读配置、起服务、起 getty。
    因为不经过内核，PID 仍为 1；exec 失败则进入 rescue shell，而不是让机器卡死。

任何阶段出错或 panic 都不会让 PID 1 退出，而是进入 rescue shell 循环。

## 登录与运行级

jiminit 只负责拉起 getty 和处理运行级，用户名/密码校验完全交给外部 `login`
（测试镜像是 busybox 的 `getty`/`login`）。默认在 `/dev/tty1`–`/dev/tty6` 和
`/dev/ttyS0`（存在时）各起一个 getty，波特率分别由 `getty_baud`、
`serial_getty_baud` 指定。

三个运行级：

| 名称 | 数字别名 | 内容 |
| --- | --- | --- |
| `single` | 1 | 只有显式标了 `runlevels = ["single"]` 的服务及其依赖；console 上是 root shell，不起 getty |
| `multi-user` | 3 | 默认级；未标 `runlevels` 的服务都算在内；getty 提供登录 |
| `graphical` | 5 | 同 multi-user，另加标了 `graphical` 的服务；jiminit 本身不启动图形界面 |

服务可用 `runlevels = ["single"]` 或 `["multi-user", "graphical"]` 标注归属；
未标注等价于 `["multi-user", "graphical"]`。切换级别时，不属于新级别的服务会先
停掉（SIGTERM，3s 后 SIGKILL），属于新级别但尚未运行的再启动；登录会话同样先结束，
避免新旧 shell 争抢 console 的控制终端。

运行中的 init 可通过控制 socket 查询和切换（`/run/jiminit.sock`）：

```sh
jiminit --status                # 运行级 + 每个服务一行状态
jiminit --runlevel single       # 切到单用户，原登录会话会被结束
jiminit --runlevel 3            # 数字别名
jiminit --runlevel reboot       # 6 / reboot 触发重启；0 / poweroff / halt 触发关机
jiminit --service network restart   # start | stop | restart | reload（reload 发 SIGHUP）
```

`--status` 的首行仍是 `status <level>`，之后每个服务一行：

```
status multi-user
cg running pid=118 restarts=0 ready=yes memory=1.2M
after stopped pid=- restarts=1 ready=- memory=-
```

字段依次是：服务名、状态（`running`/`stopped`，未运行或已退出都算 stopped）、pid、
自 stage2 启动以来的启动次数减一（软重启会重置）、是否就绪（`notify` 服务在收到
`READY=1` 前为 no，非 notify 服务启动即 yes）、`memory.current`（未启用 memory
控制器时为 `-`）。

## 服务（`/etc/jiminit/services.d`）

每个 `*.toml` 是一个服务，文件名（去掉 `.toml`）就是服务名。所有键都可省略，但
`exec` 必填：

```toml
exec = "/bin/sh"            # 可执行文件；写成带空格的一行时自动拆成 argv
                            # （支持单/双引号与反斜杠转义，但不会展开变量）
args = ["-c", "echo hi"]    # 显式参数（优先于上面的自动拆分）
depends_on = ["network"]    # 依赖的服务名
restart = "on-failure"      # always | on-failure | never（默认 on-failure）
oneshot = false             # true 表示跑完就结束、不重启
working_dir = "/"           # 工作目录，默认 /
env = { FOO = "bar" }       # 额外环境变量
runlevels = ["multi-user"]  # 默认 ["multi-user", "graphical"]
user = "nobody"             # 以该用户运行（名字或数字 uid），默认 root
group = "nogroup"           # 覆盖主组（名字或数字 gid）
supplementary_groups = []   # 附加组；留空则用 /etc/group 里该用户的组
notify = false              # true 时等待服务在 NOTIFY_SOCKET 上发 READY=1
watchdog = "30s"            # 可选：要求服务每 30s 在 NOTIFY_SOCKET 发 WATCHDOG=1

[limits]                    # 可选：cgroup v2 资源限制，缺省不限制
memory_max = "64M"          # 字节或可选 K/M/G/T 后缀，或 "max"
pids_max = 64               # 最多任务数
cpu_max = "50000 100000"    # <quota> <period>，单位微秒
```

启动语义：

- 按 `depends_on` 建依赖图，无依赖关系的服务并行拉起；依赖缺失或成环的服务会被
  跳过并告警；
- 依赖项启动失败或最终退出（restart 策略决定不再拉起、连续失败放弃、或被手动
  stop）时，尚未启动的依赖方会被标记失败并记 `not started: dependency ...`；
  已经在运行的依赖方收到 SIGTERM 级联停止，依赖恢复后不会自动把它们再拉起来；
- 默认 fork+exec 成功即视为就绪。`notify = true` 的服务会从环境里拿到
  `NOTIFY_SOCKET`（`/run/jiminit/notify/<name>.sock`，权限 0666），发出 `READY=1`
  后其依赖才会启动；超过 `ready_timeout_ms` 仍没收到则告警并按就绪继续，避免死等；
  `STATUS=...` 会转发到日志。`jiminit --notify-send "$NOTIFY_SOCKET" READY=1`
  可用于脚本；
- `watchdog = "30s"` 时服务还必须周期性发 `WATCHDOG=1`（环境里同时提供
  `WATCHDOG_USEC`）。超过一个周期没收到就记 `watchdog timeout` 并 SIGKILL，随后
  照常交给 restart 策略：`never` 会停下来并标记失败，其他策略会退避重启；
- 服务在独立 session/进程组中运行，stdin 是 `/dev/null`，stdout/stderr 经管道转发到
  `/dev/kmsg`，每行带 `svc/<name>: ` 前缀；
- 默认每个服务被放进 `/sys/fs/cgroup/jiminit/<name>`（cgroup v2 统一层级），关机超时
  时用 `cgroup.kill` 连逃出进程组的孙进程一起杀；
- `user`/`group` 只用 `/etc/passwd`、`/etc/group` 解析（不查 NSS），先
  `setgroups` 再 `setgid` 再 `setuid`，HOME/USER/LOGNAME 同步设置；
- `[limits]` 通过 cgroup v2 施加 `memory_max`/`pids_max`/`cpu_max`；只有真有服务
  设置某一项时才启用对应控制器（`+memory`/`+pids`/`+cpu`），写入失败只告警、
  不会带崩服务；`memory_max` 触发的 OOM 从 `memory.events` 读出并记
  `svc/<name>: OOM-killed (N total)`，随后照常交给 restart 策略；
- 控制 socket 支持 `service <name> start|stop|restart|reload`；手动 stop 不会被重启
  策略覆盖，restart 等旧进程退出后再拉起；
- 退出后按策略重启：连续失败从 1s 开始指数退避（2s、4s…上限 60s），连续 5 次失败后
  放弃并告警；连续运行满 10s 会重置失败计数；
- 只有当前运行级（以及依赖它的服务所在级）的成员才会被启动，见上一节；
- 关机时按启动逆序发 SIGTERM，超时后 SIGKILL。

## 配置文件

`/etc/jiminit.toml`，所有键都可省略：

```toml
hostname = "jiminit"
console  = "/dev/console"
shell    = "/bin/sh"
respawn  = true                    # single 级 root shell 退出后是否重新拉起
shutdown_timeout_ms = 3000         # 关机时等待 SIGTERM 的时长
cgroup = true                      # 每个服务放进独立 cgroup v2 子组
log_file = "/var/log/jiminit.log"  # 追加日志；设为 "" 关闭；超过 1 MiB 滚动为 .1
ready_timeout_ms = 5000            # notify 服务等 READY=1 的上限
init_path = "/sbin/jiminit"        # stage2 可执行文件路径
services_dir = "/etc/jiminit/services.d"
default_runlevel = "multi-user"    # single | multi-user | graphical（或 1/3/5）
getty = "/sbin/getty"              # getty 程序（认证由它 exec 的 login 完成）
getty_baud = "38400"               # VT getty 波特率
serial_getty_baud = "115200"       # 串口 getty 波特率
ttys = ["/dev/tty1", "/dev/tty2", "/dev/tty3", "/dev/tty4", "/dev/tty5", "/dev/tty6"]
serial_ttys = ["/dev/ttyS0"]       # 不存在的设备会被跳过
```

解析是严格的：未知键、字段类型错误、文件读取失败都会让该阶段失败并进入 rescue
shell；只有文件缺失才使用全默认值。rescue shell 自己则用 `load_or_default`，保证
配置写坏时仍能登录修复。

## 构建与测试

依赖：Rust、`qemu-system-x86`、`busybox`（静态链接）、`e2fsprogs`、`cpio`、`gzip`，
以及宿主内核 `/boot/vmlinuz-linux`（测试直接借宿主机内核）。

```sh
./scripts/build.sh           # 生成静态链接的 out/jiminit
./scripts/mk-initramfs.sh    # 生成 out/initramfs.cpio.gz
./scripts/mk-rootfs.sh       # 生成 out/rootfs.ext4
./scripts/run-qemu.sh        # 交互式启动（ctrl-a x 退出）
./scripts/smoke-test.sh      # 自动验证并输出 PASS/FAIL

# 可选组件用环境变量打进 initramfs（默认为最小镜像）
FSCK=1 ./scripts/mk-initramfs.sh      # 打包 e2fsprogs，fsck -p 真正执行
COLDPLUG=1 ./scripts/mk-initramfs.sh  # kmod modprobe + 存储/文件系统模块
./scripts/smoke-real-test.sh          # coldplug + 真实 e2fsck 冒烟

# /usr 独立分区：fstab 按 UUID 挂载，pivot_root 后从 /usr/sbin/jiminit 启动
./scripts/smoke-usr-test.sh

# LUKS + LVM 根：先以普通用户构建 rootfs，再用 root 造磁盘
./scripts/build.sh && ./scripts/mk-rootfs.sh
qt-sudo ./scripts/mk-luks-image.sh   # out/luks.img + out/crypttab
./scripts/smoke-luks-test.sh         # 解锁 → 激活 LVM → 登录 → 关机
                             # （mk-initramfs 支持 FSCK_STUB=1、MODPROBE_STUB=1
                             #   注入 stub fsck/modprobe；MODPROBE_STUB_FAIL=1
                             #   让 stub 失败，用于手工验证 rescue 路径）
```

QEMU 磁盘用 virtio（`/dev/vda`），因为宿主内核里 `ata_piix` 是模块而 virtio-blk
是内建。启动参数：

```
console=ttyS0 rdinit=/init root=/dev/vda rootfstype=ext4 rw
```

也可以换成 `root=UUID=...` 或 `root=PARTUUID=...`。

### 已验证

- initramfs → 挂载 `/dev/vda` → `pivot_root` 切换根 → stage2 以 PID 1 运行；
- 读取 `/etc/jiminit.toml`（hostname 生效）、`/proc/1/comm` 为 `jiminit`；
- 串口 console 上出现 `jiminit-qemu login:`，用 root/`jiminit` 经 busybox `login`
  登录后得到 root shell（`id` 显示 `uid=0(root)`）；默认还在 tty1–tty6 起了 getty；
- stage1 按 `/etc/modules` 调用 `modprobe`（含 modprobe.d 的 blacklist/options/install/
  alias），并验证了 modprobe 失败时确实落入 rescue shell；
- stage1 在挂载根之前调用 `fsck -p`（冒烟测试用 stub fsck 验证调用与退出码判定），
  并按 `/etc/fstab` 挂载了 `tmpfs /data`；
- 服务：`ticker` 每秒输出并被转发到 kmsg（`svc/ticker: tick`）；
  `failer` 退出码 1 后按 1s→2s→4s 退避重启；关机时 `ticker` 收到 SIGTERM 退出；
- 服务在 `/sys/fs/cgroup/jiminit/<name>` 中运行（`/proc/self/cgroup` 显示
  `0::/jiminit/cg`）；`user = "nobody"` 的服务日志显示 uid/gid 65534；
- `notify = true` 的 `notifier` 发 `READY=1` 后 `svc/notifier: ready` 才出现，
  依赖它的 `after` 在之后才 `started`；超时路径有单测/告警；
- `jiminit --service after stop|start|restart` 分别输出 `ok after stopping/started/
  restarting`，停止后不会被重启策略拉起；`--service notifier reload` 发 SIGHUP；
- `[limits]` 生效：`limited` 服务读回 `memory.max` 为 `8388608`、`pids.max` 为
  `16`，对应控制器日志 `cgroup: enabled controller memory/pids`（按需启用）；
- `hog` 服务（`memory_max = "4M"`）分配超限后被内核杀掉，日志出现
  `svc/hog: OOM-killed (… total)`，`restart = "never"` 时不再拉起；
- `jiminit --status` 返回 `status multi-user`，并列出每个服务的 pid/重启次数/就绪/
  内存占用（`ticker running pid=…`、`cg running pid=… memory=…`）；
- 日志同时追加到 `/var/log/jiminit.log`（`test -s` 验证非空），超过 1 MiB 时滚动
  为 `.1`；
- 控制 socket 权限为 `0600`，非 root 用户无法查询或切换运行级；
- `jiminit --runlevel single` → 停 getty/ticker → `now in runlevel single`，
  console 上出现单用户 root shell；`jiminit --runlevel multi-user` → 重新起 getty，
  可以再次登录；
- `kill -TERM 1` → `reboot: Power down`，QEMU 正常退出（登录会话用 SIGHUP 结束，
  不再等满超时）；
- `kill -INT 1` → `reboot: Restarting system`；
- `kill -USR1 1` → `SIGUSR1: soft reboot` → 停机 → `fresh tmpfs on /run`、
  `/tmp` → `soft reboot #1: exec /sbin/jiminit --stage2` → 再次以 PID 1 启动、
  服务与 getty 全部重新拉起并出现新的 login 提示；之后再登录并 `kill -TERM 1`
  正常 `Power down`；
- `/usr` 在独立 ext4 上（fstab `UUID=` 挂载）→ `pivot_root` → PID 1 的运行路径为
  `/usr/sbin/jiminit` → 登录 → 关机；
- `FSCK=1 COLDPLUG=1` 镜像：扫描到 31 个 modalias，kmod modprobe 按 alias 加载，
  `e2fsck -p /dev/vda` 真实执行并以 0 退出；
- LUKS2 容器（crypttab `UUID=` + 交互口令）→ `lvm vgchange -ay` →
  `root=/dev/mapper/jiminitvg-root` → 登录 → 关机，冒烟测试全过；
- `root=UUID=<fs-uuid>`、`root=PARTUUID=<gpt-partition-guid>` 均能定位设备
  （GPT 分区测试得到 `/dev/vda1`）。

### 已知限制

- 模块只按 `/etc/modules` 显式加载，不在启动后处理 uevent 热插拔；
- initramfs 必须自带 modprobe 和对应的 `/lib/modules/<kver>`（含 `modules.dep`，
  构建镜像时用 `depmod` 生成），jiminit 不负责生成模块依赖；
- fsck 只检查根文件系统，且依赖 initramfs 里自带 `fsck` 程序（没有就跳过并记日志）；
  默认镜像不带 fsck，需要 `FSCK=1` 打包 e2fsprogs；
- fstab 只用于 switch_root 前挂载根以外的条目：不支持 `LABEL=`/`/dev/disk/by-*`，
  不处理 swap，也不自动创建挂载点；
- fstab 里不认识的选项按文件系统参数透传（`data=journal` 之类可用；拼错的选项会
  由内核拒绝并中止启动），没有实现 util-linux 的完整选项表；
- `PARTUUID=` 的 MBR 分支只支持主分区（`diskid-01`…`-04`），不支持扩展/逻辑分区；
  GPT 只扫描前 128 个表项；GPT 的逻辑扇区大小会自动探测，4Kn 磁盘可用；
- LUKS 只支持 plain LUKS1/LUKS2 的 `luks` 类型（没有 tcrypt/bitlk），不支持
  detached header 以外的密钥来源组合；口令/密钥文件之外没有 keyring、TPM 等；
  测试镜像用 pbkdf2/2000 轮是为了在 TCG 下快，Argon2 的默认参数在无 KVM 时会很慢；
- LVM 只做整个卷组的 `vgchange -ay`：不处理 thin/raid/cache 的特殊激活，也不读
  宿主的 lvm.conf（initramfs 里用一份精简配置，关掉 udev 同步）；
- initramfs 打包 cryptsetup/lvm/modprobe 时按 `ldd` 收库，镜像会涨到约 11MB；
- coldplug 只扫启动时 `/sys/bus/*/devices/*/modalias` 已存在的设备：不处理
  uevent 热插拔、udev 规则和固件加载（`/lib/firmware` 不会打包）；模块集合由
  `COLDPLUG_MODULES` 决定，默认为常见存储/文件系统驱动；
- 打包真实 fsck 时只带 e2fsprogs（ext2/3/4），xfs/btrfs 的检查工具没有打包；
- `/usr` 不会像 systemd 那样自动探测并挂载：必须写在 `/etc/fstab` 里；
- jiminit 不内置认证：没有 PAM，也不解析 `/etc/shadow`，完全依赖外部 `login`
  （测试镜像里是 busybox）；替换成 util-linux login/agetty 时应同样可用；
- 运行级只有三个固定级别，没有 SysV 的任意 rcN.d 脚本目录，也没有 inittab；
- VT 之间切换只能靠内核快捷键，jiminit 不处理 `chvt`/键盘；
- 资源限制只有 `memory.max`/`pids.max`/`cpu.max`，没有 io.max、cpuset、权重，
  也没有全局默认限制；
- 健康检查只有 sd_notify 看门狗，没有 `health_cmd` 之类的外部探测；看门狗只校验
  有没有收到 `WATCHDOG=1`，不校验 `WATCHDOG_PID`，也不支持 `WATCHDOG=trigger`；
- 依赖只有 `depends_on`（类似 Requires），没有 After/Before 的独立顺序控制，也没有
  自动重启依赖方的反向传播；级联停止只发生在依赖最终失败或被手动 stop 时；
- OOM 靠主循环轮询 `memory.events` 发现（不是事件驱动）；cgroup 目录回收后计数
  归零，只用于日志提示，重启决策仍由退出状态和 restart 策略决定；
- 日志文件只保留一份旧的 `.1`，不压缩、不外发 syslog；文件写失败会静默停用该
  sink（kmsg/console 仍在）；
- `--status` 只报内存，不报 CPU/IO 用量；`restarts` 从 stage2 本次启动算起，
  服务被手动 stop/start 也会计入；
- 只支持 x86_64 上验证过的路径，其他架构未测试；
- `root=` 只支持 `/dev` 路径、UUID、PARTUUID，不支持 LABEL；
- 软重启只重新执行 stage2：initramfs 已经不在，内核也不会重新引导，所以
  `root=`/initramfs/内核参数相关的改动必须走真正的重启；
- getty 反复启动失败时固定每 500ms 重拉一次，没有退避，会持续刷日志；
- 配置解析失败（未知键/类型错误）会进入 rescue 而不是带默认值继续启动；
- 软重启用 tmpfs 顶替 `/run`、`/tmp`，旧内容若原本在根文件系统上仍会占用空间
  （真重启才会回收）。

## 安装到真实系统

本地安装（先 `./scripts/build.sh`）：

```sh
./scripts/install.sh                      # /sbin/jiminit + /etc/jiminit.toml
./scripts/install.sh --prefix /usr        # 装到 /usr/sbin（配置仍在 /etc）
./scripts/install.sh --destdir out/stage  # 打包用（DESTDIR）
```

已有的 `/etc/jiminit.toml` 不会被覆盖；`/etc/jiminit/services.d/` 里会放一个
`example.toml.disabled` 示例。安装后按需编辑配置与服务。若机器不需要 initramfs
（根设备驱动已内建、没有 LUKS/LVM），内核命令行写 `init=/sbin/jiminit` 即可直接引导。

Arch/Artix 也可以直接打包（PKGBUILD 会构建当前 checkout）：

```sh
makepkg -si
```

### mkinitcpio 集成

把 hook 装到 mkinitcpio 的目录（PKGBUILD 会自动做）：

```sh
install -Dm755 contrib/mkinitcpio/install/jiminit /usr/lib/initcpio/install/jiminit
install -Dm755 contrib/mkinitcpio/hooks/jiminit   /usr/lib/initcpio/hooks/jiminit
```

在 `/etc/mkinitcpio.conf` 里把 `jiminit` 放在 **最后**，并移除 `fsck`（jiminit 自己
会跑 `fsck -p`）：

```
HOOKS=(base udev autodetect modconf block filesystems keyboard jiminit)
```

然后 `mkinitcpio -P`。hook 会把 `/sbin/jiminit`、`modprobe`、可选的
`fsck.ext4`/`cryptsetup`/`lvm`，以及主机上的 `/etc/crypttab`、`/etc/lvm/lvm.conf`
打进 initramfs；内核命令行仍然要写 `root=`/`rootfstype=`。注意 `/etc/crypttab` 里
的所有条目都会被解锁（目前没有按 root 过滤）。

### aarch64 / 交叉构建

代码没有 x86_64 特有假设（磁盘扇区大小、ioctl、设备号都走 libc/sysfs 或运行时
探测）；但目前只在 x86_64 上实测过。

```sh
rustup target add aarch64-unknown-linux-gnu
# 需要交叉 linker，例如 aarch64-linux-gnu-gcc，并在 .cargo/config.toml 里配置
TARGET=aarch64-unknown-linux-gnu ./scripts/build.sh
ARCH=aarch64 KERNEL=/boot/Image ./scripts/run-qemu.sh   # 需要 aarch64 内核与 qemu-system-aarch64
```

静态 glibc 在非 x86_64 上可能需要内核支持 static PIE；遇到问题时可以改用
`aarch64-unknown-linux-musl` 目标（musl 静态链接更省心，但这一路径尚未验证）。

## 设计取舍

依赖只引入 `nix` + `serde`/`toml` + `anyhow`，其余（cmdline、UUID/GPT 解析、fstab、
fsck 调用与退出码判定、switch_root、signalfd 主循环、服务监督）都自己实现，便于后续
替换或精简。模块加载没有自己实现 modprobe，而是解析规则后调用外部 modprobe，
这样 `.ko` 的解压、依赖和 version magic 都交给内核工具链。同理，LUKS 与 LVM
不自己实现 dm-crypt/元数据，只调用外部 `cryptsetup`/`lvm`：initramfs 打包时用
`ldd` 自动收集动态依赖，避免了把 JSON/LVM 元数据格式重新实现一遍。
PID 1 主循环用 `signalfd` + `poll` 而不是线程，避免在多线程进程里 fork 的风险；
服务日志通过管道回到主循环带前缀写入 kmsg，同样不需要额外线程。

具体决策点（QEMU 验证、TOML 配置、init 接管关机、UUID/PARTUUID、服务格式/依赖启动/
重启策略/日志去向、getty+外部 login、三个命名运行级、控制 socket 切换、fstab 范围、
fsck 策略、模块加载方式与失败即 rescue）都是和需求方逐项确认后定下的。
