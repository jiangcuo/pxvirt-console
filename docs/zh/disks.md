# 磁盘（F3）

[文档目录](README.md) · [English](../en/disks.md)

| 页面 | 显示内容 | 操作 |
|---|---|---|
| 1 物理盘 | 磁盘类型、大小、型号、健康（SMART）、寿命、用途，以及实时 IO：读写速率、IOPS、延迟、繁忙度；详情里有 IO 曲线 | SMART 报告 `s`、初始化 GPT `P`、擦除 `W`、用标记的盘创建 ZFS 池 `z`、LVM-thin 池 `t`、卷组 `v`、目录存储 `d` |
| 2 分区 | 分区和文件系统 | – |
| 3 LVM | 卷组、thin 池、逻辑卷 | – |
| 4 ZFS | 存储池的健康、用量、碎片、去重，以及读写速率、IOPS、延迟（`zpool iostat`）；vdev 树、数据集、ARC | 开始 `s` / 停止 `S` scrub、清除错误 `c`、`zpool status -v` `t`、`zpool iostat -v` `i` |
| 5 Ceph | 健康状态、OSD | 标记 OSD in `i` / out `o` |
| 6 IO | 所有块设备（磁盘、NVMe、dm、md、bcache、zvol、rbd、nbd），按繁忙度排序：速率、IOPS、读写延迟、繁忙度、队列 | `o` 排序、`v` 显示全部设备 |
| 7 网络存储 | NFS、iSCSI、光纤通道、NVMe over Fabrics、多路径 | – |
| 8 硬件 RAID | 见 [硬件 RAID](raid.md) | |

## 磁盘 IO

数据来自 `/proc/diskstats`，由实时采样每秒读取一次（SSH 模式也一样）：

- 读写速率和 IOPS：每秒完成的请求
- 延迟：每个完成的读 / 写请求的平均耗时（await）
- 繁忙度：有 IO 在进行的时间占比
- 队列：当前正在进行的请求数

## 网络存储

| 类型 | 数据来源 | 显示内容 |
|---|---|---|
| NFS | `/proc/self/mountstats` | 导出路径、挂载点、对应的 PVE 存储及状态、版本、协议、rsize / wsize、hard / soft；读写速率、IOPS、每个操作的 RTT 和执行时间、超时和重传次数 |
| iSCSI | `/sys/class/iscsi_session`、`iscsi_connection` | Target IQN、会话状态、门户、发起端、恢复超时、对应的磁盘及其 IO |
| 光纤通道 | `/sys/class/fc_host`、`fc_remote_ports` | WWPN / WWNN、端口状态、速率、fabric、收发速率、错误计数（链路故障、失步 / 失信号、CRC）、远端端口 |
| NVMe-oF | `/sys/class/nvme`（非 PCIe） | 传输方式（tcp / rdma / fc）、地址、NQN、状态、命名空间及其 IO |
| 多路径 | `multipath -ll` | 多路径设备和路径 |

速率按两次刷新（3 秒）之间计算。
