//! Process I/O sampling, identity aggregation, and text/JSON rendering for pq --io.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};

use qtools::style::{self, bold, dim, human_plain, paint};
use serde::Serialize;

use crate::cluster;
use crate::pq_query::IoMetric;
use crate::proc;

pub type IoSnapshot = HashMap<i32, (u64, u64)>;

#[derive(Debug)]
pub struct IoMember {
    pub pid: i32,
    pub read_bps: u64,
    pub write_bps: u64,
    pub read_total: u64,
    pub write_total: u64,
    pub cmd: String,
}

#[derive(Debug)]
pub struct IoCluster {
    pub identity: String,
    pub read_bps: u64,
    pub write_bps: u64,
    pub read_total: u64,
    pub write_total: u64,
    pub members: Vec<IoMember>,
}

pub struct IoOpts {
    pub metric: IoMetric,
    pub top: usize,
    pub verbose: bool,
    pub compact_json: bool,
    pub sample_interval_ms: u64,
    pub colors: bool,
}

pub fn snapshot(pids: impl Iterator<Item = i32>) -> IoSnapshot {
    pids.filter_map(|pid| read_pid_io(pid).map(|io| (pid, io))).collect()
}

fn read_pid_io(pid: i32) -> Option<(u64, u64)> {
    let raw = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let mut read = 0;
    let mut write = 0;
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("read_bytes:") {
            read = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("write_bytes:") {
            write = value.trim().parse().unwrap_or(0);
        }
    }
    Some((read, write))
}

pub fn aggregate(procs: &[proc::Proc], allowed: &HashSet<i32>, before: &IoSnapshot, after: &IoSnapshot,
    elapsed_secs: f64, metric: IoMetric) -> Vec<IoCluster> {
    let identities = cluster::effective_identities(procs);
    let mut groups: HashMap<String, Vec<IoMember>> = HashMap::new();
    for p in procs.iter().filter(|p| allowed.contains(&p.pid)) {
        let (read_total, write_total) = after.get(&p.pid).copied().unwrap_or((0, 0));
        let (old_read, old_write) = before.get(&p.pid).copied().unwrap_or((read_total, write_total));
        let rate = |delta: u64| if elapsed_secs > 0.0 { (delta as f64 / elapsed_secs).round() as u64 } else { 0 };
        let member = IoMember {
            pid: p.pid,
            read_bps: rate(read_total.saturating_sub(old_read)),
            write_bps: rate(write_total.saturating_sub(old_write)),
            read_total, write_total,
            cmd: p.cmdline.join(" "),
        };
        let identity = identities.get(&p.pid).cloned().unwrap_or_else(|| p.comm.clone());
        groups.entry(identity).or_default().push(member);
    }
    let mut clusters: Vec<IoCluster> = groups.into_iter().map(|(identity, members)| IoCluster {
        identity,
        read_bps: members.iter().map(|m| m.read_bps).sum(),
        write_bps: members.iter().map(|m| m.write_bps).sum(),
        read_total: members.iter().map(|m| m.read_total).sum(),
        write_total: members.iter().map(|m| m.write_total).sum(),
        members,
    }).collect();
    clusters.sort_by_key(|c| std::cmp::Reverse(metric_value(c, metric)));
    for cluster in &mut clusters {
        cluster.members.sort_by_key(|m| std::cmp::Reverse(member_metric_value(m, metric)));
    }
    clusters
}

fn metric_value(c: &IoCluster, metric: IoMetric) -> u64 {
    match metric { IoMetric::Combined => c.read_bps + c.write_bps, IoMetric::Read => c.read_bps, IoMetric::Write => c.write_bps }
}

fn member_metric_value(m: &IoMember, metric: IoMetric) -> u64 {
    match metric { IoMetric::Combined => m.read_bps + m.write_bps, IoMetric::Read => m.read_bps, IoMetric::Write => m.write_bps }
}

pub fn write_text<W: Write>(out: &mut W, clusters: &[IoCluster], opts: &IoOpts) -> io::Result<()> {
    writeln!(out, "{}  {}  process I/O  {}  {} ms sample", bold("pq", opts.colors),
        dim("|", opts.colors), dim("|", opts.colors), opts.sample_interval_ms)?;
    writeln!(out, "{}", dim("----------------------------------------------------", opts.colors))?;
    writeln!(out, "{}", dim(&format!("{:>10}  {:>10}  {:>10}  {:>5}  process", "read/s", "write/s", "total/s", "procs"), opts.colors))?;
    if clusters.is_empty() {
        writeln!(out, "{}", dim("no process I/O observed during this sample", opts.colors))?;
        return Ok(());
    }
    for c in clusters.iter().take(opts.top) {
        writeln!(out, "{}  {}  {}  {:>5}  {}",
            rate_cell(c.read_bps, opts.colors), rate_cell(c.write_bps, opts.colors),
            rate_cell(c.read_bps + c.write_bps, opts.colors), c.members.len(), c.identity)?;
        if opts.verbose {
            for m in &c.members {
                writeln!(out, "  {:>8}  {:>10}  {:>10}  pid {}  {}",
                    "", rate_text(m.read_bps), rate_text(m.write_bps), m.pid, m.cmd)?;
            }
        }
    }
    Ok(())
}

fn rate_text(bytes: u64) -> String { format!("{}/s", human_plain(bytes)) }

fn rate_cell(bytes: u64, colors: bool) -> String {
    paint(&format!("{:>10}", rate_text(bytes)), style::magnitude_color(bytes), bytes >= 1_000_000_000, colors)
}

pub fn write_json<W: Write>(out: &mut W, clusters: &[IoCluster], opts: &IoOpts) -> io::Result<()> {
    #[derive(Serialize)]
    struct Member<'a> { pid: i32, read_bytes_per_sec: u64, write_bytes_per_sec: u64, read_bytes: u64, write_bytes: u64, cmd: &'a str }
    #[derive(Serialize)]
    struct Cluster<'a> { identity: &'a str, read_bytes_per_sec: u64, write_bytes_per_sec: u64, total_bytes_per_sec: u64, count: usize, read_bytes: u64, write_bytes: u64, members: Vec<Member<'a>> }
    #[derive(Serialize)]
    struct Report<'a> { mode: &'static str, generated_at_unix_ms: u128, sample_interval_ms: u64, sort: &'static str, clusters: Vec<Cluster<'a>> }

    let clusters = clusters.iter().take(opts.top).map(|c| Cluster {
        identity: &c.identity, read_bytes_per_sec: c.read_bps, write_bytes_per_sec: c.write_bps,
        total_bytes_per_sec: c.read_bps + c.write_bps, count: c.members.len(),
        read_bytes: c.read_total, write_bytes: c.write_total,
        members: c.members.iter().map(|m| Member { pid: m.pid, read_bytes_per_sec: m.read_bps,
            write_bytes_per_sec: m.write_bps, read_bytes: m.read_total, write_bytes: m.write_total, cmd: &m.cmd }).collect()
    }).collect();
    let generated_at_unix_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    let report = Report { mode: "io", generated_at_unix_ms, sample_interval_ms: opts.sample_interval_ms,
        sort: match opts.metric { IoMetric::Combined => "io", IoMetric::Read => "read", IoMetric::Write => "write" }, clusters };
    if opts.compact_json { serde_json::to_writer(&mut *out, &report) } else { serde_json::to_writer_pretty(&mut *out, &report) }
        .map_err(io::Error::other)?;
    writeln!(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: i32, comm: &str) -> proc::Proc {
        proc::Proc { pid, ppid: 1, comm: comm.into(), cmdline: vec![comm.into()],
            rss: 100, swap: 0, uid: 1000, cpu_pct: 1.0 }
    }

    #[test]
    fn aggregates_deltas_and_sorts_by_selected_metric() {
        let procs = vec![process(10, "reader"), process(20, "writer")];
        let before = [(10, (100, 0)), (20, (0, 100))].into_iter().collect();
        let after = [(10, (1_100, 0)), (20, (0, 2_100))].into_iter().collect();
        let allowed = [10, 20].into_iter().collect();
        let clusters = aggregate(&procs, &allowed, &before, &after, 1.0, IoMetric::Write);
        assert_eq!(clusters[0].identity, "writer");
        assert_eq!(clusters[0].write_bps, 2_000);
        assert_eq!(clusters[1].read_bps, 1_000);
    }

    #[test]
    fn compact_json_is_one_line_with_io_schema() {
        let clusters = vec![IoCluster { identity: "db".into(), read_bps: 10, write_bps: 20,
            read_total: 100, write_total: 200, members: vec![] }];
        let opts = IoOpts { metric: IoMetric::Combined, top: 15, verbose: false,
            compact_json: true, sample_interval_ms: 400, colors: false };
        let mut out = Vec::new();
        write_json(&mut out, &clusters, &opts).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["mode"], "io");
        assert_eq!(value["clusters"][0]["total_bytes_per_sec"], 30);
    }
}
