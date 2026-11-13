//! Local rescue operations that must work without the PVE API:
//! management network reset (with backup/rollback) and root password reset.

use std::fs;
use std::process::Command;

#[derive(Clone, Default, Debug)]
pub struct NetworkInfo {
    pub ip: String,
    pub netmask: String,
    pub gateway: String,
    pub iface: String,
}

#[derive(Clone, Debug)]
pub struct NetIface {
    pub name: String,
    pub mac: String,
    pub has_carrier: bool,
}

pub fn scrub_string(value: &mut String) {
    let mut bytes = std::mem::take(value).into_bytes();
    bytes.fill(0);
}

/// Reset a local password with chpasswd (works without pveproxy / pmxcfs).
pub fn chpasswd(user: &str, password: &str) -> Result<(), String> {
    let mut input = format!("{user}:{password}");
    let result = Command::new("chpasswd")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(input.as_bytes())?;
            }
            child.wait()
        });
    scrub_string(&mut input);
    match result {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err("chpasswd failed".into()),
        Err(e) => Err(format!("chpasswd: {e}")),
    }
}

pub fn build_network_config(iface_name: &str, info: &NetworkInfo) -> String {
    let cidr = netmask_to_cidr_num(&info.netmask);
    format!(
        r#"auto lo
iface lo inet loopback

auto {iface_name}
iface {iface_name} inet manual

auto vmbr0
iface vmbr0 inet static
    address {}/{cidr}
    gateway {}
    bridge-ports {iface_name}
    bridge-stp off
    bridge-fd 0
"#,
        info.ip, info.gateway
    )
}

fn backup_interfaces_dir(timestamp: &str) -> Result<Option<String>, String> {
    let interfaces_d = std::path::Path::new("/etc/network/interfaces.d");
    if !interfaces_d.exists() {
        return Ok(None);
    }
    let backup_dir = format!("/etc/network/interfaces.d.bak.{timestamp}");
    fs::create_dir_all(&backup_dir).map_err(|e| format!("interfaces.d backup dir failed: {e}"))?;
    for entry in fs::read_dir(interfaces_d).map_err(|e| format!("Read interfaces.d failed: {e}"))? {
        let entry = entry.map_err(|e| format!("Read interfaces.d entry failed: {e}"))?;
        let file_type = entry.file_type().map_err(|e| format!("Read interfaces.d entry type failed: {e}"))?;
        if file_type.is_dir() {
            return Err("interfaces.d contains a directory; refusing automatic reset".to_string());
        }
        let target = std::path::Path::new(&backup_dir).join(entry.file_name());
        fs::copy(entry.path(), &target).map_err(|e| format!("interfaces.d backup copy failed: {e}"))?;
    }
    Ok(Some(backup_dir))
}

fn clear_interfaces_dir() -> Result<(), String> {
    let interfaces_d = std::path::Path::new("/etc/network/interfaces.d");
    if !interfaces_d.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(interfaces_d).map_err(|e| format!("Read interfaces.d failed: {e}"))? {
        let entry = entry.map_err(|e| format!("Read interfaces.d entry failed: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            return Err("interfaces.d contains a directory; refusing automatic reset".to_string());
        }
        fs::remove_file(&path).map_err(|e| format!("Clear interfaces.d failed: {e}"))?;
    }
    Ok(())
}

fn restore_interfaces_dir(backup_dir: Option<&str>) -> Result<(), String> {
    let interfaces_d = std::path::Path::new("/etc/network/interfaces.d");
    fs::create_dir_all(interfaces_d).map_err(|e| format!("Recreate interfaces.d failed: {e}"))?;
    clear_interfaces_dir()?;
    if let Some(backup_dir) = backup_dir {
        for entry in fs::read_dir(backup_dir).map_err(|e| format!("Read interfaces.d backup failed: {e}"))? {
            let entry = entry.map_err(|e| format!("Read interfaces.d backup entry failed: {e}"))?;
            let target = interfaces_d.join(entry.file_name());
            fs::copy(entry.path(), target).map_err(|e| format!("Restore interfaces.d failed: {e}"))?;
        }
    }
    Ok(())
}

/// Replace /etc/network/interfaces with a single vmbr0 on `iface_name`, ifreload, roll back on failure.
pub fn apply_network_config(iface_name: &str, info: &NetworkInfo) -> Result<String, String> {
    validate_network_config(info)?;
    let config = build_network_config(iface_name, info);
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let backup_path = format!("/etc/network/interfaces.bak.{timestamp}");

    fs::copy("/etc/network/interfaces", &backup_path).map_err(|e| format!("Backup failed: {e}"))?;
    let interfaces_d_backup = backup_interfaces_dir(&timestamp)?;

    let apply_result = (|| -> Result<(), String> {
        fs::write("/etc/network/interfaces", &config).map_err(|e| format!("Write failed: {e}"))?;
        clear_interfaces_dir()?;
        let status = Command::new("ifreload").arg("-a").status().map_err(|e| format!("ifreload failed: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err("ifreload returned error".to_string())
        }
    })();

    match apply_result {
        Ok(()) => Ok(backup_path),
        Err(apply_error) => {
            let rollback_interfaces = fs::copy(&backup_path, "/etc/network/interfaces").map_err(|e| format!("Rollback failed: {e}"));
            let rollback_dir = restore_interfaces_dir(interfaces_d_backup.as_deref());
            if rollback_interfaces.is_ok() && rollback_dir.is_ok() {
                let _ = Command::new("ifreload").arg("-a").status();
                Err(format!("{apply_error}; configuration was rolled back"))
            } else {
                let mut details = vec![apply_error];
                if let Err(e) = rollback_interfaces {
                    details.push(e);
                }
                if let Err(e) = rollback_dir {
                    details.push(e);
                }
                Err(details.join("; "))
            }
        }
    }
}

/// Current management IP: the interface carrying the default route.
pub fn get_network_info() -> NetworkInfo {
    let mut info = NetworkInfo::default();
    if let Ok(output) = Command::new("ip").args(["route", "show", "default"]).output() {
        let line = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 5 {
            info.gateway = parts.get(2).unwrap_or(&"").to_string();
            info.iface = parts.get(4).unwrap_or(&"").to_string();
        }
    }
    if !info.iface.is_empty() {
        if let Ok(output) = Command::new("ip").args(["-o", "-4", "addr", "show", &info.iface]).output() {
            let line = String::from_utf8_lossy(&output.stdout);
            for part in line.split_whitespace() {
                if part.contains('/') && part.contains('.') {
                    if let Some((ip, mask)) = part.split_once('/') {
                        info.ip = ip.to_string();
                        info.netmask = cidr_to_netmask(mask.parse().unwrap_or(24));
                    }
                    break;
                }
            }
        }
    }
    info
}

pub fn cidr_to_netmask(cidr: u8) -> String {
    let mask: u32 = if cidr >= 32 { !0 } else if cidr == 0 { 0 } else { !0 << (32 - cidr) };
    format!("{}.{}.{}.{}", (mask >> 24) & 0xFF, (mask >> 16) & 0xFF, (mask >> 8) & 0xFF, mask & 0xFF)
}

pub fn netmask_to_cidr_num(netmask: &str) -> u8 {
    netmask.split('.').filter_map(|s| s.parse::<u8>().ok()).map(|b| b.count_ones() as u8).sum()
}

fn is_valid_netmask(value: &str) -> bool {
    let Ok(addr) = value.parse::<std::net::Ipv4Addr>() else { return false };
    let inverted = !u32::from(addr);
    (inverted & inverted.wrapping_add(1)) == 0
}

fn parse_ipv4_field(value: &str, field_name: &str) -> Result<std::net::Ipv4Addr, String> {
    value.parse::<std::net::Ipv4Addr>().map_err(|_| format!("Invalid {field_name}"))
}

pub fn validate_network_config(info: &NetworkInfo) -> Result<(), String> {
    let ip = parse_ipv4_field(&info.ip, "IP address")?;
    let gateway = parse_ipv4_field(&info.gateway, "gateway")?;
    if !is_valid_netmask(&info.netmask) {
        return Err("Invalid netmask".to_string());
    }
    let netmask = parse_ipv4_field(&info.netmask, "netmask")?;
    let mask = u32::from(netmask);
    let prefix = mask.count_ones();
    if prefix == 0 || prefix >= 31 {
        return Err("Netmask must be between /1 and /30".to_string());
    }
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return Err("IP address must be a usable unicast host address".to_string());
    }
    if gateway.is_unspecified() || gateway.is_loopback() || gateway.is_multicast() {
        return Err("Gateway must be a usable unicast host address".to_string());
    }
    let ip_u32 = u32::from(ip);
    let gateway_u32 = u32::from(gateway);
    let network = ip_u32 & mask;
    let broadcast = network | !mask;
    if ip_u32 == network || ip_u32 == broadcast {
        return Err("IP address cannot be the network or broadcast address".to_string());
    }
    if gateway_u32 == network || gateway_u32 == broadcast {
        return Err("Gateway cannot be the network or broadcast address".to_string());
    }
    if ip == gateway {
        return Err("IP address cannot be the same as gateway".to_string());
    }
    if (gateway_u32 & mask) != network {
        return Err("Gateway must be in the same subnet as the IP address".to_string());
    }
    Ok(())
}

/// Physical NICs with link state (brings links up to detect carrier).
pub fn detect_physical_interfaces() -> Vec<NetIface> {
    let mut interfaces = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if ["lo", "veth", "vmbr", "tap", "fwbr", "fwpr", "fwln"].iter().any(|p| name.starts_with(p)) {
                continue;
            }
            if !std::path::Path::new(&format!("/sys/class/net/{name}/device")).exists() {
                continue;
            }
            let mac = fs::read_to_string(format!("/sys/class/net/{name}/address")).unwrap_or_default().trim().to_string();
            let _ = Command::new("ip").args(["link", "set", &name, "up"]).status();
            std::thread::sleep(std::time::Duration::from_millis(500));
            let carrier = fs::read_to_string(format!("/sys/class/net/{name}/carrier"))
                .unwrap_or_default()
                .trim()
                .parse::<u8>()
                .unwrap_or(0);
            interfaces.push(NetIface { name, mac, has_carrier: carrier == 1 });
        }
    }
    interfaces.sort_by(|a, b| b.has_carrier.cmp(&a.has_carrier).then(a.name.cmp(&b.name)));
    interfaces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netmask() {
        assert_eq!(cidr_to_netmask(24), "255.255.255.0");
        assert_eq!(netmask_to_cidr_num("255.255.252.0"), 22);
        assert!(is_valid_netmask("255.255.255.0"));
        assert!(!is_valid_netmask("255.0.255.0"));
    }

    #[test]
    fn validate() {
        let ok = NetworkInfo { ip: "192.168.1.10".into(), netmask: "255.255.255.0".into(), gateway: "192.168.1.1".into(), iface: String::new() };
        assert!(validate_network_config(&ok).is_ok());
        let bad = NetworkInfo { gateway: "10.0.0.1".into(), ..ok };
        assert!(validate_network_config(&bad).is_err());
    }
}
