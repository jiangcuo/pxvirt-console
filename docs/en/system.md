# System (F1)

[Documentation](README.md) · [中文](../zh/system.md)

| Page | Shows | Actions |
|---|---|---|
| 1 Overview | Console / PVE / OS / kernel versions; product, board, BIOS, boot mode; CPU model, topology, usage, load, virtualization; RAM, swap, ZFS ARC, KSM; root filesystem; guests running / total; management URLs; top processes by CPU and memory | Reboot node `R`, shut down node `S`, start all guests in boot order `A`, shut down all guests `X`, Wake-on-LAN an offline node `W` |
| 2 Health | Hardware-first health checks: disks / SMART, kernel errors, PCIe, ECC, temperatures, BMC, NICs, RAID, ZFS / LVM / Ceph, file systems, services, time, certificates, cluster, backups. See [Health checks](health.md) | Open related page Enter / `o`, acknowledge `a`, remove acknowledgement `u`, check now `c` |
| 3 Memory | meminfo breakdown, swap devices, swappiness, overcommit, ZFS ARC size and limit, huge pages, transparent huge pages, KSM | ZFS ARC limit `z`, swappiness `w`, huge pages `p`, THP mode `T`, KSM on / off `K`, drop page cache `d`, flush swap `S` |
| 4 Processes | All processes, sortable; VM processes are named after their VM | Terminate `x` (SIGTERM), kill `X` (SIGKILL) |
| 5 PCI | PCI devices with driver, IOMMU group, link speed, SR-IOV | Bind to vfio-pci `v`, bind to a driver `b`, unbind `u`, restore default driver `d`, reset `R`, set SR-IOV VFs `s`, rescan bus `n` |
| 6 USB | USB devices and topology | Reset (re-enumerate) `R` |
| 7 Hardware | `lscpu`, DMI, memory modules, sensors (as available) | – |
| 8 BMC | IPMI event log (complete, newest first, with severity; filter with `/`), BMC sensors (fans, power supplies, voltages, temperatures), controller / LAN / chassis / FRU information. Needs `ipmitool` and `/dev/ipmi0` | Export the event log to `/var/log/pxvirt-console/` `w`, clear it (exported first) `C`, identify LED `i` / off `O`, power restore policy `p`, BMC network (static / DHCP, VLAN) `n`, BMC cold reset `R`, install ipmitool `I`, load the IPMI modules `M`; switch event log / sensors / info `v` |
| 9 Updates | Pending updates, package search, package details | Refresh index `u`, upgrade all `U`, upgrade one / install `i`, changelog `c`, info `v`, search `s` |
| 10 Users | PVE users, realms, groups, ACLs | Change password `p`, reset local root password `R` (rescue) |
| 11 Certificates | Node certificates and their validity | Regenerate node certificates `R` |
| 12 Settings | Time zone, local time / UTC, hardware clock, NTP service and sync state (chrony), offset, reference, configured NTP servers and every source with its state | Time zone `z`, NTP servers `n` (written to `/etc/chrony/sources.d/pxvirt-console.sources`; the default sources of `chrony.conf` can be switched off, a backup is kept), sync now `s`, restart chrony `S`, set date and time by hand `t`, write the hardware clock `w`, install chrony `I` |

Dangerous actions (reboot, kill, upgrades, driver changes) ask for confirmation; the most dangerous ones need the
object's name typed in.
