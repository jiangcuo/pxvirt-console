# 健康检查

[文档目录](README.md) · [English](../en/health.md)

系统 → 2 健康 在后台检查节点，硬件优先。状态栏显示未处理的警告和严重问题数量（点击它或按 `g` `A` 打开页面），概览页显示最严重的几项。

| 范围 | 检查内容 | 来源 |
|---|---|---|
| 磁盘 | SMART 状态；重映射、待映射、不可纠正扇区；接口 CRC 错误（增长）；SAS 新增缺陷和未纠正错误；NVMe 严重警告、介质错误、寿命 | `smartctl`（包括 MegaRAID 控制器后面的磁盘） |
| 内核日志 | I/O 错误、hung task、ext4 / XFS / Btrfs 错误、OOM、软 / 硬死锁和 RCU 停顿、机器检查、NFS 无响应、过热降频、段错误 | 本次启动以来的 `journalctl -k` |
| PCIe | AER 可纠正错误（增长）、非致命和致命错误；链路宽度低于设备支持的宽度 | sysfs |
| 内存 | 每个控制器 / 内存条的 ECC 可纠正错误（增长）和不可纠正错误；Swap 使用率；内存压力（PSI） | EDAC、`/proc` |
| 温度 | CPU 和主板传感器对比其 max / crit 阈值；磁盘和 NVMe 温度 | hwmon、SMART |
| 风扇 / 电源 / BMC | 状态不是 `ok` 的 BMC 传感器（风扇、电源、电压）；最近 24 小时的 BMC 事件日志 | `ipmitool`（有 BMC 但没装时给出提示）；系统 → BMC 显示完整日志。日志使用超过 90% 时给出警告 |
| 网络 | bond / 网桥成员断开、链路抖动和收发错误（增长）、速率低于网卡最高速率 | sysfs、`ethtool` |
| RAID / HBA | 控制器、虚拟盘、物理盘、电池和 NVMe 的健康状态 | [硬件 RAID](raid.md) 工具 |
| ZFS / LVM thin / Ceph | 池状态、设备错误、容量、巡检间隔；thin 池数据和元数据使用率；`ceph health detail` | `zpool`、`lvs`、`ceph` |
| 存储 | 不可用或快满的 PVE 存储 | PVE API |
| 系统 | `/`、`/var`、`/var/log`、`/tmp`、`/boot`、`/var/lib/vz` 的空间和 inode；失败的服务（PVE 核心服务为严重）；时间偏差；证书过期；新内核需要重启 | 主机 |
| 集群 / 虚拟机 | 法定票数、离线节点、corosync 链路；出错的 HA 资源；失败的备份（24 小时）；被锁定的虚拟机 | PVE API、`corosync-cfgtool` |

主机检查每分钟一次，SMART / BMC / ethtool 每 10 分钟一次，RAID 扫描每 5 分钟一次。不会清零的错误计数（AER、CRC、ECC、网卡错误、链路抖动）只有在增长时才报告，增长后在 `growth_hours` 内保持显示。

## 页面操作

| 按键 | 作用 |
|---|---|
| Enter、`o` | 打开详情所在页面（SMART → 磁盘，服务 → 所有单元 ……） |
| `a` | 确认：问题仍然列出，但不再计入告警，直到情况变差 |
| `u` | 取消确认 |
| `f` | 修复：针对选中问题的操作（见下文） |
| `c` | 立即运行全部检查，包括 SMART 和 BMC |

确认记录按节点保存在 `/etc/pxvirt-console/health-ack.json`（非 root：`~/.config/pxvirt-console/health-ack.json`）。
检查需要主机访问（本机或 `--ssh`）；API 模式下只运行 PVE 相关检查。

## 修复

`f` 列出适合该问题的操作；有风险的会先确认，所有操作都记录到审计日志。

| 问题 | 修复操作 |
|---|---|
| 文件系统空间满 | 查看占用最大的目录、把日志压缩到 200 MB、`apt-get clean`、`apt autoremove --purge`（在终端里执行） |
| inode 用尽 | 查看文件数最多的目录 |
| 服务失败 | 查看日志、重启、重置失败状态 |
| 内核日志错误 | 查看内核日志（本次启动的警告和错误） |
| 时间未同步 | 立即同步（`chronyc makestep`）、重启 chrony |
| 失去法定票数 | 紧急 `pvecm expected 1`（输入节点名确认） |
| BMC 没装 ipmitool / 事件日志满 | 安装 ipmitool；导出，或导出后清空事件日志 |
| ZFS | 开始巡检、`zpool status -v`、`zpool clear` |
| Ceph / corosync / 网卡 / 内存 | `ceph health detail`、`corosync-cfgtool -s`、`ip -s link`、内存占用最大的进程 |
| 虚拟机被锁 / HA 资源出错 | 解锁；清除错误（先停用，再请求启动） |

## 阈值

在 `config.toml` 的 `[health]` 中设置，所有项都可以省略。

| 配置项 | 默认值 | 说明 |
|---|---|---|
| `fs_warn`、`fs_crit` | 85、95 | 文件系统使用率（空间和 inode），% |
| `disk_temp_warn`、`disk_temp_crit` | 55、65 | 机械盘 / SSD 温度，°C |
| `nvme_temp_warn`、`nvme_temp_crit` | 70、80 | NVMe 温度，°C |
| `cpu_temp_warn`、`cpu_temp_crit` | 90、100 | 传感器没有阈值时的 CPU 温度，°C |
| `nvme_wear_warn` | 90 | NVMe 已用寿命，% |
| `lvm_thin_warn`、`lvm_thin_crit` | 90、95 | thin 池数据 / 元数据使用率，% |
| `zfs_cap_warn`、`zfs_cap_crit` | 80、90 | ZFS 池容量，% |
| `zfs_scrub_days` | 35 | 多少天没有巡检 |
| `storage_warn`、`storage_crit` | 90、95 | PVE 存储使用率，% |
| `time_offset_warn`、`time_offset_crit` | 1.0、5.0 | 时间偏差，秒 |
| `cert_days_warn`、`cert_days_crit` | 30、7 | 证书剩余天数 |
| `mem_pressure_warn` | 10 | 内存 PSI `some avg60`，% |
| `swap_warn` | 80 | Swap 使用率，% |
| `growth_hours` | 24 | 错误计数增长后保持报告的时长 |

```toml
[health]
fs_warn = 80
disk_temp_warn = 50
```
