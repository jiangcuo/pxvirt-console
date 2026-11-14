//! Helpers for the text output of vendor tools.

/// Split `key : value` / `key: value` at the first separator (" : " preferred, keys may contain ':').
pub fn kv(line: &str) -> Option<(String, String)> {
    let l = line.trim();
    let (k, v) = l.split_once(" : ").or_else(|| l.split_once(": ")).or_else(|| l.split_once(':'))?;
    let k = k.trim();
    if k.is_empty() {
        return None;
    }
    Some((k.to_string(), v.trim().to_string()))
}

/// First value whose key contains one of `keys` (case-insensitive, in order of `keys`).
pub fn pick(props: &[(String, String)], keys: &[&str]) -> String {
    for want in keys {
        let w = want.to_lowercase();
        if let Some((_, v)) = props.iter().find(|(k, _)| k.to_lowercase() == w) {
            return v.clone();
        }
    }
    for want in keys {
        let w = want.to_lowercase();
        if let Some((_, v)) = props.iter().find(|(k, _)| k.to_lowercase().contains(&w)) {
            return v.clone();
        }
    }
    String::new()
}

/// Leading integer of a string ("37C (98.6 F)" -> 37).
pub fn int(s: &str) -> Option<i64> {
    let t = s.trim();
    let end = t.char_indices().find(|(i, c)| !(c.is_ascii_digit() || (*i == 0 && *c == '-'))).map(|(i, _)| i).unwrap_or(t.len());
    t[..end].parse().ok()
}

/// Line of an indented listing with its more-indented children.
#[derive(Clone, Debug, Default)]
pub struct Node {
    pub text: String,
    pub kids: Vec<Node>,
}

impl Node {
    /// key/value pairs of the direct children that have no children themselves
    pub fn props(&self) -> Vec<(String, String)> {
        self.kids.iter().filter(|k| k.kids.is_empty()).filter_map(|k| kv(&k.text)).collect()
    }

    /// all nodes below (depth first) with their parent chain texts
    pub fn walk<'a>(&'a self, path: &mut Vec<&'a str>, f: &mut dyn FnMut(&'a Node, &[&'a str])) {
        f(self, path);
        path.push(&self.text);
        for k in &self.kids {
            k.walk(path, f);
        }
        path.pop();
    }
}

/// Build a tree from indentation (blank lines ignored).
pub fn tree(text: &str) -> Vec<Node> {
    let lines: Vec<(usize, &str)> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| (l.len() - l.trim_start().len(), l.trim()))
        .collect();
    fn build(lines: &[(usize, &str)], i: &mut usize, indent: usize) -> Vec<Node> {
        let mut out = Vec::new();
        while *i < lines.len() && lines[*i].0 >= indent {
            let (ind, text) = lines[*i];
            *i += 1;
            let kids = if *i < lines.len() && lines[*i].0 > ind { build(lines, i, lines[*i].0) } else { Vec::new() };
            out.push(Node { text: text.to_string(), kids });
        }
        out
    }
    let mut i = 0;
    let mut out = Vec::new();
    while i < lines.len() {
        let ind = lines[i].0;
        out.extend(build(&lines, &mut i, ind));
    }
    out
}

/// Groups of non-blank lines.
pub fn blocks(text: &str) -> Vec<Vec<&str>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for l in text.lines() {
        if l.trim().is_empty() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(l);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Table below a header line: values are cut at the start columns of the header words.
/// Separator lines (----, ====) are skipped; the table ends at a blank line.
pub fn table(text: &str, is_header: impl Fn(&str) -> bool) -> Vec<Vec<(String, String)>> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(h) = lines.iter().position(|l| is_header(l)) else { return vec![] };
    let header = lines[h];
    let mut cols: Vec<(usize, String)> = Vec::new();
    let mut start = None;
    for (i, c) in header.char_indices().chain(std::iter::once((header.len(), ' '))) {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                cols.push((s, header[s..i].to_string()));
                start = None;
            }
            _ => {}
        }
    }
    let mut rows = Vec::new();
    for l in &lines[h + 1..] {
        let t = l.trim();
        if t.is_empty() {
            if rows.is_empty() {
                continue;
            }
            break;
        }
        if t.chars().all(|c| matches!(c, '-' | '=' | ' ' | '+')) {
            continue;
        }
        let chars: Vec<char> = l.chars().collect();
        let row: Vec<(String, String)> = cols
            .iter()
            .enumerate()
            .map(|(n, (s, name))| {
                let end = cols.get(n + 1).map(|c| c.0).unwrap_or(chars.len()).min(chars.len());
                let s = (*s).min(chars.len());
                (name.clone(), chars[s..end].iter().collect::<String>().trim().to_string())
            })
            .collect();
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_and_int() {
        assert_eq!(kv("   Status of volume : Okay (OKY)"), Some(("Status of volume".into(), "Okay (OKY)".into())));
        assert_eq!(kv("Reported Channel,Device(T:L) : 0,0(0:0)"), Some(("Reported Channel,Device(T:L)".into(), "0,0(0:0)".into())));
        assert_eq!(kv("Slot: 0"), Some(("Slot".into(), "0".into())));
        assert_eq!(int(" 37C (98.60 F)"), Some(37));
        assert_eq!(int("x"), None);
    }

    #[test]
    fn indentation_tree() {
        let t = "Ctrl in Slot 0\n   Status: OK\n   Array: A\n      Logical Drive: 1\n         Status: OK\n      physicaldrive 1I:1:1\n         Status: OK\n";
        let n = tree(t);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].props(), vec![("Status".to_string(), "OK".to_string())]);
        assert_eq!(n[0].kids[1].kids.len(), 2);
    }

    #[test]
    fn column_table() {
        let t = "VDID  Name   Type   Status\n----  ----   ----   ------\n0     data   RAID1  Normal\n1            RAID5  Degraded\n\nother";
        let r = table(t, |l| l.starts_with("VDID"));
        assert_eq!(r.len(), 2);
        assert_eq!(r[1][1].1, "");
        assert_eq!(r[1][3].1, "Degraded");
    }
}
