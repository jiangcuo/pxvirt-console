//! Glue between the pxvirt-raid library and the console: runs the vendor tools through an
//! [`Exec`] (local or ssh) and caches the problems for the alert counter.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pxvirt_raid::{Controller, Detection, Found, Runner};

use crate::exec::Exec;

pub struct ExecRunner<'a>(pub &'a Exec);

impl Runner for ExecRunner<'_> {
    fn run(&self, prog: &str, args: &[String]) -> anyhow::Result<String> {
        let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        Ok(self.0.output_any(prog, &a)?.1)
    }
}

/// Everything found on one host.
#[derive(Default)]
pub struct Host {
    pub det: Detection,
    pub ctls: Vec<(Found, Vec<Controller>)>,
    pub errors: Vec<String>,
}

impl Host {
    pub fn problems(&self) -> Vec<String> {
        let mut v: Vec<String> = self.ctls.iter().flat_map(|(_, c)| c.iter().flat_map(|c| c.problems())).map(|p| p.text).collect();
        v.extend(self.errors.iter().cloned());
        v
    }
}

/// Detect controllers and tools, then read every tool.
pub fn scan_host(exec: &Exec) -> anyhow::Result<Host> {
    let (_, out) = exec.output_any("sh", &["-c", &pxvirt_raid::detect_script()])?;
    let det = pxvirt_raid::detect(&out);
    let mut host = Host::default();
    let r = ExecRunner(exec);
    for f in det.tools.clone() {
        match pxvirt_raid::scan(&r, &f) {
            Ok(mut c) => {
                // locate / fault LEDs from the kernel SES enclosures
                pxvirt_raid::attach_ses(&mut c, &det.ses);
                host.ctls.push((f, c))
            }
            Err(e) => host.errors.push(format!("{}: {e:#}", f.spec.name)),
        }
    }
    host.det = det;
    store(exec, host.problems());
    Ok(host)
}

static CACHE: Mutex<Option<HashMap<String, (Instant, Vec<String>)>>> = Mutex::new(None);

fn store(exec: &Exec, problems: Vec<String>) {
    let mut c = CACHE.lock().unwrap();
    c.get_or_insert_with(HashMap::new).insert(exec.label(), (Instant::now(), problems));
}

/// Problems for the alert counter: a full scan at most every 5 minutes.
pub fn problems_cached(exec: &Exec) -> Vec<String> {
    if let Some((t, p)) = CACHE.lock().unwrap().as_ref().and_then(|m| m.get(&exec.label()).cloned()) {
        if t.elapsed() < Duration::from_secs(300) {
            return p;
        }
    }
    match scan_host(exec) {
        Ok(h) => h.problems(),
        Err(_) => {
            store(exec, vec![]);
            vec![]
        }
    }
}
