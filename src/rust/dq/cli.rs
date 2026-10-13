use std::time::Duration;
use clap::{CommandFactory, Parser, ValueEnum, ValueHint};
use clap_complete::{generate, Shell};

use super::{parse_bytes, parse_duration};
use super::dq_query::{DiskQuery, DiskSelection, DiskView, PresentationOptions, SizeAccounting, TraversalOptions};
use crate::output::{OutputFormat, OutputOptions};
use std::fs;

#[derive(Parser, Debug)]
#[command(name = "dq", about = "dq: quantify what's eating your disk")]
pub(crate) struct Args {
    /// Print a completion script for the selected shell
    #[arg(long, value_enum, value_name = "SHELL", exclusive = true)] pub(super) generate_completion: Option<CompletionShell>,
    #[arg(long)] pub(super) files: bool,
    #[arg(long)] pub(super) owners: bool,
    #[arg(long)] pub(super) age: bool,
    #[arg(long, conflicts_with = "apparent")] pub(super) allocated: bool,
    #[arg(long, conflicts_with = "allocated")] pub(super) apparent: bool,
    #[arg(long, value_name = "PATTERN", action = clap::ArgAction::Append)] pub(super) exclude: Vec<String>,
    /// Load exclusion patterns from a text file, one per line
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)] pub(super) exclude_from: Option<String>,
    /// Select files modified at least AGE ago
    #[arg(long, value_name = "AGE", value_parser = parse_duration)] pub(super) older_than: Option<Duration>,
    /// Select files modified within the last AGE
    #[arg(long, value_name = "AGE", value_parser = parse_duration)] pub(super) newer_than: Option<Duration>,
    #[arg(long, value_name = "SIZE", value_parser = parse_bytes)] pub(super) min_size: Option<u64>,
    #[arg(long)] pub(super) top: Option<usize>,
    #[arg(long)] pub(super) depth: Option<usize>,
    #[arg(long)] pub(super) min_percent: Option<f64>,
    #[arg(long)] pub(super) all: bool,
    #[arg(short = 'v', long, hide = true)] pub(super) nosummary: bool,
    #[arg(short = 'V', long)] pub(super) zeroes: bool,
    #[arg(long)] pub(super) errors: bool,
    #[arg(long, conflicts_with = "jsonl")] pub(super) json: bool,
    #[arg(long, conflicts_with = "json")] pub(super) jsonl: bool,
    #[arg(long)] pub(super) no_color: bool,
    #[arg(long)] pub(super) cross_filesystems: bool,
    #[arg(long, conflicts_with = "cross_filesystems")] pub(super) one_file_system: bool,
    /// Scan every file (may take longer)
    #[arg(long)] pub(super) precise: bool,
    /// Maximum unresolved filesystem allocation as a percentage of capacity
    #[arg(long = "max-error", default_value_t = 3.0)] pub(super) max_error_percent: f64,
    #[arg(short, long, default_value_t = default_threads())] pub(super) threads: i32,
    #[arg(long = "noprogress")] pub(super) noprogress: bool,
    #[arg(default_value = ".", value_hint = ValueHint::DirPath)] pub(super) dir: String,
}

// Metadata concurrency is independent of CPU count. Users can override this
// compromise between outstanding reads and worker overhead with --threads.
const DEFAULT_SCAN_THREADS: i32 = 32;
fn default_threads() -> i32 { DEFAULT_SCAN_THREADS }

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum CompletionShell { Bash, Elvish, Fish, PowerShell, Zsh }

impl From<CompletionShell> for Shell {
    fn from(shell: CompletionShell) -> Self {
        match shell {
            CompletionShell::Bash => Shell::Bash,
            CompletionShell::Elvish => Shell::Elvish,
            CompletionShell::Fish => Shell::Fish,
            CompletionShell::PowerShell => Shell::PowerShell,
            CompletionShell::Zsh => Shell::Zsh,
        }
    }
}

pub(crate) fn generate_completion(shell: CompletionShell) {
    generate(Shell::from(shell), &mut Args::command(), "dq", &mut std::io::stdout());
}

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
        if !(0.0..=100.0).contains(&args.max_error_percent) {
            return Err("--max-error must be between 0 and 100".into());
        }
        let mut excludes = args.exclude.clone();
        if let Some(path) = &args.exclude_from {
            let raw = fs::read_to_string(path).map_err(|error| format!("cannot read exclude file '{path}': {error}"))?;
            excludes.extend(raw.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')).map(str::to_string));
        }
        Ok(Self {
            root: args.dir.clone(),
            selection: DiskSelection { excludes, older_than: args.older_than, newer_than: args.newer_than, min_size: args.min_size },
            traversal: TraversalOptions { threads: args.threads.max(1) as usize, show_progress: !args.noprogress,
                one_file_system: !args.cross_filesystems, precise: args.precise, max_error_percent: args.max_error_percent },
            presentation: PresentationOptions { top: args.top.unwrap_or(if args.nosummary || args.all { usize::MAX } else { 15 }),
                depth: args.depth.unwrap_or(1),
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
