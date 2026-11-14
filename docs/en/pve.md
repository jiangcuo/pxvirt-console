# PVE (F5)

[Documentation](README.md) · [中文](../zh/pve.md)

| Page | Shows | Actions |
|---|---|---|
| 1 Cluster | Quorum, nodes with resources and architecture, versions | Switch to node `w`, shell on node `S` |
| 2 Guests | VMs and containers of the cluster (architecture, node, state, CPU, memory, uptime, tags / HA); `f` filter, `o` sort, Space marks for batch actions | Start `s`, shutdown `d`, stop `D`, reboot `t`, reset `T`, suspend `p`, resume `u`, text console `c`, graphical console `v`, snapshots `n`, backup `b`, migrate `M`, clone `C`, configuration `e`, HA `H`, unlock `U`, delete `X`, create VM `N` / CT `O` |
| 3 Storage | Storages with usage | Browse content `b`, upload file `U`, download from URL `w`, download CT template `T`, enable / disable |
| 4 Backup | Backup jobs | Run now `R` |
| 5 HA | HA resources and states | Request started / stopped / disabled `s` `x` `d`, remove `X`, node maintenance on `M` / off `n` |
| 6 Replication | Replication jobs | Schedule now `R` |

## Guest details

The details of a guest have three panes:

- **Configuration**: CPU, memory, machine, BIOS, devices, notes (scrolls with Tab / ↑ ↓).
- **Monitor**: state, uptime, HA, IP addresses (guest agent / container interfaces), CPU, memory, network and disk
  with history. Sampled every 3 s; the graphs start with the last hour from the PVE RRD data.
- **Console preview** (VMs, terminals with Kitty / Sixel graphics, not on tty1): the VM screen, refreshed about once a
  second over one VNC connection while the VM is selected. Click it or press `v` to open the graphical console.

Guest names in punycode (`xn--...`) are shown decoded; non-ASCII names entered in forms are encoded.

## Configuration page (`e`)

Every config item is a row, edited with its own form; Esc returns to the list.

- VMs: memory and ballooning, processors, BIOS, display, machine, SCSI controller, disks, CD/DVD, EFI disk, TPM,
  network, USB, PCI, serial, unused disks, all options, Cloud-Init and other keys.
- Containers: memory / swap, cores, rootfs, mount points, network, hostname, DNS, features, options.
- Changes waiting for a reboot are shown as `current → pending` (`u` reverts them).
- Every write carries the config digest, so a concurrent change by someone else is rejected instead of overwritten.
- Forms only change their own fields; other options of a device are kept.
- Add device `+`, detach / remove `x`, delete unused volume `X`, resize `R`, move disk `M`, eject `i`, boot order `o`,
  raw value `E`.

## Create VM / CT (`N` / `O`)

In a cluster the node is chosen first. Every node shows its architecture, CPU usage and cores, and free / total
memory; the recommended node (★) is preselected:

- score = 55 % free memory share + 35 % idle CPU + 10 % core count
- nodes with less than 2 GiB free memory come last

The form then uses the chosen node's storages, ISOs and bridges. VMs can get a second CD/DVD drive (for example the
virtio-win driver ISO for Windows): `ide2` / `ide0` on x86, `scsi2` / `scsi3` elsewhere.

## Storage upload (`U`)

Upload ISOs, CT templates and import images (OVA / qcow2 / vmdk) from the machine the console runs on (the node in
console mode, your computer with `--ssh` / `--remote`). The upload streams with progress, speed and remaining time;
Esc continues it in the background, `x` cancels. An optional checksum is verified by PVE. The node first stores the
upload in `/var/tmp`, so it needs free space there.
