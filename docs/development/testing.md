# 测试

分两层：Rust 单元测试（解析器、退避、依赖闭包、配置校验等）和 QEMU 端到端冒烟。

## 单元测试

```sh
cargo test
```

当前 77 个用例。覆盖：cmdline、配置严格解析、fstab 解析与挂载点规范化、UUID/GPT/MBR（含 FAT/exFAT 短 UUID）、
LUKS 头探测、crypttab、modprobe.d 规则、coldplug alias 收集、运行级、服务解析与
引号拆分、依赖闭包、退避上限、看门狗时长、cgroup 限制校验、字节格式化、软重启计数。

## 冒烟测试

四个脚本都自己构建镜像、后台启动 QEMU、通过串口 FIFO 注入登录和命令、grep 日志断言，
最后打印 `PASS: ...` / `FAIL: ...`；QEMU 退出码也会检查。日志分别在 `out/` 下。

| 脚本 | 覆盖 | 额外要求 |
| --- | --- | --- |
| `smoke-test.sh` | 完整主路径：模块（stub）、fsck（stub）、fstab、pivot_root、getty/login、运行级切换、服务、cgroup、限制、OOM、notify、看门狗、依赖级联、状态、软重启、关机 | 无 |
| `smoke-real-test.sh` | 真实 coldplug（扫描 modalias）+ 打包的真实 `e2fsck` | `FSCK=1 COLDPLUG=1` 由脚本自己设置 |
| `smoke-usr-test.sh` | 独立 `/usr` 由 fstab 挂载、pivot_root、PID 1 从 `/usr/sbin/jiminit` 运行 | 脚本自己生成两块盘 |
| `smoke-luks-test.sh` | crypttab 解锁 LUKS2 → `lvm vgchange -ay` → 登录 → 关机 | 先跑 `mk-luks-image.sh`（需要 root） |

主冒烟一轮约 1–2 分钟（无 KVM，TCG）。`smoke-test.sh` 的断言包括：

```text
pivot_root: switched to the new root
stub-modprobe: virtio_blk / foo irq=5 / blacklisted_mod
alias blocked_alias -> hidden_mod is blacklisted
stub-modprobe: foo irq=9 timeout=2
stub-install: baz
stub-fsck: -p /dev/vda
fstab: mounted tmpfs on /newroot/data
stage2: starting as pid 1
status multi-user
switching runlevel multi-user -> single
now in runlevel single
SINGLE-SHELL-OK
switching runlevel single -> multi-user
srw-------                       # 控制 socket 权限
svc/ticker: tick
svc/failer: restarting
SIGUSR1: soft reboot
soft reboot: fresh tmpfs on /run|/tmp
svc/cg: 0::/jiminit/cg
svc/hog: OOM-killed
svc/notifier: ready
svc/watcher: watchdog every 2000 ms
svc/watchdog-dead: watchdog timeout
not started: dependency ...      # 依赖失败不启动
stopping because dependency ...  # 手动 stop 级联
reboot: Power down
```

以及若干“不应出现”的断言（例如 `BROKEN-CHILD-RAN`、`CHILD2-RAN`、
`svc/watcher: watchdog timeout`）。

## 测试镜像里的 stub

`mk-initramfs.sh` 支持注入假工具，避免为了测一条路径而打包整套工具链：

- `FSCK_STUB=1`：`/sbin/fsck` 是打印参数并返回 0 的脚本；
- `MODPROBE_STUB=1`：假 `modprobe` + 一份固定的 `/etc/modules`、`/etc/modprobe.d/test.conf`
  （包含 blacklist、alias、options、install）；
- `FAT=1`：打包真实的 `fat`/`vfat` 模块与 kmod；配合 `MODPROBE_STUB=1` 时，假 modprobe
  只把 `vfat`/`fat`/`msdos` 交给真 kmod，其余仍走 stub；
- `MODPROBE_STUB_FAIL=1`（配合上一条）让每次 modprobe 返回 1，用于验证 fail-fast 进
  rescue。手工这样启动时日志会看到
  `Initramfs failed: load kernel module virtio_blk: ...` 和 `starting rescue shell`。

## 冒烟脚本怎么工作

1. 调 `build.sh`、`mk-initramfs.sh`、`mk-rootfs.sh`（或对应镜像脚本）；
2. `mkfifo out/smoke.fifo`，先 `exec 9<>"$FIFO"` 保持读写端，再让 QEMU 的 stdin 接这个
   FIFO——如果 QEMU 先启动，open 会阻塞，早期输出会丢；
3. QEMU 后台跑，`-nographic -no-reboot`，日志重定向到文件；
4. 轮询日志等 `login:` / `Password:`，按次数分别登录（login 会清掉提前输入的密码，
   所以每次都要重新等提示）；
5. 通过 FIFO 发命令，轮询日志等回显，再 grep 断言；
6. 用 `kill -TERM 1` 触发关机，等 QEMU 退出并检查退出码。

## 调试技巧

- **改了代码先重跑镜像脚本**：stage1 改动重跑 `mk-initramfs.sh`，stage2 改动重跑
  `mk-rootfs.sh`；只跑一个会看到旧二进制。
- 交互调试：

  ```sh
  ./scripts/build.sh && ./scripts/mk-initramfs.sh && ./scripts/mk-rootfs.sh
  mkfifo out/console.in
  exec 3<>out/console.in
  ./scripts/run-qemu.sh <&3
  # 另一个 shell：printf 'root\n' > out/console.in
  ```

  或者直接用 `run-qemu.sh`，在终端里登录。
- 看内核/init 日志：`out/smoke.log` 同时包含内核启动日志和 `jiminit:` 行；服务行带
  `svc/<name>:` 前缀。
- 杀 QEMU 用具体 pid，不要 `pkill -f qemu-system-...`：模式会匹配到执行 pkill 的
  shell 自己的命令行，把 shell 一起杀掉。用
  `ps -eo pid,args | grep '[q]emu-system-x86_64'` 找。
- 冒烟失败时脚本会打印最后若干行日志；完整日志在 `out/smoke.log`（对应脚本的
  `LOG=` 变量）。断言失败信息里的模式是普通字符串，不要用反引号包 `$pattern`
  （会被 shell 当命令执行，曾经踩过）。
