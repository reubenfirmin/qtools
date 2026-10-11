//! Shared aggregation and modular rendering for dq views.
use std::collections::HashMap;
#[cfg(test)] use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{self, Write};
#[cfg(test)] use std::os::unix::ffi::OsStrExt;
use std::path::Path;
#[cfg(test)] use std::path::PathBuf;

use crate::{chart, donut, graphics, style};
use crate::render::{RenderContext, RenderTarget};
use crate::style::{bold, dim, human_plain};
use serde::Serialize;

use crate::dq::disk_scan::{DirectoryFact, FileFact, Scan, SummaryScan};
use crate::dq::dq_query::{DiskQuery, DiskView, SizeAccounting};
use super::{hierarchy, space};
use super::hierarchy::directory_prefix;

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
    #[serde(skip_serializing_if = "is_zero")] pruned_directories: usize,
    #[serde(skip_serializing_if = "Option::is_none")] unmeasured_bytes_upper_bound: Option<u64>,
    #[serde(skip_serializing_if = "is_zero_bytes")] known_unassigned_bytes: u64,
}

fn is_zero(value: &usize) -> bool { *value == 0 }
fn is_zero_bytes(value: &u64) -> bool { *value == 0 }

#[derive(Debug, Serialize)]
pub struct Report { metadata: Metadata, total_bytes: u64, shown_bytes: u64, hidden_bytes: u64,
    hidden_rows: usize, rows: Vec<Row>,
    #[serde(skip_serializing_if = "Option::is_none")] directory_tree: Option<hierarchy::Breakdown>,
    #[serde(skip_serializing_if = "Option::is_none")] filesystem_space: Option<space::Space>,
    #[serde(skip)] compact: bool }

struct Selected<'a> { fact: &'a FileFact, bytes: u64 }

pub fn build(scan: &Scan, query: &DiskQuery) -> Report {
    let started = std::time::Instant::now();
    let (selected, duplicates) = select_files(scan, query);
    let selected_at = started.elapsed();
    let mut levels = if query.view == DiskView::Summary {
        let depths = if query.presentation.depth == 1 { vec![1, 2, 3] } else { vec![query.presentation.depth] };
        directory_levels(scan, query, &selected, &depths)
    } else { Vec::new() };
    let rows = match query.view {
        DiskView::Summary => summary_rows(&scan.root, levels.remove(0)),
        DiskView::Files => file_rows(&selected),
        DiskView::Owners => owner_rows(scan, query, &selected),
        DiskView::Age => age_rows(&selected),
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let metadata = Metadata {
        root: scan.root.display().to_string(), view: view_name(query.view), accounting: accounting_name(query.accounting),
        generated_at_unix_ms: now.as_millis(), scan_duration_ms: scan.duration_ms, scanned_files: scan.files.len(),
        selected_files: selected.len(), directories: scan.directories.len(), duplicate_hard_links: duplicates,
        one_file_system: query.traversal.one_file_system, excludes: query.selection.excludes.clone(),
        unreadable_count: scan.errors.len(), errors: if query.presentation.show_errors { scan.errors.clone() } else { Vec::new() },
        pruned_directories: 0, unmeasured_bytes_upper_bound: None, known_unassigned_bytes: 0,
    };
    let report = finish(&scan.root, rows, &levels, metadata, query);
    if std::env::var_os("QTOOLS_DEBUG").is_some() {
        eprintln!("dq[debug]: selection={}ms aggregation={}ms", selected_at.as_millis(),
            started.elapsed().saturating_sub(selected_at).as_millis());
    }
    report
}

pub(super) fn build_summary(mut scan: SummaryScan, query: &DiskQuery) -> Report {
    let rows = summary_rows(&scan.root, scan.levels.remove(0));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let metadata = Metadata {
        root: scan.root.display().to_string(), view: view_name(query.view), accounting: accounting_name(query.accounting),
        generated_at_unix_ms: now.as_millis(), scan_duration_ms: scan.duration_ms, scanned_files: scan.scanned_files,
        selected_files: scan.selected_files, directories: scan.directories, duplicate_hard_links: scan.duplicate_hard_links,
        one_file_system: query.traversal.one_file_system, excludes: query.selection.excludes.clone(),
        unreadable_count: scan.errors.len(), errors: if query.presentation.show_errors { scan.errors } else { Vec::new() },
        pruned_directories: scan.pruned_directories, unmeasured_bytes_upper_bound: scan.unmeasured_bytes_upper_bound,
        known_unassigned_bytes: scan.known_unassigned_bytes,
    };
    finish(&scan.root, rows, &scan.levels, metadata, query)
}

fn finish(root: &Path, mut rows: Vec<Row>, levels: &[hierarchy::Totals], metadata: Metadata, query: &DiskQuery) -> Report {
    let total: u64 = rows.iter().map(|row| row.bytes).sum();
    for row in rows.iter_mut() { row.percent = percent(row.bytes, total); }
    rows.retain(|row| query.presentation.include_zeroes || row.bytes > 0);
    let all_rows = rows.len();
    rows.retain(|row| row.percent >= query.presentation.min_percent);
    rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.label.cmp(&b.label)));
    rows.truncate(query.presentation.top);
    let shown_bytes = rows.iter().map(|row| row.bytes).sum();
    let directory_tree = (query.view == DiskView::Summary && query.presentation.depth == 1).then(|| {
        hierarchy::build(root, &rows, total, metadata.selected_files as u64, levels, query.presentation.min_percent)
    });
    let filesystem_space = if directory_tree.is_some() && query.accounting == SizeAccounting::Allocated
        && query.traversal.one_file_system { space::Space::read(root) } else { None };
    Report {
        metadata, total_bytes: total, shown_bytes, hidden_bytes: total.saturating_sub(shown_bytes),
        hidden_rows: all_rows.saturating_sub(rows.len()), rows, directory_tree, filesystem_space,
        compact: query.output.compact_json(),
    }
}

fn select_files<'a>(scan: &'a Scan, query: &DiskQuery) -> (Vec<Selected<'a>>, usize) {
    // Keep the smallest path for each inode in linear time. Sorting every path just to
    // choose a deterministic hard-link representative dominated the pause after scanning.
    let mut identities = HashMap::new();
    let mut representatives: Vec<&FileFact> = Vec::with_capacity(scan.files.len());
    for fact in &scan.files {
        match identities.entry((fact.device, fact.inode)) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(representatives.len());
                representatives.push(fact);
            }
            std::collections::hash_map::Entry::Occupied(entry) => {
                let representative = &mut representatives[*entry.get()];
                if fact.path.as_os_str() < representative.path.as_os_str() { *representative = fact; }
            }
        }
    }
    let duplicates = scan.files.len() - representatives.len();
    drop(identities);
    let mut selected = Vec::with_capacity(representatives.len());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    for fact in representatives {
        let bytes = bytes(fact, query.accounting);
        if query.selection.min_size.is_some_and(|minimum| bytes < minimum) { continue; }
        if query.selection.older_than.is_some_and(|age| now.saturating_sub(fact.modified) < age.as_secs() as i64) { continue; }
        selected.push(Selected { fact, bytes });
    }
    (selected, duplicates)
}

fn summary_rows(root: &Path, totals: hierarchy::Totals) -> Vec<Row> {
    totals.into_iter().map(|(path, (bytes, files))| Row {
        label: directory_label(root, &path), bytes, percent: 0.0,
        files, uid: None, modified_unix: None, apparent_bytes: None, allocated_bytes: None, links: None }).collect()
}

fn directory_levels(scan: &Scan, query: &DiskQuery, files: &[Selected<'_>], depths: &[usize]) -> Vec<hierarchy::Totals> {
    // Files arrive in directory batches. Sum each batch before looking up its directory,
    // then compute ring prefixes once per directory instead of once per file per ring.
    let mut directories: HashMap<&OsStr, (u64, u64)> = HashMap::new();
    let mut batch: Option<(&OsStr, u64, u64)> = None;
    for file in files {
        let parent = file.fact.path.parent().unwrap_or(&scan.root).as_os_str();
        if let Some((directory, bytes, count)) = &mut batch {
            if *directory == parent { *bytes += file.bytes; *count += 1; continue; }
            let entry = directories.entry(directory).or_default();
            entry.0 += *bytes; entry.1 += *count;
        }
        batch = Some((parent, file.bytes, 1));
    }
    if let Some((directory, bytes, files)) = batch {
        let entry = directories.entry(directory).or_default();
        entry.0 += bytes; entry.1 += files;
    }
    let directory_metadata = !has_file_filter(query);
    if directory_metadata || query.presentation.include_zeroes {
        for directory in &scan.directories {
            let entry = directories.entry(directory.path.as_os_str()).or_default();
            if directory_metadata { entry.0 += directory_bytes(directory, query.accounting); }
        }
    }
    let mut levels: Vec<HashMap<&OsStr, (u64, u64)>> = depths.iter().map(|_| HashMap::new()).collect();
    for (directory, (bytes, files)) in directories {
        add_directory_usage(&scan.root, &mut levels, depths, Path::new(directory), bytes, files);
    }
    levels.into_iter().map(|groups| groups.into_iter().map(|(relative, totals)| {
        (scan.root.join(relative), totals)
    }).collect()).collect()
}

fn add_directory_usage<'a>(root: &Path, levels: &mut [HashMap<&'a OsStr, (u64, u64)>],
    depths: &[usize], path: &'a Path, bytes: u64, files: u64) {
    let relative = path.strip_prefix(root).unwrap_or(path);
    for (groups, depth) in levels.iter_mut().zip(depths) {
        let entry = groups.entry(directory_prefix(relative, *depth)).or_default();
        entry.0 += bytes; entry.1 += files;
    }
}

#[cfg(test)]
fn directory_group(root: &Path, directory: &Path, depth: usize) -> PathBuf {
    let relative = directory.strip_prefix(root).unwrap_or(directory);
    if relative.as_os_str().is_empty() { return root.to_path_buf(); }
    root.join(directory_prefix(relative, depth))
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
        writeln!(out, "{}  {}  {}  {}  {} space  {}  {} {}", bold("dq", context.colors), dim("|", context.colors),
            self.metadata.view, dim("|", context.colors), self.metadata.accounting, dim("|", context.colors), human_plain(self.total_bytes),
            if self.metadata.pruned_directories > 0 { "measured" } else { "total" })?;
        writeln!(out, "{}", self.metadata.root)?;
        writeln!(out, "{}", dim("----------------------------------------------------------------", context.colors))
    }
    fn write_text<W: Write>(&self, out: &mut W, context: RenderContext) -> io::Result<()> { self.write_header(out, context)?; self.write_rows(out, context) }
    fn write_rows<W: Write>(&self, out: &mut W, context: RenderContext) -> io::Result<()> {
        if let Some(tree) = &self.directory_tree { return self.write_directory_rows(out, context, tree); }
        let bar_heading = if self.metadata.view == "files" { "relative size" } else { "share of total" };
        writeln!(out, "{}", dim(&format!("{:>10}  {:>7}  {:>8}  {:<20}  {}", "size", "percent", "files", bar_heading, "name"), context.colors))?;
        if self.rows.is_empty() { return writeln!(out, "{}", dim("no files matched this query", context.colors)); }
        let largest = self.rows.iter().map(|row| row.bytes).max().unwrap_or(0);
        let colors = chart::identity_colors(self.rows.iter().map(|row| row.label.as_str()));
        for (row, color) in self.rows.iter().zip(colors) {
            let ratio = if self.metadata.view == "files" {
                if largest == 0 { 0.0 } else { row.bytes as f64 / largest as f64 }
            } else { row.percent / 100.0 };
            let fill = chart::bar_fill(ratio, chart::BAR_WIDTH);
            let bar = format!("{}{}", "#".repeat(fill), ".".repeat(chart::BAR_WIDTH - fill));
            let label = context.width.map_or_else(|| row.label.clone(), |width| style::truncate_middle(&row.label, width.saturating_sub(55).max(12)));
            let swatch = if context.colors { format!("{}  ", style::color_swatch(color, true)) } else { String::new() };
            writeln!(out, "{}{:>10}  {:>6.2}%  {:>8}  {}  {}", swatch, human_plain(row.bytes), row.percent, row.files, dim(&bar, context.colors), label)?;
        }
        if self.hidden_rows > 0 {
            writeln!(out, "{}", dim(&format!("{} across {} hidden row(s) ({:.2}% of total)",
                human_plain(self.hidden_bytes), self.hidden_rows, percent(self.hidden_bytes, self.total_bytes)), context.colors))?;
        }
        Ok(())
    }
    fn write_directory_rows<W: Write>(&self, out: &mut W, context: RenderContext, tree: &hierarchy::Breakdown) -> io::Result<()> {
        let bars = context.target == RenderTarget::Text && context.width.is_none_or(|width| width >= 80);
        let share_label = if self.filesystem_space.is_some() { "capacity" } else { "total" };
        writeln!(out, "{}", dim(&format!("   {:>10}  {:>7}  {}path", "size", share_label, if bars { "share         " } else { "" }), context.colors))?;
        if self.total_bytes == 0 && tree.nodes.is_empty() {
            writeln!(out, "{}", dim("no files matched this query", context.colors))?;
        }
        for node in &tree.nodes { self.write_directory_node(out, context, node, 0, bars)?; }
        if let Some(remainder) = &tree.remainder {
            write_directory_row(out, context, remainder.bytes, self.directory_percent(remainder.bytes), remainder.color,
                &format!("[{} hidden directory groups]", self.hidden_rows), 0, bars)?;
        }
        if let Some(space) = &self.filesystem_space {
            writeln!(out, "Filesystem: {} capacity, {} available.",
                human_plain(space.capacity_bytes), human_plain(space.available_bytes))?;
            write_directory_row(out, context, space.free_bytes, percent(space.free_bytes, space.capacity_bytes),
                space::FREE_COLOR, "[free space, including reserved blocks]", 0, bars)?;
            if space.used_bytes > self.total_bytes {
                let other = space.used_bytes - self.total_bytes;
                write_directory_row(out, context, other, percent(other, space.capacity_bytes),
                    space::USED_COLOR, if self.metadata.pruned_directories > 0 {
                        "[unmeasured + other filesystem use]"
                    } else { "[other filesystem use outside scan]" }, 0, bars)?;
            }
        }
        if let Some(path) = tree.next_path() {
            writeln!(out, "Dig deeper: {}", bold(&format!("dq -- {}", quote_path(path)), context.colors))?;
        }
        Ok(())
    }
    fn write_graphics<W: Write>(&self, out: &mut W, context: RenderContext) -> io::Result<()> {
        self.write_graphics_with(out, context, graphics::draw)
    }
    fn write_graphics_with<W: Write>(&self, out: &mut W, context: RenderContext,
        mut draw: impl FnMut(&image::DynamicImage, &viuer::Config) -> bool) -> io::Result<()> {
        self.write_header(out, context)?;
        if (self.total_bytes == 0 || self.rows.is_empty()) && self.directory_tree.is_none() {
            return self.write_rows(out, context);
        }
        // A long legend scrolls an earlier image into history. Kitty's unlimited
        // scrollback can discard that image, leaving only its reserved blank rows.
        // Finish the text first so the chart stays on screen when dq exits.
        self.write_rows(out, context)?;
        let started = std::time::Instant::now();
        let mut image = if self.directory_tree.is_some() {
            donut::build_sunburst_image(&self.directory_rings(), chart::DONUT_PX)
        } else {
            let items: Vec<(String, u64)> = self.rows.iter().map(|row| (row.label.clone(), row.bytes)).collect();
            let segments = chart::identity_segments(&items, self.total_bytes, style::PALETTE.len());
            donut::build_donut_image(&chart::slices(&segments), chart::DONUT_PX)
        };
        let total = self.filesystem_space.as_ref().map_or(self.total_bytes, |space| space.used_bytes);
        donut::label_center(&mut image, &style::human_bytes(total),
            if self.directory_tree.is_some() { 0.16 } else { 0.27 });
        let chart_at = started.elapsed();
        out.flush()?;
        let config = graphics::config(context.width, if self.directory_tree.is_some() { 48 } else { 40 });
        if !draw(&image, &config) && self.directory_tree.is_some()
            && context.width.is_none_or(|width| width >= 80) {
            // The directory legend above omits bars when a chart is expected. Restore them after
            // a failed draw so this path remains as useful as ordinary text output.
            writeln!(out, "Chart unavailable; showing text bars:")?;
            self.write_rows(out, RenderContext { target: RenderTarget::Text, ..context })?;
        }
        if std::env::var_os("QTOOLS_DEBUG").is_some() {
            eprintln!("dq[debug]: chart={}ms terminal={}ms", chart_at.as_millis(),
                started.elapsed().saturating_sub(chart_at).as_millis());
        }
        Ok(())
    }
    fn directory_rings(&self) -> Vec<Vec<donut::LabeledSlice>> {
        let mut rings = self.directory_tree.as_ref().expect("directory chart has a tree").rings();
        if let Some(space) = &self.filesystem_space {
            for (level, ring) in rings.iter_mut().enumerate() {
                if self.total_bytes > space.used_bytes {
                    // Map cumulative boundaries, not individual rounded slices, so
                    // children still align when shared blocks exceed physical use.
                    let (mut offset, mut mapped) = (0_u128, 0_u64);
                    for slice in &mut *ring {
                        offset += slice.value as u128;
                        let end = (offset * space.used_bytes as u128 / self.total_bytes as u128) as u64;
                        slice.value = end - mapped;
                        mapped = end;
                    }
                }
                ring.push(donut::LabeledSlice { value: space.used_bytes.saturating_sub(self.total_bytes),
                    color: (level == 0).then_some(space::USED_COLOR), label: "filesystem-other".into() });
                ring.push(donut::LabeledSlice { value: space.free_bytes,
                    color: (level == 0).then_some(space::FREE_COLOR), label: "filesystem-free".into() });
            }
        }
        rings
    }
    fn directory_percent(&self, bytes: u64) -> f64 {
        if let Some(space) = &self.filesystem_space {
            if self.total_bytes > space.used_bytes {
                let scaled = (bytes as u128 * space.used_bytes as u128 / self.total_bytes as u128) as u64;
                percent(scaled, space.capacity_bytes)
            } else { percent(bytes, space.capacity_bytes) }
        } else { percent(bytes, self.total_bytes) }
    }
    fn write_directory_node<W: Write>(&self, out: &mut W, context: RenderContext, node: &hierarchy::Node,
        depth: usize, bars: bool) -> io::Result<()> {
        write_directory_row(out, context, node.bytes, self.directory_percent(node.bytes), node.color, &node.label, depth, bars)?;
        for child in &node.children { self.write_directory_node(out, context, child, depth + 1, bars)?; }
        if let Some(remainder) = &node.remainder {
            write_directory_row(out, context, remainder.bytes, self.directory_percent(remainder.bytes),
                remainder.color, "[files here + smaller subdirs]", depth + 1, bars)?;
        }
        Ok(())
    }
    fn write_json<W: Write>(&self, out: &mut W) -> io::Result<()> {
        if self.compact { serde_json::to_writer(&mut *out, self) } else { serde_json::to_writer_pretty(&mut *out, self) }.map_err(io::Error::other)?;
        writeln!(out)
    }
}

#[allow(clippy::too_many_arguments)]
fn write_directory_row<W: Write>(out: &mut W, context: RenderContext, bytes: u64, percent: f64,
    color: chart::Color, label: &str, depth: usize, bars: bool) -> io::Result<()> {
    let bar = if bars {
        let fill = chart::bar_fill(percent / 100.0, 12);
        format!("{}{}  ", "#".repeat(fill), ".".repeat(12 - fill))
    } else { String::new() };
    let indent = "  ".repeat(depth);
    let label = context.width.map_or_else(|| label.to_string(), |width| {
        style::truncate_middle(label, width.saturating_sub(24 + bar.len() + indent.len()).max(12))
    });
    writeln!(out, "{} {:>10}  {:>6.2}%  {}{}{}", style::color_swatch(color, context.colors),
        human_plain(bytes), percent, dim(&bar, context.colors), indent,
        if depth == 0 { bold(&label, context.colors) } else { label })
}

fn quote_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    if !path.is_empty() && path.chars().all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)) {
        path.into_owned()
    } else { format!("'{}'", path.replace('\'', "'\\''")) }
}

fn bytes(file: &FileFact, accounting: SizeAccounting) -> u64 { match accounting { SizeAccounting::Allocated => file.allocated, SizeAccounting::Apparent => file.apparent } }
fn directory_bytes(directory: &DirectoryFact, accounting: SizeAccounting) -> u64 {
    match accounting { SizeAccounting::Allocated => directory.allocated, SizeAccounting::Apparent => 0 }
}
fn has_file_filter(query: &DiskQuery) -> bool { query.selection.older_than.is_some() || query.selection.min_size.is_some() }
pub(super) fn percent(value: u64, total: u64) -> f64 { if total == 0 { 0.0 } else { (value as f64 / total as f64 * 10_000.0).round() / 100.0 } }
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
    use crate::dq::Args;
    use std::path::PathBuf;
    #[test] fn directory_group_caps_depth() { let root = Path::new("/x");
        for (depth, expected) in [(0, "/x"), (1, "/x/a"), (2, "/x/a/b"), (usize::MAX, "/x/a/b/c")] {
            assert_eq!(directory_group(root, Path::new("/x/a/b/c"), depth), PathBuf::from(expected));
        }
        let relative = Path::new(OsStr::from_bytes(b"a/\xff/b"));
        assert_eq!(directory_prefix(relative, 2), OsStr::from_bytes(b"a/\xff"));
    }
    #[test] fn percentages_are_stable() { assert_eq!(percent(1, 4), 25.0); assert_eq!(percent(0, 0), 0.0); }
    #[test] fn age_boundaries_are_explicit() { assert_eq!(age_bucket(6), "under 7 days");
        assert_eq!(age_bucket(7), "7–30 days"); assert_eq!(age_bucket(365), "over 1 year"); }

    fn hierarchy_scan() -> Scan {
        let root = PathBuf::from("/expanse");
        let mut files: Vec<_> = [
            ("code/sites/large/a", 400), ("code/sites/other/b", 180),
            ("code/oss/project/a", 220), ("code/direct", 30), ("code/cache/small", 10),
            ("Documents/report", 80), ("swapfile", 80),
        ].into_iter().enumerate().map(|(inode, (path, size))| FileFact {
            path: root.join(path), apparent: size, allocated: size, device: 1, inode: inode as u64,
            links: 1, uid: 1000, modified: 0,
        }).collect();
        let mut link = files[0].clone();
        link.path = root.join("code/sites/large/a-link"); link.links = 2; files.push(link);
        let directories = ["", "code", "code/sites", "code/sites/large", "code/oss", "Documents"]
            .into_iter().map(|path| DirectoryFact { path: root.join(path), allocated: 10, uid: 1000 }).collect();
        Scan { root, files, directories, errors: Vec::new(), duration_ms: 0 }
    }

    fn interval(ring: &[donut::LabeledSlice], label: &str) -> (u64, u64) {
        let mut offset = 0;
        for slice in ring {
            if slice.label == label { return (offset, offset + slice.value); }
            offset += slice.value;
        }
        panic!("missing arc {label}");
    }

    #[test]
    fn parent_distribution_and_subdirectory_arcs_are_additive_and_aligned() {
        let scan = hierarchy_scan();
        let query = DiskQuery::from_args(&Args::try_parse_from(["dq", "--apparent"]).unwrap()).unwrap();
        let report = build(&scan, &query);
        assert_eq!(report.total_bytes, 1000);
        assert_eq!(report.metadata.duplicate_hard_links, 1);
        assert_eq!(report.rows.len(), 3);
        assert_eq!(report.rows[0].label, "/expanse/code");
        assert_eq!(report.rows[0].bytes, 840);
        let tree = report.directory_tree.as_ref().unwrap();
        let code = &tree.nodes[0];
        fn collect_colors(node: &hierarchy::Node, colors: &mut HashSet<chart::Color>) {
            assert!(colors.insert(node.color), "significant directories must have distinct colors");
            for child in &node.children { collect_colors(child, colors); }
        }
        let mut colors = HashSet::new();
        for node in &tree.nodes { collect_colors(node, &mut colors); }
        assert_eq!(code.children.iter().map(|child| child.label.as_str()).collect::<Vec<_>>(), ["code/sites", "code/oss"]);
        assert_eq!(code.remainder.as_ref().unwrap().bytes, 40);
        assert_eq!(code.remainder.as_ref().unwrap().files, 2);
        assert_eq!(code.children[0].children[0].label, "code/sites/large");
        let rings = tree.rings();
        assert_eq!(rings.len(), 3);
        for ring in &rings { assert_eq!(ring.iter().map(|slice| slice.value).sum::<u64>(), report.total_bytes); }
        assert_eq!(interval(&rings[0], "1"), (0, 840));
        assert_eq!(interval(&rings[1], "1.1"), (0, 580));
        assert_eq!(interval(&rings[1], "1.2"), (580, 800));
        assert_eq!(interval(&rings[1], "1.*"), (800, 840));
        assert_eq!(interval(&rings[2], "1.1.1"), (0, 400));
        assert_eq!(interval(&rings[2], "1.1.2"), (400, 580));
        assert_eq!(interval(&rings[2], "1.2.1"), (580, 800));
        let mut output = Vec::new();
        report.write_graphics_with(&mut output,
            RenderContext { target: RenderTarget::Graphics, colors: true, width: Some(80) }, |image, _| {
                assert_eq!(image.width(), chart::DONUT_PX);
                true
            }).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("code/sites/large"));
        assert!(output.contains("dq -- /expanse/code/sites/large"));
        for node in [&tree.nodes[0], &code.children[0], &code.children[0].children[0]] {
            assert!(output.contains(&style::color_swatch(node.color, true)));
        }
        assert!(output.contains(&style::color_swatch(code.remainder.as_ref().unwrap().color, true)));
        let mut relative_scan = hierarchy_scan();
        relative_scan.root = PathBuf::from(".");
        for file in &mut relative_scan.files { file.path = Path::new(".").join(file.path.strip_prefix("/expanse").unwrap()); }
        for directory in &mut relative_scan.directories { directory.path = Path::new(".").join(directory.path.strip_prefix("/expanse").unwrap()); }
        let relative = build(&relative_scan, &query);
        assert_eq!(relative.directory_tree.as_ref().unwrap().nodes[0].color, code.color);
    }

    #[test]
    fn directory_tree_uses_selected_accounting_and_retains_hidden_bytes() {
        let scan = hierarchy_scan();
        for (arguments, expected) in [
            (vec!["dq"], 1060), (vec!["dq", "--min-size", "100"], 800),
            (vec!["dq", "--apparent", "--top", "1"], 1000),
        ] {
            let query = DiskQuery::from_args(&Args::try_parse_from(arguments).unwrap()).unwrap();
            let report = build(&scan, &query);
            assert_eq!(report.total_bytes, expected);
            let tree = report.directory_tree.as_ref().unwrap();
            for ring in tree.rings() { assert_eq!(ring.iter().map(|slice| slice.value).sum::<u64>(), expected); }
            let mut json = Vec::new(); report.write_json(&mut json).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
            assert_eq!(value["directory_tree"]["nodes"][0]["bytes"], value["rows"][0]["bytes"]);
            if query.presentation.top == 1 {
                assert_eq!(tree.remainder.as_ref().unwrap().bytes, 160);
                assert_eq!(tree.remainder.as_ref().unwrap().files, 2);
            }
        }
        let query = DiskQuery::from_args(&Args::try_parse_from(["dq", "--depth", "2"]).unwrap()).unwrap();
        assert!(build(&scan, &query).directory_tree.is_none());
        assert_eq!(quote_path(Path::new("/data/a'b")), "'/data/a'\\''b'");
    }

    #[test]
    fn free_space_shares_the_parent_ring_and_stays_accurate_with_filters() {
        let query = DiskQuery::from_args(&Args::try_parse_from(["dq", "--top", "1", "--min-size", "100"]).unwrap()).unwrap();
        let mut report = build(&hierarchy_scan(), &query);
        assert_eq!(report.total_bytes, 800);
        report.filesystem_space = Some(space::Space {
            capacity_bytes: 2000, used_bytes: 1500, free_bytes: 500, available_bytes: 400,
        });
        let rings = report.directory_rings();
        assert_eq!(rings.len(), 3, "capacity must not add a redundant fourth ring");
        for (level, ring) in rings.iter().enumerate() {
            assert_eq!(ring.iter().map(|slice| slice.value).sum::<u64>(), 2000);
            assert_eq!(interval(ring, "filesystem-other"), (800, 1500));
            assert_eq!(interval(ring, "filesystem-free"), (1500, 2000));
            let other = ring.iter().find(|slice| slice.label == "filesystem-other").unwrap();
            let free = ring.iter().find(|slice| slice.label == "filesystem-free").unwrap();
            assert_eq!(other.color, (level == 0).then_some(space::USED_COLOR));
            assert_eq!(free.color, (level == 0).then_some(space::FREE_COLOR));
        }
        assert_eq!(interval(&rings[0], "1"), (0, 800));
        assert_eq!(interval(&rings[1], "1.1"), (0, 580));
        let mut text = Vec::new();
        report.write_text(&mut text, RenderContext { target: RenderTarget::Text, colors: false, width: Some(100) }).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("25.00%") && text.contains("[free space, including reserved blocks]"));
        assert!(text.contains("35.00%") && text.contains("[other filesystem use outside scan]"));
        assert!(text.contains("40.00%") && text.contains("code"));
        assert!(!text.contains("[filesystem used]"));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["total_bytes"], 800);
        assert_eq!(json["filesystem_space"]["available_bytes"], 400);
    }

    #[test]
    fn chart_center_shows_used_space_and_selected_totals() {
        for (arguments, filesystem, expected, radius) in [
            (vec!["dq", "--min-size", "100"], true, "1.5KB", 0.16),
            (vec!["dq", "--min-size", "100"], false, "800B", 0.16),
            (vec!["dq", "--files", "--apparent"], false, "1KB", 0.27),
        ] {
            let query = DiskQuery::from_args(&Args::try_parse_from(arguments).unwrap()).unwrap();
            let mut report = build(&hierarchy_scan(), &query);
            report.filesystem_space = filesystem.then_some(space::Space {
                capacity_bytes: 2000, used_bytes: 1500, free_bytes: 500, available_bytes: 400,
            });
            let mut expected_image = image::DynamicImage::new_rgba8(chart::DONUT_PX, chart::DONUT_PX);
            donut::label_center(&mut expected_image, expected, radius);
            let expected_image = expected_image.to_rgba8();
            report.write_graphics_with(&mut Vec::new(),
                RenderContext { target: RenderTarget::Graphics, colors: false, width: Some(80) }, |image, _| {
                    let actual = image.to_rgba8();
                    let center = chart::DONUT_PX as f64 / 2.0;
                    for (x, y, pixel) in actual.enumerate_pixels() {
                        if (x as f64 - center).hypot(y as f64 - center) < chart::DONUT_PX as f64 * radius - 2.0 {
                            assert_eq!(pixel, expected_image.get_pixel(x, y), "center must show {expected}");
                        }
                    }
                    true
                }).unwrap();
        }
    }

    #[test]
    fn partial_measurements_keep_free_space_and_unresolved_use_in_the_outer_ring() {
        let query = DiskQuery::from_args(&Args::try_parse_from(["dq"]).unwrap()).unwrap();
        let mut report = build(&hierarchy_scan(), &query);
        report.metadata.pruned_directories = 10;
        report.metadata.unmeasured_bytes_upper_bound = Some(600);
        report.filesystem_space = Some(space::Space {
            capacity_bytes: 2000, used_bytes: 1500, free_bytes: 500, available_bytes: 400,
        });
        for (level, ring) in report.directory_rings().iter().enumerate() {
            assert_eq!(ring.iter().map(|slice| slice.value).sum::<u64>(), 2000);
            assert_eq!(interval(ring, "filesystem-free"), (1500, 2000));
            let unknown = ring.iter().find(|slice| slice.label == "filesystem-other").unwrap();
            assert_eq!(unknown.value, 1500 - report.total_bytes);
            assert_eq!(unknown.color.is_some(), level == 0);
        }
        let mut output = Vec::new();
        report.render(&mut output, RenderContext { target: RenderTarget::Text, colors: false, width: None }).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("measured\n"));
        assert!(text.contains("[unmeasured + other filesystem use]"));
    }

    #[test]
    fn capacity_remains_accurate_when_file_blocks_exceed_physical_use() {
        let query = DiskQuery::from_args(&Args::try_parse_from(["dq"]).unwrap()).unwrap();
        let mut report = build(&hierarchy_scan(), &query);
        report.filesystem_space = Some(space::Space {
            capacity_bytes: 1000, used_bytes: 750, free_bytes: 250, available_bytes: 200,
        });
        let rings = report.directory_rings();
        assert_eq!(rings.len(), 3);
        for ring in &rings {
            assert_eq!(interval(ring, "filesystem-free"), (750, 1000));
            assert_eq!(ring.iter().map(|slice| slice.value).sum::<u64>(), 1000);
        }
        assert_eq!(interval(&rings[0], "1"), (0, 622));
        assert_eq!(interval(&rings[1], "1.1"), (0, 424));
        assert_eq!(interval(&rings[1], "1.2"), (424, 587));
        assert_eq!(interval(&rings[2], "1.1.1"), (0, 290));
        assert_eq!(interval(&rings[2], "1.1.2"), (290, 417));
        for arguments in [["dq", "--apparent"], ["dq", "--cross-filesystems"]] {
            let query = DiskQuery::from_args(&Args::try_parse_from(arguments).unwrap()).unwrap();
            assert!(build(&hierarchy_scan(), &query).filesystem_space.is_none());
        }
    }

    #[test]
    fn scan_order_does_not_change_hard_link_accounting_or_directory_totals() {
        let mut scan = hierarchy_scan();
        // Different metadata on two observations still belongs to the smallest path,
        // including when filtering it out. Inodes on different devices stay separate.
        scan.files[0].apparent = 450;
        let mut other_device = scan.files[0].clone();
        other_device.device = 2;
        other_device.path = scan.root.join("code/sites/large/other-device");
        scan.files.push(other_device);
        let mut reference = None;
        for _ in 0..2 {
            let query = DiskQuery::from_args(&Args::try_parse_from(["dq", "--files", "--all", "--apparent"]).unwrap()).unwrap();
            let report = build(&scan, &query);
            assert_eq!(report.total_bytes, 1500);
            assert_eq!(report.metadata.duplicate_hard_links, 1);
            let rows = serde_json::to_value(&report.rows).unwrap();
            if let Some(expected) = &reference { assert_eq!(&rows, expected); } else { reference = Some(rows); }
            let query = DiskQuery::from_args(&Args::try_parse_from(["dq", "--apparent", "--min-size", "425"]).unwrap()).unwrap();
            let report = build(&scan, &query);
            assert_eq!(report.total_bytes, 900);
            for ring in report.directory_tree.unwrap().rings() {
                assert_eq!(ring.iter().map(|slice| slice.value).sum::<u64>(), 900);
            }
            scan.files.reverse();
        }
    }

    #[test]
    fn streaming_summary_matches_detailed_accounting_and_filters() {
        use std::fs::{self, File, FileTimes};
        use std::os::unix::fs::symlink;
        use crate::dq::disk_scan;
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("qtools-dq-summary-{}-{unique}", std::process::id()));
        for directory in ["alpha/data/deep", "zeta/data", "empty", "ignored"] { fs::create_dir_all(root.join(directory)).unwrap(); }
        fs::write(root.join("alpha/data/deep/large"), vec![0; 8192]).unwrap();
        fs::write(root.join("alpha/small"), b"a").unwrap();
        fs::write(root.join("zeta/data/file"), vec![0; 4096]).unwrap();
        fs::hard_link(root.join("zeta/data/file"), root.join("alpha/link")).unwrap();
        fs::write(root.join("ignored/file"), vec![0; 16384]).unwrap();
        fs::write(root.join("zero"), b"").unwrap();
        fs::write(root.join(OsStr::from_bytes(b"raw-\xff")), b"raw name").unwrap();
        symlink(root.join("alpha"), root.join("alias")).unwrap();
        File::open(root.join("alpha/data/deep/large")).unwrap()
            .set_times(FileTimes::new().set_modified(std::time::UNIX_EPOCH)).unwrap();
        for arguments in [
            vec!["dq"], vec!["dq", "--apparent"], vec!["dq", "--all", "--zeroes"],
            vec!["dq", "--apparent", "--all", "--zeroes"], vec!["dq", "--top", "1"],
            vec!["dq", "--min-size", "8K"], vec!["dq", "--apparent", "--min-size", "8K", "--zeroes"],
            vec!["dq", "--older-than", "1d"], vec!["dq", "--min-size", "8K", "--older-than", "1d"],
            vec!["dq", "--exclude", "ignored"], vec!["dq", "--exclude", "alpha/*"],
            vec!["dq", "--depth", "0"], vec!["dq", "--depth", "2"], vec!["dq", "--depth", "5"],
        ] {
            let mut query = DiskQuery::from_args(&Args::try_parse_from(&arguments).unwrap()).unwrap();
            query.root = root.display().to_string(); query.traversal.show_progress = false;
            let detailed = disk_scan::scan(&root, 2, true, &query.selection.excludes, false);
            let mut expected = serde_json::to_value(build(&detailed, &query)).unwrap();
            expected["metadata"].as_object_mut().unwrap().remove("scan_duration_ms");
            expected["metadata"].as_object_mut().unwrap().remove("generated_at_unix_ms");
            for threads in [1, 4] {
                query.traversal.threads = threads;
                let mut actual = serde_json::to_value(build_summary(disk_scan::summarize(&root, &query), &query)).unwrap();
                actual["metadata"].as_object_mut().unwrap().remove("scan_duration_ms");
                actual["metadata"].as_object_mut().unwrap().remove("generated_at_unix_ms");
                assert_eq!(actual, expected, "streaming totals changed for {arguments:?} with {threads} threads");
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn graphics_follow_the_legend_and_failed_images_restore_text_bars() {
        use std::cell::RefCell;
        use std::rc::Rc;
        struct SharedOutput(Rc<RefCell<Vec<u8>>>);
        impl Write for SharedOutput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(bytes); Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        let query = DiskQuery::from_args(&Args::try_parse_from(["dq", "--apparent"]).unwrap()).unwrap();
        let report = build(&hierarchy_scan(), &query);
        for succeeds in [true, false] {
            let captured = Rc::new(RefCell::new(Vec::new()));
            let mut output = SharedOutput(Rc::clone(&captured));
            let mut length_at_image = 0;
            report.write_graphics_with(&mut output,
                RenderContext { target: RenderTarget::Graphics, colors: true, width: Some(80) }, |_, _| {
                    let bytes = captured.borrow();
                    let text = std::str::from_utf8(&bytes).unwrap();
                    assert!(text.contains("code/sites/large"));
                    assert!(text.contains("Dig deeper:"));
                    length_at_image = bytes.len();
                    succeeds
                }).unwrap();
            let bytes = captured.borrow();
            if succeeds {
                assert_eq!(bytes.len(), length_at_image,
                    "text after the image could scroll it out of the terminal");
            } else {
                let fallback = std::str::from_utf8(&bytes[length_at_image..]).unwrap();
                assert!(fallback.contains("Chart unavailable; showing text bars:"));
                assert!(fallback.contains("share"));
                assert!(fallback.contains("#"));
                assert!(!fallback.contains("dq  |"), "fallback repeated the report header");
            }
        }
    }

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
