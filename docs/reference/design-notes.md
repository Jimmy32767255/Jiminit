# 设计取舍

## 总体约束

- **单二进制、两阶段**：同一个可执行文件既是 initramfs 的 `/init`，也是真实根上的
  PID 1。不需要第二套代码路径，也方便把 stage2 换成任意 `init_path`。
- **PID 1 永不退出**：任何错误都进 rescue shell；shutdown 只通过 `reboot(2)` 结束。
- **不引线程**：主循环用 `signalfd` + `poll`，避免多线程进程里 `fork` 的风险；
  服务日志通过管道回到主循环写 kmsg，也不需要额外线程。
- **依赖极少**：只引 `nix` + `serde`/`toml` + `anyhow`。cmdline、UUID/GPT、fstab、
  fsck 调用、switch_root、服务监督都自己实现，便于替换或精简。

## 为什么把复杂格式交给外部工具

模块加载没有自己实现 modprobe，而是解析 `modprobe.d` 规则后调用外部 `modprobe`，
`.ko` 解压、依赖、version magic 都交给内核工具链。LUKS/LVM 同理：只调用
`cryptsetup`/`lvm`，initramfs 打包时用 `ldd` 自动收集动态依赖。自己重实现一遍
dm-crypt 或 LVM 元数据格式不划算，也容易和真实系统行为分叉。

认证也一样：jiminit 只拉起 getty，密码校验交给 `login`。内置认证需要 crypt/PAM，
静态链接 libxcrypt 本身就不可行，而且会把“个人 init”变成半个发行版。

## root 切换

`pivot_root(".", ".")` 能把整棵新根挂载树（包括 fstab 挂进来的 `/usr`）原子地变成
`/`，比 `MS_MOVE` + `chroot` 更完整。内核在某些挂载传播模式下会拒绝，所以保留
`MS_MOVE` + `chroot` 回退，并记录走了哪条路径。

## 配置

TOML 适合手写，serde 能直接映射到结构体。解析是严格的：未知键和类型错误直接进
rescue，而不是悄悄用默认值——init 的配置错一位可能导致起不来，静默降级更难查。
只有文件缺失才用默认值。rescue 是例外：它必须能在配置写坏时登录，所以用
`load_or_default`。

## 运行级

没有实现任意 `rcN.d` 脚本目录：三个命名级别覆盖了“维护模式 / 正常 / 图形”的实际
需求，数字别名兼容习惯。服务用 `runlevels` 标签声明归属，未标注默认进
multi-user/graphical；single 只跑显式标注的服务及其依赖闭包，避免在维护模式里
意外启动网络或数据库。

切换时先结束会话再切服务：交互式 shell 忽略 SIGTERM，所以用 SIGHUP；同时避免新旧
shell 争抢 console 的控制终端。

## 服务模型

- 每个服务一个 TOML 文件，文件名即服务名。改一个服务不需要动其他文件，也方便
  包管理器/用户各自投放。
- 依赖只做 `depends_on`：更细的 After/Before 排序对个人 init 是过度设计。
- 失败语义选择“快失败”：依赖失败时依赖方不启动，已经运行的级联停止。这样不会留下
  半启动的服务树，代价是恢复要手动。
- 就绪用最小 sd_notify：只认 `READY=1`/`STATUS=`/`WATCHDOG=1`，够和常见守护进程
  配合，不必实现完整的 systemd notify 协议。
- 重启退避 1s→60s、连续 5 次放弃、稳定 10s 重置：避免崩溃循环刷日志，同时给临时
  故障留出恢复机会。
- cgroup v2 统一层级：每服务一个子组，限制和 OOM 统计都直接读内核接口；只有真的
  设置限制时才启用对应控制器，保持最小系统轻量。
- 手动 stop 优先于重启策略；restart 等旧进程退出再拉起，避免两个实例并存。

## 日志

stderr + `/dev/kmsg` + 可选文件三路，单次 write 一行，避免多进程交错。文件限制
1 MiB 并只留一份 `.1`：个人系统不需要日志归档，写满磁盘更糟。写失败静默停用文件
sink，console/kmsg 仍在——PID 1 不能因为日志挂了而死。

## 决策记录

以下决策点都是和需求方逐项确认后定下的：

- 用 QEMU 做验证环境；第一版只做骨架（伪文件系统、hostname、收尸、shell、关机）。
- 配置用 TOML；依赖限定成熟 crate（nix/serde/toml/anyhow）。
- 交互入口直接 getty + 外部 login，不做内置 getty/login。
- 支持 `switch_root` 到磁盘根，也支持真实根上 `init=` 直接启动。
- `root=` 支持路径、UUID、PARTUUID（不做 LABEL）。
- init 接管关机：SIGTERM 关机、SIGINT 重启、有序停服务、remount ro、`reboot(2)`。
- 服务用 `services.d/*.toml`，依赖拓扑、重启退避、日志进 kmsg。
- 软重启用 `SIGUSR1` 触发，清 `/run`、`/tmp` 后 exec 自己。
- 运行级用三个命名级别 + 数字别名，控制 socket 切换。
- initramfs 阶段支持模块、fsck、fstab、coldplug、LUKS/LVM，模块加载失败即 rescue。
- 服务强化：cgroup v2、自解析 passwd/group、sd_notify、控制 socket。
- 资源限制用 `[limits]` 子表，按需启用控制器，OOM 轮询 `memory.events`。
- 状态输出人类可读，日志落盘并限大小。
- 打包走标准路径 + PKGBUILD + mkinitcpio hook，构建支持 aarch64。
- 依赖失败级联、看门狗超时按 restart 策略处理。

## 非目标

不追求 systemd 兼容（unit 语法、D-Bus、socket 激活、cgroup 委托），不实现 udev
规则引擎，不做容器运行时集成，也不提供任意 rc 脚本兼容层。这些如果要做，应该作为
独立项目/插件，而不是塞进 PID 1。
