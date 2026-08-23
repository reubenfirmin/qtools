//! Explicit, permission-sensitive proportional-memory view using smaps_rollup.

use std::collections::{HashMap, HashSet};
use std::fs;

use crate::pq::cluster;
use crate::pq::proc;
use crate::pq::ranked_report::{Report, Row, ValueKind};

#[derive(Default)]
struct Smaps { pss: u64, private: u64, shared: u64, anonymous: u64, swap_pss: u64 }

pub fn build(procs: &[proc::Proc], allowed: &HashSet<i32>) -> Report {
    let identities = cluster::effective_identities(procs);
    let mut groups: HashMap<String, (Smaps, Vec<String>)> = HashMap::new();
    for process in procs.iter().filter(|p| allowed.contains(&p.pid)) {
        let Some(smaps) = read_smaps(process.pid) else { continue };
        let identity = identities.get(&process.pid).cloned().unwrap_or_else(|| process.comm.clone());
        let entry = groups.entry(identity).or_default();
        entry.0.pss += smaps.pss; entry.0.private += smaps.private; entry.0.shared += smaps.shared;
        entry.0.anonymous += smaps.anonymous; entry.0.swap_pss += smaps.swap_pss;
        entry.1.push(format!("pid {}  pss {}  private {}  shared {}  anon {}  swap pss {}", process.pid,
            human(smaps.pss), human(smaps.private), human(smaps.shared), human(smaps.anonymous), human(smaps.swap_pss)));
    }
    let mut rows: Vec<Row> = groups.into_iter().map(|(identity, (smaps, details))| Row {
        identity, value: smaps.pss,
        summary: format!("private {}  shared {}  anon {}  swap {}", human(smaps.private), human(smaps.shared), human(smaps.anonymous), human(smaps.swap_pss)), details
    }).collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.value));
    Report { mode: "pss", title: "proportional memory", value_label: "pss", value_kind: ValueKind::Bytes,
        rows, empty_message: "no proportional-memory data readable for matching processes" }
}

fn read_smaps(pid: i32) -> Option<Smaps> {
    let raw = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    Some(parse_smaps(&raw))
}

fn parse_smaps(raw: &str) -> Smaps {
    let mut out = Smaps::default();
    for line in raw.lines() {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let bytes = rest.split_whitespace().next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
        match name {
            "Pss" => out.pss = bytes,
            "Private_Clean" | "Private_Dirty" | "Private_Hugetlb" => out.private += bytes,
            "Shared_Clean" | "Shared_Dirty" | "Shared_Hugetlb" => out.shared += bytes,
            "Anonymous" => out.anonymous = bytes,
            "SwapPss" => out.swap_pss = bytes,
            _ => {}
        }
    }
    out
}

fn human(bytes: u64) -> String { crate::style::human_plain(bytes) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_sums_smaps_rollup_fields() {
        let parsed = parse_smaps("Pss: 10 kB\nPrivate_Clean: 2 kB\nPrivate_Dirty: 3 kB\nShared_Clean: 4 kB\nAnonymous: 7 kB\nSwapPss: 1 kB\n");
        assert_eq!(parsed.pss, 10 * 1024);
        assert_eq!(parsed.private, 5 * 1024);
        assert_eq!(parsed.shared, 4 * 1024);
        assert_eq!(parsed.anonymous, 7 * 1024);
    }
}
