# System (F1)

[Documentation](README.md) · [中文](../zh/system.md)

| Page | Shows | Actions |
|---|---|---|
| 1 Overview | Console / PVE / OS / kernel versions; product, board, BIOS, boot mode; CPU model, topology, usage, load, virtualization; RAM, swap, ZFS ARC, KSM; root filesystem; guests running / total; management URLs; top processes by CPU and memory | Reboot node `R`, shut down node `S`, start all guests in boot order `A`, shut down all guests `X`, Wake-on-LAN an offline node `W` |
| 2 Memory | meminfo breakdown, swap devices, swappiness, overcommit, ZFS ARC size and limit, huge pages, transparent huge pages, KSM | ZFS ARC limit `z`, swappiness `w`, huge pages `p`, THP mode `T`, KSM on / off `K`, drop page cache `d`, flush swap `S` |
| 3 Processes | All processes, sortable; VM processes are named after their VM | Terminate `x` (SIGTERM), kill `X` (SIGKILL) |
| 4 PCI | PCI devices with driver, IOMMU group, link speed, SR-IOV | Bind to vfio-pci `v`, bind to a driver `b`, unbind `u`, restore default driver `d`, reset `R`, set SR-IOV VFs `s`, rescan bus `n` |
| 5 USB | USB devices and topology | Reset (re-enumerate) `R` |
| 6 Hardware | `lscpu`, DMI, memory modules, sensors (as available) | – |
| 7 Updates | Pending updates, package search, package details | Refresh index `u`, upgrade all `U`, upgrade one / install `i`, changelog `c`, info `v`, search `s` |
| 8 Users | PVE users, realms, groups, ACLs | Change password `p`, reset local root password `R` (rescue) |
| 9 Certificates | Node certificates and their validity | Regenerate node certificates `R` |

Dangerous actions (reboot, kill, upgrades, driver changes) ask for confirmation; the most dangerous ones need the
object's name typed in.
