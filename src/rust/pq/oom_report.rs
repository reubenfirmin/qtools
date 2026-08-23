//! Relative OOM-killer preference from the kernel's per-process score and adjustment.

use std::collections::HashSet;
use std::fs;

use crate::pq::cluster;
use crate::pq::proc;
use crate::pq::ranked_report::{Report, Row, ValueKind};

pub fn build(procs: &[proc::Proc], allowed: &HashSet<i32>) -> Report {
    let identities = cluster::effective_identities(procs);
    let mut rows = Vec::new();
    for process in procs.iter().filter(|process| allowed.contains(&process.pid)) {
        let Some(score) = read_number::<u64>(process.pid, "oom_score") else { continue };
        let adjustment = read_number::<i32>(process.pid, "oom_score_adj").unwrap_or(0);
        let identity = identities.get(&process.pid).cloned().unwrap_or_else(|| process.comm.clone());
        let protection = if adjustment == -1000 { "protected" } else if adjustment < 0 { "less preferred" }
            else if adjustment > 0 { "more preferred" } else { "default adjustment" };
        rows.push(Row {
            identity: format!("{identity} (pid {})", process.pid), value: score,
            summary: format!("mem {}  adjustment {adjustment}  {protection}", crate::style::human_plain(process.rss)),
            details: vec![format!("pid {}  oom_score {}  oom_score_adj {}  {}", process.pid, score,
                adjustment, crate::style::truncate_middle(&process.cmdline.join(" "), 200))],
        });
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.value));
    Report { mode: "oom", title: "relative OOM-kill preference", value_label: "oom score",
        value_kind: ValueKind::Score, rows, empty_message: "no OOM scores readable for matching processes" }
}

fn read_number<T: std::str::FromStr>(pid: i32, name: &str) -> Option<T> {
    fs::read_to_string(format!("/proc/{pid}/{name}")).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_process_oom_metadata() {
        let pid = std::process::id() as i32;
        assert!(read_number::<u64>(pid, "oom_score").is_some());
        assert!(read_number::<i32>(pid, "oom_score_adj").is_some());
    }
}
