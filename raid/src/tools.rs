//! The known tools. Adding a tool whose output matches an existing dialect = adding a
//! [`ToolSpec`] here; nothing else changes.

use crate::model::{Health, Role};
use crate::spec::{act, choice, drives, text, ActionSpec, Dialect, Param, Risk, Target, ToolSpec};

use Health::{Critical, Ok as Good, Warning};
use Risk::{High, Low, Medium, View};
use Target::{Controller as C, Pd, Vd};

const SBIN: &[&str] = &["/usr/sbin", "/usr/bin", "/sbin", "/usr/local/sbin", "/usr/local/bin"];

// ---------------------------------------------------------------------------
// Broadcom MegaRAID: StorCLI, StorCLI2, Dell PERCCLI
// ---------------------------------------------------------------------------

const MR_QUERIES: &[(&str, &str)] = &[
    ("all", "/call show all J"),
    ("vds", "/call/vall show all J"),
    ("pds", "/call/eall/sall show all J"),
    ("pds_noenc", "/call/sall show all J"),
    ("rebuild", "/call/eall/sall show rebuild J"),
    ("cc", "/call/vall show cc J"),
    ("init", "/call/vall show init J"),
    ("pr", "/call show patrolread J"),
    ("foreign", "/call/fall show J"),
    ("encl", "/call/eall show all J"),
];

const MR_STATES: &[(&str, Health)] = &[
    ("optl", Good),
    ("optimal", Good),
    ("onln", Good),
    ("jbod", Good),
    ("ugood", Good),
    ("ghs", Good),
    ("dhs", Good),
    ("pdgd", Warning),
    ("dgrd", Warning),
    ("rec", Warning),
    ("rbld", Warning),
    ("cpybck", Warning),
    ("ugunsp", Warning),
    ("needs attention", Warning),
    ("ofln", Critical),
    ("offln", Critical),
    ("ubad", Critical),
    ("msng", Critical),
];

const MR_LEVELS: &[(&str, &str)] = &[("r1", "RAID 1"), ("r5", "RAID 5"), ("r6", "RAID 6"), ("r10", "RAID 10"), ("r0", "RAID 0"), ("r50", "RAID 50"), ("r60", "RAID 60")];
const MR_WCACHE: &[(&str, &str)] = &[("wb", "write back"), ("wt", "write through"), ("awb", "always write back")];
const MR_RCACHE: &[(&str, &str)] = &[("ra", "read ahead"), ("nora", "no read ahead")];
const MR_STRIP: &[(&str, &str)] = &[("256", "256 KB"), ("64", "64 KB"), ("128", "128 KB"), ("512", "512 KB"), ("1024", "1 MB")];
const MR_CREATE: &[Param] = &[
    choice("level", "RAID level", MR_LEVELS),
    drives("drives", "Drives"),
    text("pdperarray", "Drives per span (RAID 10/50/60)", "", false),
    text("name", "Name", "", false),
    choice("strip", "Strip size", MR_STRIP),
    choice("wcache", "Write cache", MR_WCACHE),
    choice("rcache", "Read cache", MR_RCACHE),
];

const MR_ACTIONS: &[ActionSpec] = &[
    act('E', "Event log (latest 50)", C, View, &["/c{ctl} show events type=latest=50"]),
    act('I', "Controller details", C, View, &["/c{ctl} show all"]),
    act('M', "Silence alarm", C, Low, &["/c{ctl} set alarm=silence"]),
    act('P', "Start patrol read", C, Low, &["/c{ctl} start patrolread"]),
    act('p', "Stop patrol read", C, Low, &["/c{ctl} stop patrolread"]),
    act('F', "Import foreign configuration", C, Medium, &["/c{ctl}/fall import"]),
    act('X', "Clear foreign configuration", C, High, &["/c{ctl}/fall delete"]),
    act('N', "Create virtual drive", C, High, &["/c{ctl} add vd {level} drives={drives} [pdperarray={pdperarray}] [name={name}] strip={strip} {wcache} {rcache}"]).with(MR_CREATE),
    act('i', "Virtual drive details", Vd, View, &["/c{ctl}/v{vd} show all"]),
    act('c', "Start consistency check", Vd, Low, &["/c{ctl}/v{vd} start cc"]),
    act('C', "Stop consistency check", Vd, Low, &["/c{ctl}/v{vd} stop cc"]),
    act('w', "Write cache policy", Vd, Medium, &["/c{ctl}/v{vd} set wrcache={wcache}"]).with(&[choice("wcache", "Write cache", MR_WCACHE)]),
    act('R', "Read cache policy", Vd, Medium, &["/c{ctl}/v{vd} set rdcache={rcache}"]).with(&[choice("rcache", "Read cache", MR_RCACHE)]),
    act('n', "Rename", Vd, Low, &["/c{ctl}/v{vd} set name={name}"]).with(&[text("name", "Name", "", true)]),
    act('I', "Fast initialize (erases data)", Vd, High, &["/c{ctl}/v{vd} start init"]),
    act('X', "Delete virtual drive", Vd, High, &["/c{ctl}/v{vd} del force"]),
    act('i', "Drive details", Pd, View, &["/c{ctl}{pd} show all"]),
    act('L', "Locate LED on", Pd, Low, &["/c{ctl}{pd} start locate"]).led(true),
    act('K', "Locate LED off", Pd, Low, &["/c{ctl}{pd} stop locate"]).led(false),
    act('s', "Make global hot spare", Pd, Medium, &["/c{ctl}{pd} add hotsparedrive"]).roles(&[Role::Unconfigured]),
    act('S', "Remove hot spare", Pd, Medium, &["/c{ctl}{pd} delete hotsparedrive"]).roles(&[Role::HotSpare]),
    act('J', "Make JBOD", Pd, Medium, &["/c{ctl}{pd} set jbod"]).roles(&[Role::Unconfigured]),
    act('U', "Make unconfigured good (removes the disk from the OS)", Pd, High, &["/c{ctl}{pd} set good force"]).roles(&[Role::Jbod]),
    act('d', "Clear bad state", Pd, Medium, &["/c{ctl}{pd} set good"]).roles(&[Role::Failed]),
    act('b', "Start rebuild", Pd, Medium, &["/c{ctl}{pd} start rebuild"]).roles(&[Role::Member, Role::Failed]),
    act('n', "Set online", Pd, Medium, &["/c{ctl}{pd} set online"]).roles(&[Role::Failed]),
    act('O', "Set offline", Pd, High, &["/c{ctl}{pd} set offline"]).roles(&[Role::Member]),
];

const BROADCOM: &str = "Broadcom StorCLI: https://www.broadcom.com/support/download-search (search \"StorCLI\")";

// ---------------------------------------------------------------------------
// HPE ssacli
// ---------------------------------------------------------------------------

const SSA_LEVELS: &[(&str, &str)] = &[("1", "RAID 1"), ("5", "RAID 5"), ("6", "RAID 6"), ("1+0", "RAID 10"), ("0", "RAID 0"), ("50", "RAID 50"), ("60", "RAID 60")];
const SSA_ACTIONS: &[ActionSpec] = &[
    act('I', "Controller details", C, View, &["ctrl slot={ctl} show detail"]),
    act('T', "Status", C, View, &["ctrl slot={ctl} show status"]),
    act('N', "Create logical drive", C, High, &["ctrl slot={ctl} create type=ld drives={drives} raid={level}"]).with(&[choice("level", "RAID level", SSA_LEVELS), drives("drives", "Drives")]),
    act('i', "Logical drive details", Vd, View, &["ctrl slot={ctl} ld {vd} show detail"]),
    act('w', "Caching", Vd, Medium, &["ctrl slot={ctl} ld {vd} modify caching={caching}"]).with(&[choice("caching", "Caching", &[("enable", "enable"), ("disable", "disable")])]),
    act('X', "Delete logical drive", Vd, High, &["ctrl slot={ctl} ld {vd} delete forced"]),
    act('i', "Drive details", Pd, View, &["ctrl slot={ctl} pd {pd} show detail"]),
    act('L', "Locate LED on", Pd, Low, &["ctrl slot={ctl} pd {pd} modify led=on"]).led(true),
    act('K', "Locate LED off", Pd, Low, &["ctrl slot={ctl} pd {pd} modify led=off"]).led(false),
    act('s', "Add as spare of an array", Pd, Medium, &["ctrl slot={ctl} array {target_array} add spares={pd}"]).with(&[text("target_array", "Array (A, B, ...)", "A", true)]).roles(&[Role::Unconfigured]),
    act('S', "Remove spare", Pd, Medium, &["ctrl slot={ctl} array {array} remove spares={pd}"]).roles(&[Role::HotSpare]),
];

// ---------------------------------------------------------------------------
// Microchip / Adaptec arcconf
// ---------------------------------------------------------------------------

const ARC_LEVELS: &[(&str, &str)] = &[("1", "RAID 1"), ("5", "RAID 5"), ("6", "RAID 6"), ("10", "RAID 10"), ("0", "RAID 0"), ("50", "RAID 50"), ("60", "RAID 60")];
const ARC_ACTIONS: &[ActionSpec] = &[
    act('I', "Controller details", C, View, &["GETCONFIG {ctl} AD"]),
    act('N', "Create logical drive", C, High, &["CREATE {ctl} LOGICALDRIVE MAX {level} {drives*} noprompt"]).with(&[choice("level", "RAID level", ARC_LEVELS), drives("drives", "Drives")]),
    act('i', "Logical drive details", Vd, View, &["GETCONFIG {ctl} LD {vd}"]),
    act('X', "Delete logical drive", Vd, High, &["DELETE {ctl} LOGICALDRIVE {vd} noprompt"]),
    act('i', "Drive details", Pd, View, &["GETCONFIG {ctl} PD {ch} {dev}"]),
    act('L', "Locate LED (60 s)", Pd, Low, &["IDENTIFY {ctl} DEVICE {ch} {dev} TIME 60"]).led(true),
    act('s', "Make hot spare", Pd, Medium, &["SETSTATE {ctl} DEVICE {ch} {dev} HSP noprompt"]).roles(&[Role::Unconfigured]),
    act('S', "Remove hot spare", Pd, Medium, &["SETSTATE {ctl} DEVICE {ch} {dev} RDY noprompt"]).roles(&[Role::HotSpare]),
];

// ---------------------------------------------------------------------------
// Broadcom sas2ircu / sas3ircu
// ---------------------------------------------------------------------------

const IRCU_LEVELS: &[(&str, &str)] = &[("RAID1", "RAID 1"), ("RAID10", "RAID 10"), ("RAID1E", "RAID 1E"), ("RAID0", "RAID 0")];
const IRCU_ACTIONS: &[ActionSpec] = &[
    act('I', "Controller details", C, View, &["{ctl} DISPLAY"]),
    act('T', "Volume status", C, View, &["{ctl} STATUS"]),
    act('N', "Create volume", C, High, &["{ctl} CREATE {level} MAX {drives*} noprompt"]).with(&[choice("level", "RAID level", IRCU_LEVELS), drives("drives", "Drives")]),
    act('X', "Delete volume", Vd, High, &["{ctl} DELETEVOLUME {vd} noprompt"]),
    act('L', "Locate LED on", Pd, Low, &["{ctl} LOCATE {enc}:{slot} ON"]).led(true),
    act('K', "Locate LED off", Pd, Low, &["{ctl} LOCATE {enc}:{slot} OFF"]).led(false),
    act('s', "Make hot spare", Pd, Medium, &["{ctl} HOTSPARE {enc}:{slot}"]).roles(&[Role::Jbod, Role::Unconfigured]),
    act('S', "Remove hot spare", Pd, Medium, &["{ctl} HOTSPARE DELETE {enc}:{slot}"]).roles(&[Role::HotSpare]),
];

// ---------------------------------------------------------------------------
// nvme-cli
// ---------------------------------------------------------------------------

const NVME_ACTIONS: &[ActionSpec] = &[
    act('I', "Identify controller", C, View, &["id-ctrl /dev/{ctl} -H"]),
    act('T', "SMART / health log", C, View, &["smart-log /dev/{ctl}"]),
    act('E', "Error log", C, View, &["error-log /dev/{ctl} -e 16"]),
    act('W', "Firmware log", C, View, &["fw-log /dev/{ctl}"]),
    act('Y', "Self-test log", C, View, &["self-test-log /dev/{ctl}"]),
    act('t', "Start short self-test", C, Low, &["device-self-test /dev/{ctl} -s 1"]),
    act('e', "Start extended self-test", C, Low, &["device-self-test /dev/{ctl} -s 2"]),
    act('A', "Abort self-test", C, Low, &["device-self-test /dev/{ctl} -s 0xf"]),
    act('s', "Rescan namespaces", C, Low, &["ns-rescan /dev/{ctl}"]),
    act('Z', "Sanitize: block erase (whole drive)", C, High, &["sanitize /dev/{ctl} -a 2"]),
    act('C', "Sanitize: crypto erase (whole drive)", C, High, &["sanitize /dev/{ctl} -a 4"]),
    act('i', "Identify namespace", Vd, View, &["id-ns /dev/{ns} -H"]),
    act('F', "Format namespace with secure erase", Vd, High, &["format /dev/{ns} --ses=1 --force"]),
    act('f', "Format namespace (no erase)", Vd, High, &["format /dev/{ns} --ses=0 --force"]),
    act('X', "Delete namespace", Vd, High, &["delete-ns /dev/{ctl} -n {nsid}"]),
    act('i', "SMART / health log", Pd, View, &["smart-log /dev/{ctl}"]),
];

// ---------------------------------------------------------------------------
// Others
// ---------------------------------------------------------------------------

const HIRAID_ACTIONS: &[ActionSpec] = &[
    act('I', "Controller details", C, View, &["c{ctl} show info"]),
    act('T', "Status", C, View, &["c{ctl} show status"]),
    act('i', "Virtual drive details", Vd, View, &["c{ctl}:vd{vd} show"]),
    act('X', "Delete virtual drive", Vd, High, &["c{ctl}:vd{vd} delete noprompt"]),
];

const ARECA_ACTIONS: &[ActionSpec] = &[act('I', "System information", C, View, &["sys info"]), act('E', "Event log", C, View, &["event info"]), act('i', "Drive details", Pd, View, &["disk info drv={pd}"])];

const HPT_ACTIONS: &[ActionSpec] = &[act('I', "Controllers", C, View, &["-u RAID -p hpt query controllers"]), act('i', "Array details", Vd, View, &["-u RAID -p hpt query arrays {vd}"]), act('i', "Drive details", Pd, View, &["-u RAID -p hpt query devices {pd}"])];

const MV_ACTIONS: &[ActionSpec] = &[act('I', "Adapter details", C, View, &["info -o hba"]), act('i', "SMART", Pd, View, &["smart -p {pd}"])];

const TW_ACTIONS: &[ActionSpec] = &[
    act('I', "Controller details", C, View, &["/c{ctl} show all"]),
    act('E', "Alarms", C, View, &["/c{ctl} show alarms"]),
    act('i', "Drive details", Pd, View, &["/c{ctl}/p{pd} show all"]),
    act('L', "Locate LED on", Pd, Low, &["/c{ctl}/p{pd} set identify=on"]).led(true),
    act('K', "Locate LED off", Pd, Low, &["/c{ctl}/p{pd} set identify=off"]).led(false),
];

macro_rules! mr {
    ($id:expr, $name:expr, $group:expr, $bins:expr, $dirs:expr, $drivers:expr, $install:expr, $tested:expr) => {
        ToolSpec {
            id: $id,
            name: $name,
            group: $group,
            bins: $bins,
            dirs: $dirs,
            drivers: $drivers,
            pci_vendors: &[],
            dialect: Dialect::Storcli,
            queries: MR_QUERIES,
            states: MR_STATES,
            drive_sep: ",",
            actions: MR_ACTIONS,
            install: $install,
            tested: $tested,
        }
    };
}

pub static TOOLS: &[ToolSpec] = &[
    mr!("storcli2", "Broadcom MegaRAID 9600 (StorCLI2)", "megaraid8", &["storcli2"], &["/opt/MegaRAID/storcli2", "/opt/broadcom/storcli2"], &["mpi3mr"], BROADCOM, false),
    mr!("perccli2", "Dell PERC 12 (PERCCLI2)", "megaraid8", &["perccli2"], &["/opt/MegaRAID/perccli2", "/opt/dell/perccli2"], &["mpi3mr"], "Dell PERCCLI2: https://www.dell.com/support (search \"PERCCLI\")", false),
    mr!("storcli", "Broadcom MegaRAID (StorCLI)", "megaraid", &["storcli64", "storcli"], &["/opt/MegaRAID/storcli", "/opt/lsi/storcli", "/opt/MegaRAID/StorCLI"], &["megaraid_sas"], BROADCOM, true),
    mr!("perccli", "Dell PERC (PERCCLI)", "megaraid", &["perccli64", "perccli"], &["/opt/MegaRAID/perccli", "/opt/lsi/perccli", "/opt/dell/perccli"], &["megaraid_sas"], "Dell PERCCLI: https://www.dell.com/support (search \"PERCCLI\")", false),
    ToolSpec {
        id: "ssacli",
        name: "HPE Smart Array (ssacli)",
        group: "smartpqi",
        bins: &["ssacli", "hpssacli", "hpacucli"],
        dirs: &["/opt/smartstorageadmin/ssacli/bin", "/usr/sbin"],
        drivers: &["smartpqi", "hpsa"],
        pci_vendors: &["0x103c", "0x1590"],
        dialect: Dialect::Ssacli,
        queries: &[("config", "ctrl all show config detail")],
        states: &[("ok", Good), ("interim recovery mode", Warning), ("ready for rebuild", Warning), ("rebuilding", Warning), ("predictive failure", Warning), ("failed", Critical)],
        drive_sep: ",",
        actions: SSA_ACTIONS,
        install: "HPE ssacli: https://support.hpe.com (search \"ssacli\"), or the HPE MCP repository",
        tested: false,
    },
    ToolSpec {
        id: "arcconf",
        name: "Microchip / Adaptec (arcconf)",
        group: "smartpqi",
        bins: &["arcconf"],
        dirs: &["/usr/Arcconf", "/opt/Arcconf", "/usr/StorMan"],
        drivers: &["smartpqi", "aacraid"],
        pci_vendors: &[],
        dialect: Dialect::Arcconf,
        queries: &[("list", "LIST"), ("config", "GETCONFIG {ctl} AL")],
        states: &[("optimal", Good), ("online", Good), ("ready", Good), ("hot spare", Good), ("raw (pass through)", Good), ("degraded", Warning), ("rebuilding", Warning), ("failed", Critical), ("offline", Critical)],
        drive_sep: " ",
        actions: ARC_ACTIONS,
        install: "Microchip arcconf: https://www.microchip.com (search \"ARCCONF\")",
        tested: false,
    },
    ToolSpec {
        id: "sas3ircu",
        name: "Broadcom SAS3 HBA (sas3ircu)",
        group: "mpt3sas",
        bins: &["sas3ircu"],
        dirs: &["/opt/lsi", "/opt/sas3ircu"],
        drivers: &["mpt3sas"],
        pci_vendors: &[],
        dialect: Dialect::Ircu,
        queries: &[("list", "LIST"), ("display", "{ctl} DISPLAY")],
        states: IRCU_STATES,
        drive_sep: " ",
        actions: IRCU_ACTIONS,
        install: "Broadcom SAS3IRCU: https://www.broadcom.com/support/download-search (search \"SAS3IRCU\")",
        tested: false,
    },
    ToolSpec {
        id: "sas2ircu",
        name: "Broadcom SAS2 HBA (sas2ircu)",
        group: "mpt2sas",
        bins: &["sas2ircu"],
        dirs: &["/opt/lsi"],
        drivers: &["mpt2sas", "mpt3sas"],
        pci_vendors: &[],
        dialect: Dialect::Ircu,
        queries: &[("list", "LIST"), ("display", "{ctl} DISPLAY")],
        states: IRCU_STATES,
        drive_sep: " ",
        actions: IRCU_ACTIONS,
        install: "Broadcom SAS2IRCU: https://www.broadcom.com/support/download-search (search \"SAS2IRCU\")",
        tested: false,
    },
    ToolSpec {
        id: "hiraidadm",
        name: "Huawei SP186 / SP686C (hiraidadm)",
        group: "hiraid",
        bins: &["hiraidadm"],
        dirs: &["/opt/hiraidadm", "/opt/huawei/hiraidadm"],
        drivers: &["hiraid"],
        pci_vendors: &[],
        dialect: Dialect::Hiraidadm,
        queries: &[("ctrls", "show allctrl"), ("vdlist", "c{ctl} show vdlist"), ("pdlist", "c{ctl} show pdlist"), ("status", "c{ctl} show status")],
        states: &[("normal", Good), ("online", Good), ("degraded", Warning), ("rebuilding", Warning), ("fault", Critical), ("offline", Critical)],
        drive_sep: ",",
        actions: HIRAID_ACTIONS,
        install: "Huawei hiraidadm: https://support.huawei.com (search \"hiraidadm\")",
        tested: false,
    },
    ToolSpec {
        id: "areca",
        name: "Areca (cli64)",
        group: "arcmsr",
        bins: &["cli64", "cli32"],
        dirs: &["/opt/areca", "/usr/local/areca"],
        drivers: &["arcmsr"],
        pci_vendors: &[],
        dialect: Dialect::Areca,
        queries: &[("sys", "sys info"), ("vsf", "vsf info"), ("disk", "disk info")],
        states: &[("normal", Good), ("checking", Good), ("degraded", Warning), ("rebuilding", Warning), ("incompleted", Critical), ("failed", Critical)],
        drive_sep: ",",
        actions: ARECA_ACTIONS,
        install: "Areca CLI: https://www.areca.com.tw/support/downloads.html",
        tested: false,
    },
    ToolSpec {
        id: "hptraidconf",
        name: "HighPoint RAID (hptraidconf)",
        group: "highpoint",
        bins: &["hptraidconf", "hptcli"],
        dirs: &["/usr/share/hpt", "/opt/hpt"],
        drivers: &["hptiop", "hptnvme", "rr3740a", "rr272x_1x", "rr2840a"],
        pci_vendors: &[],
        dialect: Dialect::Hpt,
        queries: &[("arrays", "-u RAID -p hpt query arrays"), ("devices", "-u RAID -p hpt query devices")],
        states: &[("normal", Good), ("verifying", Good), ("single", Good), ("raid", Good), ("spare", Good), ("rebuilding", Warning), ("initializing", Warning), ("critical", Critical), ("disabled", Critical)],
        drive_sep: ",",
        actions: HPT_ACTIONS,
        install: "HighPoint CLI: https://www.highpoint-tech.com (product downloads)",
        tested: false,
    },
    ToolSpec {
        id: "mvcli",
        name: "Marvell / Dell BOSS (mvcli)",
        group: "boss",
        bins: &["mvcli", "mnv_cli", "mnvcli"],
        dirs: &["/opt/mvcli", "/opt/dell/boss", "/opt/marvell"],
        drivers: &["ahci"],
        pci_vendors: &["0x1b4b"],
        dialect: Dialect::Mvcli,
        queries: &[("hba", "info -o hba"), ("vd", "info -o vd"), ("pd", "info -o pd")],
        states: &[("functional", Good), ("normal", Good), ("degrade", Warning), ("rebuilding", Warning), ("offline", Critical)],
        drive_sep: ",",
        actions: MV_ACTIONS,
        install: "Dell BOSS mvcli: https://www.dell.com/support (search \"BOSS mvcli\")",
        tested: false,
    },
    ToolSpec {
        id: "tw_cli",
        name: "3ware (tw_cli)",
        group: "3ware",
        bins: &["tw_cli", "tw-cli"],
        dirs: &["/opt/3ware"],
        drivers: &["3w-9xxx", "3w-sas", "3w-xxxx"],
        pci_vendors: &[],
        dialect: Dialect::Twcli,
        queries: &[("list", "show"), ("show", "/c{ctl} show")],
        states: &[("ok", Good), ("verifying", Good), ("initializing", Warning), ("rebuilding", Warning), ("degraded", Warning), ("ecc-error", Warning), ("inoperable", Critical)],
        drive_sep: ",",
        actions: TW_ACTIONS,
        install: "3ware tw_cli (Broadcom legacy downloads)",
        tested: false,
    },
    ToolSpec {
        id: "nvme",
        name: "NVMe (nvme-cli)",
        group: "nvme",
        bins: &["nvme"],
        dirs: SBIN,
        drivers: &["nvme"],
        pci_vendors: &[],
        dialect: Dialect::Nvme,
        queries: &[("list", "list -v -o json"), ("list_flat", "list -o json"), ("smart", "smart-log /dev/{ctl} -o json")],
        states: &[],
        drive_sep: ",",
        actions: NVME_ACTIONS,
        install: "nvme-cli: apt install nvme-cli",
        tested: true,
    },
    // controllers without a supported tool: only listed with a hint
    ToolSpec {
        id: "sssraid",
        name: "3SNIC SSSRAID",
        group: "3snic",
        bins: &[],
        dirs: &[],
        drivers: &["sssraid"],
        pci_vendors: &[],
        dialect: Dialect::Storcli,
        queries: &[],
        states: &[],
        drive_sep: ",",
        actions: &[],
        install: "3SNIC controllers: management tool not supported yet; drive health via smartctl -d sssraid",
        tested: false,
    },
];

const IRCU_STATES: &[(&str, Health)] = &[
    ("okay (oky)", Good),
    ("optimal (opt)", Good),
    ("ready (rdy)", Good),
    ("available (avl)", Good),
    ("standby (sby)", Good),
    ("hot spare (hsp)", Good),
    ("degraded (dgd)", Warning),
    ("out of sync (osy)", Warning),
    ("failed (fld)", Critical),
    ("missing (mis)", Critical),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{parse_detect, render};

    #[test]
    fn every_template_renders() {
        // all placeholders an object can provide
        let vars: Vec<(String, String)> = [
            "ctl", "vd", "dg", "pd", "enc", "slot", "ch", "dev", "array", "ns", "nsid", "level", "drives", "pdperarray", "name", "strip", "wcache", "rcache", "caching", "target_array",
        ]
        .iter()
        .map(|k| (k.to_string(), "1".to_string()))
        .collect();
        for t in TOOLS {
            assert!(t.group.len() > 1);
            for a in t.actions {
                for c in a.cmds {
                    render(c, &vars).unwrap_or_else(|e| panic!("{} {}: {e}", t.id, a.label));
                }
            }
            let mut keys: Vec<(char, Target)> = t.actions.iter().map(|a| (a.key, a.target)).collect();
            keys.sort_by_key(|k| (k.0, k.1 as u8));
            keys.dedup();
            assert_eq!(keys.len(), t.actions.len(), "{}: duplicate key for one target", t.id);
            for a in t.actions {
                assert!(!matches!(a.key, 'h' | 'j' | 'k' | 'l' | 'r' | 'a' | 'g' | 'm' | 'G' | ' '), "{}: {} uses a reserved key", t.id, a.label);
            }
        }
    }

    #[test]
    fn detection_picks_tools_for_present_controllers() {
        let out = "PCI 0000:1c:00.0 0x010400 0x1000 0x005d 0x19e5 0xda07 megaraid_sas\n\
                   PCI 0000:86:00.0 0x010802 0x144d 0xa824 0x144d 0xa812 nvme\n\
                   PCI 0000:00:17.0 0x010601 0x8086 0xa182 0x8086 0x7270 ahci\n\
                   PCI 0000:5e:00.0 0x010700 0x9005 0x028f 0x103c 0x0602 smartpqi\n\
                   BIN /opt/MegaRAID/storcli/storcli64\nBIN /usr/sbin/nvme\nBIN /usr/sbin/arcconf\n";
        let d = parse_detect(TOOLS, out);
        assert_eq!(d.pci.len(), 3, "plain AHCI is not listed");
        let ids: Vec<&str> = d.tools.iter().map(|t| t.spec.id).collect();
        assert_eq!(ids, vec!["storcli", "arcconf", "nvme"]);
        assert!(d.missing.is_empty());
        // without storcli the MegaRAID controller is reported as missing a tool
        let d = parse_detect(TOOLS, &out.replace("BIN /opt/MegaRAID/storcli/storcli64\n", ""));
        assert_eq!(d.missing.len(), 1);
        assert_eq!(d.missing[0].1.id, "storcli");
    }
}
