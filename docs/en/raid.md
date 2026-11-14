# Hardware RAID (F3 → Hardware RAID)

[Documentation](README.md) · [中文](../zh/raid.md)

The page shows RAID / HBA controllers and NVMe drives through the vendors' command line tools: controllers, virtual
drives with their member drives, the other drives grouped by role (failed, hot spare, unconfigured, JBOD), battery /
CacheVault, background tasks, locate / fault LEDs and live IO. Problems (degraded, failed, predictive failure,
battery, foreign configuration) also go into the alert counter.

It needs host access (local or SSH mode) and runs the tools as root.

## Tools to install

Install the tool that matches your controller on the node. The console does not ship them (vendor licenses).

| Controllers | Tool | Kernel driver | Download | Verified on hardware |
|---|---|---|---|---|
| Broadcom / LSI MegaRAID, OEM cards (Huawei, Lenovo, Inspur, Fujitsu, Cisco, Intel RS3, HPE MR) | `storcli64` / `storcli` | `megaraid_sas` | [Broadcom](https://www.broadcom.com/support/download-search), search "StorCLI" | yes (Huawei SAS3108) |
| Dell PERC (H730, H740, H750, H755, H965 ...) | `perccli64` / `perccli` | `megaraid_sas` | [Dell](https://www.dell.com/support), search "PERCCLI" | no |
| Broadcom MegaRAID 9600 / 9660 / 9670 | `storcli2` | `mpi3mr` | Broadcom, "StorCLI2" | no |
| Dell PERC 12 | `perccli2` | `mpi3mr` | Dell, "PERCCLI" | no |
| HPE Smart Array (P408i, P816i, E208i ...) | `ssacli` (also `hpssacli`, `hpacucli`) | `smartpqi`, `hpsa` (HPE vendor id) | [HPE](https://support.hpe.com), or the HPE MCP repository | no |
| Microchip / Adaptec SmartRAID, SmartHBA, Adaptec series | `arcconf` | `smartpqi`, `aacraid` | [Microchip](https://www.microchip.com), search "ARCCONF" | no |
| Broadcom SAS3 HBA (9300 / 9400 / 9500, IT / IR) | `sas3ircu` | `mpt3sas` | Broadcom, "SAS3IRCU" | no |
| Broadcom SAS2 HBA | `sas2ircu` | `mpt2sas` | Broadcom, "SAS2IRCU" | no |
| Huawei SP186 / SP686C | `hiraidadm` | `hiraid` | [Huawei](https://support.huawei.com), "hiraidadm" | no |
| Areca | `cli64` / `cli32` | `arcmsr` | [Areca](https://www.areca.com.tw/support/downloads.html) | no |
| HighPoint | `hptraidconf` / `hptcli` | `hptiop`, `hptnvme`, `rr*` | [HighPoint](https://www.highpoint-tech.com) | no |
| Dell BOSS (Marvell 88SE92xx) | `mvcli` (BOSS-N1: `mnv_cli`) | `ahci` with Marvell vendor id | Dell, "BOSS mvcli" | no |
| 3ware (legacy) | `tw_cli` | `3w-9xxx`, `3w-sas`, `3w-xxxx` | Broadcom legacy downloads | no |
| NVMe drives | `nvme` (nvme-cli) | `nvme` | `apt install nvme-cli` | yes (Samsung PM1733) |
| 3SNIC SSSRAID | – | `sssraid` | not supported yet (drive health: `smartctl -d sssraid`) | – |

"Verified" means the parser was checked against real output; the others follow vendor documentation and published
examples. Please send the output of the read-only commands (for example `storcli64 /call show all J`,
`ssacli ctrl all show config detail`, `arcconf GETCONFIG 1 AL`) when something looks wrong.

## How a tool is found

1. **Controllers**: every PCI device of class `0x01xxxx` (mass storage) in `/sys/bus/pci/devices` is read with its
   vendor, subsystem vendor and bound kernel driver. A controller belongs to a tool when its driver is in the tool's
   driver list (and, for HPE and Dell BOSS, the vendor / subsystem vendor matches). Plain AHCI SATA controllers are
   ignored.
2. **Binaries**: each tool's names are looked up in `PATH` (`command -v`) and in its install directories:

   | Tool | Directories searched besides PATH |
   |---|---|
   | storcli | `/opt/MegaRAID/storcli`, `/opt/lsi/storcli`, `/opt/MegaRAID/StorCLI` |
   | perccli | `/opt/MegaRAID/perccli`, `/opt/lsi/perccli`, `/opt/dell/perccli` |
   | storcli2 / perccli2 | `/opt/MegaRAID/storcli2`, `/opt/broadcom/storcli2`, `/opt/MegaRAID/perccli2`, `/opt/dell/perccli2` |
   | ssacli | `/opt/smartstorageadmin/ssacli/bin`, `/usr/sbin` |
   | arcconf | `/usr/Arcconf`, `/opt/Arcconf`, `/usr/StorMan` |
   | sas2ircu / sas3ircu | `/opt/lsi`, `/opt/sas3ircu` |
   | hiraidadm | `/opt/hiraidadm`, `/opt/huawei/hiraidadm` |
   | cli64 | `/opt/areca`, `/usr/local/areca` |
   | hptraidconf | `/usr/share/hpt`, `/opt/hpt` |
   | mvcli | `/opt/mvcli`, `/opt/dell/boss`, `/opt/marvell` |
   | tw_cli | `/opt/3ware` |
   | nvme | `/usr/sbin`, `/usr/bin`, `/sbin`, `/usr/local/sbin`, `/usr/local/bin` |

3. **One tool per controller family**: tools that manage the same controllers form a group (storcli / perccli,
   storcli2 / perccli2, ssacli / arcconf); the first one found is used.
4. **Only for present hardware**: an installed tool is only run when a matching controller exists.
5. **Missing tool**: a controller without a usable tool is listed with the tool to install and where to get it.

The detection is one shell script, so it works the same locally and over SSH.

## What is read

| Tool | Commands (read only) |
|---|---|
| storcli family | `/call show all J`, `/call/vall show all J`, `/call/eall/sall show all J`, `/call/eall/sall show rebuild J`, `/call/vall show cc J`, `/call/vall show init J`, `/call show patrolread J`, `/call/fall show J`, `/call/eall show all J` |
| ssacli | `ctrl all show config detail` |
| arcconf | `LIST`, `GETCONFIG <n> AL` |
| sas2/3ircu | `LIST`, `<n> DISPLAY` |
| hiraidadm | `show allctrl`, `c<n> show vdlist`, `c<n> show pdlist`, `c<n> show status` |
| cli64 | `sys info`, `vsf info`, `disk info` |
| hptraidconf | `query arrays`, `query devices` |
| mvcli | `info -o hba`, `info -o vd`, `info -o pd` |
| tw_cli | `show`, `/c<n> show` |
| nvme-cli | `list -v -o json` (1.x: `list -o json`), `smart-log /dev/nvmeX -o json` |

A full scan runs when the page is opened or refreshed (30 s); for the alert counter at most every 5 minutes.

## Locate and fault LEDs

Most tools cannot report whether a locate LED is on. The console reads the Linux SES enclosure driver instead
(`/sys/class/enclosure/*/<slot>/locate` and `fault`) and maps the slots to drives:

1. by the OS block device of the drive (HBA / JBOD drives), else
2. by enclosure and slot: the tool's enclosure serial (storcli "Enclosure Serial Number") equals the SES enclosure id,
   or there is exactly one enclosure with that number of slots.

The details show `Locate LED: on / off (SES enclosure)`; the menu then offers only the switch that changes something.
Without SES the console remembers which LEDs it switched itself (`switched here`). When a tool has no LED command but
SES is available, the LED is switched through SES.

## Actions and confirmation

Actions depend on the selected object (controller, virtual drive, physical drive) and the drive's role; for example a
JBOD drive offers "make unconfigured good", an unconfigured drive "make hot spare" and "make JBOD".

| Risk | Examples | Confirmation |
|---|---|---|
| View | Details, event log, SMART log, self-test log | Runs and shows the output |
| Low | Locate LED, start / stop consistency check or patrol read, silence alarm, NVMe self-test | Confirm |
| Medium | Hot spare, JBOD, cache policy, import foreign configuration, rebuild, online | Confirm with the full command line |
| High | Create / delete virtual drive, initialize, set offline, clear foreign configuration, NVMe format / sanitize / delete namespace | Type the object id, full command line shown |

Every executed command is written to the journal of the node.

## Adding a tool

The code is in the `pxvirt-raid` crate (`raid/`):

- `model.rs`: tool independent data (controller, virtual drive, physical drive, health, roles)
- `spec.rs`: `ToolSpec` (binaries, directories, drivers, queries, state words, actions)
- `tools.rs`: the list of tools
- `dialect/`: one parser per output format

A tool whose output looks like an existing one (for example another StorCLI OEM build) only needs a new `ToolSpec` in
`tools.rs`: binary names, install directories, kernel drivers, and the actions as command templates
(`{ctl}`, `{vd}`, `{pd}`, `{enc}`, `{slot}`; `[opt={x}]` is dropped when empty; `{drives*}` expands into several
arguments). A new output format needs a parser in `dialect/` that fills the model.
