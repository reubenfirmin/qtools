//! Static mapped-address-space breadth and opt-in referenced-page interval sampling.

use std::collections::{HashMap, HashSet};
use std::fs;

use crate::pq::cluster;
use crate::pq::proc;
use crate::pq::ranked_report::{Report, Ring, Row, Table, ValueKind};

#[derive(Default)]
struct MapStats {
    regions: u64,
    mapped: u64,
    file_regions: u64,
    anonymous_regions: u64,
    shared_regions: u64,
    executable_regions: u64,
    files: HashSet<String>,
}

impl MapStats {
    fn merge(&mut self, other: &Self) {
        self.regions += other.regions;
        self.mapped += other.mapped;
        self.file_regions += other.file_regions;
        self.anonymous_regions += other.anonymous_regions;
        self.shared_regions += other.shared_regions;
        self.executable_regions += other.executable_regions;
        self.files.extend(other.files.iter().cloned());
    }
}

#[derive(Default)]
struct AccessStats { regions: u64, active_regions: u64, mapped: u64, referenced: u64 }

impl AccessStats {
    fn merge(&mut self, other: &Self) {
        self.regions += other.regions;
        self.active_regions += other.active_regions;
        self.mapped += other.mapped;
        self.referenced += other.referenced;
    }
}

pub fn maps(procs: &[proc::Proc], allowed: &HashSet<i32>) -> Report {
    let identities = cluster::effective_identities(procs);
    let mut groups: HashMap<String, (MapStats, Vec<String>)> = HashMap::new();
    for process in procs.iter().filter(|process| allowed.contains(&process.pid)) {
        let Ok(raw) = fs::read_to_string(format!("/proc/{}/maps", process.pid)) else { continue };
        let stats = parse_maps(&raw);
        let identity = identities.get(&process.pid).cloned().unwrap_or_else(|| process.comm.clone());
        let group = groups.entry(identity).or_default();
        group.1.push(format!("pid {}  {} regions  virtual {}  files {}  anon {}", process.pid,
            stats.regions, human(stats.mapped), stats.files.len(), stats.anonymous_regions));
        group.0.merge(&stats);
    }
    let mut counted: Vec<(Row, [u64; 7])> = groups.into_iter().map(|(identity, (stats, details))| {
        let values = [stats.regions, stats.mapped, stats.files.len() as u64, stats.file_regions,
            stats.anonymous_regions, stats.shared_regions, stats.executable_regions];
        (Row { identity, value: stats.regions,
            summary: format!("virtual {}  files {}  file regions {}  anon {}  shared {}  executable {}",
                human(stats.mapped), stats.files.len(), stats.file_regions, stats.anonymous_regions,
                stats.shared_regions, stats.executable_regions), details }, values)
    }).collect();
    counted.sort_by_key(|(row, _)| std::cmp::Reverse(row.value));
    let (rows, values): (Vec<_>, Vec<_>) = counted.into_iter().unzip();
    let ring = |label, index| Ring { label, values: values.iter().map(|row| row[index]).collect() };
    let rings = vec![ring("regions", 0), ring("virtual", 1), ring("files", 2), ring("file-backed", 3),
        ring("anonymous", 4), ring("shared", 5), ring("executable", 6)];
    let table = Table {
        headers: vec!["regions", "virtual", "files", "file-rgn", "anon", "shared", "exec"],
        rows: values.iter().map(|row| vec![row[0].to_string(), human(row[1]), row[2].to_string(),
            row[3].to_string(), row[4].to_string(), row[5].to_string(), row[6].to_string()]).collect(),
    };
    Report { mode: "maps", title: "virtual memory maps", value_label: "regions", value_kind: ValueKind::Count,
        rows, rings, table: Some(table), empty_message: "no memory maps readable for matching processes" }
}

/// Clear page-reference state only for processes selected for the explicit `--access` sample.
/// Unreadable or non-writable processes are omitted, matching the permission-sensitive `--pss`
/// behavior rather than failing the entire machine-wide report.
pub fn begin_access_sample(allowed: &HashSet<i32>) -> HashSet<i32> {
    allowed.iter().copied().filter(|pid| fs::write(format!("/proc/{pid}/clear_refs"), b"1\n").is_ok()).collect()
}

pub fn access(procs: &[proc::Proc], sampled: &HashSet<i32>) -> Report {
    let identities = cluster::effective_identities(procs);
    let mut groups: HashMap<String, (AccessStats, Vec<String>)> = HashMap::new();
    for process in procs.iter().filter(|process| sampled.contains(&process.pid)) {
        let Ok(raw) = fs::read_to_string(format!("/proc/{}/smaps", process.pid)) else { continue };
        let stats = parse_smaps_access(&raw);
        let identity = identities.get(&process.pid).cloned().unwrap_or_else(|| process.comm.clone());
        let group = groups.entry(identity).or_default();
        group.1.push(format!("pid {}  active {} / {} regions  referenced {}  virtual {}  coverage {}%",
            process.pid, stats.active_regions, stats.regions, human(stats.referenced), human(stats.mapped),
            percent(stats.referenced, stats.mapped)));
        group.0.merge(&stats);
    }
    let mut counted: Vec<(Row, [u64; 5])> = groups.into_iter().map(|(identity, (stats, details))| {
        let values = [stats.active_regions, stats.referenced, stats.regions, stats.mapped,
            percent(stats.referenced, stats.mapped)];
        (Row { identity, value: stats.active_regions,
            summary: format!("referenced {}  active {} / {} regions  virtual {}  coverage {}%",
                human(stats.referenced), stats.active_regions, stats.regions, human(stats.mapped), values[4]),
            details }, values)
    }).collect();
    counted.sort_by_key(|(row, _)| std::cmp::Reverse(row.value));
    let (rows, values): (Vec<_>, Vec<_>) = counted.into_iter().unzip();
    let ring = |label, index| Ring { label, values: values.iter().map(|row| row[index]).collect() };
    let rings = vec![ring("active regions", 0), ring("referenced", 1), ring("total regions", 2),
        ring("virtual", 3), ring("coverage", 4)];
    let table = Table {
        headers: vec!["active", "referenced", "regions", "virtual", "coverage"],
        rows: values.iter().map(|row| vec![row[0].to_string(), human(row[1]), row[2].to_string(),
            human(row[3]), format!("{}%", row[4])]).collect(),
    };
    Report { mode: "access", title: "recently referenced memory", value_label: "regions",
        value_kind: ValueKind::Count, rows, rings, table: Some(table),
        empty_message: "no referenced-memory sample available (processes exited or permission was denied)" }
}

fn parse_maps(raw: &str) -> MapStats {
    let mut stats = MapStats::default();
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let Some((start, end)) = fields.next().and_then(parse_range) else { continue };
        let perms = fields.next().unwrap_or("");
        let _offset = fields.next();
        let dev = fields.next().unwrap_or("");
        let inode = fields.next().and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);
        stats.regions += 1;
        stats.mapped += end.saturating_sub(start);
        if perms.as_bytes().get(2) == Some(&b'x') { stats.executable_regions += 1; }
        if perms.as_bytes().get(3) == Some(&b's') { stats.shared_regions += 1; }
        if inode > 0 {
            stats.file_regions += 1;
            stats.files.insert(format!("{dev}:{inode}"));
        } else {
            stats.anonymous_regions += 1;
        }
    }
    stats
}

fn parse_smaps_access(raw: &str) -> AccessStats {
    let mut stats = AccessStats::default();
    let mut current_size = None;
    let mut referenced = 0u64;
    let finish = |stats: &mut AccessStats, size: Option<u64>, referenced: u64| {
        let Some(size) = size else { return };
        stats.regions += 1;
        stats.mapped += size;
        stats.referenced += referenced;
        if referenced > 0 { stats.active_regions += 1; }
    };
    for line in raw.lines() {
        if let Some((start, end)) = line.split_whitespace().next().and_then(parse_range) {
            finish(&mut stats, current_size, referenced);
            current_size = Some(end.saturating_sub(start));
            referenced = 0;
        } else if let Some(rest) = line.strip_prefix("Referenced:") {
            referenced = rest.split_whitespace().next().and_then(|value| value.parse::<u64>().ok()).unwrap_or(0) * 1024;
        }
    }
    finish(&mut stats, current_size, referenced);
    stats
}

fn parse_range(value: &str) -> Option<(u64, u64)> {
    let (start, end) = value.split_once('-')?;
    Some((u64::from_str_radix(start, 16).ok()?, u64::from_str_radix(end, 16).ok()?))
}

fn percent(part: u64, total: u64) -> u64 {
    if total == 0 { 0 } else { ((part as u128 * 100) / total as u128) as u64 }
}

fn human(bytes: u64) -> String { crate::style::human_plain(bytes) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_parser_counts_regions_categories_bytes_and_unique_files() {
        let raw = "1000-3000 r-xp 0000 08:01 7 /bin/a\n3000-4000 rw-s 0000 08:01 7 /bin/a\n5000-9000 rw-p 0000 00:00 0 [heap]\n";
        let stats = parse_maps(raw);
        assert_eq!(stats.regions, 3);
        assert_eq!(stats.mapped, 0x7000);
        assert_eq!(stats.file_regions, 2);
        assert_eq!(stats.anonymous_regions, 1);
        assert_eq!(stats.shared_regions, 1);
        assert_eq!(stats.executable_regions, 1);
        assert_eq!(stats.files.len(), 1);
    }

    #[test]
    fn smaps_parser_counts_only_regions_with_referenced_pages_as_active() {
        let raw = "1000-3000 rw-p 0000 00:00 0\nSize: 8 kB\nReferenced: 4 kB\n3000-7000 r--p 0000 08:01 7 /bin/a\nSize: 16 kB\nReferenced: 0 kB\n";
        let stats = parse_smaps_access(raw);
        assert_eq!(stats.regions, 2);
        assert_eq!(stats.active_regions, 1);
        assert_eq!(stats.mapped, 0x6000);
        assert_eq!(stats.referenced, 4 * 1024);
        assert_eq!(percent(stats.referenced, stats.mapped), 16);
    }
}
