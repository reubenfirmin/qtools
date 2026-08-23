//! Directly attributable blocked-task view from /proc/PID/status and wchan.

use std::collections::{HashMap, HashSet};
use std::fs;

use crate::pq::cluster;
use crate::pq::proc;
use crate::pq::ranked_report::{Report, Row, ValueKind};

pub fn build(procs: &[proc::Proc], allowed: &HashSet<i32>) -> Report {
    let identities = cluster::effective_identities(procs);
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for process in procs.iter().filter(|p| allowed.contains(&p.pid)) {
        let status = fs::read_to_string(format!("/proc/{}/status", process.pid)).unwrap_or_default();
        let blocked = status.lines().find_map(|line| line.strip_prefix("State:"))
            .is_some_and(|state| state.trim_start().starts_with('D'));
        if !blocked { continue; }
        let wchan = fs::read_to_string(format!("/proc/{}/wchan", process.pid)).unwrap_or_default();
        let identity = identities.get(&process.pid).cloned().unwrap_or_else(|| process.comm.clone());
        groups.entry(identity).or_default().push(format!("pid {}  wchan {}  {}", process.pid,
            if wchan.trim().is_empty() { "unknown" } else { wchan.trim() }, process.cmdline.join(" ")));
    }
    let mut rows: Vec<Row> = groups.into_iter().map(|(identity, details)| Row {
        identity, value: details.len() as u64, summary: format!("{} blocked task(s)", details.len()), details
    }).collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.value));
    Report { mode: "blocked", title: "blocked processes", value_label: "tasks", value_kind: ValueKind::Count,
        rows, empty_message: "no matching processes are in uninterruptible D state" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_nonblocked_and_missing_processes_produce_an_empty_report() {
        let processes = vec![
            proc::Proc { pid: std::process::id() as i32, ppid: 1, comm: "self".into(), cmdline: vec!["self".into()],
                rss: 0, swap: 0, uid: 0, cpu_pct: 0.0 },
            proc::Proc { pid: i32::MAX, ppid: 1, comm: "gone".into(), cmdline: vec!["gone".into()],
                rss: 0, swap: 0, uid: 0, cpu_pct: 0.0 },
        ];
        let allowed = processes.iter().map(|process| process.pid).collect();
        let report = build(&processes, &allowed);
        assert!(report.rows.is_empty());
        assert_eq!(report.mode, "blocked");
    }
}
