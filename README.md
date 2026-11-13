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
  service.rs  服务监督：TOML 解析、依赖拓扑、重启退避、日志转发、停机
  rootdev.rs  根设备解析：/dev 路径、UUID=、PARTUUID=
  modules.rs  /etc/modules + modprobe.d 规则，加载根设备所需的内核模块
  fstab.rs    /etc/fstab 解析（switch_root 前挂载额外文件系统）
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
2. 按 initramfs 里的 `/etc/modules` 逐行 `modprobe`（支持 `模块名 参数...`），并应用
   `/etc/modprobe.d/*.conf` 的 `options`/`blacklist`/`install`/`alias` 规则；找不到
   modprobe 或任一模块加载失败都会中止启动、进入 rescue；
3. 解析内核命令行：
   - `root=`：`/dev/xxx`、`UUID=...`、`PARTUUID=...`（不支持 `LABEL=`）；
   - `rootfstype=`（可省略，省略时先让内核自动探测，失败再依次试 ext4/xfs/btrfs/vfat）；
   - `rootflags=` 透传给 mount；
   - `rw`：默认只读挂载，出现 `rw` 才读写；
4. 设备节点可能还没就绪，所以按 20 次 × 250ms 重试；
5. 挂载前对根设备执行 `fsck -p`：initramfs 里找得到 `/sbin/fsck`（或 `/bin`、
   `/usr/sbin`、`/usr/bin`）才执行，`fsck=no` 可在内核命令行关闭；退出码 4/8/16/32
   等视为失败，拒绝挂载并进入 rescue（0/1/2 视为可用）；
6. 挂载到 `/newroot`，读取 `/newroot/etc/fstab`，把根以外的条目（跳过 swap、伪文件
   系统、`noauto` 和重复挂载点）挂到 `/newroot/<mountpoint>`；`nofail` 的失败只告警，
   其余失败中止启动；
7. 校验 `/newroot/sbin/jiminit` 存在；
8. 把 `/dev`、`/proc`、`/sys` 的挂载点 MS_MOVE 进新根；
9. `chdir("/newroot")` → `mount(".", "/", MS_MOVE)` → `chroot(".")` → `chdir("/")`
   （与 busybox switch_root 相同的收尾，缺 `chroot(".")` 会导致后续 exec 仍看旧根）；
10. `execve("/sbin/jiminit", ["--stage2"])`。

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
7. 监听 `/run/jiminit.sock`，接受 `status` 与 `runlevel <name>` 请求（见下文）；
8. `SIGINT`（ctrl-alt-del）：重启；`SIGTERM`：关机；`SIGUSR1`：软重启；
9. 关机流程：SIGHUP 结束所有 getty/单用户 shell → 按启动逆序给服务发 SIGTERM → 等待
   `shutdown_timeout_ms` → 剩余进程 SIGKILL → `sync` → 根重新挂载为只读 →
   `reboot(RB_POWER_OFF | RB_AUTOBOOT)`；
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
jiminit --status                # 输出 status multi-user
jiminit --runlevel single       # 切到单用户，原登录会话会被结束
jiminit --runlevel 3            # 数字别名
jiminit --runlevel reboot       # 6 / reboot 触发重启；0 / poweroff / halt 触发关机
```

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
runlevels = ["multi-user"]  # 默认 ["multi-user", "graphical"]
```

启动语义：

- 按 `depends_on` 建依赖图，无依赖关系的服务并行拉起（启动完成 = fork+exec 成功，
  没有就绪探测协议）；依赖缺失或成环的服务会被跳过并告警；
- 服务在独立 session/进程组中运行，stdin 是 `/dev/null`，stdout/stderr 经管道转发到
  `/dev/kmsg`，每行带 `svc/<name>: ` 前缀；
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
init_path = "/sbin/jiminit"        # stage2 可执行文件路径
services_dir = "/etc/jiminit/services.d"
default_runlevel = "multi-user"    # single | multi-user | graphical（或 1/3/5）
getty = "/sbin/getty"              # getty 程序（认证由它 exec 的 login 完成）
getty_baud = "38400"               # VT getty 波特率
serial_getty_baud = "115200"       # 串口 getty 波特率
ttys = ["/dev/tty1", "/dev/tty2", "/dev/tty3", "/dev/tty4", "/dev/tty5", "/dev/tty6"]
serial_ttys = ["/dev/ttyS0"]       # 不存在的设备会被跳过
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

- initramfs → 挂载 `/dev/vda` → MS_MOVE 切换根 → stage2 以 PID 1 运行；
- 读取 `/etc/jiminit.toml`（hostname 生效）、`/proc/1/comm` 为 `jiminit`；
- 串口 console 上出现 `jiminit-qemu login:`，用 root/`jiminit` 经 busybox `login`
  登录后得到 root shell（`id` 显示 `uid=0(root)`）；默认还在 tty1–tty6 起了 getty；
- stage1 按 `/etc/modules` 调用 `modprobe`（含 modprobe.d 的 blacklist/options/install/
  alias），并验证了 modprobe 失败时确实落入 rescue shell；
- stage1 在挂载根之前调用 `fsck -p`（冒烟测试用 stub fsck 验证调用与退出码判定），
  并按 `/etc/fstab` 挂载了 `tmpfs /data`；
- 服务：`ticker` 每秒输出并被转发到 kmsg（`svc/ticker: tick`）；
  `failer` 退出码 1 后按 1s→2s→4s 退避重启；关机时 `ticker` 收到 SIGTERM 退出；
- `jiminit --status` 返回 `status multi-user`；
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
- `root=UUID=<fs-uuid>`、`root=PARTUUID=<gpt-partition-guid>` 均能定位设备
  （GPT 分区测试得到 `/dev/vda1`）。

### 已知限制

- 模块只按 `/etc/modules` 显式加载，不做 udev 式的 modalias coldplug；
- initramfs 必须自带 modprobe 和对应的 `/lib/modules/<kver>`（含 `modules.dep`，
  构建镜像时用 `depmod` 生成），jiminit 不负责生成模块依赖；
- fsck 只检查根文件系统，且依赖 initramfs 里自带 `fsck` 程序（没有就跳过并记日志）；
  真正可用的 e2fsprogs 还没打进测试镜像；
- fstab 只用于 switch_root 前挂载根以外的条目：不支持 `LABEL=`/`/dev/disk/by-*`，
  不处理 swap，也不做挂载点自动创建；
- 不支持 LUKS/LVM（没有 cryptsetup/dm 初始化），也还没处理 `/usr` 单独挂载后
  切根前的 pivot 场景；
- jiminit 不内置认证：没有 PAM，也不解析 `/etc/shadow`，完全依赖外部 `login`
  （测试镜像里是 busybox）；替换成 util-linux login/agetty 时应同样可用；
- 运行级只有三个固定级别，没有 SysV 的任意 rcN.d 脚本目录，也没有 inittab；
- VT 之间切换只能靠内核快捷键，jiminit 不处理 `chvt`/键盘；
- 服务没有就绪探测（没有 sd_notify 之类），依赖只保证启动顺序；
- 服务没有 cgroup 隔离，也不切换用户/组运行；
- 只支持 x86_64 上验证过的路径，其他架构未测试；
- `root=` 只支持 `/dev` 路径、UUID、PARTUUID，不支持 LABEL；
- 软重启只重新执行 stage2：initramfs 已经不在，内核也不会重新引导，所以
  `root=`/initramfs/内核参数相关的改动必须走真正的重启；
- 软重启用 tmpfs 顶替 `/run`、`/tmp`，旧内容若原本在根文件系统上仍会占用空间
  （真重启才会回收）。

## 设计取舍

依赖只引入 `nix` + `serde`/`toml` + `anyhow`，其余（cmdline、UUID/GPT 解析、fstab、
fsck 调用与退出码判定、switch_root、signalfd 主循环、服务监督）都自己实现，便于后续
替换或精简。模块加载没有自己实现 modprobe，而是解析规则后调用外部 modprobe，
这样 `.ko` 的解压、依赖和 version magic 都交给内核工具链。
PID 1 主循环用 `signalfd` + `poll` 而不是线程，避免在多线程进程里 fork 的风险；
服务日志通过管道回到主循环带前缀写入 kmsg，同样不需要额外线程。

具体决策点（QEMU 验证、TOML 配置、init 接管关机、UUID/PARTUUID、服务格式/依赖启动/
重启策略/日志去向、getty+外部 login、三个命名运行级、控制 socket 切换、fstab 范围、
fsck 策略、模块加载方式与失败即 rescue）都是和需求方逐项确认后定下的。
