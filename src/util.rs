//! Small formatting and JSON helpers shared by all modules.

use serde_json::Value;

pub fn fmt_bytes(b: f64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut v = b.max(0.0);
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0}{}", UNITS[i])
    } else if v >= 100.0 {
        format!("{v:.0}{}", UNITS[i])
    } else {
        format!("{v:.1}{}", UNITS[i])
    }
}

pub fn fmt_rate(bps: f64) -> String {
    format!("{}/s", fmt_bytes(bps))
}

pub fn fmt_duration(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {}s", secs % 60)
    } else {
        format!("{secs}s")
    }
}

pub fn fmt_time(epoch: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(epoch, 0) {
        chrono::LocalResult::Single(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => "-".into(),
    }
}

/// Extract a string-ish value: strings as-is, numbers formatted, null/missing as "".
pub fn js(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Like `js` but with a default for missing/empty values.
pub fn jsd(v: &Value, key: &str, default: &str) -> String {
    let s = js(v, key);
    if s.is_empty() {
        default.to_string()
    } else {
        s
    }
}

pub fn jf(v: &Value, key: &str) -> f64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.parse().unwrap_or(0.0),
        Some(Value::Bool(b)) => *b as u8 as f64,
        _ => 0.0,
    }
}

pub fn ji(v: &Value, key: &str) -> i64 {
    jf(v, key) as i64
}

pub fn jb(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(s)) => s == "1" || s == "true",
        _ => false,
    }
}

pub fn jarr(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        _ => Vec::new(),
    }
}

/// Usage bar like `[#####.....]`.
pub fn bar(frac: f64, width: usize) -> String {
    let frac = if frac.is_finite() { frac.clamp(0.0, 1.0) } else { 0.0 };
    let filled = (frac * width as f64).round() as usize;
    format!("{}{}", "#".repeat(filled), ".".repeat(width.saturating_sub(filled)))
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max <= 1 {
        s.chars().take(max).collect()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('~');
        out
    }
}

/// Quote a string for a POSIX shell (used for commands sent over ssh).
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@,+%".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Percent-encode a path segment / query value.
pub fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Repair UTF-8 text that was decoded as Latin-1 somewhere upstream (e.g. "å¯é").
pub fn fix_mojibake(s: &str) -> String {
    if s.chars().all(|c| (c as u32) < 256) && s.chars().any(|c| (c as u32) >= 0x80) {
        let bytes: Vec<u8> = s.chars().map(|c| c as u8).collect();
        if let Ok(t) = String::from_utf8(bytes) {
            return t;
        }
    }
    s.to_string()
}

pub fn read_file(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// True when running on a system console (VT like /dev/tty1 or a serial console /dev/ttyS0),
/// where the console is a permanent service and must not be quit; false for ssh / terminal
/// emulators (/dev/pts/*).
pub fn on_system_console() -> bool {
    let name = unsafe {
        let p = libc::ttyname(0);
        if p.is_null() {
            return false;
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().to_string()
    };
    if name == "/dev/console" {
        return true;
    }
    let rest = name.strip_prefix("/dev/tty").unwrap_or("");
    let num = rest.trim_start_matches(|c: char| c.is_ascii_uppercase());
    !rest.is_empty() && !num.is_empty() && num.chars().all(|c| c.is_ascii_digit())
}

pub fn is_root_process() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Check a local account password with pam_unix's `unix_chkpwd` helper (reads /etc/shadow,
/// supports every hash the system does) instead of linking libpam.
pub fn check_local_password(user: &str, password: &str) -> anyhow::Result<bool> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let helper = ["/usr/sbin/unix_chkpwd", "/sbin/unix_chkpwd"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .ok_or_else(|| anyhow::anyhow!("unix_chkpwd not found"))?;
    let mut child = Command::new(helper)
        .args([user, "nullok"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    {
        // The helper reads the password NUL-terminated from stdin (max 512 bytes).
        let mut stdin = child.stdin.take().expect("piped stdin");
        let mut buf = password.as_bytes().to_vec();
        buf.push(0);
        let _ = stdin.write_all(&buf);
    }
    Ok(child.wait()?.success())
}

pub fn local_hostname() -> String {
    let h = read_file("/etc/hostname");
    let h = h.trim();
    if h.is_empty() {
        read_file("/proc/sys/kernel/hostname").trim().to_string()
    } else {
        h.split('.').next().unwrap_or(h).to_string()
    }
}

/// Random alphanumeric string (for temporary VNC passwords etc.).
pub fn random_token(len: usize) -> String {
    let mut buf = vec![0u8; len];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
    buf.iter().map(|b| CHARS[*b as usize % CHARS.len()] as char).collect()
}

/// IPv4/IPv6 addresses of the host (excluding loopback and link-local), via `ip -j addr`.
pub fn local_addresses() -> Vec<(String, String)> {
    let out = std::process::Command::new("ip")
        .args(["-j", "addr", "show"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    let mut res = Vec::new();
    for iface in jarr(&v) {
        let name = js(&iface, "ifname");
        if name == "lo" {
            continue;
        }
        for a in iface.get("addr_info").map(jarr).unwrap_or_default() {
            let ip = js(&a, "local");
            let scope = js(&a, "scope");
            if scope == "global" && !ip.is_empty() {
                res.push((name.clone(), format!("{}/{}", ip, js(&a, "prefixlen"))));
            }
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        assert_eq!(fmt_bytes(0.0), "0B");
        assert_eq!(fmt_bytes(1536.0), "1.5K");
        assert_eq!(fmt_bytes(512110190592.0), "477G");
    }

    #[test]
    fn quote() {
        assert_eq!(shell_quote("abc"), "abc");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn enc() {
        assert_eq!(urlenc("PVE:root@pam:1"), "PVE%3Aroot%40pam%3A1");
    }
}
