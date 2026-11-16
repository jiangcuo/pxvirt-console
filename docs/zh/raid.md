# 硬件 RAID（F3 → 硬件 RAID）

[文档目录](README.md) · [English](../en/raid.md)

这个页面通过各厂商的命令行工具显示 RAID / HBA 控制器和 NVMe 盘：控制器、虚拟磁盘及其成员盘、按角色分组的其余磁盘（故障、热备、未配置、JBOD）、电池 / 电容、后台任务、定位灯和故障灯，以及实时 IO。
发现的问题（降级、故障、预测性故障、电池异常、外来配置）会计入[健康检查](health.md)和告警数。

需要主机访问（本机或 SSH 模式），工具以 root 身份运行。

## 需要安装的工具

在节点上安装和控制器对应的工具。控制台不内置这些工具（厂商许可证不允许分发）。

| 控制器 | 工具 | 内核驱动 | 下载 | 真机验证 |
|---|---|---|---|---|
| Broadcom / LSI MegaRAID 及 OEM 卡（华为、联想、浪潮、富士通、思科、Intel RS3、HPE MR） | `storcli64` / `storcli` | `megaraid_sas` | [Broadcom](https://www.broadcom.com/support/download-search)，搜索 "StorCLI" | 是（华为 SAS3108） |
| Dell PERC（H730、H740、H750、H755、H965 等） | `perccli64` / `perccli` | `megaraid_sas` | [Dell](https://www.dell.com/support)，搜索 "PERCCLI" | 否 |
| Broadcom MegaRAID 9600 / 9660 / 9670 | `storcli2` | `mpi3mr` | Broadcom，"StorCLI2" | 否 |
| Dell PERC 12 | `perccli2` | `mpi3mr` | Dell，"PERCCLI" | 否 |
| HPE Smart Array（P408i、P816i、E208i 等） | `ssacli`（以及 `hpssacli`、`hpacucli`） | `smartpqi`、`hpsa`（HPE 厂商 ID） | [HPE](https://support.hpe.com)，或 HPE MCP 软件源 | 否 |
| Microchip / Adaptec SmartRAID、SmartHBA、Adaptec 系列 | `arcconf` | `smartpqi`、`aacraid` | [Microchip](https://www.microchip.com)，搜索 "ARCCONF" | 否 |
| Broadcom SAS3 HBA（9300 / 9400 / 9500，IT / IR 固件） | `sas3ircu` | `mpt3sas` | Broadcom，"SAS3IRCU" | 否 |
| Broadcom SAS2 HBA | `sas2ircu` | `mpt2sas` | Broadcom，"SAS2IRCU" | 否 |
| 华为 SP186 / SP686C | `hiraidadm` | `hiraid` | [华为](https://support.huawei.com)，"hiraidadm" | 否 |
| Areca | `cli64` / `cli32` | `arcmsr` | [Areca](https://www.areca.com.tw/support/downloads.html) | 否 |
| HighPoint | `hptraidconf` / `hptcli` | `hptiop`、`hptnvme`、`rr*` | [HighPoint](https://www.highpoint-tech.com) | 否 |
| Dell BOSS（Marvell 88SE92xx） | `mvcli`（BOSS-N1：`mnv_cli`） | `ahci`，且厂商 ID 为 Marvell | Dell，"BOSS mvcli" | 否 |
| 3ware（老卡） | `tw_cli` | `3w-9xxx`、`3w-sas`、`3w-xxxx` | Broadcom 旧版下载 | 否 |
| NVMe 盘 | `nvme`（nvme-cli） | `nvme` | `apt install nvme-cli` | 是（三星 PM1733） |
| 3SNIC SSSRAID | – | `sssraid` | 暂不支持（盘的健康状态可用 `smartctl -d sssraid`） | – |

"真机验证"指解析器用真实输出核对过；其余按厂商文档和公开的输出样例编写。显示不对时，请把只读命令的输出发给我们，例如 `storcli64 /call show all J`、`ssacli ctrl all show config detail`、`arcconf GETCONFIG 1 AL`。

## 怎么判断用哪个工具

1. **识别控制器**：读取 `/sys/bus/pci/devices` 里所有 `0x01xxxx` 类（大容量存储）的 PCI 设备，包括厂商 ID、子系统厂商 ID 和当前绑定的内核驱动。
   驱动在某个工具的驱动列表里，这个控制器就归这个工具管（HPE 和 Dell BOSS 还要求厂商 / 子系统厂商 ID 匹配）。普通 AHCI SATA 控制器忽略。
2. **查找可执行文件**：每个工具的程序名先在 `PATH` 里找（`command -v`），再在它的安装目录里找：

   | 工具 | PATH 之外额外查找的目录 |
   |---|---|
   | storcli | `/opt/MegaRAID/storcli`、`/opt/lsi/storcli`、`/opt/MegaRAID/StorCLI` |
   | perccli | `/opt/MegaRAID/perccli`、`/opt/lsi/perccli`、`/opt/dell/perccli` |
   | storcli2 / perccli2 | `/opt/MegaRAID/storcli2`、`/opt/broadcom/storcli2`、`/opt/MegaRAID/perccli2`、`/opt/dell/perccli2` |
   | ssacli | `/opt/smartstorageadmin/ssacli/bin`、`/usr/sbin` |
   | arcconf | `/usr/Arcconf`、`/opt/Arcconf`、`/usr/StorMan` |
   | sas2ircu / sas3ircu | `/opt/lsi`、`/opt/sas3ircu` |
   | hiraidadm | `/opt/hiraidadm`、`/opt/huawei/hiraidadm` |
   | cli64 | `/opt/areca`、`/usr/local/areca` |
   | hptraidconf | `/usr/share/hpt`、`/opt/hpt` |
   | mvcli | `/opt/mvcli`、`/opt/dell/boss`、`/opt/marvell` |
   | tw_cli | `/opt/3ware` |
   | nvme | `/usr/sbin`、`/usr/bin`、`/sbin`、`/usr/local/sbin`、`/usr/local/bin` |

3. **同一类控制器只用一个工具**：管理同一类控制器的工具编为一组（storcli / perccli、storcli2 / perccli2、ssacli / arcconf），用先找到的那个。
4. **只为存在的硬件运行**：装了工具但没有对应的控制器时，不会运行它。
5. **缺少工具**：有控制器但没有可用工具时，页面会列出这个控制器，并提示该装哪个工具、去哪里下载。

检测是一个 shell 脚本，本机和 SSH 模式下的行为完全一样。

## 读取哪些信息

| 工具 | 执行的命令（全部只读） |
|---|---|
| storcli 系 | `/call show all J`、`/call/vall show all J`、`/call/eall/sall show all J`、`/call/eall/sall show rebuild J`、`/call/vall show cc J`、`/call/vall show init J`、`/call show patrolread J`、`/call/fall show J`、`/call/eall show all J` |
| ssacli | `ctrl all show config detail` |
| arcconf | `LIST`、`GETCONFIG <n> AL` |
| sas2/3ircu | `LIST`、`<n> DISPLAY` |
| hiraidadm | `show allctrl`、`c<n> show vdlist`、`c<n> show pdlist`、`c<n> show status` |
| cli64 | `sys info`、`vsf info`、`disk info` |
| hptraidconf | `query arrays`、`query devices` |
| mvcli | `info -o hba`、`info -o vd`、`info -o pd` |
| tw_cli | `show`、`/c<n> show` |
| nvme-cli | `list -v -o json`（1.x：`list -o json`）、`smart-log /dev/nvmeX -o json` |

打开或刷新页面时做一次完整扫描（每 30 秒）；用于告警计数时最多每 5 分钟扫描一次。

## 定位灯和故障灯

大多数工具查不到定位灯当前是亮还是灭。控制台改为读取 Linux 内核的 SES 机箱驱动（`/sys/class/enclosure/*/<槽位>/locate` 和 `fault`），再把槽位对应到磁盘：

1. 按磁盘在系统里的块设备名对应（HBA / JBOD 盘）；否则
2. 按机箱和槽位号对应：工具报告的机箱序列号（storcli 的 "Enclosure Serial Number"）等于 SES 机箱 ID，或者只有一个机箱的槽位数与之相同。

详情里显示"定位灯：亮 / 灭（SES 机箱）"，菜单里只出现能改变当前状态的那一项。
没有 SES 时，控制台记住自己点亮过哪些灯（显示"本控制台设置"）。工具本身没有定位灯命令但有 SES 时，直接通过 SES 开关定位灯。

## 操作和确认

可用的操作取决于选中的对象（控制器、虚拟磁盘、物理盘）和盘的角色。例如 JBOD 盘可以"设为未配置"，未配置的盘可以"设为热备"和"设为 JBOD"。

| 风险 | 例子 | 确认方式 |
|---|---|---|
| 只读 | 详情、事件日志、SMART 日志、自检日志 | 直接执行并显示输出 |
| 低 | 定位灯、开始 / 停止一致性检查或巡检、静音蜂鸣器、NVMe 自检 | 普通确认 |
| 中 | 热备、JBOD、缓存策略、导入外来配置、重建、设为在线 | 确认，并显示完整命令 |
| 高 | 创建 / 删除虚拟磁盘、初始化、设为离线、清除外来配置、NVMe 格式化 / sanitize / 删除命名空间 | 输入对象 ID 才能执行，并显示完整命令 |

所有执行过的命令都记录到节点的 journal。

## 添加新工具

代码在 `pxvirt-raid` 这个 crate 里（`raid/` 目录）：

- `model.rs`：与工具无关的数据模型（控制器、虚拟磁盘、物理盘、健康度、角色）
- `spec.rs`：`ToolSpec`（程序名、目录、驱动、查询命令、状态字、操作）
- `tools.rs`：工具清单
- `dialect/`：每种输出格式一个解析器

如果新工具的输出格式和已有的某种一样（比如另一个 OEM 版的 StorCLI），只需要在 `tools.rs` 里加一个 `ToolSpec`：
填写程序名、安装目录、内核驱动，以及用命令模板写的操作。模板里 `{ctl}`、`{vd}`、`{pd}`、`{enc}`、`{slot}` 会替换成对象的地址；`[opt={x}]` 在值为空时整段省略；`{drives*}` 会展开成多个参数。
新的输出格式需要在 `dialect/` 里写一个解析器，把结果填进数据模型。
