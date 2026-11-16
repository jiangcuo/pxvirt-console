# PXVIRT Console

English | [中文](README.zh-CN.md)

A text-mode node management console for PXVIRT / Proxmox VE, in the spirit of the VMware ESXi DCUI.
It runs on the physical console (tty1) of a node, in any terminal on the node, or on your workstation to manage a
node over SSH or the PVE API.

![Overview](res/overview.jpg)

## Features

- **System**: overview, health checks with fixes (SMART, kernel errors, PCIe, ECC, temperatures, BMC, NICs, RAID, ZFS / Ceph, ...), memory, processes, PCI / USB passthrough, hardware, BMC (complete event log, sensors, BMC network), updates, users, certificates, settings (time zone, NTP), power
- **Network**: management network with automatic rollback, bridges / bonds / VLANs, live traffic, connections per process with disconnect, tools, routes, firewall
- **Disks**: SMART, LVM, ZFS, Ceph, live IO of every device, NFS / iSCSI / FC / NVMe-oF, hardware RAID / HBA / NVMe through the vendor tools (StorCLI, PERCCLI, ssacli, arcconf, sas2/3ircu, hiraidadm, Areca, HighPoint, mvcli, tw_cli, nvme-cli)
- **Services & Logs**: systemd services, journal, PVE tasks
- **PVE**: guests with live monitor and console preview, full configuration editor, create wizard with node recommendation, storage upload / download, backup, HA, replication
- **Graphical VM console**: SDL2/KMSDRM on tty1, Kitty / Sixel graphics in a terminal, with mouse
- PVE login with TFA and per-user permissions, keyboard and mouse, English and Chinese

![Network traffic](res/net.jpg)

![VM console in a terminal](res/vm.jpg)

## Quick start

```bash
pxvirt-console --ssh root@node      # manage a node from your workstation (Linux / macOS binaries in Releases)
pxvirt-console                      # on a workstation: pick one of the saved connections
sudo pxvirt-console                 # on the node itself
```

On a node with the Debian package, enable the tty1 console:

```bash
systemctl enable --now pxvirt-console.service
```

Quit with **Ctrl+Q**, help with **?**. Recommended terminal: [Ghostty](https://ghostty.org).

## Documentation

| | |
|---|---|
| [Installation and run modes](docs/en/install.md) | Downloads, tty1 service, local / SSH / API modes, configuration, building |
| [Using the console](docs/en/usage.md) | Layout, keys, mouse, pausing, permissions |
| [System](docs/en/system.md) · [Network](docs/en/network.md) · [Disks](docs/en/disks.md) · [PVE](docs/en/pve.md) | The pages of each module |
| [Hardware RAID](docs/en/raid.md) | Controllers, tools to install, how they are found, LEDs, actions |

Downloads: [Releases](https://github.com/jiangcuo/pxvirt-console/releases) (static Linux binaries for x86_64, aarch64,
riscv64, loongarch64; macOS arm64 / x86_64).
