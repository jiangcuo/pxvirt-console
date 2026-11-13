//! System module: overview, power, users, time, DNS/hosts, updates, processes, certificates.

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
use crate::ui::{draw_lines, loading_or_error, style_bad, style_dim, style_good, style_warn, Kv, ACCENT};
use crate::util::{fmt_time, jarr, jb, ji, js};

pub fn views() -> Vec<Box<dyn View>> {
    vec![
        Box::new(super::memory::Memory::default()),
        Box::new(super::processes::Processes::default()),
        Box::new(super::updates::Updates::default()),
        Box::new(Users::default()),
        Box::new(Certs::default()),
        Box::new(Power::default()),
    ]
}

// ===========================================================================
// Power
// ===========================================================================

#[derive(Default)]
struct Power {
    guests: Loader<(usize, usize)>,
}

impl View for Power {
    fn title(&self) -> &'static str {
        "Power"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        self.guests.load(move || {
            let r = api.get("/cluster/resources?type=vm")?;
            let mine: Vec<Value> = jarr(&r).into_iter().filter(|g| js(g, "node") == node).collect();
            Ok((mine.iter().filter(|g| js(g, "status") == "running").count(), mine.len()))
        });
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        self.guests.poll()
    }

    fn has_detail(&self) -> bool {
        false
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let mut lines = vec![
            Line::from(Span::styled(format!("{}: {}", tr("Node"), ctx.node), Style::default().fg(ACCENT))),
            Line::from(""),
        ];
        if let Some((run, total)) = self.guests.data {
            lines.push(Line::from(format!("{run} / {total} guests running on this node")));
            lines.push(Line::from(""));
        }
        for it in self.menu(ctx) {
            let st = if it.disabled.is_some() { style_dim() } else { Style::default() };
            let mut spans = vec![Span::styled(format!("  ({}) ", it.key), Style::default().fg(ACCENT)), Span::styled(tr(&it.label).to_string(), st)];
            if let Some(d) = it.disabled {
                spans.push(Span::styled(format!("  {d}"), style_dim()));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Reboot/Shutdown ask you to type the node name. Guests are shut down according to their startup/shutdown order.",
            style_dim(),
        )));
        draw_lines(f, area, "Power", focused, lines, 0);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let p = ctx.need_node_priv("Sys.PowerMgmt");
        let vm = ctx.need_node_priv("VM.PowerMgmt");
        let offline: Vec<_> = ctx.nodes.iter().filter(|n| !n.online).collect();
        vec![
            item('R', "Reboot node").require(p.clone()),
            item('S', "Shutdown node").require(p.clone()),
            item('A', "Start all guests (onboot order)").require(vm.clone()),
            item('X', "Shut down all guests").require(vm),
            item('W', "Wake-on-LAN an offline node").require(p).disabled_if(offline.is_empty(), "no offline node"),
        ]
    }

    fn on_menu(&mut self, tag: &str, key: char, ctx: &mut Ctx) {
        if tag == "wol" {
            let off: Vec<String> = ctx.nodes.iter().filter(|n| !n.online).map(|n| n.name.clone()).collect();
            if let Some(n) = off.get(key as usize - 'a' as usize) {
                ctx.api_call(format!("Wake-on-LAN {n}"), "POST", format!("/nodes/{n}/wakeonlan"), vec![]);
            }
            return;
        }
        let node = ctx.node.clone();
        match key {
            'R' => ctx.popup(Popup::confirm_typed(
                "Reboot node",
                "reboot",
                vec![format!("Reboot node {node}?"), format!("{} guests running will be shut down.", self.guests.data.map(|g| g.0).unwrap_or(0))],
                &node,
            )),
            'S' => ctx.popup(Popup::confirm_typed(
                "Shutdown node",
                "shutdown",
                vec![format!("Power off node {node}?"), "You will need physical/IPMI access to start it again.".into()],
                &node,
            )),
            'A' => ctx.popup(Popup::confirm("Start all guests", "startall", vec![format!("Start all guests with onboot=1 on {node}?")], false)),
            'X' => ctx.popup(Popup::confirm_typed("Shut down all guests", "stopall", vec![format!("Shut down ALL guests on {node}?")], &node)),
            'W' => {
                let items: Vec<MenuItem> = ctx
                    .nodes
                    .iter()
                    .filter(|n| !n.online)
                    .enumerate()
                    .map(|(i, n)| item((b'a' + i as u8) as char, &n.name))
                    .collect();
                ctx.popup(Popup::menu("Wake-on-LAN", "wol", items));
            }
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        let n = ctx.node.clone();
        match tag {
            "reboot" => ctx.api_call(format!("Reboot {n}"), "POST", format!("/nodes/{n}/status"), vec![("command", "reboot".into())]),
            "shutdown" => ctx.api_call(format!("Shutdown {n}"), "POST", format!("/nodes/{n}/status"), vec![("command", "shutdown".into())]),
            "startall" => ctx.api_call(format!("Start all guests on {n}"), "POST", format!("/nodes/{n}/startall"), vec![]),
            "stopall" => ctx.api_call(format!("Stop all guests on {n}"), "POST", format!("/nodes/{n}/stopall"), vec![]),
            _ => {}
        }
    }
}

// ===========================================================================
// Users
// ===========================================================================

#[derive(Default)]
struct Users {
    data: Loader<Vec<Value>>,
    table: Table,
    scroll: u16,
}

impl Users {
    fn sel(&self) -> Option<&Value> {
        let k = self.table.selected()?;
        self.data.data.as_ref()?.iter().find(|u| js(u, "userid") == k)
    }
}

impl View for Users {
    fn title(&self) -> &'static str {
        "Users"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        self.data.load(move || Ok(jarr(&api.get("/access/users?full=1")?)));
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        self.data.poll()
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(30)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flatten()
            .map(|u| {
                let en = jb(u, "enable") || u.get("enable").is_none();
                let exp = ji(u, "expire");
                Row::new(js(u, "userid"))
                    .c(js(u, "userid"))
                    .cs(if en { "yes" } else { "no" }, if en { style_good() } else { style_bad() })
                    .c(if exp == 0 { "never".into() } else { fmt_time(exp) })
                    .c(format!("{} {}", js(u, "firstname"), js(u, "lastname")).trim().to_string())
                    .c(js(u, "email"))
                    .dim(!en)
            })
            .collect();
        let cols = [
            col("User", Constraint::Length(22)),
            col("Enabled", Constraint::Length(7)),
            col("Expire", Constraint::Length(19)),
            col("Name", Constraint::Min(10)),
            col("Email", Constraint::Min(10)),
        ];
        let st = loading_or_error(&self.data.error, self.data.loading());
        self.table.draw(f, area, "Users", &cols, &rows, focused, st);
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some(u) = self.sel() {
            kv.add("User", js(u, "userid"));
            kv.add("Realm type", js(u, "realm-type"));
            kv.add("Groups", js(u, "groups"));
            kv.add("Comment", js(u, "comment"));
            let tfa = js(u, "tfa-locked-until");
            if !tfa.is_empty() {
                kv.styled("TFA locked until", fmt_time(tfa.parse().unwrap_or(0)), style_bad());
            }
            let tokens = u.get("tokens").map(jarr).unwrap_or_default();
            if !tokens.is_empty() {
                kv.section("API tokens");
                for t in tokens {
                    kv.add(&js(&t, "tokenid"), format!("privsep={} expire={}", js(&t, "privsep"), js(&t, "expire")));
                }
            }
        }
        draw_lines(f, area, "Details", focused, kv.lines(), self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let Some(u) = self.sel() else { return vec![] };
        let uid = js(u, "userid");
        let en = jb(u, "enable") || u.get("enable").is_none();
        let is_self = uid == ctx.session.user;
        let user_modify = ctx.need_priv(&format!("/access/groups"), "User.Modify");
        let realm = uid.rsplit('@').next().unwrap_or("").to_string();
        vec![
            item('p', "Change password")
                .disabled_if(!(is_self || ctx.session.can("/access", "User.Modify")), "needs User.Modify")
                .disabled_if(!matches!(realm.as_str(), "pam" | "pve"), "realm does not support it"),
            item(if en { 'd' } else { 'e' }, if en { "Disable user" } else { "Enable user" })
                .require(user_modify)
                .disabled_if(uid == "root@pam", "cannot disable root@pam"),
            item('R', "Reset local root password (chpasswd)")
                .require(ctx.need_root())
                .disabled_if(!ctx.this_machine(), "local only"),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let Some(u) = self.sel() else { return };
        let uid = js(u, "userid");
        match key {
            'p' => {
                let mut fields = vec![form::info("User", &uid), form::password("password", "New password"), form::password("confirm", "Confirm password")];
                if !ctx.session.rescue {
                    fields.push(form::password("confirmation", "Your current password").req(false).help("required by PVE when changing passwords"));
                }
                ctx.popup(Popup::Form(Form::new("Change password", &format!("pw:{uid}"), fields)));
            }
            'd' | 'e' => {
                let en = if key == 'e' { "1" } else { "0" };
                ctx.api_call(format!("{} {uid}", if key == 'e' { "Enable" } else { "Disable" }), "PUT", format!("/access/users/{}", crate::util::urlenc(&uid)), vec![("enable", en.into())]);
            }
            'R' => ctx.popup(Popup::Form(Form::new(
                "Reset root password",
                "rootpw",
                vec![
                    form::info("User", "root (local, /etc/shadow)"),
                    form::password("password", "New password"),
                    form::password("confirm", "Confirm password"),
                ],
            ))),
            _ => {}
        }
    }

    fn on_form(&mut self, tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        let pw = fv(v, "password").to_string();
        if pw != fv(v, "confirm") {
            ctx.popup(Popup::error("Password", "Passwords do not match"));
            return;
        }
        if pw.len() < 5 {
            ctx.popup(Popup::error("Password", "Password is too short (min. 5 characters)"));
            return;
        }
        if tag == "rootpw" {
            ctx.act("Reset root password", move || {
                crate::rescue::chpasswd("root", &pw).map_err(|e| anyhow::anyhow!(e))?;
                Ok(ActionOutcome::Msg("root password has been reset".into()))
            });
        } else if let Some(uid) = tag.strip_prefix("pw:") {
            let uid = uid.to_string();
            if ctx.session.rescue && uid.ends_with("@pam") {
                let user = uid.trim_end_matches("@pam").to_string();
                ctx.act(format!("Change password {uid}"), move || {
                    crate::rescue::chpasswd(&user, &pw).map_err(|e| anyhow::anyhow!(e))?;
                    Ok(ActionOutcome::Msg("password changed".into()))
                });
                return;
            }
            let mut params = vec![("userid", uid.clone()), ("password", pw)];
            let c = fv(v, "confirmation");
            if !c.is_empty() {
                params.push(("confirmation-password", c.to_string()));
            }
            ctx.api_call(format!("Change password {uid}"), "PUT", "/access/password".into(), params);
        }
    }

    fn reset(&mut self) {}
}

// ===========================================================================
// Certificates
// ===========================================================================

#[derive(Default)]
struct Certs {
    data: Loader<Vec<Value>>,
    table: Table,
    scroll: u16,
}

impl View for Certs {
    fn title(&self) -> &'static str {
        "Certificates"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        self.data.load(move || Ok(jarr(&api.get(&format!("/nodes/{node}/certificates/info"))?)));
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        self.data.poll()
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(300)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let now = chrono::Utc::now().timestamp();
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flatten()
            .map(|c| {
                let days = (ji(c, "notafter") - now) / 86400;
                let st = if days < 0 { style_bad() } else if days < 30 { style_warn() } else { style_good() };
                Row::new(js(c, "filename")).c(js(c, "filename")).cs(format!("{days} days"), st).c(js(c, "subject"))
            })
            .collect();
        let cols = [col("File", Constraint::Length(24)), col("Expires in", Constraint::Length(12)), col("Subject", Constraint::Min(20))];
        self.table.draw(f, area, "Certificates", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some(c) = self.table.selected().and_then(|k| self.data.data.as_ref()?.iter().find(|c| js(c, "filename") == k)) {
            kv.add("File", js(c, "filename"));
            kv.add("Subject", js(c, "subject"));
            kv.add("Issuer", js(c, "issuer"));
            kv.add("Valid from", fmt_time(ji(c, "notbefore")));
            kv.add("Valid until", fmt_time(ji(c, "notafter")));
            kv.add("Key", format!("{} {}", js(c, "public-key-type"), js(c, "public-key-bits")));
            kv.add("Fingerprint", js(c, "fingerprint"));
            let san: Vec<String> = c.get("san").map(jarr).unwrap_or_default().iter().map(|s| s.as_str().unwrap_or("").to_string()).collect();
            kv.add("SAN", san.join(", "));
        }
        draw_lines(f, area, "Details", focused, kv.lines(), self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        vec![item('R', "Regenerate node certificates (pvecm updatecerts --force)").require(ctx.need_root()).require(ctx.need_exec("Sys.Modify").map(|_| ()))]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        if key == 'R' {
            ctx.popup(Popup::confirm(
                "Regenerate certificates",
                "regen",
                vec!["Regenerate the self-signed node certificate?".into(), "pveproxy will be restarted; Web UI sessions reconnect.".into()],
                true,
            ));
        }
    }

    fn on_confirm(&mut self, _tag: &str, ctx: &mut Ctx) {
        if let Some(e) = ctx.exec() {
            let e2 = e.clone();
            ctx.act("Regenerate certificates", move || {
                e.output("pvecm", &["updatecerts", "--force"])?;
                e2.output("systemctl", &["restart", "pveproxy"])?;
                Ok(ActionOutcome::Msg("certificates regenerated, pveproxy restarted".into()))
            });
        }
    }
}
