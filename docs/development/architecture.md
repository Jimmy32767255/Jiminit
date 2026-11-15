# 架构

jiminit 是单二进制、两阶段的 init：同一个可执行文件既能当 initramfs 的 `/init`，
也能当真实根上的 PID 1。核心约束只有一条：**PID 1 永远不能退出**。所有错误都在
记录后进入 rescue shell 循环。

## 阶段判定

`main` 先按子命令处理一次性客户端命令（`status`、`reboot` 等），然后：

1. 显式指定：参数 `--stage1` / `--stage2`，或环境变量 `JIMINIT_STAGE=1|2`；
2. 自动判断：`/` 是 ramfs/tmpfs（`statfs` 魔数）且 `/proc/cmdline` 里有 `root=`，
   视为 initramfs 阶段；否则是真实根阶段。

判定前已经初始化日志并挂载了伪文件系统（`/dev`、`/proc`、`/sys`），两个阶段都需要它们。

## 源码地图

| 文件 | 职责 |
| --- | --- |
| `src/main.rs` | 入口、阶段判定、CLI 客户端、panic 兜底 |
| `src/stage1.rs` | initramfs 阶段：模块、crypttab、LVM、根设备、fsck、fstab、pivot_root |
| `src/stage2.rs` | PID 1 主循环：信号、收尸、getty、控制 socket、关机/软重启、rescue |
| `src/service.rs` | 服务解析、依赖图、重启、就绪、看门狗、cgroup、日志转发 |
| `src/runlevel.rs` | `single`/`multi-user`/`graphical`、数字别名、特殊目标 |
| `src/accounts.rs` | `/etc/passwd`、`/etc/group` 解析（无 NSS） |
| `src/cgroup.rs` | cgroup v2 子组、控制器、限制、OOM 计数 |
| `src/rootdev.rs` | 根设备解析：路径、UUID=、PARTUUID=、GPT/MBR |
| `src/modules.rs` | `/etc/modules` 与 modprobe.d 规则，调用外部 modprobe |
| `src/coldplug.rs` | 扫描 sysfs modalias，尽力加载已发现设备的驱动 |
| `src/fstab.rs` | fstab 解析、挂载点规范化、选项映射 |
| `src/crypttab.rs` / `src/luks.rs` | crypttab 解析与 cryptsetup 解锁（3 次重试） |
| `src/config.rs` | `/etc/jiminit.toml` 严格解析 |
| `src/cmdline.rs` | `/proc/cmdline` 解析（重复键后者覆盖） |
| `src/mount.rs` | 挂载辅助、mountpoint 探测 |
| `src/log.rs` | stderr + `/dev/kmsg` + 可选文件（1 MiB 滚动） |

## stage1：initramfs

按顺序：

1. 挂载伪文件系统（`main` 已完成；stage1 会确保存在）。
2. 读 `/etc/modules` 并用外部 `modprobe` 加载；应用 `/etc/modprobe.d/*.conf` 的
   `options`/`blacklist`/`install`/`alias`。blacklist 只拦截 alias 解析，显式列出的
   模块仍会加载。单条 modprobe/install 超过 30s 判失败。任何失败 → rescue。
3. 扫描 `/sys/bus/*/devices/*/modalias`，去重后逐个 `modprobe -q`；尽力而为，失败只记日志。
4. 读 `/etc/crypttab`，逐个 `cryptsetup luksOpen`；每个容器最多 3 次，仍失败 → rescue。
   只识别 `discard`、`readonly`、`keyslot=N`、`header=PATH`，未知选项告警忽略。
5. 需要时激活 LVM：命令行有 `lvm=1`/`lvm`，或 `root=/dev/mapper/...`，执行
   `lvm vgchange -ay`；失败 → rescue。
6. 解析命令行：`root=`（路径/UUID=/PARTUUID=）、`rootfstype=`、`rootflags=`、`rw`。
   默认只读挂载。
7. 解析根设备（设备节点可能没就绪，20 次 × 250ms 重试）。
8. 对根设备跑 `fsck -p`（可用 `fsck=no` 关闭）；退出码 0/1/2 放行，其余 → rescue。
9. 挂载到 `/newroot`；读 `/newroot/etc/fstab`，把根以外的条目挂进去。挂载点先规范化，
   `noauto`/swap/伪文件系统跳过；`nofail` 失败只告警，其余失败中止。
10. 从 `/newroot/etc/jiminit.toml` 读 `init_path`，校验可执行文件存在。
11. 把 `/dev`、`/proc`、`/sys` 的挂载点 `MS_MOVE` 进新根。
12. `chdir("/newroot")` → `pivot_root(".", ".")` → `umount2(".", MNT_DETACH)` →
    `chdir("/")`；内核拒绝时回退 `MS_MOVE` + `chroot(".")`。
13. `execve(init_path, ["--stage2"])`。

## stage2：PID 1

1. 读 `/etc/jiminit.toml`（严格解析，失败 → rescue）。
2. `sethostname`；打开 `console` 并 dup2 到 0/1/2。PID 1 自己不 `TIOCSCTTY`，
   控制终端留给各会话。
3. 阻塞信号并创建 `signalfd`；主循环用 `poll` 等信号、服务日志、notify socket、
   控制 socket 或 100ms 超时。
4. 按 `default_runlevel` 选出服务成员，加载服务目录，按依赖启动。
5. 非 single 级为每个存在的 tty fork getty；single 级在 console 上 fork root shell。
6. 每轮回收所有子进程：服务退出交给监督器，getty 退出安排重拉，其余孤儿只记录。
7. 控制 socket 处理 `status`、`runlevel <name>`、`service <name> <action>`。
8. 收到 `SIGTERM`/`SIGINT`/`SIGUSR1` 进入停机流程（关机/重启/软重启）。

## 服务监督器内部

`Supervisor` 维护：

- `services: Vec<ServiceSpec>`：按启动顺序排好的定义；
- `running: HashMap<Pid, Running>`：pid → 服务下标、启动时刻、失败次数、就绪状态、
  看门狗 deadline、OOM 计数；
- `pending_start`：还没轮到/依赖未就绪的成员；
- `pending`：退避中的重启（到期时间）；
- `failed` / `completed`：最终失败的（依赖方要级联停）与成功完成的 oneshot；
- `manual_stop` / `restart_after`：控制命令的意图，优先于重启策略；
- `logs` / `notify`：管道与 notify socket 的 fd 和行缓冲。

主循环每轮把这些 fd 加入 `poll`，然后 `tick()` 处理到期重启、依赖就绪的启动排队、
看门狗超时和 OOM 轮询。所有子进程都用 `waitpid(-1)` 收，不依赖 SIGCHLD 计数。

进程放置：服务先 fork，再在子进程里 `setsid`、写 cgroup、降权、exec；日志管道是
带 `O_CLOEXEC` 的 `pipe2`，读端非阻塞。杀进程时同时发给 pid 和 `-pid`（进程组），
cgroup 只是最后兜底。

## 日志

`log::info/warn/err` 同时写 stderr、`/dev/kmsg` 和可选日志文件；每次单次 `write`
一行，避免多进程交错。文件超过 1 MiB 时删除旧的 `.1` 并滚动；写失败静默停用文件
sink，不影响 kmsg/console。服务输出复用同一套日志，行首加 `svc/<name>: `。

## rescue

`stage2::rescue()` 在 `console` 上循环 fork shell。它在对配置加载失败时也使用默认
配置，保证还能登录。任何 stage 的 `Err` 或 panic 都走到这里。
