# 用户级会话实例

除了作为 PID 1 的系统实例，jiminit 还能为每个登录会话跑一个**用户实例**：它用同一套
Supervisor 监督 `~/.config/jiminit/services.d/*.toml`，适合接管 dinit 的 `userspawn`
与用户级 dbus/pipewire。

## 组成

| 组件 | 作用 |
| --- | --- |
| `jiminit userspawn`（root，PID 1 的服务） | 每秒扫描 `/run/user/<uid>`；出现会话就以该用户身份启动 `jiminit --user`，目录消失时发 SIGTERM；意外退出 3 秒后重试 |
| `jiminit --user`（会话内进程） | 监督本用户的服务；控制 socket 在 `$XDG_RUNTIME_DIR/jiminit/jiminit.sock`（0600）；会话结束时停掉全部服务再退出 |

`/run/user/<uid>` 由 elogind 在登录时创建，所以 `userspawn` 不需要 utmp，也不依赖
dinit。服务文件由系统实例的 `user-sessions.toml` 拉起：

```toml
# /etc/jiminit/services.d/user-sessions.toml
exec = "/sbin/jiminit"
args = ["userspawn"]
depends_on = ["elogind"]
restart = "always"
log_file = "/var/log/jiminit/user-sessions.log"
```

## 配置位置

| 路径 | 内容 |
| --- | --- |
| `~/.config/jiminit.toml` | 可选；只用到 `services_dir`、`ready_timeout_ms`、`log_level`、`color` |
| `~/.config/jiminit/services.d/*.toml` | 用户服务定义，字段与系统服务相同 |
| `$XDG_RUNTIME_DIR/jiminit/jiminit.sock` | 控制 socket |
| `$XDG_RUNTIME_DIR/jiminit/notify/*.sock` | 服务的 sd_notify 套接字 |

用户实例**不**接管控制台、getty、hostname、运行级与电源，也不启用 cgroup：`status`
里的 `memory`/`cpu`/`pids`/`io` 显示 `-`，`[limits]` 会被忽略。日志写到自己的
stderr（由 `user-sessions` 服务的 `log_file` 收集）；`log_file` 键在用户实例里不生效，
避免非 root 写 `/var/log`。

## 命令行

```sh
jiminit --user                 # 启动当前会话的实例（通常由 userspawn 代劳）
jiminit --user status          # 会话内服务状态
jiminit --user status sddm     # 单个服务
jiminit --user list
jiminit --user metrics
jiminit --user service restart pipewire
jiminit --user runlevel        # 固定回报 graphical
jiminit --user stop            # 停止整个用户实例
```

`reboot`/`poweroff`/`kexec`/`suspend` 等在用户实例里会被拒绝（退出码 2），
`runlevel <name>` 也不能切换。

## 服务示例

会话 bus 与音频栈（与 dinit 的 `/etc/dinit.d/user/*` 对应）：

```toml
# ~/.config/jiminit/services.d/dbus.toml
exec = "/bin/sh"
args = ["-c", "exec dbus-daemon --session --address=\"unix:path=$XDG_RUNTIME_DIR/bus\" --nofork --nopidfile"]
restart = "always"
```

```toml
# ~/.config/jiminit/services.d/pipewire.toml
exec = "/usr/bin/pipewire"
depends_on = ["dbus"]
restart = "always"

# pipewire-pulse.toml / wireplumber.toml 同理，depends_on = ["pipewire"]
```

服务继承 `jiminit --user` 的会话环境（`XDG_RUNTIME_DIR`、`DBUS_SESSION_BUS_ADDRESS`、
`DISPLAY`/`WAYLAND_DISPLAY` 等），不要在服务里写 `user = ...`。dinit 的
`ready-notification = pipevar:...` 没有对应字段：jiminit 以 exec 成功为就绪，
`notify = true` + `READY=1` 的 sd_notify 方式仍可用。

完整模板见 `contrib/example/user/services/`。

## 从 dinit 迁移

1. 把 `/etc/dinit.d/user/*` 翻译成 `~/.config/jiminit/services.d/*.toml`。
2. 删除 `/etc/jiminit/services.d/userspawn.toml`（dinit 的 userspawn）。
3. 新增 `/etc/jiminit/services.d/user-sessions.toml`（见上）。
4. 下次用 jiminit 启动时生效；Artix/dinit 的默认启动项不受影响，仍可回滚。

退出登录时 `/run/user/<uid>` 消失，`userspawn` 会停掉该用户实例；服务进程在独立
session 里启动，不受 `userspawn` 重启影响。
