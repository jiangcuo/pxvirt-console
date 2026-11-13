# pxvirt-console 设计方案（草案 v0.1）

> 目标：做一个对齐 VMware ESXi DCUI、但功能远超它的本地/远程运维终端。
> 能在 tty1 上**直接运维、直接操作**，在 Web UI 不可用、只有键盘显示器时也能救场；
> 日常也可以替代一部分简单的图形界面操作。不是展示品。

---

## 0. 现状与探测结论

### 0.1 现有代码（git 中仅剩 init + add 两个提交）

| 文件 | 内容 |
|---|---|
| `src/main.rs` | crossterm + ratatui 主循环，100ms 轮询，`q` 登出，进入 bash 子 shell |
| `src/app.rs` | 单一 `Screen` 枚举状态机；root 登录（pwcheck）、失败锁定、改 root 密码、管理口网络重置（备份/回滚 `/etc/network/interfaces`）、ping、重启 5 个固定服务 |
| `src/ui.rs` | 左 30% 菜单 / 右 70% 内容，弹窗、确认框 |
| `debian/` | systemd 服务占用 tty1（`Conflicts=getty@tty1`） |

现有能力全部保留，作为新架构里的“救援功能”迁移进去（见 §6）。

### 0.2 本机 PVE 探测（2026-11-13）

- PVE 9.2.26 / Debian 13 trixie / x86_64，单节点 `lucas-pc`，未配置 ZFS 池和 Ceph（命令存在，API 返回错误）。
- `perl -MPVE::AccessControl -e 'print assemble_ticket("root\@pam")'` 生成的票据可直接以 `Cookie: PVEAuthCookie=...` 调用 `https://127.0.0.1:8006/api2/json/*`，已验证 `version / services / disks/list / disks/lvm / network / rrddata / apt/update / hardware/pci / certificates/info / subscription`。
- 写操作（POST/PUT/DELETE）还需要 `CSRFPreventionToken` 头，可用 `PVE::AccessControl::assemble_csrf_prevention_token("root\@pam")` 生成（已验证可用）。
- `/nodes/{node}/journal` 返回 **gzip 压缩**的响应，HTTP 客户端需要支持解压。
- `ZFS/Ceph` 未配置时 API 返回 `data:null + message`，UI 需要优雅显示“未配置”，而不是报错。
- `/var/run/qemu-server/` 普通用户无权访问（控制台以 root 运行，无影响）。
- tty1 当前尺寸 240×67（4K framebuffer）；但服务器常见 VGA 控制台是 **80×25**，布局必须能降级。
- `/dev/dri/card0` 存在，`libsdl2-2.0-0` 已安装 → SDL2 的 KMSDRM 后端可行（见 §7）。

### 0.3 两个硬约束

1. **Linux VT（tty1）字体不能显示中文**（内核控制台只有 256/512 个字形）。✅ 已确认：tty1 上用英文；
   通过 SSH/终端模拟器使用时可以切中文。→ 需要 i18n，检测到 `TERM=linux` 时强制 English。
2. **pveproxy 挂了恰恰是最需要控制台的时候**。所以凡是“救援”类功能不能依赖 8006 API，必须有本地实现。

---

## 1. 总体架构

```
┌──────────────────────────── UI (ratatui, 同步主循环) ─────────────────────────────┐
│  TopBar(模块)  SubMenu  Columns(列表/详情/操作)  StatusBar(实时监控)  Popup/Task  │
└──────────────┬───────────────────────────────────────────────────────▲────────────┘
               │ Command (异步请求)                                     │ Event (结果)
┌──────────────▼───────────────────────────────────────────────────────┴────────────┐
│                         Worker 层（tokio runtime / 线程池）                          │
├───────────────────┬──────────────────────┬─────────────────────┬──────────────────┤
│  PveApi           │  HostExec            │  Metrics            │  TaskWatcher     │
│  HTTP 8006 客户端 │  Local / Ssh 两种实现│  /proc 采样或 API   │  UPID 跟踪       │
│  ticket+CSRF      │  journalctl/systemctl│  1s 采样 + 环形缓冲 │  实时任务日志    │
│  fallback: pvesh  │  zpool/lvs/lsblk/ip  │                     │                  │
└───────────────────┴──────────────────────┴─────────────────────┴──────────────────┘
```

### 1.1 数据源分工

| 来源 | 用途 | 说明 |
|---|---|---|
| **PVE API** (`PveApi`) | PVE 模块全部、节点服务/日志/磁盘/网络/apt/电源 | 天然支持集群：请求 `/nodes/<任意节点>/...` 由 pveproxy 代理，**切换节点视角只需要换 node 参数** |
| **pvesh**（本地 fallback） | pveproxy 宕机时的只读 + 有限写 | `pvesh get /nodes/x/... --output-format json` 进程内调用 API，不经过 pveproxy（仍依赖 pmxcfs） |
| **HostExec::Local** | API 不覆盖或需要更细的：journal 搜索(`journalctl -g`)、`systemctl enable/disable`、`zpool status`、`zfs list`、`lsblk`、`ip -s link`、进程、救援网络重置 | 只有 root@pam 会话可用 |
| **HostExec::Ssh** | 远程模式下执行上面这些命令 | 可选，见 §1.4 |
| **Metrics** | 底部状态栏 | 本地读 `/proc`；远程节点读 API |

原则：**能用 API 的就用 API**（权限、审计、集群代理都免费获得），API 做不到或救援场景才用本地命令。

### 1.2 登录与权限

“使用就是 pxvirt 的用户”——建议：

```
┌ Login ────────────────────────┐
│ User : root                   │
│ Realm: [pam] pve ldap ...     │  ← realm 列表来自 GET /access/domains（免认证）
│ Pass : ********               │
└───────────────────────────────┘
```

1. **正常路径**：`POST /access/ticket`（用户名/密码/realm，含 TFA 时提示输入 OTP）→ 拿到用户自己的 ticket + CSRF。
   之后所有 API 操作**按该用户的 PVE ACL 生效**，界面根据 `GET /access/permissions` 隐藏/禁用无权限的操作。
2. **救援路径**：8006 连不上时，只允许 `root@pam`，用现有 `pwcheck` 验证本机密码 → 进入“救援模式”（顶部红色 `RESCUE` 标记），只开放本地功能 + pvesh。
3. **perl 生成票据**的用途：root@pam 已通过 pwcheck 验证、需要续期（ticket 2 小时过期）或 pveproxy 重启后恢复会话时，免再输密码；
   以及将来的“无人值守看板模式”。非 root 用户**不**使用伪造票据，避免越权。
4. 本地命令和进入 Shell 的权限见下面 1.2.1。

#### 1.2.1 多用户与权限模型（已讨论，2026-11-13）

结论：**不自建用户体系，PVE 的用户 + ACL 就是唯一的权限来源**。我们是 PVE 控制台，不做通用 Linux 多用户。

把“谁在跑进程”和“谁在操作”分开，这两件事互相独立：

| 身份 | 是谁 | 决定什么 |
|---|---|---|
| **进程身份** | tty1 上由 systemd 以 root 启动；SSH 下是执行命令的那个 Unix 用户 | **能不能**执行本地命令（读 journal、systemctl、zpool、改网络） |
| **操作者身份** | 登录界面输入的 PVE 用户（`root@pam`、`alice@pve`、LDAP 用户…） | **允不允许**执行某个操作 |

做法和 pvedaemon 一样：进程以 root 执行，执行前先按操作者的 PVE 权限检查。
登录后调用一次 `GET /access/permissions?path=/nodes/<node>`（以及 `/vms`、`/storage`）缓存权限，切节点时重新获取。

本地功能 → PVE 权限映射（权限名取自本机 `pveum role list`）：

| 本地功能 | 需要的 PVE 权限（路径 `/nodes/<node>`） | 说明 |
|---|---|---|
| 底部监控、系统概览、进程列表（只读）、磁盘/LVM/ZFS/Ceph 查看、网卡查看 | `Sys.Audit` | `/proc` 本身所有用户都能读，监控**不需要 root**；限制只是为了和 Web UI 一致 |
| 日志查看（journal、dmesg）、任务日志 | `Sys.Syslog` | 和 Web UI 的 Syslog 页面相同 |
| 服务 start/stop/restart、改网络、改 DNS/时间、apt 升级、ZFS scrub、kill 进程 | `Sys.Modify` | |
| 节点重启/关机 | `Sys.PowerMgmt` | |
| VM/CT 操作、存储操作 | 直接调 API，由 PVE 自己检查 `VM.*`、`Datastore.*` | 不需要我们再判断 |
| 进入 Shell、改 root 密码、救援网络重置、`systemctl enable/disable`、擦盘 | **只允许 `root@pam`** | Web UI 里非 root 用户开的 Shell 也要再输一次 Linux 登录密码，所以我们直接只给 root |

界面上没权限的操作**显示但灰掉**，右栏注明缺少哪个权限（例如 `needs Sys.Modify`），而不是直接隐藏，方便管理员排查。

内置角色登录后看到的效果：

| 角色 | 能做什么 |
|---|---|
| `PVEAuditor` | 只读：监控、概览、磁盘、VM 列表（看不到日志，因为没有 `Sys.Syslog`） |
| `PVESysAdmin` | 只读 + 看日志 |
| `Administrator`（非 root） | 除“仅 root@pam”那一行之外全部 |
| `root@pam` | 全部，包括 Shell 和救援功能 |

三种运行场景：

| 场景 | 进程身份 | 结果 |
|---|---|---|
| tty1（systemd） | root | 完整功能，按 PVE 权限控制 |
| SSH 以 root 登录后运行，或 `sudo pxvirt-console` | root | 同上，同样要求 PVE 用户登录（可配置为：Unix root 直接视为 `root@pam`，免二次登录） |
| SSH 以普通 Unix 用户运行 | 非 root | 自动进入 **API-only 模式**：只用 API，journal 正则搜索、zpool status 等本地功能灰显；`/proc` 监控仍可用 |
| 远程模式（连别的 PVE） | 任意 | 只用 API（有 SSH 配置时补齐本地功能） |

其他规则：

- **登录前画面**：tty1 上未登录也显示主机名、管理 IP、`https://IP:8006`、版本和底部监控（只读，类似 ESXi DCUI）。能接触到物理控制台的人本来就能看到这些，风险可以接受；可配置关闭。
- **救援模式**（pveproxy 不可用）：只允许 `root@pam` + pwcheck，这时拿不到 ACL，也不需要。
- **审计**：所有写操作写入 journal（`SYSLOG_IDENTIFIER=pxvirt-console`，记录操作者、节点、操作、结果）。API 操作 PVE 自己会记任务日志；本地命令靠这条补齐。
- **伪造票据**：只给 `root@pam` 用。非 root 用户永远走 `/access/ticket`，不会让控制台变成提权的跳板。

### 1.3 节点视角（Context）

```
Context = { connection: Local | Remote(host), user, node: "lucas-pc" }
```

- 顶栏右侧常驻：`root@pam │ cluster: pxc1 │ node: [lucas-pc ▾]`
- 快捷键 `F2`（或 `Ctrl-N`）弹出节点选择（来源 `GET /cluster/status`，显示 online/IP/本机标记）。
- 切换后，所有“节点级”页面（系统/网络/磁盘/服务日志）用 API 重新拉取目标节点数据；
  依赖本地命令的功能对非本机节点：有 SSH 则走 SSH，否则灰显并提示 “local only”。
- PVE 模块本身是集群级的，不受影响（但 VM 列表默认高亮当前节点）。

### 1.4 远程模式

两层，按需要逐步做：

| 方式 | 能力 | 实现 |
|---|---|---|
| **远程 API**（推荐优先） | 连接任意 PVE：`pxvirt-console --remote 10.0.0.5` | 直连 `https://host:8006`，用户名密码登录；TLS 证书首次连接显示指纹确认（TOFU），存 `/etc/pxvirt-console/known_hosts` |
| **SSH** | 补齐 API 不覆盖的本地命令（journal 正则搜索、zfs list、进程、/proc 实时监控） | 调系统 `ssh`（ControlMaster 复用连接、仅密钥认证）执行命令；也可 `ssh -L` 转发 8006，适合 8006 不对外开放的环境 |

说明：同一集群内**不需要 SSH**就能管理所有节点（API 代理），SSH 只为“更深的主机级运维”或“8006 不可达”。

### 1.5 异步模型

- UI 主循环保持同步（现在的 100ms 轮询），所有 IO 进 worker，结果通过 channel 回 UI。
- 每个视图有 `Loading / Ready(data, fetched_at) / Error(msg)` 三态，数据带 TTL 缓存；`F5`/`r` 强制刷新。
- 现在每个功能各写一个 `mpsc::Receiver` 字段的方式不可扩展，统一成 `Request{id, kind}` → `Response{id, result}`。

---

## 2. 界面布局

### 2.1 总体（≥ 120 列）

```
 PXVIRT 9.2.26  lucas-pc                                   root@pam │ node:[lucas-pc ▾] │ 14:32:10
┌─[1 System]──[2 Network]──[3 Disks]──[4 Services & Logs]──[5 PVE]──────────────────────────────┐
│ Overview  Time/NTP  DNS  Updates  Processes  Users  Certificates  Power                         │  ← 子菜单
├──────────────┬──────────────────────────────────────────────┬──────────────────────────────────┤
│ 列1 导航/对象 │ 列2 主列表 / 详情                            │ 列3 详情 / 可用操作 / 帮助       │
│              │                                              │                                  │
│              │                                              │  [Enter] Restart                 │
│              │                                              │  [s] Stop  [e] Enable            │
├──────────────┴──────────────────────────────────────────────┴──────────────────────────────────┤
│ CPU ▂▃▅▇▅ 23% │ Load 0.52 0.48 0.40 │ Mem 13.3/15.3G │ Net ↓1.2M ↑340K │ Disk R 4M W 12M │ 812 thr │  ← 实时状态
│ ←→ column  ↑↓ select  Enter open  / search  F2 node  F5 refresh  ? help  F10 shell  q logout     │  ← 按键提示
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
```

- **顶部**：1 行标题 + 1 行模块 Tab + 1 行子菜单。
- **中间**：按宽度自适应栏数（见 2.3）。三栏语义固定：**导航 → 列表 → 详情/操作**，类似 WebBIOS 左侧树 + 右侧属性。
  第 4 栏（≥200 列时）可选：实时图表或日志尾随（例如在服务页右侧直接 tail 选中服务的日志）。
- **底部**：状态栏 5 格 + 按键提示 1 行；高度富余时状态栏扩展为 3 行迷你曲线（sparkline）。

### 2.2 底部状态栏（5 格）

| 格 | 内容 | 本地来源 | 远程节点来源 |
|---|---|---|---|
| CPU/Load | CPU% + sparkline，loadavg 1/5/15 | `/proc/stat` `/proc/loadavg` | `GET /nodes/x/status` |
| 内存 | used/total、swap、KSM/ARC（若有 ZFS） | `/proc/meminfo` `/proc/spl/kstat/zfs/arcstats` | 同上 |
| 网络 | 总/指定口 ↓↑ 速率 | `/proc/net/dev` 差分 | `rrddata` (分钟平均，标注 ~1m) |
| 磁盘 IO | 读写 B/s、IOPS | `/proc/diskstats` 差分 | `rrddata` |
| 进程/集群 | 进程数/线程数、VM 运行数、集群 quorum 状态 | `/proc/loadavg` 第 4 列 + API | API |

本地 1s 采样，保留 120 个点的环形缓冲，供 sparkline 和“系统-概览”大图使用。

### 2.3 尺寸降级

| 终端宽度 | 中间栏 | 说明 |
|---|---|---|
| ≥ 200 | 4 栏 | 多出实时图表/日志 |
| 120–199 | 3 栏 | 标准 |
| 90–119 | 2 栏 | 导航栏收进子菜单，列表 + 详情 |
| < 90（如 80×25） | 1 栏 | 下钻式：Enter 进入详情、Esc 返回；状态栏压缩为 1 行 |

### 2.4 通用交互规范（参考 pvetui）

交互参考 [pvetui](https://github.com/devnullvoid/pvetui)（Go + tview 写的 PVE TUI），借鉴下面这些做法：

| 借鉴点 | pvetui 的做法 | 我们的做法 |
|---|---|---|
| **主从视图** | 左边列表、右边“标签: 值”详情，移动光标详情立即跟着变 | 同样，作为三栏布局里的“列表 → 详情”两栏 |
| **上下文菜单** | 选中对象按 `m` 弹出动作菜单，每项前面有快捷键 `(s) Open Shell`、`(D) Stop (force)` | 同样，`m` 或 `Enter` 弹出；菜单里的快捷键**不打开菜单也能直接按** |
| **大小写区分危险程度** | 小写 `d` Shutdown，大写 `D` Stop (force)；`r`/`R` 同理 | 同样：**大写字母 = 更暴力的版本**，并按 §2.4 安全分级弹确认 |
| **全局菜单** | `g` / `Ctrl-g`：连接配置、设置、全部刷新、自动刷新开关、帮助、关于、退出 | `g` 弹出：切节点、切连接、设置、语言、自动刷新开关、帮助、关于、登出 |
| **`/` 实时过滤** | 搜索框出现在底部提示栏上方，边输入边过滤当前列表；**每个页面单独记住过滤词** | 同样。列表里 `/` = 过滤；日志查看器里 `/` = 搜索跳转、`&` = 只显示匹配行 |
| **vim 导航** | `h j k l`、`gg`/`G` | 同时支持方向键和 `h j k l`、`Home`/`G`（`g` 留给全局菜单，所以不做 `gg`） |
| **状态符号** | `▲` 绿色运行、`▼` 红色停止、灰色文字表示停止 | 同样，但**不用 emoji**（tty1 字体没有），只用 `▲▼●○` 这类 VT 字体有的字符 |
| **批量操作** | 多选后对选中的一批 VM 执行操作 | `Space` 多选，`m` 菜单作用于全部选中项 |
| **底部提示栏** | 一行列出当前可用的按键 | 同样，按当前焦点动态变化 |
| **自动刷新** | `a` 开关 | 同样，默认开，间隔可配置 |
| **可配置按键** | 配置文件改键位 | P2 |

按键汇总：

| 按键 | 作用 |
|---|---|
| `1`–`5` / `Alt-1`–`Alt-5` | 切换顶部模块 |
| `[` `]` | 切换子菜单 |
| `←` `→` / `h` `l` / `Tab` | 栏间移动焦点 |
| `↑↓` / `j` `k`，`Home` `End`/`G`，`PgUp/PgDn` | 列表移动 |
| `Enter` / `m` | 打开上下文菜单（对象的全部操作） |
| 菜单里的字母 | 直接执行；大写 = 强制版本 |
| `Space` | 多选 |
| `/` | 过滤（列表）/ 搜索（日志），`n`/`N` 下一个/上一个，`Esc` 清除 |
| `g` | 全局菜单 |
| `F2` | 切换节点 |
| `r` / `F5` | 刷新；`a` 自动刷新开关 |
| `F9` | 任务中心（PVE 任务 + 本地后台任务） |
| `F10` | 进入 Shell（root） |
| `?` | 帮助 |
| `Esc` | 返回 / 关闭弹窗 |
| `q` | 登出 |

pvetui 和我们的区别：它是装在**你电脑上**的远程客户端（只走 API + SSH），我们是装在**节点上**的控制台，
多了本机运维（日志、服务、磁盘、网络救援）和 tty1 物理控制台场景。所以“系统 / 网络 / 磁盘 / 服务日志”这四个模块是我们的重点，PVE 模块可以大量参考它。

操作安全分级：

| 级别 | 示例 | 确认方式 |
|---|---|---|
| 只读 | 查看 | 无 |
| 普通 | 启动 VM、重启非关键服务 | `y/n` 确认框 |
| 危险 | 停止 VM（强制）、重启 pveproxy/pve-cluster、改管理口 IP | 确认框 + 说明影响（例如“会断开 Web UI”） |
| 破坏性 | 删除 VM、擦盘、销毁存储、重启/关机节点 | **输入对象名**确认（输入 VMID / 磁盘名 / 主机名） |

所有 PVE 写操作返回 UPID → 弹出任务窗口实时 tail 日志（`GET /nodes/x/tasks/{upid}/log`），可 `Esc` 转后台、在 F9 里查看。

会话超时：✅ 已确认，从 60s 改为默认 15 分钟，可在 config.toml 中配置。

---

## 3. 模块一：System（系统）

| 子菜单 | 查看 | 操作 | 来源 | 优先级 |
|---|---|---|---|---|
| **Overview** | 主机名、PVE/内核版本、CPU 型号/核数、内存、运行时间、启动时间、负载、当前登录用户(`who`)、管理 URL `https://IP:8006`、订阅状态、仓库状态、大号 CPU/内存/网络曲线 | 无 | API `/nodes/x/status` + 本地 | P0 |
| **Power** | — | Reboot / Shutdown 节点（破坏性确认，提示运行中的 VM 数） | API `POST /nodes/x/status` | P0 |
| **Users / Password** | PVE 用户列表、realm、是否启用、TFA | 改 root 密码（现有）、改 PVE 用户密码、启用/禁用用户 | 本地 `chpasswd` / API `/access/password` `/access/users` | P0(改密) / P1 |
| **Time / NTP** | 时区、本地时间、chrony 源及同步状态 | 改时区 | API `/nodes/x/time` + `chronyc sources` | P1 |
| **DNS / Hosts** | search domain、DNS 服务器、`/etc/hosts` | 修改 DNS | API `/nodes/x/dns` | P1 |
| **Updates** | 可更新包列表（区分 PVE 相关包）、仓库配置 | `apt update` 刷新、执行升级（后台任务窗口，像 Web UI 的 upgrade shell） | API `/nodes/x/apt/*` | P1 |
| **Processes** | 类 top：按 CPU/内存排序，显示属于哪个 VMID（qemu 进程映射） | kill (TERM/KILL)、renice | 本地 `/proc` | P1 |
| **Certificates** | 证书信息、过期时间 | 重新生成自签证书（`pvecm updatecerts`） | API + 本地 | P2 |
| **Hostname** | — | 改主机名（单节点才允许；集群中给出警告并拒绝） | 本地 | P2 |

## 4. 模块二：Network（网络）

| 子菜单 | 查看 | 操作 | 来源 | 优先级 |
|---|---|---|---|---|
| **Interfaces** | 物理口/网桥/bond/VLAN 树、IP/CIDR、网关、MTU、link/carrier、MAC、速率，**pending 变更 diff** | 编辑 IP/网关/MTU/桥接端口、新建 bridge/bond/vlan（P1）、Apply（`PUT /nodes/x/network` → ifreload）、Revert pending | API `/nodes/x/network` | P0 查看 / P1 编辑 |
| **Management Rescue** | 当前管理口 | 现有“重置管理网络”（备份/回滚，纯本地，不依赖 API） | 本地 | P0（迁移） |
| **Traffic** | 每口实时 ↓↑ 速率、包数、错误/丢包，sparkline | — | 本地 `/proc/net/dev` / API rrd | P0 |
| **Tools** | — | ping、traceroute、DNS 查询、arping、端口测试（`nc -z`），结果实时滚动 | 本地 | P0 ping（已有）/ P1 其他 |
| **Routes** | 路由表、策略路由 | — | 本地 `ip -j route` | P1 |
| **SDN** | zones / vnets 状态 | — | API `/cluster/sdn` | P2 |
| **Firewall** | 集群/节点防火墙开关、规则 | 开关防火墙（救场：被防火墙锁在外面时一键关） | API | P1 |

注：“Firewall 一键关闭”和“Management Rescue”是 DCUI 最核心的救场价值，优先级要高。

## 5. 模块三：Disks（磁盘）——优先实现查看

| 子菜单 | 查看 | 操作 | 来源 | 优先级 |
|---|---|---|---|---|
| **Physical** | 磁盘列表：设备、型号、序列号、类型(SSD/HDD/NVMe)、容量、寿命 wearout、SMART 健康、用途（LVM/ZFS/OSD/分区） | — | API `/nodes/x/disks/list` | P0 |
| **SMART** | 选中盘的 SMART 属性/NVMe 日志 | — | API `/nodes/x/disks/smart` | P0 |
| **Partitions** | `lsblk` 树：分区、文件系统、挂载点、使用率 | — | 本地 `lsblk -J` + `df` | P0 |
| **LVM** | VG（大小/剩余）→ LV；thin pool 数据/元数据使用率（元数据快满时红色告警） | P2：创建 VG/thin pool | API `/disks/lvm` `/disks/lvmthin` + 本地 `lvs` | P0 |
| **ZFS** | pool 列表（size/alloc/frag/health）、`zpool status` vdev 树与错误计数、scrub 进度、dataset 列表、ARC 统计 | scrub 开始/停止、clear errors（P1）；创建 pool（P2） | API `/disks/zfs` + 本地 `zpool status -j`/`zfs list` | P0 |
| **Ceph** | health 及详细告警、mon/mgr/mds、OSD tree（up/in、使用率）、pool 列表、PG 状态 | OSD in/out、noout 标志（维护时常用）（P1） | API `/nodes/x/ceph/*`、`/cluster/ceph/*` | P0 查看 |
| **Storage (PVE)** | → 跳到 PVE 模块的 Storage | | | |
| **Wipe / Init** | — | 擦除磁盘、初始化 GPT（破坏性确认） | API `/disks/wipedisk` `/disks/initgpt` | P2 |

未安装/未配置时显示 “ZFS: no pools configured” 之类，而不是报错。

## 6. 模块四：Services & Logs（服务日志）

| 子菜单 | 查看 | 操作 | 来源 | 优先级 |
|---|---|---|---|---|
| **PVE Services** | PVE 关键服务（pveproxy、pvedaemon、pve-cluster、pvestatd、corosync、chrony、ssh…）状态、开机启用 | start/stop/restart/reload | API `/nodes/x/services` | P0 |
| **All Units** | 全部 systemd 服务，可过滤 failed/running | 同上 + enable/disable、`reset-failed` | 本地 `systemctl list-units -o json` | P1 |
| **Journal** | 日志查看器（见下） | | | P0 |
| **Kernel** | `dmesg` / `journalctl -k` | | 本地 | P1 |
| **PVE Tasks** | 集群任务列表（类型、对象、用户、状态、耗时），失败任务高亮 | 查看日志、停止运行中的任务 | API `/cluster/tasks` `/nodes/x/tasks` | P0 |

**服务 ↔ 日志联动**：服务列表选中某服务，右栏直接 tail 它的日志（`journalctl -u <svc> -f`）；
按 `l` 全屏打开该服务日志，按 `R` 重启后日志窗口不关闭，直接看到重启过程。

**日志查看器**（全屏组件，多处复用）：

- 模式：跟随（tail -f）/ 暂停浏览，`Space` 切换。
- 搜索：`/` 正则搜索，高亮，`n/N` 跳转；`&` 过滤模式（只显示匹配行，类似 less 的 `&`）。
- 过滤：优先级（emerg..debug，`p` 切换）、时间范围（`t`：最近 15m/1h/今天/本次启动/自定义）、unit。
- 本地实现：`journalctl -o json --since ... -u ... -p ... -g <regex>`（服务端过滤，海量日志也快）；
  远程节点：API `/nodes/x/journal`（gzip，按 since/until/条数分页）+ 客户端过滤。
- `w` 导出当前视图到 `/root/pxvirt-console-<ts>.log`。

## 7. 模块五：PVE

| 子菜单 | 查看 | 操作 | 来源 | 优先级 |
|---|---|---|---|---|
| **Cluster** | 集群名、quorum、节点列表（在线、IP、CPU/内存）、HA 状态、corosync 链路 | 节点视角切换 | API `/cluster/status` `/cluster/resources` `/cluster/ha/status` | P0 |
| **Guests** | VM + CT 统一列表：VMID、名称、类型、**架构**、节点、状态、CPU/内存/磁盘、uptime、tags、HA；排序/过滤/搜索（可按架构过滤） | start / shutdown / stop / reboot / suspend / resume；锁状态 unlock | API `/cluster/resources?type=vm` + `/nodes/x/qemu|lxc/{id}/status/*` | P0 |
| **Guest 详情** | 硬件配置（CPU/内存/磁盘/网卡/PCI 直通）、快照树、备份列表、IP（guest agent / lxc interfaces）、实时曲线 | 快照 创建/回滚/删除、迁移（选目标节点，支持在线）、克隆、改 CPU/内存/开机自启 | API | P1 |
| **Console** | — | 文本控制台：`qm terminal <id>`（串口）/ `pct enter <id>`（同进入 Shell 的机制，挂起 TUI 再恢复）；图形控制台见 §8 | 本地 | P1 |
| **Create** | — | 向导：创建 VM（节点→VMID→名称→ISO→CPU/内存→磁盘(存储/大小)→网络(桥)→确认）、创建 CT（模板→…） | API `POST /nodes/x/qemu|lxc` | P1 |
| **Storage** | 所有存储：类型、共享、启用、使用率、内容类型；存储内容浏览（ISO/模板/备份/磁盘镜像） | 启用/禁用、删除内容；下载 ISO（URL）；P2：新建存储 | API `/storage` `/nodes/x/storage/*` | P0 查看 / P1 操作 |
| **Backup** | 备份作业、最近备份结果 | 立即备份某 VM（vzdump）、恢复（P2） | API `/cluster/backup` `/nodes/x/vzdump` | P1 |
| **HA / Replication** | HA 资源与组、复制作业状态 | 加入/移出 HA、维护模式（node maintenance） | API | P2 |

> 删除 VM/CT 属于破坏性操作，需要输入 VMID 确认，放 P2。

### 7.1 多架构展示（pxvirt 特色）

pxvirt 支持多架构，界面上处处要能看出架构：

| 位置 | 显示 | 数据来源 |
|---|---|---|
| 节点列表 / 系统概览 | 节点架构 `x86_64` / `aarch64` / `loongarch64` / `riscv64` | `GET /nodes/x/status` 的 `current-kernel.machine`（本机验证返回 `x86_64`） |
| Guests 列表 | 架构列，简写 `x86` `arm` `la` `rv`；**与宿主机不同的架构**（TCG 模拟，跨架构运行）高亮显示，因为性能差很多 | VM：配置里的 `arch`，没写则等于节点架构；CT：配置里的 `arch`（`amd64`/`arm64`/…） |
| Guest 详情 | 架构、机型（machine）、是否 KVM 加速 | VM 配置 `arch` `machine` `kvm` |
| 集群概览 | 各架构节点数和 VM 数，例如 `x86_64: 3 nodes / 40 guests, aarch64: 2 / 12` | 汇总 |
| 创建向导 | 先选架构，再按架构过滤可用节点、ISO、CPU 型号、显卡类型 | |
| 迁移 | 目标节点下拉框里，架构不同的节点灰掉并注明原因 | |

注意：`/cluster/resources` 不返回架构，需要逐个读 VM 配置。做法是先显示列表，再在后台批量读取配置（只读可见行，带缓存），架构列逐步填上。

## 8. 图形控制台（DRM/SDL）

背景：QEMU 的显示后端在启动时确定，PVE 的 VM 都是 `-vnc unix:/var/run/qemu-server/<vmid>.vnc`，**不能**运行时切成 SDL。所以正确做法是“做一个 VNC 客户端，用 SDL 画到 DRM 上”：

- 单独小程序 `pxvirt-viewer`（或同一二进制的子命令），SDL2 使用 **KMSDRM** 后端（无需 X/Wayland），VNC 协议用现成 crate（如 `vnc-rs`）。
- 本机 VM：连 `qm vncproxy <vmid>`（stdio）或直接连 unix socket；远程节点 VM：`POST /nodes/x/qemu/<id>/vncproxy` + `vncwebsocket`。
- TUI 侧流程：选中 VM → `v` → 离开 alternate screen、释放 tty → 启动 viewer → 退出（如 `Ctrl-Alt-Q`）后恢复 TUI（与现有进入 Shell 一致）。
- 键盘/鼠标通过 SDL 读 evdev 转发；需要 `Ctrl-Alt-Del` 等特殊键菜单。
- 风险：KMSDRM 在 fbcon 已占用时的切换、多显卡选择、无 DRM 的服务器（只有 VGA fbdev）→ 检测不到 DRM 时隐藏此功能。

### 8.1 SSH 下能不能用？

**SDL 的 KMSDRM 后端在 SSH 里不能用**：它画在服务器自己的显示器上，不是画在你的终端里；
而且 SSH 会话拿不到 DRM master（显卡被 tty1 / fbcon 占用）。所以 SSH 下要换一种“显示后端”。
VNC 客户端部分（连接、解码、键鼠转发）是共用的，只是把画面输出到不同的地方：

```
             ┌─ KMSDRM (SDL2)        ← tty1 物理控制台
VNC 客户端 ──┼─ 终端图形协议         ← SSH，终端支持 Kitty / Sixel / iTerm2
(共用)       ├─ 半块字符 ▀ 真彩色     ← SSH，普通终端（低分辨率预览）
             └─ 转交给本地 viewer     ← SSH，给出 ssh -L 端口转发命令 / noVNC URL
```

| 场景 | 后端 | 效果 |
|---|---|---|
| tty1，有 DRM | SDL2 KMSDRM | 全分辨率、流畅，**主要方案** |
| SSH，终端支持 Kitty 图形协议（kitty、WezTerm、Ghostty） | Kitty 协议，只传变化区域 | 清晰可用，适合装系统、进 BIOS、看蓝屏 |
| SSH，终端支持 Sixel（foot、WezTerm、新版 Windows Terminal、xterm -ti vt340） | Sixel | 可用，帧率较低 |
| SSH，普通终端 | Unicode 半块字符 + 真彩色（1 个字符格 = 2 个像素） | 只能当缩略图看大概状态（240×67 终端 ≈ 240×134 像素），看不清文字 |
| SSH，想要完整体验 | 显示一条 `ssh -L 5900:...` 命令或 noVNC 链接，用你电脑上的 VNC 客户端/浏览器打开 | 最好，但不在 TUI 里 |
| SSH + `ssh -X` | SDL2 x11 后端 | 能用但慢，需要客户端有 X server，不推荐 |

实现要点：

- 终端图形检测和渲染可以用 `ratatui-image` 这类 crate（自动探测 Kitty/Sixel/iTerm2，失败时退回半块字符），需要实际验证。
- 输入转发：键盘用 crossterm；开启 Kitty 键盘协议后才能拿到按键抬起和更多组合键；
  `Ctrl-Alt-Del`、`Ctrl-Alt-F1` 这类组合在 SSH 下发不出去，统一做成底部状态栏按钮（“Keys” 按钮弹出菜单），不用任何快捷键触发，按键全部转发给 guest。
  鼠标用终端鼠标上报（只有字符格精度；支持 SGR-Pixels 的终端可以拿到像素坐标）。
- 带宽：只传 VNC 告诉我们的“脏矩形”，并限制帧率（例如 10 fps），避免慢链路卡死。
- **文本控制台优先**：`qm terminal`（串口）和 `pct enter` 在 SSH 下完全正常，Linux 虚拟机配了串口时比图形控制台好用得多。

### 8.2 ratatui-image 实测（2026-11-13）

原型放在 `experiments/term-image-probe/`（独立 crate，不影响主项目）。

**版本**：最新稳定版 `ratatui-image 11.1.0`，依赖 `ratatui 0.30` + `crossterm 0.29`；
支持 `ratatui 0.29`（我们现在用的版本）的最后一个版本是 8.x。**建议主项目升级到 ratatui 0.30**，反正要重构。
关掉默认的 `chafa-dyn` 特性，避免依赖系统的 libchafa。

**编码开销**（模拟 VM 画面：BIOS 文字 + 渐变 + 移动方块，每帧都完整重新编码，release 构建，本机 Ryzen 7 5800U）：

| 协议 | VM 分辨率 | 终端格数 | 编码耗时/帧 | 每帧数据量 | 10fps 带宽 |
|---|---|---|---|---|---|
| Halfblocks | 1024×768 | 200×60 | 43 ms | 17 KiB* | 1.4 Mbit* |
| Sixel | 1024×768 | 200×60 | 63 ms | 175 KiB | 14 Mbit |
| Kitty | 1024×768 | 200×60 | 44 ms | 4.1 MiB | 346 Mbit |
| iTerm2 | 1024×768 | 200×60 | 37 ms | 814 KiB | 67 Mbit |
| Halfblocks | 800×600 | 120×40 | 16 ms | 9 KiB* | 0.7 Mbit* |
| Sixel | 800×600 | 120×40 | 31 ms | 116 KiB | 9.5 Mbit |
| Kitty | 800×600 | 120×40 | 21 ms | 2.5 MiB | 211 Mbit |

\* Halfblocks 只统计了字符，没算颜色转义序列（每格约 20–40 字节），实际大约 ×10；不过 ratatui 只重绘变化的格子，静止画面几乎不占带宽。

**真实终端测试**（Claude 桌面应用的终端面板，`TERM=xterm-256color`）：检测结果为 Halfblocks（不支持 Kitty/Sixel），约 40 fps，流畅。
还没有在 kitty / WezTerm / foot / Windows Terminal 上测，需要你在自己常用的终端里通过 SSH 跑一下：
`term-image-probe live`，结果会写到 `/tmp/term-image-probe.txt`。

**结论**：

1. **能力探测 + Halfblocks + Sixel 直接用 ratatui-image**，效果可以。
2. **Kitty 不能直接用它的编码**：它每帧发未压缩的 RGBA，1024×768 一帧 4 MiB，SSH 上不可用。
   要自己写 Kitty 输出：用 PNG 或 zlib 压缩（`f=100` / `o=z`），并且**只发送 VNC 给出的变化区域**（Kitty 支持在指定位置放置子图）。估计能降到每帧几十 KiB。
3. 编码在 worker 线程里做，限制 10 fps，跟不上就丢帧，避免越积越多。
4. 终端不支持图形时默认用 Halfblocks 作为“缩略图”，并在提示栏给出 noVNC 链接 / `ssh -L` 命令（pvetui 也是用浏览器 noVNC 的方式）。

### 8.3 排期

放 Phase 3。先做文本控制台（串口 / pct enter），再做 tty1 的 KMSDRM，最后做 SSH 下的终端图形协议。

## 9. 代码结构建议

```
src/
  main.rs              终端初始化、主循环、挂起/恢复 TUI（shell/console/viewer）
  app.rs               全局状态：会话、Context、当前模块、弹窗栈、任务中心
  config.rs            /etc/pxvirt-console/config.toml（超时、语言、默认模块、远程列表）
  i18n.rs
  backend/
    pve_api.rs         HTTP 客户端（ticket、CSRF、gzip、TLS 指纹）
    pvesh.rs           fallback
    exec.rs            HostExec trait + Local / Ssh
    metrics.rs         /proc 采样、环形缓冲
    tasks.rs           UPID 跟踪
  ui/
    layout.rs          顶栏/子菜单/多栏/状态栏、尺寸降级
    widgets/           table（排序/过滤/搜索）、log_viewer、form、confirm、task_popup、sparkline
  modules/
    mod.rs             trait Module { tabs(), draw(), on_key(), on_event(), refresh() }
    system.rs network.rs disks.rs services.rs pve.rs
    rescue/            现有：重置管理网络、改 root 密码（保持纯本地）
```

依赖建议：HTTP 用 `ureq`（同步、体积小，放 worker 线程里跑；需 gzip + rustls + 自定义证书校验）或 `reqwest`（已有 tokio）。
`serde_json` 必需。现有 `sysinfo` 可以用于进程页，监控直接读 `/proc` 更轻。

## 10. 里程碑

| 阶段 | 内容 |
|---|---|
| **Phase 0 框架** | 新布局（顶栏/子菜单/多栏/状态栏/尺寸降级）、Module trait、worker+channel、PveApi(本地 ticket)、登录(API+pwcheck 救援)、现有功能迁移 |
| **Phase 1 能看** | 各模块 P0 查看项：系统概览、网卡、磁盘/LVM/ZFS/Ceph、服务+日志查看器(搜索/跟随)、集群/Guests/存储列表、任务中心、节点视角切换 |
| **Phase 2 能操作** | 服务启停、VM 电源操作、网络编辑+Apply、节点电源、防火墙一键关闭、快照/迁移、文本控制台、apt 更新 |
| **Phase 3 进阶** | 远程 API 模式、SSH 模式、创建 VM/CT 向导、图形控制台(SDL/KMSDRM)、破坏性磁盘操作、HA/备份 |

---

## 11. 已定事项

1. tty1 用英文；SSH 下可以切中文。
2. 用户和权限直接用 PVE 的（§1.2.1）。
3. 未登录时只读显示主机名、管理 IP、`https://IP:8006`、版本和底部监控。
4. 会话超时默认 15 分钟，可配置。
5. 底部状态栏按 §2.2 的 5 格；有运行中的 HA 告警、Ceph 不健康、ZFS 降级等问题时，最后一格换成红色告警计数。
6. 中间三栏：导航 → 列表 → 详情/操作。
7. 远程模式先做直连远程 8006（API），SSH 作为补充且只允许密钥登录。
8. 丢失的改动：想起一条记一条，逐步补进来。
9. pxvirt 特色：多架构展示（§7.1），其他与上游 PVE 一致。
10. 交互参考 pvetui（§2.4）。
11. 升级到 ratatui 0.30，以便使用 ratatui-image 11。


---

## 12. 实现状态（2026-11-13）

### 12.1 代码结构

```
src/
  main.rs        参数（--remote/--ssh/--lang/--vnc-test）、主循环、挂起 TUI 运行 shell/控制台
  app.rs         状态、按键路由、布局（顶栏/模块/子菜单/三栏/状态栏/提示栏）、全局菜单、告警
  login.rs       登录前看板、登录（/access/ticket + TFA）、救援登录（pwcheck + pvesh）
  api.rs         PVE API 客户端（ticket/CSRF、TLS 指纹固定 + TOFU、pvesh 后备、perl 生成 root 票据）
  ctx.rs         Session（PVE 权限判断）、节点视角、命令出口（弹窗/后台操作/挂起/VNC）
  exec.rs        主机命令：本机或 ssh（集群其他节点）
  metrics.rs     底部监控：本机 /proc 每秒采样，其他节点走 API
  rescue.rs      管理网络重置（带备份回滚）、chpasswd
  i18n.rs        中英文（TERM=linux 强制英文）
  ui/            表格（过滤/多选/窄屏自动隐藏列）、弹窗、表单、日志查看器、任务窗口
  modules/       system / network / disks / services / pve / pve_create
  vnc/           RFB 客户端、连接方式（qm vncproxy / API websocket / unix socket）、终端渲染、SDL
```

### 12.2 已实现

- 五个模块全部子菜单（§3–§7），共 32 个页面；所有写操作走确认分级，PVE 任务弹出实时日志。
- 登录：PVE 用户 + 认证域 + TFA；API 不可用时 root 进入救援模式（pvesh），API 恢复后自动切回。
- 权限：按 §1.2.1 用 `/access/permissions` 判断，没权限的操作灰显并注明缺什么权限。
- 节点视角切换（F2），其他节点的主机命令走 root ssh。
- 多架构：节点架构、虚拟机架构列、跨架构高亮、创建向导选架构、迁移时提示架构不同。
- 图形控制台：tty1 上 SDL2/KMSDRM；SSH 里 Kitty（自己的 zlib 编码）/ Sixel / iTerm2 / 半块字符。
  控制台不占用任何快捷键（所有按键都转发给 guest）：`Ctrl+Alt+Del`、特殊键、刷新、关闭都是底部状态栏上的按钮，用鼠标点；SDL 路径用 font8x8 位图字体自绘状态栏（不依赖 SDL2_ttf）。
- 审计：所有写操作写入 journal（`journalctl -t pxvirt-console`）。

### 12.3 测试情况

| 内容 | 方式 | 结果 |
|---|---|---|
| 32 个页面 + 菜单 + 帮助，三种尺寸（200×50 / 110×32 / 80×25） | `sudo pxvirt-console --dump-screens DIR`，对本机 PVE 9.2.26 渲染成文本检查 | 通过（修了中文对齐、窄屏列宽、状态栏截断） |
| VNC 协议、编码、画面 | `--vnc-shot` 连独立 QEMU（SeaBIOS 画面） | 通过 |
| VNC 密码认证（DES） | PVE 的 `/usr/bin/kvm` + `set_password`，错误密码被拒、正确密码出画面 | 通过 |
| 单元测试 | `cargo test`（10 个） | 通过 |

**还没测过（需要真实环境或会改动系统）**：

- 任何写操作（启停服务、改网络、建虚拟机等）——测试时只读，没有执行写操作。
- `qm vncproxy` 和 API websocket 两种连接（本机没有虚拟机）。
- SDL/KMSDRM 显示（需要在 tty1 上运行）；终端图形只做了编码单元测试，没在 kitty/WezTerm 里实际看过。
- 交互式按键流程（登录、表单、日志查看器）——只验证了渲染。
- 多节点集群、远程模式、非 root 用户的权限灰显。

### 12.4 调试参数

- `sudo pxvirt-console --dump-screens DIR`：以 root@pam 登录并把所有页面渲染成文本。
- `pxvirt-console --vnc-test SOCKET` / `--vnc-test qm:VMID`：直接打开图形控制台。
- `pxvirt-console --vnc-shot SOCKET OUT.ppm`：抓一帧画面（`PXVIRT_VNC_PASSWORD` 指定密码）。


---

## 13. 运行模式（2026-11-13 更新）

| 模式 | 用法 | 说明 |
|---|---|---|
| 本机 | tty1 服务，或在节点上直接运行 | 全部功能；tty1/串口上不能退出，终端里 Ctrl+Q 退出 |
| SSH | `pxvirt-console --ssh root@节点[:端口]` | 先建一条 SSH 主连接（ControlMaster），API 走端口转发，主机命令、shell、控制台、实时监控都复用这条连接；SSH 用户是 root 时自动以 root@pam 登录；其他集群节点从这台节点再 ssh 一跳；`q` = 断开 |
| API | `pxvirt-console --remote 节点[:端口]` | 只用 API，证书指纹首次确认后记住；需要主机命令的页面（内存、进程、分区、路由、工具、流量、全部服务）和操作直接隐藏 |

非 root 用户运行时，配置和证书指纹保存在 `~/.config/pxvirt-console/`。
SSH 模式下审计日志写到被管理节点的 journal。
