# 安装和运行模式

[文档目录](README.md) · [English](../en/install.md)

## 软件包和二进制

| 类型 | 获取 | 用途 |
|---|---|---|
| Debian 包 | `make deb`（见[构建](#构建)） | 装在 PXVIRT / PVE 节点上：tty1 控制台，带 SDL 图形控制台 |
| Linux 静态二进制（musl） | [Releases](https://github.com/jiangcuo/pxvirt-console/releases)：x86_64、aarch64、riscv64、loongarch64 | 作为客户端使用 `--ssh` / `--remote`，或在节点上免安装直接运行 |
| macOS（arm64、x86_64） | Releases 里的 zip | 作为客户端使用 `--ssh` / `--remote` |

macOS 版本没有经过苹果公证，解压后先去掉隔离标记：

```bash
xattr -d com.apple.quarantine pxvirt-console
```

## 运行模式

| 模式 | 启动方式 | 可用功能 |
|---|---|---|
| 控制台 | tty1 上的 `pxvirt-console.service` | 全部功能。替换 tty1 登录界面；不能退出，只能注销（空闲超时也会自动注销） |
| 本机 | 在节点的终端里运行 `sudo pxvirt-console` | 全部功能；自动以 `root@pam` 登录 |
| SSH | `pxvirt-console --ssh root@节点[:端口]` | 全部功能。一条 SSH 主连接同时承载 API（端口转发）、主机命令、Shell、控制台和实时监控；SSH 用户是 root 时自动以 `root@pam` 登录 |
| API | `pxvirt-console --remote 节点[:端口]` | 只用 PVE API；首次连接确认证书指纹；需要主机命令的页面（IO、RAID、连接等）会隐藏 |

macOS 等非 Linux 系统上只能用 `--ssh` 和 `--remote`。

安装 Debian 包后启用 tty1 控制台：

```bash
systemctl enable --now pxvirt-console.service
```

### 命令行参数

| 参数 | 说明 |
|---|---|
| `--ssh [用户@]主机[:端口]` | SSH 模式 |
| `--remote 主机[:端口]` | API 模式 |
| `--lang en\|zh\|auto` | 界面语言（tty1 固定为英文） |
| `--version` | 显示版本 |

## 配置

配置文件是 `/etc/pxvirt-console/config.toml`；非 root 用户用 `~/.config/pxvirt-console/config.toml`，已确认的证书指纹也存在这个目录。
示例：[`debian/config.toml.example`](../../debian/config.toml.example)。

| 配置项 | 默认值 | 说明 |
|---|---|---|
| `idle_timeout_secs` | 900 | 无操作多少秒后自动注销 |
| `lang` | `auto` | `auto`、`en`、`zh` |
| `refresh_secs` | 5 | 页面自动刷新的最短间隔 |
| `prelogin_dashboard` | true | 登录前显示主机名、管理地址和实时指标 |
| `default_realm` | `pam` | 登录界面默认的认证域 |
| `console_display` | `auto` | 图形控制台：`auto`（tty1 用 SDL，其他用终端图形）、`sdl`、`terminal` |
| `console_fps` | 10 | 终端图形控制台的帧率上限 |
| `ssh_user` | `root` | SSH 模式下执行主机命令的用户 |

## 推荐终端

推荐 [Ghostty](https://ghostty.org)（Linux 和 macOS）：支持 Kitty 图形协议和像素级鼠标定位，图形控制台和画面预览都很清晰。
Kitty、WezTerm 和支持 Sixel 的终端也能用；其他终端退回半块字符显示，不显示画面预览。

## 构建

```bash
cargo build --release                       # 需要 Rust 和 libsdl2-dev
make deb                                    # 构建 Debian 包
```

不含 SDL 的静态构建，和 CI 一样（`pxvirt/musl` 镜像，链接器配置在 `.cargo/config.toml`）：

```bash
cargo build --release --no-default-features --target x86_64-unknown-linux-musl
```

GitHub Actions 每次推送都会构建（Linux 4 个架构、macOS 2 个）；打 `v*` 标签时自动发布 Release，附带二进制和 What's Changed 列表。
