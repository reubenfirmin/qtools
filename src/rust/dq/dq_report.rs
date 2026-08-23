//! Shared aggregation and modular rendering for dq views.
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::{chart, donut, graphics, style};
use crate::render::{RenderContext, RenderTarget};
use crate::style::{bold, dim, human_plain};
use serde::Serialize;

use crate::dq::disk_scan::{DirectoryFact, FileFact, Scan};
use crate::dq::dq_query::{DiskQuery, DiskView, SizeAccounting};

#[derive(Debug, Serialize)]
pub struct Row {
    pub label: String,
    pub bytes: u64,
    pub percent: f64,
    pub files: u64,
    #[serde(skip_serializing_if = "Option::is_none")] pub uid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")] pub modified_unix: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub apparent_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub allocated_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub links: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct Metadata {
    root: String, view: &'static str, accounting: &'static str, generated_at_unix_ms: u128,
    scan_duration_ms: u128, scanned_files: usize, selected_files: usize, directories: usize,
    duplicate_hard_links: usize, one_file_system: bool, excludes: Vec<String>,
    unreadable_count: usize, errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Report { metadata: Metadata, total_bytes: u64, shown_bytes: u64, hidden_bytes: u64,
    hidden_rows: usize, rows: Vec<Row>, #[serde(skip)] compact: bool }

struct Selected<'a> { fact: &'a FileFact, bytes: u64 }

pub fn build(scan: &Scan, query: &DiskQuery) -> Report {
    let (selected, duplicates) = select_files(scan, query);
    let mut rows = match query.view {
        DiskView::Summary => summary_rows(scan, query, &selected),
        DiskView::Files => file_rows(&selected),
        DiskView::Owners => owner_rows(scan, query, &selected),
        DiskView::Age => age_rows(&selected),
    };
    let total: u64 = rows.iter().map(|row| row.bytes).sum();
    for row in &mut rows { row.percent = percent(row.bytes, total); }
    rows.retain(|row| query.presentation.include_zeroes || row.bytes > 0);
    let all_rows = rows.len();
    rows.retain(|row| row.percent >= query.presentation.min_percent);
    rows.sort_by_key(|row| std::cmp::Reverse(row.bytes));
    rows.truncate(query.presentation.top);
    let shown_bytes = rows.iter().map(|row| row.bytes).sum();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    Report {
        metadata: Metadata {
            root: scan.root.display().to_string(), view: view_name(query.view), accounting: accounting_name(query.accounting),
            generated_at_unix_ms: now.as_millis(), scan_duration_ms: scan.duration_ms, scanned_files: scan.files.len(),
            selected_files: selected.len(), directories: scan.directories.len(), duplicate_hard_links: duplicates,
            one_file_system: query.traversal.one_file_system, excludes: query.selection.excludes.clone(),
            unreadable_count: scan.errors.len(), errors: if query.presentation.show_errors { scan.errors.clone() } else { Vec::new() },
        }, total_bytes: total, shown_bytes, hidden_bytes: total.saturating_sub(shown_bytes),
        hidden_rows: all_rows.saturating_sub(rows.len()), rows, compact: query.output.compact_json(),
    }
}

fn select_files<'a>(scan: &'a Scan, query: &DiskQuery) -> (Vec<Selected<'a>>, usize) {
    let mut ordered: Vec<&FileFact> = scan.files.iter().collect();
    ordered.sort_by(|a, b| a.path.cmp(&b.path));
    let mut seen = HashSet::new(); let mut duplicates = 0; let mut selected = Vec::new();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    for fact in ordered {
        if !seen.insert((fact.device, fact.inode)) { duplicates += 1; continue; }
        let bytes = bytes(fact, query.accounting);
        if query.selection.min_size.is_some_and(|minimum| bytes < minimum) { continue; }
        if query.selection.older_than.is_some_and(|age| now.saturating_sub(fact.modified) < age.as_secs() as i64) { continue; }
        selected.push(Selected { fact, bytes });
    }
    (selected, duplicates)
}

fn summary_rows(scan: &Scan, query: &DiskQuery, files: &[Selected<'_>]) -> Vec<Row> {
    let mut groups: HashMap<PathBuf, (u64, u64)> = HashMap::new();
    for file in files {
        let parent = file.fact.path.parent().unwrap_or(&scan.root);
        let group = directory_group(&scan.root, parent, query.presentation.depth);
        let entry = groups.entry(group).or_default(); entry.0 += file.bytes; entry.1 += 1;
    }
    if !has_file_filter(query) {
        for directory in &scan.directories {
            let group = directory_group(&scan.root, &directory.path, query.presentation.depth);
            groups.entry(group).or_default().0 += directory_bytes(directory, query.accounting);
        }
    }
    if query.presentation.include_zeroes {
        for directory in &scan.directories { groups.entry(directory_group(&scan.root, &directory.path, query.presentation.depth)).or_default(); }
    }
    groups.into_iter().map(|(path, (bytes, files))| Row { label: directory_label(&scan.root, &path), bytes, percent: 0.0,
        files, uid: None, modified_unix: None, apparent_bytes: None, allocated_bytes: None, links: None }).collect()
}

fn directory_group(root: &Path, directory: &Path, depth: usize) -> PathBuf {
    let relative = directory.strip_prefix(root).unwrap_or(directory);
    if relative.as_os_str().is_empty() { return root.to_path_buf(); }
    root.join(relative.components().take(depth).collect::<PathBuf>())
}

fn directory_label(root: &Path, path: &Path) -> String {
    if path == root { "[root files and metadata]".into() } else { path.display().to_string() }
}

fn file_rows(files: &[Selected<'_>]) -> Vec<Row> {
    files.iter().map(|file| Row { label: file.fact.path.display().to_string(), bytes: file.bytes, percent: 0.0, files: 1,
        uid: Some(file.fact.uid), modified_unix: Some(file.fact.modified), apparent_bytes: Some(file.fact.apparent),
        allocated_bytes: Some(file.fact.allocated), links: Some(file.fact.links) }).collect()
}

fn owner_rows(scan: &Scan, query: &DiskQuery, files: &[Selected<'_>]) -> Vec<Row> {
    let names = usernames(); let mut groups: HashMap<u32, (u64, u64)> = HashMap::new();
    for file in files { let entry = groups.entry(file.fact.uid).or_default(); entry.0 += file.bytes; entry.1 += 1; }
    if !has_file_filter(query) {
        for directory in &scan.directories { groups.entry(directory.uid).or_default().0 += directory_bytes(directory, query.accounting); }
    }
    groups.into_iter().map(|(uid, (bytes, files))| Row { label: names.get(&uid).cloned().unwrap_or_else(|| uid.to_string()),
        bytes, percent: 0.0, files, uid: Some(uid), modified_unix: None, apparent_bytes: None, allocated_bytes: None, links: None }).collect()
}

fn age_rows(files: &[Selected<'_>]) -> Vec<Row> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let mut groups: HashMap<&'static str, (u64, u64)> = HashMap::new();
    for file in files {
        let days = now.saturating_sub(file.fact.modified) / 86_400;
        let bucket = age_bucket(days);
        let entry = groups.entry(bucket).or_default(); entry.0 += file.bytes; entry.1 += 1;
    }
    groups.into_iter().map(|(label, (bytes, files))| Row { label: label.into(), bytes, percent: 0.0, files,
        uid: None, modified_unix: None, apparent_bytes: None, allocated_bytes: None, links: None }).collect()
}

impl Report {
    pub fn render<W: Write>(&self, out: &mut W, context: RenderContext) -> io::Result<()> {
        match context.target {
            RenderTarget::Json | RenderTarget::JsonLines => self.write_json(out),
            RenderTarget::Graphics => self.write_graphics(out, context),
            RenderTarget::Text => self.write_text(out, context),
        }
    }
    fn write_header<W: Write>(&self, out: &mut W, context: RenderContext) -> io::Result<()> {
        writeln!(out, "{}  {}  {}  {}  {} space  {}  {} total", bold("dq", context.colors), dim("|", context.colors),
            self.metadata.view, dim("|", context.colors), self.metadata.accounting, dim("|", context.colors), human_plain(self.total_bytes))?;
        writeln!(out, "{}", dim("----------------------------------------------------------------", context.colors))
    }
    fn write_text<W: Write>(&self, out: &mut W, context: RenderContext) -> io::Result<()> { self.write_header(out, context)?; self.write_rows(out, context) }
    fn write_rows<W: Write>(&self, out: &mut W, context: RenderContext) -> io::Result<()> {
        let bar_heading = if self.metadata.view == "files" { "relative size" } else { "share of total" };
        writeln!(out, "{}", dim(&format!("{:>10}  {:>7}  {:>8}  {:<20}  {}", "size", "percent", "files", bar_heading, "name"), context.colors))?;
        if self.rows.is_empty() { return writeln!(out, "{}", dim("no files matched this query", context.colors)); }
        let largest = self.rows.iter().map(|row| row.bytes).max().unwrap_or(0);
        for row in &self.rows {
            let ratio = if self.metadata.view == "files" {
                if largest == 0 { 0.0 } else { row.bytes as f64 / largest as f64 }
            } else { row.percent / 100.0 };
            let fill = chart::bar_fill(ratio, chart::BAR_WIDTH);
            let bar = format!("{}{}", "#".repeat(fill), ".".repeat(chart::BAR_WIDTH - fill));
            let label = context.width.map_or_else(|| row.label.clone(), |width| style::truncate_middle(&row.label, width.saturating_sub(55).max(12)));
            writeln!(out, "{:>10}  {:>6.2}%  {:>8}  {}  {}", human_plain(row.bytes), row.percent, row.files, dim(&bar, context.colors), label)?;
        }
        if self.hidden_rows > 0 {
            writeln!(out, "{}", dim(&format!("{} across {} hidden row(s) ({:.2}% of total); use --all or adjust --top/--min-percent",
                human_plain(self.hidden_bytes), self.hidden_rows, percent(self.hidden_bytes, self.total_bytes)), context.colors))?;
        }
        Ok(())
    }
    fn write_graphics<W: Write>(&self, out: &mut W, context: RenderContext) -> io::Result<()> {
        self.write_graphics_with(out, context, |image, config| match viuer::print(image, config) {
            Ok(_) => true,
            Err(error) => { graphics::debug_print_failure(&error); false }
        })
    }
    fn write_graphics_with<W: Write>(&self, out: &mut W, context: RenderContext,
        mut draw: impl FnMut(&image::DynamicImage, &viuer::Config) -> bool) -> io::Result<()> {
        self.write_header(out, context)?;
        if self.total_bytes == 0 || self.rows.is_empty() { return self.write_rows(out, context); }
        let items: Vec<(String, u64)> = self.rows.iter().map(|row| (row.label.clone(), row.bytes)).collect();
        let segments = chart::segments(&items, self.total_bytes, self.rows.len().min(style::PALETTE.len()));
        let image = donut::build_donut_image(&chart::slices(&segments), chart::DONUT_PX);
        out.flush()?;
        let config = viuer::Config { absolute_offset: false, width: Some(40), height: Some(20), ..Default::default() };
        let _ = draw(&image, &config);
        self.write_rows(out, context)
    }
    fn write_json<W: Write>(&self, out: &mut W) -> io::Result<()> {
        if self.compact { serde_json::to_writer(&mut *out, self) } else { serde_json::to_writer_pretty(&mut *out, self) }.map_err(io::Error::other)?;
        writeln!(out)
    }
}

fn bytes(file: &FileFact, accounting: SizeAccounting) -> u64 { match accounting { SizeAccounting::Allocated => file.allocated, SizeAccounting::Apparent => file.apparent } }
fn directory_bytes(directory: &DirectoryFact, accounting: SizeAccounting) -> u64 {
    match accounting { SizeAccounting::Allocated => directory.allocated, SizeAccounting::Apparent => 0 }
}
fn has_file_filter(query: &DiskQuery) -> bool { query.selection.older_than.is_some() || query.selection.min_size.is_some() }
fn percent(value: u64, total: u64) -> f64 { if total == 0 { 0.0 } else { (value as f64 / total as f64 * 10_000.0).round() / 100.0 } }
fn view_name(view: DiskView) -> &'static str { match view { DiskView::Summary => "directories", DiskView::Files => "files", DiskView::Owners => "owners", DiskView::Age => "age" } }
fn accounting_name(mode: SizeAccounting) -> &'static str { match mode { SizeAccounting::Allocated => "allocated", SizeAccounting::Apparent => "apparent" } }
fn usernames() -> HashMap<u32, String> {
    std::fs::read_to_string("/etc/passwd").unwrap_or_default().lines().filter_map(|line| { let mut fields = line.split(':');
        let name = fields.next()?.to_string(); fields.next()?; let uid = fields.next()?.parse().ok()?; Some((uid, name)) }).collect()
}
fn age_bucket(days: i64) -> &'static str {
    if days < 7 { "under 7 days" } else if days < 30 { "7–30 days" }
    else if days < 90 { "30–90 days" } else if days < 365 { "90–365 days" } else { "over 1 year" }
}

#[cfg(test)] mod tests {
    use super::*; use clap::Parser; use std::fs; use std::io::Write;
    #[test] fn directory_group_caps_depth() { let root = Path::new("/x");
        assert_eq!(directory_group(root, Path::new("/x/a/b/c"), 2), PathBuf::from("/x/a/b")); }
    #[test] fn percentages_are_stable() { assert_eq!(percent(1, 4), 25.0); assert_eq!(percent(0, 0), 0.0); }
    #[test] fn age_boundaries_are_explicit() { assert_eq!(age_bucket(6), "under 7 days");
        assert_eq!(age_bucket(7), "7–30 days"); assert_eq!(age_bucket(365), "over 1 year"); }

    #[test]
    fn sparse_and_hard_link_accounting_and_json_schema() {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("qtools-dq-report-{}-{unique}", std::process::id()));
        fs::create_dir_all(root.join("nested")).unwrap();
        let mut real = fs::File::create(root.join("nested/real")).unwrap();
        real.write_all(&vec![1u8; 4096]).unwrap();
        fs::hard_link(root.join("nested/real"), root.join("nested/real-link")).unwrap();
        fs::File::create(root.join("sparse")).unwrap().set_len(1024 * 1024).unwrap();
        let scan = crate::dq::disk_scan::scan(&root, 2, true, &[], false);

        let apparent_args = crate::dq::Args::try_parse_from(["dq", "--files", "--apparent", "--all",
            root.to_str().unwrap()]).unwrap();
        let apparent_query = DiskQuery::from_args(&apparent_args).unwrap();
        let apparent = build(&scan, &apparent_query);
        assert_eq!(apparent.metadata.duplicate_hard_links, 1);
        assert_eq!(apparent.total_bytes, 1024 * 1024 + 4096);
        assert_eq!(apparent.rows.len(), 2);
        let summary_args = crate::dq::Args::try_parse_from(["dq", "--apparent", "--all", root.to_str().unwrap()]).unwrap();
        let summary = build(&scan, &DiskQuery::from_args(&summary_args).unwrap());
        assert_eq!(summary.total_bytes, apparent.total_bytes);

        let allocated_args = crate::dq::Args::try_parse_from(["dq", "--files", "--all", root.to_str().unwrap()]).unwrap();
        let allocated = build(&scan, &DiskQuery::from_args(&allocated_args).unwrap());
        assert!(allocated.total_bytes < apparent.total_bytes);
        let mut json = Vec::new();
        allocated.render(&mut json, RenderContext { target: RenderTarget::Json, colors: false, width: None }).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["metadata"]["accounting"], "allocated");
        assert!(value["rows"].is_array());
        let owner_args = crate::dq::Args::try_parse_from(["dq", "--owners", "--all", root.to_str().unwrap()]).unwrap();
        let owners = build(&scan, &DiskQuery::from_args(&owner_args).unwrap());
        assert_eq!(owners.rows.len(), 1);
        assert_eq!(owners.rows[0].files, 2);

        let age_args = crate::dq::Args::try_parse_from(["dq", "--age", "--all", root.to_str().unwrap()]).unwrap();
        let age = build(&scan, &DiskQuery::from_args(&age_args).unwrap());
        assert_eq!(age.metadata.view, "age");
        assert!(!age.rows.is_empty());

        let limited_args = crate::dq::Args::try_parse_from(["dq", "--files", "--apparent", "--top", "1",
            root.to_str().unwrap()]).unwrap();
        let limited = build(&scan, &DiskQuery::from_args(&limited_args).unwrap());
        assert_eq!(limited.rows.len(), 1);
        assert!(limited.hidden_rows > 0);
        let mut text = Vec::new();
        limited.render(&mut text, RenderContext { target: RenderTarget::Text, colors: false, width: Some(60) }).unwrap();
        assert!(String::from_utf8(text).unwrap().contains("hidden row"));

        let empty_scan = Scan { root: root.clone(), files: Vec::new(), directories: Vec::new(),
            errors: Vec::new(), duration_ms: 0 };
        let empty = build(&empty_scan, &DiskQuery::from_args(&allocated_args).unwrap());
        let mut graphics = Vec::new();
        empty.render(&mut graphics, RenderContext { target: RenderTarget::Graphics, colors: false, width: None }).unwrap();
        assert!(String::from_utf8(graphics).unwrap().contains("no files matched"));
        for succeeds in [true, false] {
            let mut graphics = Vec::new();
            allocated.write_graphics_with(&mut graphics,
                RenderContext { target: RenderTarget::Graphics, colors: false, width: None },
                |image, config| {
                    assert!(image.width() > 0);
                    assert_eq!(config.width, Some(40));
                    succeeds
                }).unwrap();
            assert!(!graphics.is_empty());
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
