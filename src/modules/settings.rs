//! System > Settings: advanced node settings. Time: time zone, NTP servers (chrony), sync
//! state and sources, sync now, manual date / time, hardware clock.

use std::collections::HashMap;

use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    text::{Line, Span},
    Frame,
};
use serde_json::Value;

use super::{fv, scroll, View};
use crate::ctx::{ActionOutcome, Ctx};
use crate::i18n::tr;
use crate::loader::Loader;
use crate::ui::form::{self, Form};
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, loading_or_error, style_bad, style_dim, style_good, style_label, style_warn};
use crate::util::js;

/// NTP servers added by the console go here (chrony reads sources.d on Debian 12+).
pub const OUR_SOURCES: &str = "/etc/chrony/sources.d/pxvirt-console.sources";
const CHRONY_CONF: &str = "/etc/chrony/chrony.conf";
/// Marker for pool / server lines of chrony.conf that the console disabled.
const MARK: &str = "#pxvirt-console# ";

const SCRIPT: &str = r#"export LC_ALL=C
echo '@@td'; timedatectl show 2>/dev/null
echo '@@date'; date '+%Y-%m-%d %H:%M:%S %Z (%z)'; date -u '+%Y-%m-%d %H:%M:%S UTC'
echo '@@chrony'; command -v chronyc >/dev/null 2>&1 && systemctl is-active chrony 2>/dev/null || echo missing
echo '@@tracking'; chronyc -n tracking 2>/dev/null
echo '@@sources'; chronyc -n sources 2>/dev/null
echo '@@conf'; grep -E "^[[:space:]]*(#pxvirt-console# )?(pool|server|peer|sourcedir)[[:space:]]" /etc/chrony/chrony.conf 2>/dev/null
echo '@@ours'; cat /etc/chrony/sources.d/pxvirt-console.sources 2>/dev/null
true
"#;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Source {
    /// ^ server, = peer, # local clock
    pub mode: char,
    /// * synced, + combined, - not combined, ? unusable, x falseticker, ~ too variable
    pub state: char,
    pub name: String,
    pub stratum: String,
    pub reach: String,
    pub last: String,
}

/// `chronyc -n sources`: "^* 192.168.1.1   2   6   377    33   +12us[  +15us] +/-   20ms"
pub fn parse_sources(text: &str) -> Vec<Source> {
    text.lines()
        .filter_map(|l| {
            let mut c = l.chars();
            let (mode, state) = (c.next()?, c.next()?);
            if !"^=#".contains(mode) {
                return None;
            }
            let w: Vec<&str> = l[2..].split_whitespace().collect();
            (w.len() >= 5).then(|| {
                // "+12us[  +15us] +/-   20ms" -> "offset +12us ±20ms"
                let rest = w[5..].join(" ");
                let off = rest.split('[').next().unwrap_or("").trim();
                let err = rest.rsplit(' ').next().unwrap_or("");
                let last = if off.is_empty() || off == "-" { String::new() } else { format!("offset {off} ±{err}") };
                Source { mode, state, name: w[0].into(), stratum: w[1].into(), reach: w[3].into(), last }
            })
        })
        .collect()
}

pub fn source_state(c: char) -> (&'static str, Style) {
    match c {
        '*' => ("synced to", style_good()),
        '+' => ("combined", Style::default()),
        '-' => ("not combined", style_dim()),
        '?' => ("unreachable", style_bad()),
        'x' => ("false ticker", style_bad()),
        '~' => ("too variable", style_warn()),
        _ => ("", Style::default()),
    }
}

/// NTP server names: host names and IP addresses only (they go into a config file).
pub fn valid_server(s: &str) -> bool {
    !s.is_empty() && s.len() <= 253 && s.chars().all(|c| c.is_ascii_alphanumeric() || ".-:_".contains(c)) && !s.starts_with('-')
}

/// Shell script that writes the console's NTP sources and (de)activates those of chrony.conf.
pub fn ntp_script(servers: &[String], keep_default: bool) -> String {
    let mut s = String::from("set -e\nmkdir -p /etc/chrony/sources.d\n");
    if servers.is_empty() {
        s.push_str(&format!("rm -f {OUR_SOURCES}\n"));
    } else {
        let lines: Vec<String> = servers.iter().map(|x| format!("server {x} iburst")).collect();
        s.push_str(&format!("printf '%s\\n' '# managed by pxvirt-console' {} > {OUR_SOURCES}\n", lines.iter().map(|l| format!("'{l}'")).collect::<Vec<_>>().join(" ")));
    }
    s.push_str(&format!("[ -f {CHRONY_CONF}.pxvirt-console.bak ] || cp {CHRONY_CONF} {CHRONY_CONF}.pxvirt-console.bak\n"));
    if keep_default {
        s.push_str(&format!("sed -i 's/^{MARK}//' {CHRONY_CONF}\n"));
    } else {
        s.push_str(&format!("sed -i -E 's/^((pool|server|peer)[[:space:]])/{MARK}\\1/' {CHRONY_CONF}\n"));
    }
    s.push_str(&format!("grep -qE '^sourcedir[[:space:]]+/etc/chrony/sources.d' {CHRONY_CONF} || echo 'sourcedir /etc/chrony/sources.d' >> {CHRONY_CONF}\n"));
    s.push_str("systemctl restart chrony\necho 'NTP servers saved, chrony restarted'\n");
    s
}

#[derive(Default, Clone)]
struct Data {
    td: HashMap<String, String>,
    date: Vec<String>,
    chrony: String,
    tracking: HashMap<String, String>,
    sources: Vec<Source>,
    /// configured in chrony.conf: (line, disabled by the console)
    conf: Vec<(String, bool)>,
    ours: Vec<String>,
    /// API view (also works without host access)
    api: Value,
}

#[derive(Default)]
pub struct Settings {
    data: Loader<Data>,
    zones: Loader<Vec<String>>,
    table: Table,
    scroll: u16,
    pending_time: Option<String>,
}

fn kvmap(text: &str, sep: char) -> HashMap<String, String> {
    text.lines().filter_map(|l| l.split_once(sep).map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))).collect()
}

impl Settings {
    fn d(&self) -> Option<&Data> {
        self.data.data.as_ref()
    }

    fn host_ok(&self, ctx: &Ctx) -> Result<(), String> {
        ctx.need_root()?;
        ctx.need_exec("Sys.Modify").map(|_| ())
    }

    fn chrony_ok(&self, ctx: &Ctx) -> Result<(), String> {
        self.host_ok(ctx)?;
        match self.d() {
            Some(d) if d.chrony == "missing" => Err(tr("chrony is not installed").into()),
            Some(_) => Ok(()),
            None => Err(tr("loading").into()),
        }
    }

    fn run(ctx: &mut Ctx, title: &str, script: String) {
        let Some(e) = ctx.exec() else { return };
        ctx.act(title, move || {
            let out = e.output("sh", &["-c", &script])?;
            let last = out.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("OK").trim().to_string();
            Ok(ActionOutcome::Msg(last))
        });
    }

    fn timezone(&self) -> String {
        self.d().map(|d| d.td.get("Timezone").cloned().unwrap_or_else(|| js(&d.api, "timezone"))).unwrap_or_default()
    }
}

impl View for Settings {
    fn title(&self) -> &'static str {
        "Settings"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        let exec = ctx.exec();
        self.data.load(move || {
            let mut d = Data { api: api.get(&format!("/nodes/{node}/time")).unwrap_or(Value::Null), ..Default::default() };
            if let Some(e) = exec {
                let s = crate::bmc::sections(&e.output("sh", &["-c", SCRIPT])?);
                let sec = |n: &str| s.get(n).cloned().unwrap_or_default();
                d.td = kvmap(&sec("td"), '=');
                d.date = sec("date").lines().map(|l| l.to_string()).collect();
                d.chrony = sec("chrony").trim().to_string();
                d.tracking = kvmap(&sec("tracking"), ':');
                d.sources = parse_sources(&sec("sources"));
                d.conf = sec("conf").lines().map(|l| (l.trim().trim_start_matches(MARK).to_string(), l.trim().starts_with(MARK))).collect();
                d.ours = sec("ours").lines().filter(|l| l.starts_with("server")).filter_map(|l| l.split_whitespace().nth(1).map(|x| x.to_string())).collect();
            }
            Ok(d)
        });
        if self.zones.data.is_none() {
            if let Some(e) = ctx.exec() {
                self.zones.load(move || Ok(e.output("timedatectl", &["list-timezones"])?.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()));
            }
        }
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        self.data.poll() | self.zones.poll()
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(10)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let status = loading_or_error(&self.data.error, self.data.loading() && self.data.data.is_none());
        let mut rows: Vec<Row> = Vec::new();
        if let Some(d) = self.d() {
            let head = |t: &str| Row::new(format!("#{t}")).cs(tr(t), style_label()).c("");
            let kv = |k: &str, v: String, st: Style| Row::new(k.to_string()).c(format!("  {}", tr(k))).cs(v, st);
            rows.push(head("Time"));
            rows.push(kv("Time zone", self.timezone(), Style::default()));
            if let Some(l) = d.date.first() {
                rows.push(kv("Local time", l.clone(), Style::default()));
            } else {
                rows.push(kv("Local time", crate::util::fmt_time(js(&d.api, "localtime").parse().unwrap_or(0)), Style::default()));
            }
            if let Some(l) = d.date.get(1) {
                rows.push(kv("UTC", l.clone(), Style::default()));
            }
            if let Some(rtc) = d.td.get("RTCTimeUSec").filter(|v| !v.is_empty() && *v != "n/a") {
                let local = d.td.get("LocalRTC").map(|v| v == "yes").unwrap_or(false);
                rows.push(kv("Hardware clock", format!("{rtc} ({})", if local { tr("local time") } else { "UTC" }), if local { style_warn() } else { Style::default() }));
            }
            if ctx.exec().is_some() {
                rows.push(head("NTP"));
                let synced = d.td.get("NTPSynchronized").map(|v| v == "yes").unwrap_or(false);
                let (svc, st) = match d.chrony.as_str() {
                    "active" => ("chrony, active".to_string(), style_good()),
                    "missing" => (tr("chrony is not installed").to_string(), style_bad()),
                    s => (format!("chrony, {s}"), style_bad()),
                };
                rows.push(kv("NTP service", svc, st));
                rows.push(kv("Synchronized", tr(if synced { "yes" } else { "no" }).to_string(), if synced { style_good() } else { style_warn() }));
                if let Some(t) = d.tracking.get("System time") {
                    rows.push(kv("Offset", t.clone(), Style::default()));
                }
                if let Some(t) = d.tracking.get("Reference ID") {
                    rows.push(kv("Reference", format!("{t}  stratum {}", d.tracking.get("Stratum").cloned().unwrap_or_default()), Style::default()));
                }
                let mut servers: Vec<String> = d.ours.iter().map(|s| format!("{s} ({})", tr("console"))).collect();
                servers.extend(d.conf.iter().filter(|c| !c.0.starts_with("sourcedir")).map(|(l, off)| if *off { format!("{l} ({})", tr("disabled")) } else { l.clone() }));
                rows.push(kv("NTP servers", if servers.is_empty() { "-".into() } else { servers.join(", ") }, Style::default()));
                if !d.sources.is_empty() {
                    rows.push(head("NTP sources"));
                    for s in &d.sources {
                        let (label, st) = source_state(s.state);
                        rows.push(Row::new(format!("src:{}", s.name)).c(format!("  {}", s.name)).cs(format!("{}  stratum {}  reach {}  {}", tr(label), s.stratum, s.reach, s.last), st));
                    }
                }
            }
        }
        let cols = [col("Setting", Constraint::Length(28)), col("Value", Constraint::Min(30))];
        self.table.draw(f, area, "Settings", &cols, &rows, focused, status);
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let sel = self.table.selected().unwrap_or("").to_string();
        let say = |lines: &mut Vec<Line<'static>>, s: &str| lines.push(Line::from(tr(s).to_string()));
        match sel.as_str() {
            "Time zone" => say(&mut lines, "z: choose the time zone. Guests and logs use the node's time zone; the cluster itself works in UTC."),
            "Local time" | "UTC" => say(&mut lines, "t: set the date and time by hand (only useful without NTP); s: step the clock to NTP time now."),
            "Hardware clock" => say(&mut lines, "w: write the system time to the hardware clock (RTC). The RTC should keep UTC."),
            "NTP servers" => {
                say(&mut lines, "n: set the NTP servers. The console writes them to");
                lines.push(Line::from(format!("  {OUR_SOURCES}")));
                say(&mut lines, "and can switch off the default sources of chrony.conf (a backup is kept as chrony.conf.pxvirt-console.bak).");
            }
            s if s.starts_with("src:") => {
                if let Some(x) = self.d().and_then(|d| d.sources.iter().find(|x| format!("src:{}", x.name) == s)) {
                    let (label, st) = source_state(x.state);
                    lines.push(Line::from(vec![Span::styled(format!("{}  ", x.name), style_label()), Span::styled(tr(label).to_string(), st)]));
                    lines.push(Line::from(format!("stratum {}  reach {} ({})  {}", x.stratum, x.reach, tr("octal, 377 = last 8 polls answered"), x.last)));
                }
                lines.push(Line::from(""));
                say(&mut lines, "* synced to, + combined, - not combined, ? unreachable, x false ticker, ~ too variable");
            }
            _ => {
                if let Some(d) = self.d() {
                    if !d.tracking.is_empty() {
                        lines.push(Line::from(Span::styled("chronyc tracking", style_label())));
                        for k in ["Reference ID", "Stratum", "System time", "Last offset", "RMS offset", "Frequency", "Leap status"] {
                            if let Some(v) = d.tracking.get(k) {
                                lines.push(Line::from(format!("{k:<14} {v}")));
                            }
                        }
                    }
                }
            }
        }
        if ctx.exec().is_none() {
            lines.push(Line::from(""));
            say(&mut lines, "Only the time zone can be changed without host access (local or --ssh).");
        }
        draw_lines(f, area, "Details", focused, lines, self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let mut v = vec![item('z', "Time zone").require(ctx.need_node_priv("Sys.Modify"))];
        if ctx.exec().is_none() {
            return v;
        }
        if self.d().map(|d| d.chrony == "missing").unwrap_or(false) {
            v.push(item('I', "Install chrony (NTP)").require(self.host_ok(ctx)));
        }
        v.extend([
            item('n', "NTP servers").require(self.chrony_ok(ctx)),
            item('s', "Sync now (step the clock to NTP time)").require(self.chrony_ok(ctx)),
            item('S', "Restart the NTP service (chrony)").require(self.chrony_ok(ctx)),
            item('t', "Set date and time by hand").require(self.host_ok(ctx)),
            item('w', "Write system time to the hardware clock").require(self.host_ok(ctx)),
        ]);
        v
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        match key {
            'z' => {
                let cur = self.timezone();
                let field = match &self.zones.data {
                    Some(z) if !z.is_empty() => form::select("tz", "Time zone", form::opts(z), &cur),
                    _ => form::text("tz", "Time zone", &cur).help("e.g. Asia/Shanghai, Europe/Berlin, UTC"),
                };
                ctx.popup(Popup::Form(Form::new("Time zone", "tz", vec![field])));
            }
            'n' => {
                let d = self.d().cloned().unwrap_or_default();
                let keep = d.conf.iter().any(|(l, off)| !off && !l.starts_with("sourcedir"));
                ctx.popup(Popup::Form(Form::new(
                    "NTP servers",
                    "ntp",
                    vec![
                        form::text("servers", "Servers", &d.ours.join(" ")).help("host names or addresses, separated by spaces"),
                        form::check("keep", "Also use the sources of chrony.conf (Debian pool)", keep || d.ours.is_empty()),
                    ],
                )));
            }
            's' => ctx.popup(Popup::confirm("Sync now", "makestep", vec![tr("Step the clock to NTP time at once (instead of slowly slewing it).").into(), tr("Running programs see the time jump.").into()], true)),
            'S' => Self::run(ctx, "Restart chrony", "systemctl restart chrony && echo 'chrony restarted'".into()),
            'I' => crate::modules::updates::Updates::suspend_apt(ctx, vec!["install".into(), "chrony".into()], format!("apt-get install chrony on {}\n", ctx.node)),
            't' => {
                let now = self.d().and_then(|d| d.date.first().map(|l| l.chars().take(19).collect::<String>())).unwrap_or_default();
                ctx.popup(Popup::Form(Form::new("Set date and time", "time", vec![form::text("t", "Local time", &now).help("YYYY-MM-DD HH:MM:SS")])));
            }
            'w' => Self::run(ctx, "Write hardware clock", "hwclock --systohc && echo 'hardware clock set from the system time'".into()),
            _ => {}
        }
    }

    fn on_form(&mut self, tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        match tag {
            "tz" => {
                let tz = fv(v, "tz").trim().to_string();
                let known = self.zones.data.as_ref().map(|z| z.is_empty() || z.contains(&tz)).unwrap_or(true);
                if tz.is_empty() || !known || !tz.chars().all(|c| c.is_ascii_alphanumeric() || "/_-+".contains(c)) {
                    return ctx.toast_err(format!("{}: {tz}", tr("unknown time zone")));
                }
                let n = ctx.node.clone();
                ctx.api_call(format!("Time zone {tz}"), "PUT", format!("/nodes/{n}/time"), vec![("timezone", tz)]);
            }
            "ntp" => {
                let servers: Vec<String> = fv(v, "servers").split([' ', ',', ';']).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                if let Some(bad) = servers.iter().find(|s| !valid_server(s)) {
                    return ctx.toast_err(format!("{}: {bad}", tr("not a host name or address")));
                }
                let keep = fv(v, "keep") == "1";
                if servers.is_empty() && !keep {
                    return ctx.toast_err(tr("no NTP source left"));
                }
                Self::run(ctx, "NTP servers", ntp_script(&servers, keep));
            }
            "time" => {
                let t = fv(v, "t").trim();
                if chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S").is_err() {
                    return ctx.toast_err(tr("format: YYYY-MM-DD HH:MM:SS"));
                }
                self.pending_time = Some(t.to_string());
                let synced = self.d().map(|d| d.chrony == "active").unwrap_or(false);
                let mut body = vec![format!("{} {t}", tr("Set the clock to"))];
                if synced {
                    body.push(tr("chrony is running and will pull the clock back to NTP time.").into());
                }
                ctx.popup(Popup::confirm("Set date and time", "settime", body, true));
            }
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        match tag {
            "makestep" => Self::run(ctx, "Sync time now", "chronyc -a makestep >/dev/null && chronyc -n tracking | grep 'System time'".into()),
            "settime" => {
                // validated above: digits, '-', ':' and one space only
                if let Some(t) = self.pending_time.take() {
                    Self::run(ctx, "Set date and time", format!("date -s '{t}' >/dev/null && hwclock --systohc && date"));
                }
            }
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.data.clear();
        self.zones.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrony_sources() {
        let s = "MS Name/IP address         Stratum Poll Reach LastRx Last sample\n===============================================================================\n^* 192.168.18.1                  2   6   377    33   +12us[  +15us] +/-   20ms\n^? 10.0.0.9                      0   7     0     -     +0ns[   +0ns] +/-    0ns\n";
        let v = parse_sources(s);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].state, v[0].name.as_str(), v[0].reach.as_str()), ('*', "192.168.18.1", "377"));
        assert_eq!(v[0].last, "offset +12us ±20ms");
        assert_eq!((v[1].state, v[1].last.as_str()), ('?', "offset +0ns ±0ns"));
    }

    #[test]
    fn ntp_config_script() {
        assert!(valid_server("ntp.aliyun.com") && valid_server("10.0.0.1") && valid_server("fd00::1"));
        assert!(!valid_server("a;reboot") && !valid_server("-x") && !valid_server("a b"));
        let s = ntp_script(&["ntp1.example.com".into(), "10.0.0.1".into()], false);
        assert!(s.contains("'server ntp1.example.com iburst' 'server 10.0.0.1 iburst' > /etc/chrony/sources.d/pxvirt-console.sources"));
        assert!(s.contains("s/^((pool|server|peer)[[:space:]])/#pxvirt-console# \\1/"));
        assert!(ntp_script(&[], true).contains("rm -f /etc/chrony/sources.d/pxvirt-console.sources"));
    }
}
