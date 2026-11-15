# 服务

jiminit 的服务监督器读取 `/etc/jiminit/services.d/*.toml`，每个文件一个服务，
文件名去掉 `.toml` 就是服务名。它负责依赖顺序、重启、就绪通知、看门狗和资源限制；
不做日志轮转、不做 socket 激活，也不解析 systemd unit。

## 最小示例

```toml
# /etc/jiminit/services.d/ticker.toml
exec = "/bin/sh"
args = ["-c", "while true; do echo tick; sleep 1; done"]
restart = "always"
```

`exec` 是唯一的必填键。服务输出会进内核日志，行首带 `svc/ticker: `：

```text
svc/ticker: tick
```

## 完整字段

```toml
exec = "/usr/bin/mydaemon"      # 必填；带空格的一行会自动拆成 argv
args = ["--foreground"]         # 显式参数，优先于自动拆分
depends_on = ["network"]        # 依赖的服务名
restart = "on-failure"          # always | on-failure | never
oneshot = false                 # true：跑完就结束，不重启
working_dir = "/"               # 默认 /
env = { RUST_LOG = "info" }     # 额外环境变量
runlevels = ["multi-user"]      # 默认 ["multi-user", "graphical"]
user = "nobody"                 # 名字或数字 uid
group = "nogroup"               # 名字或数字 gid
supplementary_groups = []       # 留空则用 /etc/group 里该用户的组
notify = false                  # 等 READY=1 才算就绪
watchdog = "30s"                # 每 30s 要发一次 WATCHDOG=1

[limits]
memory_max = "64M"
pids_max = 64
cpu_max = "50000 100000"
```

`exec` 写成一行时支持单引号、双引号和反斜杠转义，但不做变量展开；如果参数复杂，
直接用 `args`。未知键会让这个服务解析失败：启动时告警并跳过，不影响其他服务。

## 依赖与启动

- 按 `depends_on` 建图，无依赖的服务并行启动。
- 依赖项必须**就绪**后依赖方才会启动：普通服务以 fork+exec 成功为就绪；
  `oneshot` 以成功退出为就绪；`notify = true` 要等 `READY=1`。
- 依赖不存在或成环：该服务被跳过并告警。
- 依赖项最终失败时（启动失败、连续重启放弃、`restart = "never"` 退出、手动 stop），
  尚未启动的依赖方标记失败并记
  `svc/x: not started: dependency 'y' failed`；已经运行的依赖方收到 SIGTERM 级联停止。
  依赖恢复后不会自动把停掉的依赖方拉回来，需要手动 `jiminit service start x`。

## 重启策略

| 值 | 行为 |
| --- | --- |
| `always` | 退出就重启，无论退出码 |
| `on-failure`（默认） | 非 0 退出才重启 |
| `never` | 不重启；退出即视为最终失败 |

重启从 1s 开始指数退避：1s、2s、4s……上限 60s。连续 5 次失败后放弃并告警。
连续运行满 10s 会重置失败计数。`oneshot = true` 的服务不适用重启策略：
成功退出算完成，非 0 退出算失败。

## 就绪通知（notify）

```toml
exec = "/usr/bin/ready-daemon"
notify = true
```

设置了 `notify = true` 的服务会从环境里拿到 `NOTIFY_SOCKET`
（`/run/jiminit/notify/<name>.sock`，权限 0666），在准备好的时候发一条
`READY=1`。在此之前依赖它的服务不会启动；超过 `ready_timeout_ms` 还没收到，
jiminit 记一条告警并按就绪继续，避免整条依赖链死等。

`STATUS=...` 会被转发到日志。脚本里可以直接用 jiminit 自己的客户端：

```sh
jiminit notify-send "$NOTIFY_SOCKET" READY=1
```

看门狗复用同一个 socket：

```toml
watchdog = "30s"
```

服务必须每个周期内至少发一次 `WATCHDOG=1`；环境里同时提供 `WATCHDOG_USEC`。
超时后 jiminit 记 `watchdog timeout`、SIGKILL 该服务，然后照常交给重启策略处理。

## 运行身份

```toml
user = "nobody"
group = "nogroup"
```

只解析 `/etc/passwd` 和 `/etc/group`（不查 NSS/LDAP）。启动前依次
`setgroups`、`setgid`、`setuid`，并把 `HOME`、`USER`、`LOGNAME` 改成目标用户。
`supplementary_groups` 留空时自动收集该用户在 `/etc/group` 里的组。

## 资源限制

```toml
[limits]
memory_max = "64M"          # 字节，或带 K/M/G/T 后缀，或 "max"
pids_max = 64
cpu_max = "50000 100000"    # quota period，微秒
```

每项都可省略，默认不限制。只有真有服务设置某一项时，才启用对应的 cgroup v2
控制器（`+memory`、`+pids`、`+cpu`），所以最小系统不会因为 cgroup 开销变形。
写入失败只告警，不会拒绝启动服务。

内存超限被内核 OOM 杀掉时，jiminit 从 `memory.events` 读出计数并记
`svc/x: OOM-killed (N total)`，之后仍按重启策略处理；`restart = "never"` 就不会再起来。

## 日志与 cgroup

- 服务在独立 session/进程组里，stdin 是 `/dev/null`；stdout/stderr 经管道回到主循环，
  每行加 `svc/<name>: ` 前缀写入 `/dev/kmsg`、console 和配置的日志文件。
- `cgroup = true`（默认）时每个服务在 `/sys/fs/cgroup/jiminit/<name>` 下，
  关机超时会用 `cgroup.kill` 连脱离进程组的孙进程一起清理。
- 服务退出、目录为空后，下一轮 `tick` 会尝试删除该目录；如果还有逃逸进程，删除失败、
  目录保留，`status` 里对应服务显示为 stopped。

## 控制与状态

运行中的 init 通过 `/run/jiminit.sock`（权限 0600）接受以下命令：

```sh
jiminit status                    # 运行级 + 每服务一行
jiminit service start ticker
jiminit service stop ticker
jiminit service restart ticker
jiminit service reload ticker     # 发 SIGHUP
```

手动 stop 不会被重启策略覆盖；`restart` 会等旧进程退出后再拉起。
`stop` 也会级联停止依赖它的服务。

## 关机顺序

SIGHUP 结束 getty/单用户 shell → 按启动逆序给服务发 SIGTERM → 向所有剩余进程发
SIGTERM → 等 `shutdown_timeout_ms` → 仍存活则 SIGKILL 再等 2s → `sync` →
根重新挂载为只读 → `reboot(2)`。

## 例子

**oneshot 初始化任务**

```toml
exec = "/bin/sh"
args = ["-c", "echo boot-time=$(date +%s) > /run/boot-time"]
oneshot = true
runlevels = ["single", "multi-user", "graphical"]
```

**先起数据库再起应用**

```toml
# db.toml
exec = "/usr/bin/mydb"
notify = true

# app.toml
exec = "/usr/bin/myapp"
depends_on = ["db"]
restart = "always"
```
