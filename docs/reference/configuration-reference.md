# 配置与命令行速查

## `/etc/jiminit.toml`

所有键可省略。缺失文件 → 全默认；未知键/类型错误 → 进 rescue。

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `hostname` | string | `"jiminit"` | stage2 设置的 hostname |
| `console` | string | `"/dev/console"` | init 日志与 single shell 的终端 |
| `shell` | string | `"/bin/sh"` | single 级 / rescue 的 shell |
| `respawn` | bool | `true` | single shell 退出后是否重拉 |
| `shutdown_timeout_ms` | int | `3000` | 关机 SIGTERM 宽限期 |
| `init_path` | string | `"/sbin/jiminit"` | stage2 路径（stage1 读真实根配置后 exec） |
| `log_file` | string | `"/var/log/jiminit.log"` | 追加日志，`""` 关闭，1 MiB 滚动 |
| `color` | string | `"auto"` | `auto` 只在终端上色，`always`/`never` 强制 |
| `services_dir` | string | `"/etc/jiminit/services.d"` | 服务定义目录 |
| `cgroup` | bool | `true` | 每服务独立 cgroup v2 子组 |
| `ready_timeout_ms` | int | `5000` | notify 服务等 `READY=1` 的上限 |
| `default_runlevel` | string | `"multi-user"` | `single`/`multi-user`/`graphical` 或 `1`/`3`/`5` |
| `getty` | string | `"/sbin/getty"` | getty 程序 |
| `getty_baud` | string | `"38400"` | VT getty 波特率 |
| `serial_getty_baud` | string | `"115200"` | 串口 getty 波特率 |
| `getty_baud_first` | bool | `true` | `true` 传 `-L <baud> <tty>`（busybox getty）；`false` 传 `-L <tty> <baud>`（util-linux agetty） |
| `getty_tty_basename` | bool | `false` | `true` 时把 `/dev/tty1` 传成 `tty1`；util-linux agetty 会自己加 `/dev/` 前缀，必须开启 |
| `ttys` | array | `tty1`…`tty6` | 要起 getty 的 VT |
| `serial_ttys` | array | `["/dev/ttyS0"]` | 要起 getty 的串口（不存在则跳过） |

配置模板见 `contrib/jiminit.toml`。

## 内核命令行

| 参数 | 作用 |
| --- | --- |
| `jiminit.runlevel=<single\|multi-user\|graphical\|1\|3\|5>` | 指定本次启动的运行级，优先级高于 `default_runlevel`；值非法时告警并回落 |

## 服务文件 `/etc/jiminit/services.d/<name>.toml`

`exec` 必填，其余可省略；未知键使该服务解析失败（启动时告警并跳过）。

| 键 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `exec` | string | 必填 | 可执行文件；含空格时自动拆 argv |
| `args` | array | `[]` | 显式参数，优先于自动拆分 |
| `depends_on` | array | `[]` | 依赖的服务名 |
| `restart` | string | `"on-failure"` | `always`/`on-failure`/`never` |
| `oneshot` | bool | `false` | 跑完不重启；成功退出算完成 |
| `working_dir` | string | `"/"` | 工作目录 |
| `env` | table | `{}` | 额外环境变量 |
| `runlevels` | array | `["multi-user","graphical"]` | 所属运行级 |
| `user` | string | 无（root） | 用户名或 uid |
| `group` | string | 用户主组 | 组名或 gid |
| `supplementary_groups` | array | `[]`（自动收集） | 附加组 |
| `notify` | bool | `false` | 等 `READY=1` 才就绪 |
| `watchdog` | string | 无 | 看门狗周期，如 `"30s"`、`"500ms"` |
| `[limits].memory_max` | string | 无 | 字节或 `K/M/G/T` 后缀，或 `"max"` |
| `[limits].pids_max` | int | 无 | 最大任务数 |
| `log_file` | string | 无 | 该服务 stdout/stderr 追加到此文件（1 MiB 轮转为 `.1`），不再进 init 日志 |
| `[limits].cpu_max` | string | 无 | `"quota period"`，单位微秒 |

看门狗时长单位：`ms`/`s`/`m`/`h`/`d`，裸数字按秒；0 和小数非法。

默认服务输出进 init 日志（stderr、`/dev/kmsg`、`log_file`），行首 `svc/<name>: `；
设置服务级 `log_file` 后改为写该文件，init 日志里只保留 `svc/<name>: started` 这类事件。

## 根设备与内核命令行

| 参数 | 说明 |
| --- | --- |
| `root=/dev/sda2` | 设备路径 |
| `root=UUID=<fs-uuid>` | ext4/btrfs/xfs UUID、FAT/exFAT 卷序列号（`XXXX-XXXX`，大小写不敏感）、LUKS 容器 UUID |
| `root=PARTUUID=<guid>` | GPT 分区唯一 GUID，或 MBR `diskid-01`…`-04` |
| `rootfstype=` | 省略时先让内核探测，再依次试 ext4/xfs/btrfs/vfat |
| `rootflags=` | 原样传给 mount |
| `rw` / `ro` | 默认只读，出现 `rw` 才读写 |
| `lvm=1`（或 `lvm`） | 执行 `lvm vgchange -ay`；`root=/dev/mapper/...` 也会触发 |
| `fsck=no`（或 `fsck=0`） | 跳过根 fsck |
| `console=`、`rdinit=`、`init=` | 内核用；`rdinit=/init` 是 initramfs 入口，`init=/sbin/jiminit` 可跳过 stage1 |

除上述形式外的 `root=`（例如 `LABEL=`）不支持。

## `/etc/crypttab`（initramfs 内）

```
name  device  keyfile  options
```

| 字段 | 说明 |
| --- | --- |
| `name` | mapper 名，出现在 `/dev/mapper/<name>` |
| `device` | `/dev/*`、`UUID=`（LUKS UUID）、`PARTUUID=` |
| `keyfile` | 密钥文件；`none` 或 `-` 表示交互输入 |
| `options` | 逗号分隔；识别 `discard`、`readonly`、`keyslot=N`、`header=PATH`，其余告警忽略 |

每个容器最多尝试 3 次，失败进 rescue。所有条目都会被解锁，没有按 `root=` 过滤。

## `/etc/modules` 与 `/etc/modprobe.d/*.conf`

`/etc/modules` 每行 `模块名 [参数...]`，`#` 注释。按顺序加载，失败即 rescue。

`/etc/modprobe.d/*.conf` 支持：

| 指令 | 行为 |
| --- | --- |
| `options <mod> k=v ...` | 作为默认参数；`/etc/modules` 里的参数优先 |
| `blacklist <mod>` | 只拦截 alias 解析；显式列出的模块仍加载 |
| `install <mod> <command>` | 用 `/bin/sh -c` 执行，替代 modprobe |
| `alias <name> <mod>` | 解析到目标模块 |

外部 `modprobe` 缺失或单条命令超过 30s 视为失败。

## 文件与运行时路径

| 路径 | 用途 |
| --- | --- |
| `/etc/jiminit.toml` | 主配置 |
| `/etc/jiminit/services.d/*.toml` | 服务定义 |
| `/etc/modules`、`/etc/modprobe.d/` | initramfs 阶段模块加载 |
| `/etc/crypttab` | initramfs 阶段 LUKS 解锁 |
| `/etc/fstab` | switch_root 前挂载根以外的条目 |
| `/run/jiminit.sock` | 控制 socket（0600） |
| `/run/jiminit/notify/<name>.sock` | 服务的 NOTIFY_SOCKET（0666） |
| `/sys/fs/cgroup/jiminit/<name>` | 服务的 cgroup v2 子组 |
| `/var/log/jiminit.log`（可配） | 追加日志，滚动为 `.1` |
| `/dev/kmsg` | 内核日志，始终写入 |

## 命令行

操作一律是子命令，选项只做修饰（`--watch`、`--no-color`、`-h`、`-V`）。

| 命令 | 行为 |
| --- | --- |
| `jiminit status [service]` | 打印运行级与每个服务的状态（pid/重启次数/就绪/内存/CPU/pids/io）；给 `service` 只看一个 |
| `jiminit status --watch[=SECS]` | 每 SECS 秒（默认 1）清屏重打一次状态 |
| `jiminit list` | 列出已配置的服务名 |
| `jiminit service <start\|stop\|restart\|reload> <name>` | 控制单个服务 |
| `jiminit runlevel` | 显示当前运行级 |
| `jiminit runlevel <single\|multi-user\|graphical\|1\|3\|5>` | 切换运行级 |
| `jiminit reboot` | 有序重启 |
| `jiminit poweroff` | 有序关机 |
| `jiminit halt` | 有序停止后 halt，不断电 |
| `jiminit soft-reboot` | 停服务、flush 后 exec 重进 stage2，不重启内核 |
| `jiminit kexec` | 走与 reboot 相同的收敛流程，最后以 `RB_KEXEC` 进入内存中已加载的内核；`/sys/kernel/kexec_loaded` 不为 1 时拒绝执行 |
| `jiminit suspend` | 向 `/sys/power/state` 写 `mem`，唤醒后才返回 |
| `jiminit hibernate` | 向 `/sys/power/state` 写 `disk`，需要 swap 与 `resume=` |
| `jiminit metrics` | 以 Prometheus 文本格式输出运行级与服务指标 |
| `jiminit notify-send <socket> <message>` | 发 sd_notify 数据报（服务脚本内部使用） |
| `jiminit check` | 校验配置与服务依赖，打印缩进依赖树与启动顺序 |
| `jiminit graph` | 以 Graphviz DOT 输出服务依赖图（不连接运行中的 init） |
| `--stage1` / `--stage2` | 强制阶段（内部/调试开关，不是子命令） |

客户端连接 `/run/jiminit.sock`，回复以 `error` 开头时退出码为 1，用法错误为 2。
`suspend`/`hibernate` 只有机器唤醒后才收到回复，因此客户端对这两个命令不设读超时。

## 日志格式

每条日志固定为两列：

```text
[   12.345] [  OK  ] pivot_root: switched to the new root
[   12.350] [ INFO ] stage2: starting as pid 1
[   13.002] [ WARN ] svc/failer: 2 consecutive failures; restarting in 4000ms
[   14.100] [ FAIL ] svc/hog: 5 consecutive failures; giving up
```

- 第一列是相对 jiminit 启动的秒数；软重启会重新计时。
- 标签为 `[  OK  ]` / `[ INFO ]` / `[ WARN ]` / `[ FAIL ]`，写入 `/dev/kmsg`
  与日志文件时保持纯文本。
- `color = "auto"` 时只有 stderr/stdout 是终端才输出 ANSI 颜色；`status`
  的 running/stopped/failed 也会按同样规则着色。

日志同时写 stderr、`/dev/kmsg` 和 `log_file`；`/dev/kmsg` 由内核加时间戳，
因此不再重复第一列。

## 环境变量

| 变量 | 作用 |
| --- | --- |
| `JIMINIT_STAGE=1\|2` | 强制阶段 |
| `JIMINIT_SOFT_REBOOTS` | 内部：软重启计数，启动日志会打印 |
| `NOTIFY_SOCKET` | 服务进程可见，sd_notify 用 |
| `WATCHDOG_USEC` | 服务进程可见，看门狗周期（微秒） |

构建脚本另有 `FSCK`、`COLDPLUG`、`LUKS`、`CRYPTAB`、`COLDPLUG_MODULES`、
`FSCK_STUB`、`MODPROBE_STUB`、`MODPROBE_STUB_FAIL`、`TARGET`、`ARCH`、`KERNEL`，
见[构建与镜像](../development/building.md)。
