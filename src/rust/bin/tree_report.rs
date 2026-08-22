//! Static process-tree table. Selection remains separate from presentation: matching processes are
//! shown normally, while otherwise-unselected ancestors are retained only as dim context rows.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use qtools::render::{RenderContext, RenderTarget};
use qtools::style::{dim, human_plain, truncate_middle};
use serde::Serialize;

use crate::cluster::{self, Cluster, Metric};
use crate::proc;
use crate::report::ReportOpts;

#[derive(Serialize)]
struct Row {
    pid: i32,
    parent_pid: i32,
    depth: usize,
    context: bool,
    name: String,
    command: String,
    cpu: f64,
    memory: u64,
    swap: u64,
    state: String,
    #[serde(skip)]
    branch: String,
}

pub fn render<W: Write>(out: &mut W, procs: &[proc::Proc], clusters: &[Cluster], opts: &ReportOpts,
    context: RenderContext) -> io::Result<()> {
    let selected: HashSet<i32> = clusters.iter().take(opts.top)
        .flat_map(|cluster| cluster.members.iter().map(|member| member.pid)).collect();
    let rows = build_rows(procs, &selected, opts.metric);
    match context.target {
        RenderTarget::Json | RenderTarget::JsonLines => write_json(out, &rows, opts),
        RenderTarget::Graphics | RenderTarget::Text => write_table(out, &rows, opts, context),
    }
}

fn build_rows(procs: &[proc::Proc], selected: &HashSet<i32>, metric: Metric) -> Vec<Row> {
    let by_pid: HashMap<i32, &proc::Proc> = procs.iter().map(|process| (process.pid, process)).collect();
    let mut included = selected.clone();
    for pid in selected {
        let mut cursor = *pid;
        let mut seen = HashSet::new();
        while seen.insert(cursor) {
            let Some(process) = by_pid.get(&cursor) else { break };
            if process.ppid <= 0 || !by_pid.contains_key(&process.ppid) { break; }
            included.insert(process.ppid);
            cursor = process.ppid;
        }
    }

    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut roots = Vec::new();
    for pid in &included {
        let Some(process) = by_pid.get(pid) else { continue };
        if included.contains(&process.ppid) {
            children.entry(process.ppid).or_default().push(*pid);
        } else {
            roots.push(*pid);
        }
    }
    let order = |a: &i32, b: &i32| {
        let a = by_pid[a]; let b = by_pid[b];
        cluster::metric_ordering(metric, a.cpu_pct, a.rss, a.swap, b.cpu_pct, b.rss, b.swap)
            .then_with(|| a.pid.cmp(&b.pid))
    };
    roots.sort_by(order);
    for values in children.values_mut() { values.sort_by(order); }

    let identities = cluster::effective_identities(procs);
    let mut walker = Walker { by_pid: &by_pid, children: &children, selected, identities: &identities, rows: Vec::new() };
    for root in roots {
        walker.append(root, 0, &[], true);
    }
    walker.rows
}

struct Walker<'a> {
    by_pid: &'a HashMap<i32, &'a proc::Proc>,
    children: &'a HashMap<i32, Vec<i32>>,
    selected: &'a HashSet<i32>,
    identities: &'a HashMap<i32, String>,
    rows: Vec<Row>,
}

impl Walker<'_> {
    fn append(&mut self, pid: i32, depth: usize, ancestor_last: &[bool], is_last: bool) {
        let process = self.by_pid[&pid];
        self.rows.push(Row {
            pid, parent_pid: process.ppid, depth, context: !self.selected.contains(&pid),
            name: self.identities.get(&pid).cloned().unwrap_or_else(|| process.comm.clone()),
            command: process.cmdline.join(" "), cpu: round2(process.cpu_pct), memory: process.rss,
            swap: process.swap, state: process_state(pid),
            branch: if depth == 0 { String::new() } else {
                let trunk = ancestor_last.iter().take(depth.saturating_sub(1))
                    .map(|last| if *last { "   " } else { "│  " }).collect::<String>();
                format!("{trunk}{}", if is_last { "└─ " } else { "├─ " })
            },
        });
        if let Some(kids) = self.children.get(&pid) {
            let kids = kids.clone();
            let mut trail = ancestor_last.to_vec();
            trail.push(is_last);
            for (index, child) in kids.iter().enumerate() {
                self.append(*child, depth + 1, &trail, index + 1 == kids.len());
            }
        }
    }
}

fn write_table<W: Write>(out: &mut W, rows: &[Row], opts: &ReportOpts, context: RenderContext) -> io::Result<()> {
    writeln!(out, "pq  |  process tree  |  {} ms sample", opts.sample_interval_ms)?;
    writeln!(out, "{}", dim("----------------------------------------------------------------", context.colors))?;
    let max_name = context.width.map_or(38, |width| width.saturating_sub(56).clamp(18, 60));
    writeln!(out, "{}", dim(&format!("{:<nw$}  {:>7}  {:>7}  {:>9}  {:>9}  {:>9}  {:>5}",
        "process", "pid", "ppid", "cpu", "mem", "swap", "state", nw = max_name), context.colors))?;
    if rows.is_empty() {
        writeln!(out, "{}", dim("no matching processes", context.colors))?;
        return Ok(());
    }
    for row in rows {
        let name = truncate_middle(&format!("{}{}", row.branch, row.name), max_name);
        let line = format!("{:<nw$}  {:>7}  {:>7}  {:>6.2}%  {:>9}  {:>9}  {:>5}", name, row.pid,
            row.parent_pid, row.cpu, human_plain(row.memory), human_plain(row.swap), row.state, nw = max_name);
        if row.context { writeln!(out, "{}", dim(&line, context.colors))?; } else { writeln!(out, "{line}")?; }
    }
    Ok(())
}

fn write_json<W: Write>(out: &mut W, rows: &[Row], opts: &ReportOpts) -> io::Result<()> {
    #[derive(Serialize)]
    struct Document<'a> { mode: &'static str, generated_at_unix_ms: u128, sample_interval_ms: u64, nodes: &'a [Row] }
    let document = Document { mode: "tree", generated_at_unix_ms: std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis(),
        sample_interval_ms: opts.sample_interval_ms, nodes: rows };
    if opts.compact_json { serde_json::to_writer(&mut *out, &document) } else { serde_json::to_writer_pretty(&mut *out, &document) }
        .map_err(io::Error::other)?;
    writeln!(out)
}

fn process_state(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/status")).ok().and_then(|status| status.lines()
        .find_map(|line| line.strip_prefix("State:").and_then(|value| value.split_whitespace().next()).map(str::to_string)))
        .unwrap_or_else(|| "?".into())
}

fn round2(value: f64) -> f64 { (value * 100.0).round() / 100.0 }

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: i32, ppid: i32, cpu: f64) -> proc::Proc {
        proc::Proc { pid, ppid, comm: format!("p{pid}"), cmdline: vec![format!("p{pid}")], rss: pid as u64,
            swap: 0, uid: 0, cpu_pct: cpu }
    }

    #[test]
    fn retains_ancestors_as_context_and_orders_siblings() {
        let procs = vec![process(1, 0, 0.0), process(2, 1, 2.0), process(3, 1, 8.0)];
        let rows = build_rows(&procs, &HashSet::from([2, 3]), Metric::Cpu);
        assert_eq!(rows.iter().map(|row| row.pid).collect::<Vec<_>>(), vec![1, 3, 2]);
        assert!(rows[0].context);
        assert!(!rows[1].context);
        assert_eq!(rows[1].depth, 1);
    }
}
