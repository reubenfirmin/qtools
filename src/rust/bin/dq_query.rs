//! Typed interpretation and validation of dq's composable CLI axes.
use std::fs;
use std::time::Duration;
use qtools::output::{OutputFormat, OutputOptions};
use crate::Args;

#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum DiskView { Summary, Files, Owners, Age }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum SizeAccounting { Allocated, Apparent }
#[derive(Debug, Clone, PartialEq)] pub struct DiskSelection { pub excludes: Vec<String>, pub older_than: Option<Duration>, pub min_size: Option<u64> }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct TraversalOptions { pub threads: usize, pub show_progress: bool, pub one_file_system: bool }
#[derive(Debug, Clone, PartialEq)] pub struct PresentationOptions { pub top: usize, pub depth: usize, pub min_percent: f64, pub all: bool, pub include_zeroes: bool, pub show_errors: bool }
#[derive(Debug, Clone, PartialEq)] pub struct DiskQuery { pub root: String, pub selection: DiskSelection, pub traversal: TraversalOptions,
    pub presentation: PresentationOptions, pub accounting: SizeAccounting, pub view: DiskView, pub output: OutputOptions }

impl DiskQuery {
    pub fn from_args(args: &Args) -> Result<Self, String> {
        if [args.files, args.owners, args.age].into_iter().filter(|active| *active).count() > 1 {
            return Err("choose only one view (--files, --owners, or --age)".into());
        }
        if args.depth.is_some() && (args.files || args.owners || args.age) {
            return Err("--depth applies only to the directory summary view".into());
        }
        if args.min_percent.is_some_and(|percent| !(0.0..=100.0).contains(&percent)) {
            return Err("--min-percent must be between 0 and 100".into());
        }
        let mut excludes = args.exclude.clone();
        if let Some(path) = &args.exclude_from {
            let raw = fs::read_to_string(path).map_err(|error| format!("cannot read exclude file '{path}': {error}"))?;
            excludes.extend(raw.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')).map(str::to_string));
        }
        Ok(Self {
            root: args.dir.clone(),
            selection: DiskSelection { excludes, older_than: args.older_than, min_size: args.min_size },
            traversal: TraversalOptions { threads: args.threads.max(1) as usize, show_progress: !args.noprogress,
                one_file_system: !args.cross_filesystems },
            presentation: PresentationOptions { top: args.top.unwrap_or(if args.nosummary || args.all { usize::MAX } else { 15 }),
                depth: args.depth.unwrap_or(if args.nosummary || args.all { usize::MAX } else { 1 }),
                min_percent: if args.nosummary || args.all { 0.0 } else { args.min_percent.unwrap_or(if args.files || args.owners || args.age { 0.0 } else { 1.0 }) },
                all: args.nosummary || args.all,
                include_zeroes: args.zeroes, show_errors: args.errors },
            accounting: if args.apparent { SizeAccounting::Apparent } else { SizeAccounting::Allocated },
            view: if args.files { DiskView::Files } else if args.owners { DiskView::Owners }
                else if args.age { DiskView::Age } else { DiskView::Summary },
            output: OutputOptions { format: if args.jsonl { OutputFormat::JsonLines } else if args.json { OutputFormat::Json }
                else { OutputFormat::Rich }, top: args.top, verbose: args.nosummary || args.all, no_color: args.no_color },
        })
    }
}

#[cfg(test)] mod tests {
    use super::*; use clap::Parser;
    #[test] fn maps_composable_axes() {
        let args = Args::try_parse_from(["dq", "--files", "--apparent", "--older-than", "90d", "--exclude", "cache",
            "--top", "30", "--jsonl", "/tmp"]).unwrap();
        let plan = DiskQuery::from_args(&args).unwrap();
        assert_eq!(plan.view, DiskView::Files); assert_eq!(plan.accounting, SizeAccounting::Apparent);
        assert_eq!(plan.presentation.top, 30); assert_eq!(plan.selection.excludes, vec!["cache"]);
        assert_eq!(plan.output.format, OutputFormat::JsonLines);
    }
    #[test] fn rejects_conflicting_views_and_depth() {
        assert!(DiskQuery::from_args(&Args::try_parse_from(["dq", "--files", "--owners"]).unwrap()).is_err());
        assert!(DiskQuery::from_args(&Args::try_parse_from(["dq", "--files", "--depth", "2"]).unwrap()).is_err());
    }
    #[test] fn traversal_boundary_is_explicitly_overridable() {
        let default = DiskQuery::from_args(&Args::try_parse_from(["dq"]).unwrap()).unwrap();
        assert!(default.traversal.one_file_system);
        let crossing = DiskQuery::from_args(&Args::try_parse_from(["dq", "--cross-filesystems"]).unwrap()).unwrap();
        assert!(!crossing.traversal.one_file_system);
    }
}
