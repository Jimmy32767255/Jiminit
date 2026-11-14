# 配置

主配置是 `/etc/jiminit.toml`。所有键都可省略；缺省值保证不写配置也能启动。
逐键的表格见[配置速查](../reference/configuration-reference.md)，这里讲语义和常见改法。

## 解析规则

- 文件缺失：全部用默认值，正常启动。
- 未知键、类型错误、读取失败：该阶段失败并进入 rescue shell，不会“带病”继续启动。
- rescue shell 自己用 `load_or_default`：配置写坏时也能登录进去修。
- stage1 在挂载真实根之后、切换根之前，会读一次**真实根**上的
  `/etc/jiminit.toml`，只取 `init_path` 用来校验和 exec；其余配置由 stage2 在切换后读取。

## 完整示例

```toml
hostname = "jiminit"
console  = "/dev/console"
shell    = "/bin/sh"
respawn  = true

shutdown_timeout_ms = 3000

init_path     = "/sbin/jiminit"
services_dir  = "/etc/jiminit/services.d"
cgroup        = true
ready_timeout_ms = 5000

log_file      = "/var/log/jiminit.log"

default_runlevel   = "multi-user"
getty              = "/sbin/getty"
getty_baud         = "38400"
serial_getty_baud  = "115200"
getty_baud_first   = true
getty_tty_basename = false
ttys               = ["/dev/tty1", "/dev/tty2", "/dev/tty3", "/dev/tty4", "/dev/tty5", "/dev/tty6"]
serial_ttys        = ["/dev/ttyS0"]
```

## 各组键的语义

### 主机与控制台

- `hostname`：stage2 调 `sethostname`。
- `console`：init 打开这个设备写日志、作为 single 级 root shell 的终端。默认
  `/dev/console`；串口机器上它通常就是 `/dev/ttyS0`。
- `shell`：single 级和 rescue 使用的 shell，必须是绝对路径。

### 会话

- `getty`：登录程序，默认 `/sbin/getty`。jiminit 只负责拉起它，密码校验由它
  exec 的 `login` 完成。
- `ttys` / `serial_ttys`：要起 getty 的终端；不存在的设备会被静默跳过（serial
  默认就只探测 `/dev/ttyS0`）。删掉某个 tty 即可关掉那一路登录。
- `getty_baud` / `serial_getty_baud`：传给 getty 的波特率。
- `getty_baud_first`：参数顺序。busybox getty 用 `-L <baud> <tty>`（默认 `true`），
  util-linux agetty 用 `-L <tty> <baud>`（设 `false`）。
- `getty_tty_basename`：util-linux agetty 会自行在参数前加 `/dev/`，
  所以它需要 `tty1` 而不是 `/dev/tty1`（设 `true`）。

用 util-linux agetty 时两个选项都要改：

```toml
getty = "/sbin/agetty"
getty_baud_first = false
getty_tty_basename = true
```

否则 agetty 会尝试打开 `/dev//dev/tty1`（或把波特率当成设备名），
以状态 1 立即退出。
- `respawn`：single 级 root shell 退出后是否重新拉起。
- `default_runlevel`：启动进入的级别，接受 `single`/`multi-user`/`graphical`
  和 `1`/`3`/`5`。

### 服务

- `services_dir`：服务定义目录，默认 `/etc/jiminit/services.d`。
- `cgroup`：是否把每个服务放进 `/sys/fs/cgroup/jiminit/<name>`；
  设为 `false` 时不做 cgroup 隔离，`[limits]` 也不会生效。
- `ready_timeout_ms`：`notify = true` 的服务等待 `READY=1` 的上限，超时告警后
  按就绪继续（不会永远卡住依赖方）。

### 关机与软重启

- `shutdown_timeout_ms`：关机时给进程的 SIGTERM 宽限期，超时 SIGKILL 并再等 2s。
  交互式 shell 会忽略 SIGTERM，所以别设得太短。
- `log_file`：init 日志的追加文件，`""` 关闭；超过 1 MiB 滚动成 `.1`。

### 启动路径

- `init_path`：stage2 在自己根上的路径，默认 `/sbin/jiminit`。独立 `/usr` 布局改成
  `/usr/sbin/jiminit`，并确认 `/usr` 已由 fstab 挂载，否则 stage1 会给出明确错误。

## 常见改法

```toml
# 只留串口登录，关掉 6 个 VT
ttys = []

# 串口设备换一个
serial_ttys = ["/dev/ttyAMA0"]
serial_getty_baud = "115200"

# 更短的关机等待（嵌入式/容器式使用）
shutdown_timeout_ms = 1000

# 不写日志文件
log_file = ""
```

改完配置后如果是测试镜像，重跑 `./scripts/mk-rootfs.sh`（配置会被复制进去）；
真实安装则是直接编辑 `/etc/jiminit.toml`，下次启动生效。软重启（`kill -USR1 1`）
也会重新读取配置。
