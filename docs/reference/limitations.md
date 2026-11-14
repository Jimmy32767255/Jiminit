# 已知限制

这是当前实现的边界，不是 bug 列表。按领域分组。

## 启动与 initramfs

- 模块只按 `/etc/modules` 显式加载；coldplug 只在启动时扫一次 modalias，
  不处理启动后的 uevent 热插拔、udev 规则和固件加载（`/lib/firmware` 不打包）。
- initramfs 必须自带 `modprobe` 和对应的 `/lib/modules/<kver>`（含 `modules.dep`），
  jiminit 不生成模块依赖。
- 内核 `request_module()` 依赖 `/sbin/modprobe` 存在；最小镜像里没有它时，文件系统
  模块必须在 `/etc/modules` 里显式列出。
- fsck 只检查根文件系统，且依赖 initramfs 自带 `fsck`（没有就跳过并记日志）；
  默认镜像不带，需要 `FSCK=1` 打包 e2fsprogs。只带 ext2/3/4 工具。
- `/usr` 不会自动探测和挂载：必须写进 `/etc/fstab`，否则 `init_path` 指向
  `/usr/...` 时会明确报错。
- 没有实现 initramfs 的 `udev`/`systemd` 兼容层，也没有 resume/hibernate 处理。

## 根设备与存储

- `root=` 只支持 `/dev` 路径、`UUID=`、`PARTUUID=`，不支持 `LABEL=` 和
  `/dev/disk/by-*`。
- MBR `PARTUUID` 只支持主分区（`diskid-01`…`-04`），不支持扩展/逻辑分区。
- GPT 只扫描前 128 个表项；逻辑扇区大小自动探测，4Kn 可用。
- fstab 里不认识的选项按文件系统参数透传；拼错的选项会由内核拒绝并中止启动，
  没有实现 util-linux 的完整选项表。
- fstab 只用于 switch_root 前挂载根以外的条目：不处理 swap、不自动创建挂载点。
- LVM 只做整个卷组的 `vgchange -ay`：不处理 thin/raid/cache 的特殊激活，也不读宿主
  的 lvm.conf（initramfs 里用精简配置，关 udev 同步）。
- LUKS 只支持 plain LUKS1/LUKS2 的 `luks` 类型（没有 tcrypt/bitlk）；除口令和
  密钥文件外没有 keyring、TPM、FIDO 等密钥来源。
- 测试镜像用 pbkdf2/2000 轮是为了在 TCG 下快；Argon2 默认参数在无 KVM 时会很慢。
- 打包 cryptsetup/lvm/modprobe 时按 `ldd` 收库，initramfs 会涨到约 11 MB。

## 会话与运行级

- 不内置认证：没有 PAM，也不解析 `/etc/shadow`，完全依赖外部 `login`
  （测试镜像是 busybox）。替换成 util-linux login/agetty 时应同样可用，但未实测。
- 只有三个固定运行级，没有 SysV 的任意 `rcN.d` 脚本目录，也没有 inittab。
- VT 切换只能靠内核快捷键，jiminit 不处理 `chvt`/键盘。
- 控制 socket 一次只处理一个连接，客户端慢会阻塞主循环最多约 1 秒。

## 服务

- 依赖只有 `depends_on`（类似 Requires），没有 After/Before 的独立顺序控制；
  也没有依赖恢复后自动拉起依赖方的反向传播。
- 依赖失败/手动 stop 会级联停止依赖方，依赖方需要手动 `start` 恢复。
- 健康检查只有 sd_notify 看门狗，没有 `health_cmd` 之类的外部探测；只校验有没有
  收到 `WATCHDOG=1`，不校验 `WATCHDOG_PID`，也不支持 `WATCHDOG=trigger`。
- 资源限制只有 `memory.max`/`pids.max`/`cpu.max`，没有 io.max、cpuset、权重，
  也没有全局默认限制。
- OOM 靠主循环轮询 `memory.events` 发现（不是事件驱动）；cgroup 目录回收后计数归零，
  只用于日志提示，重启决策仍由退出状态和 restart 策略决定。
- 账号只查 `/etc/passwd`、`/etc/group`，不支持 NSS、LDAP、systemd-homed。
- 没有 socket 激活、没有服务模板（`foo@.service`）、没有环境文件（EnvironmentFile）。
- 服务输出只按行转发，单行超过 64 KiB 会被截断；不做日志轮转之外的归档。

## 日志与观测

- 日志文件只保留一份旧的 `.1`，不压缩、不外发 syslog；文件写失败会静默停用该 sink
  （kmsg/console 仍在）。
- `--status` 只报内存，不报 CPU/IO 用量；`restarts` 从 stage2 本次启动算起，
  服务被手动 stop/start 也会计入。
- 没有 metrics 接口，控制 socket 只支持文本请求。

## 软重启

- 软重启只重新执行 stage2：initramfs 已经不在，内核也不会重新引导，所以
  `root=`/initramfs/内核参数相关的改动必须走真正的重启。
- 用 tmpfs 顶替 `/run`、`/tmp`，旧内容若原本在根文件系统上仍会占用空间
  （真重启才回收）。

## 平台与打包

- 只在 x86_64 上验证过，aarch64 有构建路径但没有实测。
- 静态 glibc 在非 x86_64 上可能依赖内核的 static PIE 支持；musl 路径未验证。
- 配置解析失败（未知键/类型错误）会进入 rescue，而不是带默认值继续启动。
- 安装脚本不提供卸载；没有 SELinux/AppArmor 集成。
