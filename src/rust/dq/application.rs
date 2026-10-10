use std::fs;
use std::path::Path;

use crate::render::{self, RenderContext};

use super::{disk_scan, dq_report, progress::Progress};
use super::dq_query::{DiskQuery, DiskView};

pub(super) fn execute(plan: &DiskQuery) -> Result<(), String> {
    let metadata = fs::metadata(&plan.root)
        .map_err(|error| format!("cannot access '{}': {error}", plan.root))?;
    if !metadata.is_dir() { return Err(format!("'{}' is not a directory", plan.root)); }
    let mut progress = Progress::new(plan.traversal.show_progress && !plan.output.machine());
    let report = if plan.view == DiskView::Summary && !plan.output.machine() {
        let scan = disk_scan::summarize(Path::new(&plan.root), plan);
        if std::env::var_os("QTOOLS_DEBUG").is_some() {
            eprintln!("dq[debug]: summary-scan={}ms files={} directories={} pruned={} duplicate-links={}",
                scan.duration_ms, scan.scanned_files, scan.directories, scan.pruned_directories, scan.duplicate_hard_links);
        }
        show_errors(&scan.errors, plan);
        progress.phase("Preparing directory chart…");
        dq_report::build_summary(scan, plan)
    } else {
        let scan = disk_scan::scan(Path::new(&plan.root), plan.traversal.threads,
            plan.traversal.one_file_system, &plan.selection.excludes,
            plan.traversal.show_progress && !plan.output.machine());
        if std::env::var_os("QTOOLS_DEBUG").is_some() {
            eprintln!("dq[debug]: scan={}ms files={} directories={}", scan.duration_ms, scan.files.len(), scan.directories.len());
        }
        show_errors(&scan.errors, plan);
        progress.phase("Summarizing disk usage…");
        dq_report::build(&scan, plan)
    };
    progress.phase("Preparing report…");
    let context = RenderContext::detect(plan.output);
    progress.finish();
    render::stdout("dq", |out| report.render(out, context));
    Ok(())
}

fn show_errors(errors: &[String], plan: &DiskQuery) {
    if errors.is_empty() || plan.output.machine() { return; }
    eprintln!("dq: warning: {} path(s) could not be read; totals may be incomplete", errors.len());
    if plan.presentation.show_errors {
        for error in errors { eprintln!("  {error}"); }
    }
}
