# 会话、运行级与控制

## 登录

jiminit 不做认证。它只为每个终端 fork 一个 getty，getty 再 exec 外部 `login`；
密码校验、`/etc/shadow`、PAM 都归登录程序管（测试镜像是 busybox）。

默认终端：

- `ttys`：`/dev/tty1`–`/dev/tty6`，各一个 getty，波特率 `getty_baud`（38400）；
- `serial_ttys`：`/dev/ttyS0`，设备存在才起，波特率 `serial_getty_baud`（115200）。

getty 退出（比如登录 shell 退出）后由主循环按 deadline 重新拉起，PID 1 不会 sleep：
首次 500ms，连续崩溃按 500ms→1s→2s… 退避，上限 30s；稳定运行 ≥10s 后退避复位。
single 级的 root shell 使用同一套重拉与退避逻辑。
PID 1 自己不占控制终端，getty/shell 各自 `setsid` + `TIOCSCTTY`。

## 运行级

| 名称 | 数字别名 | 内容 |
| --- | --- | --- |
| `single` | 1 | 只跑显式标了 `runlevels = ["single"]` 的服务及其依赖；console 上是 root shell，不起 getty |
| `multi-user` | 3 | 默认级；未标 `runlevels` 的服务都算；getty 提供登录 |
| `graphical` | 5 | 同 multi-user，加上标了 `graphical` 的服务；jiminit 不启动图形界面 |

服务未标 `runlevels` 等价于 `["multi-user", "graphical"]`。显式标 `single` 的服务
同时也会自动把它的依赖拉进 single 级。

切换时：

1. 先结束旧会话：切到 single 结束所有 getty，切走 single 结束 console 上的 root shell
   （SIGHUP；交互式 shell 忽略 SIGTERM，所以用 SIGHUP）；
2. 不属于新级别的服务按逆序 SIGTERM，3s 内没退出就 SIGKILL；daemonize 的服务
   （显示管理器及其 X/Wayland 会话）会逃出服务的进程组，所以超时后按 cgroup 整树杀；
3. 离开 `graphical` 时还要把显示交回来：先终止仍打开 `/dev/dri/*` 的进程（合成器
   常被 elogind 移进自己的 session scope，不在服务 cgroup 里），再对控制台
   `KDSETMODE KD_TEXT` 并把第一个 getty 的 VT 激活。否则屏幕会停在图形模式：
   黑屏 + 残留鼠标指针，且切 TTY 没有任何反应（内核控制台不再重绘）；
4. 属于新级别但未运行的服务按依赖启动；
5. 打开新级别的会话（single 起 root shell，其他级别起 getty）。

日志会看到 `switching runlevel multi-user -> single`、`console restored to text mode`
和 `now in runlevel single`。

## 控制 socket

`/run/jiminit.sock`，权限 `0600`，只有 root 能连。客户端就是 jiminit 自己：

```sh
jiminit --status
jiminit --runlevel single
jiminit --runlevel 3           # 数字别名
jiminit --runlevel reboot      # 6/reboot 重启；0/poweroff/halt 关机
jiminit --service ticker restart
jiminit --metrics               # Prometheus 文本
```

`--runlevel 0|6` 不会“进入”某个级别，而是直接走关机/重启流程。未知级别返回
`error ...`，客户端退出码为 1。

`--status` 首行是运行级，其余每个服务一行：

```text
status multi-user
ticker running pid=126 restarts=0 ready=yes memory=84.0K cpu=5.0ms peak=256.0K pids=1 io=-
after stopped pid=- restarts=1 ready=- memory=- cpu=- peak=- pids=- io=-
```

字段：服务名、`running`/`stopped`、pid、启动次数（本次 stage2 以来，手动 start
也算；软重启会重置）、是否就绪（非 notify 服务启动即 yes）、`memory.current`、
CPU 累计时间、`memory.peak`、`pids.current`、io 读写字节。没有对应控制器的字段
显示 `-`；`io` 只在启用 io 控制器时才有值（当前没有 io 限制配置，所以通常是
`-`）。

`jiminit --metrics` 输出同一份数据的 Prometheus 文本格式，可直接交给抓取器：

```text
jiminit_build_info{version="0.1.0"} 1
jiminit_runlevel_info{runlevel="multi-user"} 1
jiminit_service_info{name="ticker",state="running",ready="yes"} 1
jiminit_service_restarts_total{name="ticker"} 0
jiminit_service_cpu_seconds_total{name="ticker"} 0.005000
jiminit_service_memory_bytes{name="ticker"} 86016
```

缺失的指标整行省略，不会输出 `NaN` 或空值。

实现上每次请求只处理一个连接：accept 后设 500ms 超时，读一行，执行，回写，关闭。
所以极慢的客户端最多阻塞主循环约 1 秒。

## 信号

| 信号 | 行为 |
| --- | --- |
| `SIGTERM` | 有序关机（`kill -TERM 1`，也对应 `poweroff`） |
| `SIGINT` | 重启（键盘 ctrl-alt-del 也映射到它） |
| `SIGUSR1` | 软重启：重新 exec stage2，不重启内核 |
| `SIGQUIT` / `SIGHUP` | 记录后忽略 |
| `SIGCHLD` | 只用来唤醒主循环，实际靠每轮 `waitpid` 收尸 |

关机流程：SIGHUP getty/单用户 shell → 逆序 SIGTERM 服务 → 向所有剩余用户进程
SIGTERM → 等 `shutdown_timeout_ms` → 仍存活 SIGKILL 再等 2s → `sync` → 根 remount
只读 → `reboot(RB_POWER_OFF)` 或 `reboot(RB_AUTOBOOT)`。

## 软重启

```sh
kill -USR1 1
```

它复用停机流程，但在 `sync` 之后停住：给 `/run`、`/tmp` 各挂一个新的 tmpfs（旧挂载
`MNT_DETACH` 摘掉），然后 `execve("/proc/self/exe", ["--stage2"])`，环境里带
`JIMINIT_SOFT_REBOOTS=n+1`。新 stage2 重新读配置、重建服务、重新起 getty。

因为不经过内核：PID 仍为 1，initramfs/stage1 不会重跑，`root=` 相关的改动必须走真正
重启；旧 `/run`、`/tmp` 里原本在根文件系统上的内容不会被删除，只是被新 tmpfs 盖住。

exec 失败会进入 rescue shell，而不是让机器卡死。

## rescue shell

任何阶段出错或 panic 都不会让 PID 1 退出，而是进入 rescue 循环：在 `console` 上
反复 fork 一个 shell。这是最后的维护入口，所以它用 `load_or_default` 读配置——
即使配置写坏也能登录修复。日志会先打印具体错误，例如：

```text
jiminit: Initramfs failed: load kernel module virtio_blk: modprobe ... exited with status: 1
jiminit: starting rescue shell (/bin/sh)
```
