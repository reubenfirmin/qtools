//! dq: composable disk accounting and filesystem views.
mod progress; mod dq_query; mod disk_scan; mod dq_report; mod hierarchy; mod space; mod pruning; mod directory_tree;
mod cli;
mod application;

use std::time::Duration;

use clap::Parser;
use crate::help::{self, Request, Topic};

use dq_query::DiskQuery;
use cli::Args;

const HELP: &str = "\
dq — query what is using your disk

Usage:
  dq [DIR]                   Directory summary
  dq --files [DIR]           Largest files
  dq --owners [DIR]          Usage by owner

Focused help:
  dq help views              Files, owners, and age
  dq help filters            Exclusions, age, and size selection
  dq help scan               Accounting and traversal
  dq help output             Limits, detail, JSON, and errors";

const HELP_VIEWS: &str = "\
dq views

  dq DIR                     Parent directories with significant subdirectories
  dq --files DIR             Largest files recursively
  dq --owners DIR            Allocated bytes grouped by owner
  dq --age DIR               Bytes grouped into modification-age buckets

Primary views conflict. Combine a view with filters instead, for example:
  dq --files --older-than 90d --top 30 /var";

const HELP_FILTERS: &str = "\
dq filters

  --exclude PATTERN          Exclude a name or root-relative path; repeatable; * and ? supported
  --exclude-from FILE        Read patterns, one per line (# comments allowed)
  --older-than AGE           Only files at least this old: 12h, 30d, 6w, 1y
  --min-size SIZE            Only files at least this large: 10M, 2GiB

Filters apply before every total and aggregation.

Examples:
  dq --exclude node_modules --exclude '*.cache' .
  dq --files --older-than 90d --min-size 1G /var";

const HELP_SCAN: &str = "\
dq accounting and traversal

  --allocated               Count allocated filesystem blocks (default)
  --apparent                Count logical file lengths
  --one-file-system         Stay on the starting filesystem (default)
  --cross-filesystems       Traverse mounted filesystems too
  --precise                 Walk every directory; disable chart-based early stopping
  --max-error PERCENT       Unresolved allocation budget (% of capacity; default 3)
  -t, --threads N           Directory workers (default 50)
  --noprogress              Disable progress

Hard links are counted once by device/inode. Symlinks are not followed.
Directory summaries accumulate totals during the walk instead of retaining every file record.
At ext4 filesystem roots, the default chart uses measured usage and filesystem used space to
bound unseen bytes, prioritize productive directories, and stop when the error budget is met.
Incomplete sizes are lower bounds; unresolved space stays visible in the outer ring.
--max-error bounds the total unresolved allocation, not the error of each directory. Use
--max-error 0.277 for approximately one degree, or --precise for complete totals.
--precise, JSON, --all, filters, and other views always complete the walk. Subdirectories and
filesystems without a suitable allocation bound also complete the walk.";

const HELP_OUTPUT: &str = "\
dq presentation and output

  --top N                   Rows to show (default 15)
  --depth N                 Directory grouping depth (default 1, with subdirectory rings)
  --min-percent PERCENT     Hide groups below this share of the scan (default 1)
  --all                     Show all groups (-v is a compatibility alias)
  -V, --zeroes              Include empty directory groups
  --errors                  List unreadable paths
  --json / --jsonl          Pretty or one-line machine-readable output
  --no-color                Disable color and terminal graphics

The default directory chart reads from the outside in: parents, children, grandchildren.
Child arcs stay within their parent's angle; swatches match the path list. Small views spread
directory colors across the whole wheel. Busier views divide the wheel by visible branch
complexity, giving busy hierarchies more room for contrasting hues.
Files here and smaller groups use their parent's hue; root files have their own color.
Gray marks hidden parent groups and filesystem use outside the scan. At a filesystem mount
point, allocated, single-filesystem accounting adds free space and other filesystem use to
the outer parent ring, without child arcs beneath them. Subdirectories use the whole pie.
Chart and path-list percentages refer to filesystem capacity when shown, otherwise the whole
scan. Free space includes reserved blocks; available space excludes them.
Run dq on a listed path to dig deeper; the chart has no mouse interaction.
--all keeps the grouping depth; use --depth N for a different grouping.";

static HELP_TOPICS: &[Topic] = &[
    Topic { name: "views", aliases: &["files", "owners", "age"], text: HELP_VIEWS },
    Topic { name: "filters", aliases: &["exclude", "selection"], text: HELP_FILTERS },
    Topic { name: "scan", aliases: &["accounting", "traversal"], text: HELP_SCAN },
    Topic { name: "output", aliases: &["json", "jsonl", "presentation"], text: HELP_OUTPUT },
];
static HELP_CONTEXTS: &[(&[&str], &str)] = &[
    (&["--files", "--owners", "--age"], "views"),
    (&["--exclude", "--exclude-from", "--older-than", "--min-size"], "filters"),
    (&["--allocated", "--apparent", "--one-file-system", "--cross-filesystems", "--precise", "--max-error", "--threads", "-t", "--noprogress"], "scan"),
    (&["--top", "--depth", "--min-percent", "--all", "--nosummary", "-v", "--zeroes", "-V", "--errors", "--json", "--jsonl", "--no-color"], "output"),
];

pub fn main() {
    if crate::update::run_internal_check() { return; }
    let invocation: Vec<String> = std::env::args().collect();
    let machine_output = invocation.iter().any(|arg| arg == "--json" || arg == "--jsonl");
    let colors = !invocation.iter().any(|arg| arg == "--no-color");
    show_help_and_exit();
    let args = Args::parse();
    if let Some(shell) = args.generate_completion {
        cli::generate_completion(shell);
        return;
    }
    let plan = DiskQuery::from_args(&args).unwrap_or_else(|message| {
        eprintln!("dq: {message}"); std::process::exit(2)
    });
    if let Err(message) = application::execute(&plan) {
        eprintln!("dq: {message}"); std::process::exit(1);
    }
    crate::update::after_invocation(machine_output, colors);
}

fn show_help_and_exit() {
    let args: Vec<String> = std::env::args().collect();
    match help::route(&args, HELP, HELP_TOPICS, HELP_CONTEXTS) {
        Request::None => {}
        Request::Show(text) => { println!("{text}"); std::process::exit(0); }
        Request::Unknown(topic) => { eprintln!("dq: unknown help topic '{topic}'\navailable topics: {}", help::topic_names(HELP_TOPICS)); std::process::exit(2); }
    }
}

fn parse_bytes(raw: &str) -> Result<u64, String> {
    let value = raw.trim(); let split = value.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(value.len());
    let number: f64 = value[..split].parse().map_err(|_| format!("invalid size: {raw}"))?;
    let factor = match value[split..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0, "k" | "kb" => 1_000.0, "m" | "mb" => 1_000_000.0, "g" | "gb" => 1_000_000_000.0,
        "t" | "tb" => 1_000_000_000_000.0, "ki" | "kib" => 1024.0, "mi" | "mib" => 1024.0 * 1024.0,
        "gi" | "gib" => 1024.0 * 1024.0 * 1024.0, "ti" | "tib" => 1024.0_f64.powi(4), unit => return Err(format!("unknown size unit: {unit}")),
    };
    if !number.is_finite() || number < 0.0 || number * factor > u64::MAX as f64 { return Err(format!("invalid size: {raw}")); }
    Ok((number * factor).round() as u64)
}

fn parse_duration(raw: &str) -> Result<Duration, String> {
    let value = raw.trim(); let split = value.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(value.len());
    let number: f64 = value[..split].parse().map_err(|_| format!("invalid age: {raw}"))?;
    let seconds = match value[split..].trim().to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" => 1.0, "m" | "min" | "mins" => 60.0, "h" | "hr" | "hrs" => 3600.0,
        "d" | "day" | "days" => 86_400.0, "w" | "week" | "weeks" => 604_800.0,
        "y" | "year" | "years" => 31_557_600.0, _ => return Err(format!("age needs a unit such as h, d, w, or y: {raw}")),
    };
    if !number.is_finite() || number < 0.0 { return Err(format!("invalid age: {raw}")); }
    Ok(Duration::from_secs_f64(number * seconds))
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn parses_sizes_and_ages() { assert_eq!(parse_bytes("2GiB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172_800)); }

    #[test]
    fn query_validation_covers_percent_and_exclude_files() {
        let invalid = Args::try_parse_from(["dq", "--min-percent", "101", "."]).unwrap();
        assert!(DiskQuery::from_args(&invalid).unwrap_err().contains("between 0 and 100"));

        let path = std::env::temp_dir().join(format!("qtools-excludes-{}", std::process::id()));
        std::fs::write(&path, "# comment\nnode_modules\n\n*.cache\n").unwrap();
        let args = Args::try_parse_from(["dq", "--exclude-from", path.to_str().unwrap(), "."]).unwrap();
        let query = DiskQuery::from_args(&args).unwrap();
        assert_eq!(query.selection.excludes, vec!["node_modules", "*.cache"]);
        std::fs::remove_file(path).unwrap();

        let missing = Args::try_parse_from(["dq", "--exclude-from", "/definitely/missing/qtools", "."]).unwrap();
        assert!(DiskQuery::from_args(&missing).unwrap_err().contains("cannot read exclude file"));
    }
}
