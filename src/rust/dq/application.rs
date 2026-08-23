use std::fs;
use std::path::Path;

use crate::render::{self, RenderContext};

use super::{disk_scan, dq_report};
use super::dq_query::DiskQuery;

pub(super) fn execute(plan: &DiskQuery) -> Result<(), String> {
    let metadata = fs::metadata(&plan.root)
        .map_err(|error| format!("cannot access '{}': {error}", plan.root))?;
    if !metadata.is_dir() { return Err(format!("'{}' is not a directory", plan.root)); }
    let scan = disk_scan::scan(Path::new(&plan.root), plan.traversal.threads,
        plan.traversal.one_file_system, &plan.selection.excludes,
        plan.traversal.show_progress && !plan.output.machine());
    if !scan.errors.is_empty() && !plan.output.machine() {
        eprintln!("dq: warning: {} path(s) could not be read; totals may be incomplete", scan.errors.len());
        if plan.presentation.show_errors {
            for error in &scan.errors { eprintln!("  {error}"); }
        }
    }
    let report = dq_report::build(&scan, plan);
    let context = RenderContext::detect(plan.output);
    render::stdout("dq", |out| report.render(out, context));
    Ok(())
}
