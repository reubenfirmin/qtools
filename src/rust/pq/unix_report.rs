//! Unix-domain socket parsing and process attribution.

use std::collections::{HashMap, HashSet};
use std::fs;

use crate::pq::cluster;
use crate::pq::net;
use crate::pq::proc;
use crate::pq::ranked_report::{Report, Row, ValueKind};

#[derive(Debug, Clone)]
struct UnixSocket { inode: u64, kind: &'static str, state: &'static str, path: String }

pub fn build(procs: &[proc::Proc], allowed: &HashSet<i32>, path_filter: Option<&str>) -> Report {
    let raw = fs::read_to_string("/proc/net/unix").unwrap_or_default();
    let sockets = parse(&raw);
    let inode_owners = net::inode_map(&procs.iter().map(|p| p.pid).collect::<Vec<_>>());
    let identities = cluster::effective_identities(procs);
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    let mut unattributed = Vec::new();
    for socket in sockets.into_iter().filter(|s| path_filter.is_none_or(|needle| s.path.contains(needle))) {
        match inode_owners.get(&socket.inode).copied() {
            Some(pid) if allowed.contains(&pid) => {
                let identity = identities.get(&pid).cloned().unwrap_or_else(|| format!("pid {pid}"));
                groups.entry(identity).or_default().push(format!("pid {}  {} {}  {}", pid, socket.kind, socket.state,
                    if socket.path.is_empty() { format!("inode {}", socket.inode) } else { socket.path }));
            }
            None if allowed.len() == procs.len() => unattributed.push(format!("{} {}  {}", socket.kind, socket.state,
                if socket.path.is_empty() { format!("inode {}", socket.inode) } else { socket.path })),
            _ => {}
        }
    }
    if !unattributed.is_empty() { groups.insert("(unattributed)".into(), unattributed); }
    let mut rows: Vec<Row> = groups.into_iter().map(|(identity, details)| Row { identity,
        value: details.len() as u64, summary: format!("{} Unix socket(s)", details.len()), details }).collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.value));
    Report { mode: "unix", title: "Unix-domain sockets", value_label: "sockets", value_kind: ValueKind::Count,
        rows, empty_message: "no matching Unix-domain sockets" }
}

fn parse(raw: &str) -> Vec<UnixSocket> {
    raw.lines().skip(1).filter_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 7 { return None; }
        let flags = u32::from_str_radix(fields[3], 16).ok()?;
        let kind = match u16::from_str_radix(fields[4], 16).ok()? { 1 => "stream", 2 => "dgram", 5 => "seqpacket", _ => "other" };
        let state_num = u8::from_str_radix(fields[5], 16).ok()?;
        let state = if flags & 0x0001_0000 != 0 { "listen" } else if state_num == 3 { "connected" } else { "unconnected" };
        Some(UnixSocket { inode: fields[6].parse().ok()?, kind, state, path: fields.get(7).unwrap_or(&"").to_string() })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_named_and_abstract_unix_sockets() {
        let raw = "Num RefCount Protocol Flags Type St Inode Path\n000: 2 0 00010000 0001 01 123 /run/a.sock\n001: 2 0 00000000 0002 03 456 @abstract\n";
        let sockets = parse(raw);
        assert_eq!(sockets.len(), 2);
        assert_eq!(sockets[0].state, "listen");
        assert_eq!(sockets[1].path, "@abstract");
    }

    #[test]
    fn live_report_handles_attributed_unattributed_and_filtered_sockets() {
        let process = proc::Proc { pid: std::process::id() as i32, ppid: 1, comm: "self".into(),
            cmdline: vec!["self".into()], rss: 0, swap: 0, uid: 0, cpu_pct: 0.0 };
        let allowed = [process.pid].into_iter().collect();
        let report = build(std::slice::from_ref(&process), &allowed, None);
        assert_eq!(report.mode, "unix");
        let filtered = build(&[process], &allowed, Some("definitely-no-such-qtools-socket"));
        assert!(filtered.rows.is_empty());
    }
}
