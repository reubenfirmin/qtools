//! Typed interpretation and validation of dq's composable CLI axes.
use std::time::Duration;
use crate::output::OutputOptions;

#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum DiskView { Summary, Files, Owners, Age }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum SizeAccounting { Allocated, Apparent }
#[derive(Debug, Clone, PartialEq)] pub struct DiskSelection {
    pub excludes: Vec<String>, pub older_than: Option<Duration>, pub newer_than: Option<Duration>, pub min_size: Option<u64>,
}
#[derive(Debug, Clone, PartialEq)] pub struct TraversalOptions { pub threads: usize, pub show_progress: bool, pub one_file_system: bool, pub precise: bool, pub max_error_percent: f64 }
#[derive(Debug, Clone, PartialEq)] pub struct PresentationOptions { pub top: usize, pub depth: usize, pub min_percent: f64, pub all: bool, pub include_zeroes: bool, pub show_errors: bool }
#[derive(Debug, Clone, PartialEq)] pub struct DiskQuery { pub root: String, pub selection: DiskSelection, pub traversal: TraversalOptions,
    pub presentation: PresentationOptions, pub accounting: SizeAccounting, pub view: DiskView, pub output: OutputOptions }

#[derive(Clone, Copy)]
pub(super) struct FileFilters { min_size: Option<u64>, older_than: Option<i64>, newer_than: Option<i64> }

impl DiskSelection {
    pub(super) fn has_file_filters(&self) -> bool {
        self.min_size.is_some() || self.older_than.is_some() || self.newer_than.is_some()
    }

    pub(super) fn file_filters(&self) -> FileFilters {
        let seconds = |age: Duration| i64::try_from(age.as_secs()).unwrap_or(i64::MAX);
        FileFilters { min_size: self.min_size, older_than: self.older_than.map(seconds), newer_than: self.newer_than.map(seconds) }
    }
}

impl FileFilters {
    pub(super) fn matches(self, bytes: u64, modified: i64, now: i64) -> bool {
        let age = now.saturating_sub(modified);
        !self.min_size.is_some_and(|minimum| bytes < minimum)
            && !self.older_than.is_some_and(|minimum| age < minimum)
            && !self.newer_than.is_some_and(|maximum| age > maximum)
    }
}


#[cfg(test)] mod tests {
    use super::*; use clap::Parser;
    use crate::dq::cli::Args;
    use crate::output::OutputFormat;
    #[test] fn maps_composable_axes() {
        let args = Args::try_parse_from(["dq", "--files", "--apparent", "--older-than", "90d", "--newer-than", "120d", "--exclude", "cache",
            "--top", "30", "--jsonl", "/tmp"]).unwrap();
        let plan = DiskQuery::from_args(&args).unwrap();
        assert_eq!(plan.view, DiskView::Files); assert_eq!(plan.accounting, SizeAccounting::Apparent);
        assert_eq!(plan.presentation.top, 30); assert_eq!(plan.selection.excludes, vec!["cache"]);
        assert_eq!(plan.selection.older_than, Some(Duration::from_secs(90 * 86_400)));
        assert_eq!(plan.selection.newer_than, Some(Duration::from_secs(120 * 86_400)));
        assert_eq!(plan.output.format, OutputFormat::JsonLines);
    }
    #[test] fn age_ranges_include_boundaries_and_combine_with_size() {
        let query = DiskQuery::from_args(&Args::try_parse_from([
            "dq", "--older-than", "2d", "--newer-than", "7d", "--min-size", "128",
        ]).unwrap()).unwrap();
        let filters = query.selection.file_filters();
        let now = 1_000_000;
        for (age, expected) in [(0, false), (172_799, false), (172_800, true),
            (345_600, true), (604_800, true), (604_801, false)] {
            assert_eq!(filters.matches(128, now - age, now), expected, "age {age}");
        }
        assert!(!filters.matches(127, now - 345_600, now));
        let recent = DiskQuery::from_args(&Args::try_parse_from(["dq", "--newer-than", "1d"]).unwrap()).unwrap();
        assert!(recent.selection.file_filters().matches(0, now, now));
        assert!(recent.selection.file_filters().matches(0, now + 1, now));
        assert!(!recent.selection.file_filters().matches(0, now - 86_401, now));
        assert!(Args::try_parse_from(["dq", "--newer-than", "invalid"]).is_err());
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
    #[test] fn precise_requests_a_complete_walk() {
        let default = DiskQuery::from_args(&Args::try_parse_from(["dq"]).unwrap()).unwrap();
        assert!(!default.traversal.precise);
        assert_eq!(default.traversal.max_error_percent, 3.0);
        let precise = DiskQuery::from_args(&Args::try_parse_from(["dq", "--precise"]).unwrap()).unwrap();
        assert!(precise.traversal.precise);
        let bounded = DiskQuery::from_args(&Args::try_parse_from(["dq", "--max-error", "0.277"]).unwrap()).unwrap();
        assert_eq!(bounded.traversal.max_error_percent, 0.277);
        for value in ["NaN", "101", "-1"] {
            if let Ok(args) = Args::try_parse_from(["dq", "--max-error", value]) {
                assert!(DiskQuery::from_args(&args).is_err());
            }
        }
    }
    #[test] fn all_keeps_parent_grouping_unless_depth_is_explicit() {
        for flag in ["--all", "-v"] {
            let query = DiskQuery::from_args(&Args::try_parse_from(["dq", flag]).unwrap()).unwrap();
            assert_eq!(query.presentation.depth, 1);
            assert_eq!(query.presentation.min_percent, 0.0);
            assert_eq!(query.presentation.top, usize::MAX);
            let deeper = DiskQuery::from_args(&Args::try_parse_from(["dq", flag, "--depth", "3"]).unwrap()).unwrap();
            assert_eq!(deeper.presentation.depth, 3);
        }
    }
}
