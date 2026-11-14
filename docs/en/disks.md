# Disks (F3)

[Documentation](README.md) · [中文](../zh/disks.md)

| Page | Shows | Actions |
|---|---|---|
| 1 Physical | Disks with type, size, model, health (SMART), wearout, usage, and live IO: read / write rate, IOPS, latency, util; IO graphs in the details | SMART report `s`, initialize GPT `P`, wipe `W`, create ZFS pool from marked disks `z`, LVM-thin pool `t`, volume group `v`, directory storage `d` |
| 2 Partitions | Partitions and filesystems | – |
| 3 LVM | Volume groups, thin pools, logical volumes | – |
| 4 ZFS | Pools with health, usage, fragmentation, dedup, read / write rate, IOPS, latency (`zpool iostat`); vdev tree, datasets, ARC | Scrub start `s` / stop `S`, clear errors `c`, `zpool status -v` `t`, `zpool iostat -v` `i` |
| 5 Ceph | Health, OSDs | Mark OSD in `i` / out `o` |
| 6 IO | Every block device (disks, NVMe, dm, md, bcache, zvol, rbd, nbd), busiest first: rates, IOPS, read / write latency, util, queue | `o` sort, `v` all devices |
| 7 Network storage | NFS, iSCSI, Fibre Channel, NVMe over Fabrics, multipath | – |
| 8 Hardware RAID | See [Hardware RAID](raid.md) | |

## Disk IO

Numbers come from `/proc/diskstats`, sampled every second by the live sampler (also over SSH):

- read / write rate and IOPS: completed requests per second
- latency: time per completed read / write (await)
- util: share of time with IO in flight
- queue: requests in flight right now

## Network storage

| Section | Source | Shows |
|---|---|---|
| NFS | `/proc/self/mountstats` | Export, mount point, PVE storage and its state, version, protocol, rsize / wsize, hard / soft; read / write rate, IOPS, RTT and execution time per operation, timeouts and retransmissions |
| iSCSI | `/sys/class/iscsi_session`, `iscsi_connection` | Target IQN, session state, portals, initiator, recovery timeout, disks with their IO |
| Fibre Channel | `/sys/class/fc_host`, `fc_remote_ports` | WWPN / WWNN, port state, speed, fabric, rates, error counters (link failures, loss of sync / signal, CRC), remote ports |
| NVMe-oF | `/sys/class/nvme` (non-PCIe) | Transport (tcp / rdma / fc), address, NQN, state, namespaces with their IO |
| Multipath | `multipath -ll` | Maps and paths |

Rates are measured between refreshes (3 s).
