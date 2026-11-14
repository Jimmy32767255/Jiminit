# Jiminit 文档

Jiminit 是一个用 Rust 写的实验性 GNU/Linux init：单二进制、两个阶段，从 initramfs
接管启动，在真实根上作为 PID 1 管理服务与登录会话。

文档按用途分三层：**使用**（怎么装、怎么配、怎么用）、**开发**（内部怎么工作、
怎么构建和测试）、**参考**（逐项速查、限制与设计取舍）。

项目主页与快速开始：[../README.md](../README.md)。

## 使用

| 文档 | 内容 |
| --- | --- |
| [上手](usage/getting-started.md) | 依赖、构建、在 QEMU 里启动、第一次登录 |
| [配置](usage/configuration.md) | `/etc/jiminit.toml` 的写法与语义 |
| [服务](usage/services.md) | 服务定义、依赖、重启、就绪、看门狗、资源限制 |
| [会话与运行级](usage/sessions-and-runlevels.md) | getty/login、运行级切换、控制 socket、信号与软重启 |
| [安装到真实系统](usage/installation.md) | install.sh、PKGBUILD、mkinitcpio、交叉构建 |

## 开发

| 文档 | 内容 |
| --- | --- |
| [架构](development/architecture.md) | 阶段判定、stage1/stage2 流程、rescue、模块划分 |
| [构建与镜像](development/building.md) | 静态构建、initramfs/rootfs/LUKS/`/usr` 镜像 |
| [测试](development/testing.md) | 单测、四种冒烟测试、stub 注入、调试技巧 |

## 参考

| 文档 | 内容 |
| --- | --- |
| [配置与命令行速查](reference/configuration-reference.md) | 所有配置键、服务字段、内核参数、文件路径、CLI |
| [已知限制](reference/limitations.md) | 按领域列出的当前边界与未实现项 |
| [设计取舍](reference/design-notes.md) | 为什么这样实现，以及已确认的决策 |

## 推荐阅读路径

- 只想跑起来看看：[上手](usage/getting-started.md) → [服务](usage/services.md) 的示例。
- 想在自己的机器上试：[上手指南](usage/getting-started.md) →
  [安装](usage/installation.md) → [限制](reference/limitations.md)。
- 想改代码：[架构](development/architecture.md) → [测试](development/testing.md) →
  [构建](development/building.md)。
