//! Shared /proc descriptor and path-holder collection for --fds, --deleted, --file, and --mount.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::cluster;
use crate::proc;
use crate::ranked_report::{Report, Row, ValueKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FdKind { File, Socket, Pipe, Device, Anon, Other }

#[derive(Debug)]
struct FdEntry { fd: String, target: String, kind: FdKind, size: u64, dev: u64, ino: u64, deleted: bool }

#[derive(Debug)]
pub struct ProcessFiles { pid: i32, identity: String, entries: Vec<FdEntry>, holds: Vec<String> }

pub fn collect(procs: &[proc::Proc], include_special_paths: bool) -> Vec<ProcessFiles> {
    let identities = cluster::effective_identities(procs);
    procs.iter().map(|p| {
        let entries = scan_fds(p.pid);
        let holds = if include_special_paths { scan_special_paths(p.pid) } else { Vec::new() };
        ProcessFiles { pid: p.pid, identity: identities.get(&p.pid).cloned().unwrap_or_else(|| p.comm.clone()), entries, holds }
    }).collect()
}

fn scan_fds(pid: i32) -> Vec<FdEntry> {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else { return Vec::new() };
    entries.flatten().filter_map(|entry| {
        let target = fs::read_link(entry.path()).ok()?.to_string_lossy().into_owned();
        // `/proc/PID/fd/N` is a symlink; follow it to the open file description so deleted-file
        // size/dev/inode describe the held file, not the tiny procfs link.
        let metadata = fs::metadata(entry.path()).ok();
        let deleted = target.ends_with(" (deleted)");
        let kind = classify(&target, metadata.as_ref());
        Some(FdEntry {
            fd: entry.file_name().to_string_lossy().into_owned(), target,
            kind, size: metadata.as_ref().map_or(0, |m| m.len()),
            dev: metadata.as_ref().map_or(0, |m| m.dev()), ino: metadata.as_ref().map_or(0, |m| m.ino()), deleted,
        })
    }).collect()
}

fn classify(target: &str, metadata: Option<&fs::Metadata>) -> FdKind {
    if target.starts_with("socket:[") { FdKind::Socket }
    else if target.starts_with("pipe:[") { FdKind::Pipe }
    else if target.starts_with("anon_inode:") { FdKind::Anon }
    else if metadata.is_some_and(|m| m.file_type().is_file()) { FdKind::File }
    else if target.starts_with("/dev/") { FdKind::Device }
    else { FdKind::Other }
}

fn scan_special_paths(pid: i32) -> Vec<String> {
    let mut holds = Vec::new();
    for label in ["cwd", "root", "exe"] {
        if let Ok(path) = fs::read_link(format!("/proc/{pid}/{label}")) {
            holds.push(format!("{label} {}", path.display()));
        }
    }
    if let Ok(raw) = fs::read_to_string(format!("/proc/{pid}/maps")) {
        let mut seen = HashSet::new();
        for path in raw.lines().filter_map(|line| line.split_whitespace().nth(5)).filter(|p| p.starts_with('/')) {
            if seen.insert(path.to_string()) { holds.push(format!("map {path}")); }
        }
    }
    holds
}

pub fn matching_pids(files: &[ProcessFiles], file: Option<&str>, mount: Option<&str>) -> HashSet<i32> {
    let file = file.map(normalize_query_path);
    let mount = mount.map(normalize_query_path);
    files.iter().filter(|process| {
        process.entries.iter().any(|entry| path_matches(clean_deleted(&entry.target), file.as_deref(), mount.as_deref()))
            || process.holds.iter().any(|hold| {
                let path = hold.split_once(' ').map(|(_, path)| path).unwrap_or(hold);
                path_matches(clean_deleted(path), file.as_deref(), mount.as_deref())
            })
    }).map(|process| process.pid).collect()
}

pub fn normalize_query_path(path: &str) -> String {
    fs::canonicalize(path).unwrap_or_else(|_| {
        let path = PathBuf::from(path);
        if path.is_absolute() { path } else { std::env::current_dir().unwrap_or_default().join(path) }
    }).to_string_lossy().into_owned()
}

fn clean_deleted(path: &str) -> &str { path.strip_suffix(" (deleted)").unwrap_or(path) }

fn path_matches(path: &str, file: Option<&str>, mount: Option<&str>) -> bool {
    file.is_none_or(|wanted| path == wanted)
        && mount.is_none_or(|wanted| Path::new(path).starts_with(Path::new(wanted)))
}

pub fn fds_report(files: &[ProcessFiles], allowed: &HashSet<i32>) -> Report {
    let mut groups: HashMap<&str, Vec<&ProcessFiles>> = HashMap::new();
    for process in files.iter().filter(|p| allowed.contains(&p.pid)) { groups.entry(&process.identity).or_default().push(process); }
    let mut rows: Vec<Row> = groups.into_iter().map(|(identity, processes)| {
        let count: usize = processes.iter().map(|p| p.entries.len()).sum();
        let kinds = |kind| processes.iter().flat_map(|p| &p.entries).filter(|e| e.kind == kind).count();
        Row { identity: identity.to_string(), value: count as u64,
            summary: format!("files {}  sockets {}  pipes {}  anon {}", kinds(FdKind::File), kinds(FdKind::Socket), kinds(FdKind::Pipe), kinds(FdKind::Anon)),
            details: processes.iter().flat_map(|p| p.entries.iter().map(move |e| format!("pid {}  fd {}  {}", p.pid, e.fd, e.target))).collect() }
    }).collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.value));
    Report { mode: "fds", title: "file descriptors", value_label: "fds", value_kind: ValueKind::Count, rows, empty_message: "no matching file descriptors" }
}

pub fn deleted_report(files: &[ProcessFiles], allowed: &HashSet<i32>) -> Report {
    let mut grouped: HashMap<&str, Vec<&ProcessFiles>> = HashMap::new();
    for process in files.iter().filter(|p| allowed.contains(&p.pid)) { grouped.entry(&process.identity).or_default().push(process); }
    let mut rows = Vec::new();
    for (identity, processes) in grouped {
        let mut seen = HashSet::new();
        let mut bytes = 0;
        let mut details = Vec::new();
        for process in processes {
            for entry in process.entries.iter().filter(|entry| entry.deleted) {
                if seen.insert((entry.dev, entry.ino)) { bytes += entry.size; }
                details.push(format!("pid {}  fd {}  {}  {}", process.pid, entry.fd, human(entry.size), entry.target));
            }
        }
        if !details.is_empty() {
            rows.push(Row { identity: identity.to_string(), value: bytes, summary: format!("{} open reference(s)", details.len()), details });
        }
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.value));
    Report { mode: "deleted", title: "deleted files still open", value_label: "bytes", value_kind: ValueKind::Bytes,
        rows, empty_message: "no matching deleted open files" }
}

pub fn holders_report(files: &[ProcessFiles], allowed: &HashSet<i32>, file: Option<&str>, mount: Option<&str>) -> Report {
    let mut groups: HashMap<&str, Vec<String>> = HashMap::new();
    for process in files.iter().filter(|p| allowed.contains(&p.pid)) {
        for entry in &process.entries {
            if path_matches(clean_deleted(&entry.target), file, mount) {
                groups.entry(&process.identity).or_default().push(format!("pid {}  fd {}  {}", process.pid, entry.fd, entry.target));
            }
        }
        for hold in &process.holds {
            let path = hold.split_once(' ').map(|(_, path)| path).unwrap_or(hold);
            if path_matches(clean_deleted(path), file, mount) {
                groups.entry(&process.identity).or_default().push(format!("pid {}  {}", process.pid, hold));
            }
        }
    }
    let mut rows: Vec<Row> = groups.into_iter().map(|(identity, details)| Row { identity: identity.to_string(),
        value: details.len() as u64, summary: format!("{} hold reason(s)", details.len()), details }).collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.value));
    Report { mode: "holders", title: "processes holding paths", value_label: "holds", value_kind: ValueKind::Count,
        rows, empty_message: "no process holds the selected path" }
}

fn human(bytes: u64) -> String { qtools::style::human_plain(bytes) }

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn this_process() -> proc::Proc {
        proc::Proc { pid: std::process::id() as i32, ppid: 1, comm: "fd-test".into(),
            cmdline: vec!["fd-test".into()], rss: 0, swap: 0, uid: 0, cpu_pct: 0.0 }
    }

    #[test]
    fn sees_deleted_file_held_by_this_process() {
        let path = std::env::temp_dir().join(format!("qtools-deleted-test-{}", std::process::id()));
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&vec![0u8; 4096]).unwrap();
        fs::remove_file(&path).unwrap();
        let procs = vec![this_process()];
        let files = collect(&procs, false);
        let allowed = [procs[0].pid].into_iter().collect();
        let report = deleted_report(&files, &allowed);
        assert_eq!(report.rows.len(), 1);
        assert!(report.rows[0].value >= 4096);
        assert!(report.rows[0].details.iter().any(|line| line.contains("(deleted)")));
    }

    #[test]
    fn mount_matching_respects_path_boundaries() {
        assert!(path_matches("/mnt/data/file", None, Some("/mnt/data")));
        assert!(!path_matches("/mnt/database/file", None, Some("/mnt/data")));
    }
}
