//! Typed interpretation of dq's CLI. Some enums currently have one active variant; naming them
//! now keeps future file/directory views and allocated-size accounting composable.

use qtools::output::{OutputFormat, OutputOptions};

use crate::Args;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskView { Summary }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeAccounting { Apparent }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskSelection {
    pub include_all_nonempty: bool,
    pub include_zeroes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraversalOptions {
    pub threads: usize,
    pub show_progress: bool,
    pub one_file_system: bool,
    pub follow_links: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskQuery {
    pub root: String,
    pub selection: DiskSelection,
    pub traversal: TraversalOptions,
    pub accounting: SizeAccounting,
    pub view: DiskView,
    pub output: OutputOptions,
}

impl DiskQuery {
    pub fn from_args(args: &Args) -> Self {
        Self {
            root: args.dir.clone(),
            selection: DiskSelection {
                include_all_nonempty: args.nosummary,
                include_zeroes: args.zeroes,
            },
            traversal: TraversalOptions {
                threads: args.threads.max(1) as usize,
                show_progress: !args.noprogress,
                one_file_system: true,
                follow_links: false,
            },
            accounting: SizeAccounting::Apparent,
            view: DiskView::Summary,
            output: OutputOptions {
                format: if args.json { OutputFormat::Json } else { OutputFormat::Rich },
                top: None,
                verbose: args.nosummary,
                no_color: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn maps_current_cli_to_a_typed_disk_plan() {
        let args = Args::try_parse_from(["dq", "-V", "--threads", "0", "--noprogress", "--json", "/tmp"]).unwrap();
        let plan = DiskQuery::from_args(&args);
        assert_eq!(plan.root, "/tmp");
        assert_eq!(plan.traversal.threads, 1);
        assert!(!plan.traversal.show_progress);
        assert!(plan.selection.include_zeroes);
        assert_eq!(plan.accounting, SizeAccounting::Apparent);
        assert_eq!(plan.view, DiskView::Summary);
        assert_eq!(plan.output.format, OutputFormat::Json);
    }
}
