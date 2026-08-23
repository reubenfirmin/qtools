//! Typed interpretation and validation of dq's composable CLI axes.
use std::time::Duration;
use crate::output::OutputOptions;

#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum DiskView { Summary, Files, Owners, Age }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum SizeAccounting { Allocated, Apparent }
#[derive(Debug, Clone, PartialEq)] pub struct DiskSelection { pub excludes: Vec<String>, pub older_than: Option<Duration>, pub min_size: Option<u64> }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct TraversalOptions { pub threads: usize, pub show_progress: bool, pub one_file_system: bool }
#[derive(Debug, Clone, PartialEq)] pub struct PresentationOptions { pub top: usize, pub depth: usize, pub min_percent: f64, pub all: bool, pub include_zeroes: bool, pub show_errors: bool }
#[derive(Debug, Clone, PartialEq)] pub struct DiskQuery { pub root: String, pub selection: DiskSelection, pub traversal: TraversalOptions,
    pub presentation: PresentationOptions, pub accounting: SizeAccounting, pub view: DiskView, pub output: OutputOptions }


#[cfg(test)] mod tests {
    use super::*; use clap::Parser;
    use crate::dq::cli::Args;
    use crate::output::OutputFormat;
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
