//! Actionable RLIMIT report: pair finite soft limits with live usage when Linux exposes one.

use std::collections::{HashMap, HashSet};
use std::fs;

use crate::cluster;
use crate::proc;
use crate::ranked_report::{Report, Row, ValueKind};

const WARN_PERCENT: u64 = 75;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resource { OpenFiles, Processes, AddressSpace, LockedMemory }

#[derive(Debug, Clone, Copy)]
struct Limit { resource: Resource, soft: u64, hard: Option<u64> }

#[derive(Debug)]
struct Observation { resource: Resource, used: u64, soft: u64, hard: Option<u64>, percent: u64, pid: i32 }

pub fn build(procs: &[proc::Proc], allowed: &HashSet<i32>, show_all: bool) -> Report {
    let identities = cluster::effective_identities(procs);
    let uid_tasks = tasks_by_uid(procs);
    let mut groups: HashMap<String, Vec<Observation>> = HashMap::new();
    for process in procs.iter().filter(|process| allowed.contains(&process.pid)) {
        for limit in read_limits(process.pid) {
            let Some(used) = usage(process, limit.resource, &uid_tasks) else { continue };
            let percent = utilization(used, limit.soft);
            if !show_all && percent < WARN_PERCENT { continue; }
            let identity = identities.get(&process.pid).cloned().unwrap_or_else(|| process.comm.clone());
            groups.entry(identity).or_default().push(Observation {
                resource: limit.resource, used, soft: limit.soft, hard: limit.hard, percent, pid: process.pid,
            });
        }
    }
    let mut rows: Vec<Row> = groups.into_iter().filter_map(|(identity, mut observations)| {
        observations.sort_by_key(|item| std::cmp::Reverse(item.percent));
        let worst = observations.first()?;
        let summary = format!("{} {} / {}  {}%", label(worst.resource), amount(worst.used, worst.resource),
            amount(worst.soft, worst.resource), worst.percent);
        let details = observations.iter().map(|item| format!("pid {}  {}  used {}  soft {}  hard {}  {}%",
            item.pid, label(item.resource), amount(item.used, item.resource), amount(item.soft, item.resource),
            item.hard.map_or_else(|| "unlimited".into(), |hard| amount(hard, item.resource)), item.percent)).collect();
        Some(Row { identity, value: worst.percent, summary, details })
    }).collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.value));
    Report { mode: "limits", title: "resource-limit utilization", value_label: "percent",
        value_kind: ValueKind::Percent, rows,
        empty_message: if show_all { "no measurable finite limits readable for matching processes" }
            else { "no matching process is at least 75% into a measurable finite limit" } }
}

fn read_limits(pid: i32) -> Vec<Limit> {
    let Ok(raw) = fs::read_to_string(format!("/proc/{pid}/limits")) else { return Vec::new() };
    raw.lines().filter_map(parse_limit).collect()
}

fn parse_limit(line: &str) -> Option<Limit> {
    let (resource, rest) = [
        (Resource::OpenFiles, "Max open files"),
        (Resource::Processes, "Max processes"),
        (Resource::AddressSpace, "Max address space"),
        (Resource::LockedMemory, "Max locked memory"),
    ].into_iter().find_map(|(resource, name)| line.strip_prefix(name).map(|rest| (resource, rest)))?;
    let mut fields = rest.split_whitespace();
    let soft = parse_value(fields.next()?)?;
    let hard = fields.next().and_then(parse_value);
    Some(Limit { resource, soft, hard })
}

fn parse_value(value: &str) -> Option<u64> {
    if value == "unlimited" { None } else { value.parse().ok() }
}

fn usage(process: &proc::Proc, resource: Resource, uid_tasks: &HashMap<u32, u64>) -> Option<u64> {
    match resource {
        Resource::OpenFiles => Some(fs::read_dir(format!("/proc/{}/fd", process.pid)).ok()?.count() as u64),
        Resource::Processes => uid_tasks.get(&process.uid).copied(),
        Resource::AddressSpace => status_kb(process.pid, "VmSize:"),
        Resource::LockedMemory => Some(status_kb(process.pid, "VmLck:").unwrap_or(0)),
    }
}

fn tasks_by_uid(procs: &[proc::Proc]) -> HashMap<u32, u64> {
    let mut counts = HashMap::new();
    for process in procs {
        let tasks = fs::read_dir(format!("/proc/{}/task", process.pid)).map_or(1, |entries| entries.count()) as u64;
        *counts.entry(process.uid).or_insert(0) += tasks;
    }
    counts
}

fn status_kb(pid: i32, field: &str) -> Option<u64> {
    fs::read_to_string(format!("/proc/{pid}/status")).ok()?.lines().find_map(|line| line.strip_prefix(field))?
        .split_whitespace().next()?.parse::<u64>().ok().map(|kb| kb * 1024)
}

fn utilization(used: u64, soft: u64) -> u64 {
    if soft == 0 { return if used == 0 { 0 } else { 100 }; }
    ((used as u128 * 100) / soft as u128).min(u64::MAX as u128) as u64
}

fn label(resource: Resource) -> &'static str {
    match resource { Resource::OpenFiles => "open files", Resource::Processes => "user tasks",
        Resource::AddressSpace => "address space", Resource::LockedMemory => "locked memory" }
}

fn amount(value: u64, resource: Resource) -> String {
    match resource { Resource::OpenFiles | Resource::Processes => value.to_string(),
        Resource::AddressSpace | Resource::LockedMemory => qtools::style::human_plain(value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_finite_and_skips_unlimited_soft_limits() {
        let limit = parse_limit("Max open files            1024                 4096                 files").unwrap();
        assert_eq!(limit.resource, Resource::OpenFiles);
        assert_eq!(limit.soft, 1024);
        assert_eq!(limit.hard, Some(4096));
        assert!(parse_limit("Max address space         unlimited            unlimited            bytes").is_none());
    }

    #[test]
    fn utilization_can_expose_an_exceeded_limit() {
        assert_eq!(utilization(75, 100), 75);
        assert_eq!(utilization(125, 100), 125);
    }
}
