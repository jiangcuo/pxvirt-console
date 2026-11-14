//! One parser per output format.

pub mod areca;
pub mod arcconf;
pub mod hiraidadm;
pub mod hpt;
pub mod ircu;
pub mod mvcli;
pub mod nvme;
pub mod ssacli;
pub mod storcli;
pub mod twcli;

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::spec::Found;
use crate::Runner;

/// Run a named query of the tool (with `{ctl}` filled in).
pub(crate) fn query(r: &dyn Runner, f: &Found, key: &str, ctl: &str) -> Result<String> {
    let q = f.spec.query(key).ok_or_else(|| anyhow!("{}: no query {key}", f.spec.id))?;
    r.run(&f.path, &crate::args(q, ctl))
}

/// JSON document inside a tool's output (some print a banner first).
pub(crate) fn json_of(s: &str) -> Result<Value> {
    let start = s.find(['{', '[']).ok_or_else(|| anyhow!("no JSON in output: {}", s.trim().lines().next().unwrap_or("")))?;
    Ok(serde_json::from_str(&s[start..])?)
}

/// String value of any JSON scalar (trimmed).
pub(crate) fn s(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(x)) => x.trim().to_string(),
        Some(Value::Null) | None => String::new(),
        Some(x) => x.to_string(),
    }
}

/// Flatten a JSON object of scalars into props.
pub(crate) fn props_of(v: &Value) -> Vec<(String, String)> {
    match v {
        Value::Object(m) => m
            .iter()
            .filter(|(_, x)| !x.is_object() && !x.is_array())
            .map(|(k, x)| (k.trim().to_string(), match x {
                Value::String(s) => s.trim().to_string(),
                o => o.to_string(),
            }))
            .collect(),
        _ => vec![],
    }
}

/// Human size from bytes.
pub(crate) fn bytes(b: f64) -> String {
    let u = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = b;
    let mut i = 0;
    while v >= 1000.0 && i + 1 < u.len() {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", u[i])
    } else {
        format!("{v:.2} {}", u[i])
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use std::cell::RefCell;

    /// Answers commands from fixture files: (args joined by spaces, output).
    pub struct Fake {
        pub answers: Vec<(String, String)>,
        pub calls: RefCell<Vec<String>>,
    }

    impl Fake {
        pub fn new(answers: &[(&str, &str)]) -> Self {
            Self { answers: answers.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(), calls: RefCell::new(vec![]) }
        }
    }

    impl Runner for Fake {
        fn run(&self, _prog: &str, args: &[String]) -> Result<String> {
            let a = args.join(" ");
            self.calls.borrow_mut().push(a.clone());
            self.answers.iter().find(|(k, _)| *k == a).map(|(_, v)| v.clone()).ok_or_else(|| anyhow!("no answer for {a}"))
        }
    }

    pub fn found(id: &str) -> Found {
        Found { spec: crate::spec(id).expect("spec"), path: format!("/usr/sbin/{id}") }
    }

    pub fn fixture(p: &str) -> String {
        std::fs::read_to_string(format!("{}/fixtures/{p}", env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{p}: {e}"))
    }
}
