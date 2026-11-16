# Health checks

[Documentation](README.md) · [中文](../zh/health.md)

System → 2 Health checks the node in the background, hardware first. The status bar shows the number of open
warnings and critical findings (click it, or `g` `A`, to open the page); the overview shows the worst ones.

| Area | Checks | Source |
|---|---|---|
| Disks | SMART status; reallocated, pending and uncorrectable sectors; interface CRC errors (growth); SAS grown defects and uncorrected errors; NVMe critical warning, media errors, wear | `smartctl` (also disks behind MegaRAID controllers) |
| Kernel log | I/O errors, hung tasks, ext4 / XFS / Btrfs errors, OOM kills, soft / hard lockups and RCU stalls, machine checks, NFS not responding, thermal throttling, segfaults | `journalctl -k` since boot |
| PCIe | AER corrected (growth), non-fatal and fatal errors; links running narrower than the device supports | sysfs |
| Memory | ECC corrected (growth) and uncorrectable errors per controller / DIMM; swap use; memory pressure (PSI) | EDAC, `/proc` |
| Temperatures | CPU and board sensors against their max / crit limits; disk and NVMe temperatures | hwmon, SMART |
| Fans / power / BMC | BMC sensors not `ok` (fans, power supplies, voltages); BMC event log entries of the last 24 h | `ipmitool` (an info finding suggests installing it when a BMC is present); System → BMC shows the complete log. The log being 90 % full is a warning |
| Network | Bond / bridge members down, link flaps and rx/tx errors (growth), speed below the NIC's maximum | sysfs, `ethtool` |
| RAID / HBA | Controller, virtual drive, physical drive, battery and NVMe health | [Hardware RAID](raid.md) tools |
| ZFS / LVM thin / Ceph | Pool state, device errors, capacity, scrub age; thin pool data and metadata use; `ceph health detail` | `zpool`, `lvs`, `ceph` |
| Storage | PVE storages not active or too full | PVE API |
| System | File system space and inodes of `/`, `/var`, `/var/log`, `/tmp`, `/boot`, `/var/lib/vz`; failed units (PVE core services are critical); clock offset; certificate expiry; reboot needed for a new kernel | host |
| Cluster / guests | Quorum, offline nodes, corosync links; HA resources in error; failed backups (24 h); locked guests | PVE API, `corosync-cfgtool` |

Host checks run every minute, SMART / BMC / ethtool every 10 minutes, the RAID scan every 5 minutes. Error counters
that are never reset (AER, CRC, ECC, NIC errors, link flaps) only count when they grow; growth stays visible for
`growth_hours`.

## Using the page

| Key | Action |
|---|---|
| Enter, `o` | Open the page with the details (SMART → Disks, units → All Units, ...) |
| `a` | Acknowledge: the finding stays listed but no longer counts as an alert, until it gets worse |
| `u` | Remove the acknowledgement |
| `f` | Fix: actions for the selected finding (below) |
| `c` | Run every check now, SMART and BMC included |

Acknowledgements are stored per node in `/etc/pxvirt-console/health-ack.json` (non-root:
`~/.config/pxvirt-console/health-ack.json`). The checks need host access (local or `--ssh`); in API mode only the
PVE checks run.

## Fixes

`f` offers the actions that fit the finding; risky ones ask first, everything is written to the audit log.

| Finding | Fixes |
|---|---|
| File system full | Largest directories, shrink the journal to 200 MB, `apt-get clean`, `apt autoremove --purge` (in the terminal) |
| File system out of inodes | Directories with the most files |
| Failed unit | Show its log, restart it, reset the failed state |
| Kernel log errors | Kernel log (warnings and errors of this boot) |
| Clock not in sync | Sync now (`chronyc makestep`), restart chrony |
| No quorum | Emergency `pvecm expected 1` (type the node name to confirm) |
| BMC without ipmitool / event log full | Install ipmitool; export, or export and clear the event log |
| ZFS | Start a scrub, `zpool status -v`, `zpool clear` |
| Ceph / corosync / NIC / memory | `ceph health detail`, `corosync-cfgtool -s`, `ip -s link`, biggest memory users |
| Locked guest / HA resource in error | Unlock; clear the error (disable, then request started) |

## Thresholds

`[health]` in `config.toml`; every key is optional.

| Key | Default | Meaning |
|---|---|---|
| `fs_warn`, `fs_crit` | 85, 95 | File system use (space and inodes), % |
| `disk_temp_warn`, `disk_temp_crit` | 55, 65 | HDD / SSD temperature, °C |
| `nvme_temp_warn`, `nvme_temp_crit` | 70, 80 | NVMe temperature, °C |
| `cpu_temp_warn`, `cpu_temp_crit` | 90, 100 | CPU temperature when the sensor has no limits, °C |
| `nvme_wear_warn` | 90 | NVMe percentage used |
| `lvm_thin_warn`, `lvm_thin_crit` | 90, 95 | Thin pool data / metadata use, % |
| `zfs_cap_warn`, `zfs_cap_crit` | 80, 90 | ZFS pool capacity, % |
| `zfs_scrub_days` | 35 | Days without a scrub |
| `storage_warn`, `storage_crit` | 90, 95 | PVE storage use, % |
| `time_offset_warn`, `time_offset_crit` | 1.0, 5.0 | Clock offset, seconds |
| `cert_days_warn`, `cert_days_crit` | 30, 7 | Days until a certificate expires |
| `mem_pressure_warn` | 10 | Memory PSI `some avg60`, % |
| `swap_warn` | 80 | Swap use, % |
| `growth_hours` | 24 | How long a growing error counter stays reported |

```toml
[health]
fs_warn = 80
disk_temp_warn = 50
```
