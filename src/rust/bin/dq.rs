//! dq: composable disk accounting and filesystem views.
mod progress; mod dq_query; mod disk_scan; mod dq_report;

use std::fs;
use std::path::Path;
use std::time::Duration;

use clap::Parser;
use qtools::help::{self, Request, Topic};
use qtools::render::{self, RenderContext};

use dq_query::DiskQuery;

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

  dq DIR                     Directory groups (default depth 1)
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
  -t, --threads N           Directory worker threads (default 50)
  --noprogress              Disable progress

Hard links are counted once by device/inode. Symlinks are not followed.";

const HELP_OUTPUT: &str = "\
dq presentation and output

  --top N                   Rows to show (default 15)
  --depth N                 Directory grouping depth (summary view only)
  --min-percent PERCENT     Hide groups below this share (default 1)
  --all                     Show all groups (-v is a compatibility alias)
  -V, --zeroes              Include empty directory groups
  --errors                  List unreadable paths
  --json / --jsonl          Pretty or one-line machine-readable output
  --no-color                Disable color and terminal graphics";

static HELP_TOPICS: &[Topic] = &[
    Topic { name: "views", aliases: &["files", "owners", "age"], text: HELP_VIEWS },
    Topic { name: "filters", aliases: &["exclude", "selection"], text: HELP_FILTERS },
    Topic { name: "scan", aliases: &["accounting", "traversal"], text: HELP_SCAN },
    Topic { name: "output", aliases: &["json", "jsonl", "presentation"], text: HELP_OUTPUT },
];
static HELP_CONTEXTS: &[(&[&str], &str)] = &[
    (&["--files", "--owners", "--age"], "views"),
    (&["--exclude", "--exclude-from", "--older-than", "--min-size"], "filters"),
    (&["--allocated", "--apparent", "--one-file-system", "--cross-filesystems", "--threads", "-t", "--noprogress"], "scan"),
    (&["--top", "--depth", "--min-percent", "--all", "--nosummary", "-v", "--zeroes", "-V", "--errors", "--json", "--jsonl", "--no-color"], "output"),
];

#[derive(Parser, Debug)]
#[command(name = "dq", about = "dq: quantify what's eating your disk")]
struct Args {
    #[arg(long)] files: bool,
    #[arg(long)] owners: bool,
    #[arg(long)] age: bool,
    #[arg(long, conflicts_with = "apparent")] allocated: bool,
    #[arg(long, conflicts_with = "allocated")] apparent: bool,
    #[arg(long, value_name = "PATTERN", action = clap::ArgAction::Append)] exclude: Vec<String>,
    #[arg(long, value_name = "FILE")] exclude_from: Option<String>,
    #[arg(long, value_name = "AGE", value_parser = parse_duration)] older_than: Option<Duration>,
    #[arg(long, value_name = "SIZE", value_parser = parse_bytes)] min_size: Option<u64>,
    #[arg(long)] top: Option<usize>,
    #[arg(long)] depth: Option<usize>,
    #[arg(long)] min_percent: Option<f64>,
    #[arg(long)] all: bool,
    #[arg(short = 'v', long, hide = true)] nosummary: bool,
    #[arg(short = 'V', long)] zeroes: bool,
    #[arg(long)] errors: bool,
    #[arg(long, conflicts_with = "jsonl")] json: bool,
    #[arg(long, conflicts_with = "json")] jsonl: bool,
    #[arg(long)] no_color: bool,
    #[arg(long)] cross_filesystems: bool,
    #[arg(long, conflicts_with = "cross_filesystems")] one_file_system: bool,
    #[arg(short, long, default_value_t = 50)] threads: i32,
    #[arg(long = "noprogress")] noprogress: bool,
    #[arg(default_value = ".")] dir: String,
}

fn main() {
    show_help_and_exit();
    let args = Args::parse();
    let plan = DiskQuery::from_args(&args).unwrap_or_else(|message| { eprintln!("dq: {message}"); std::process::exit(2) });
    let metadata = fs::metadata(&plan.root).unwrap_or_else(|error| { eprintln!("dq: cannot access '{}': {error}", plan.root); std::process::exit(1) });
    if !metadata.is_dir() { eprintln!("dq: '{}' is not a directory", plan.root); std::process::exit(1); }
    let scan = disk_scan::scan(Path::new(&plan.root), plan.traversal.threads, plan.traversal.one_file_system,
        &plan.selection.excludes, plan.traversal.show_progress && !plan.output.machine());
    if !scan.errors.is_empty() && !plan.output.machine() {
        eprintln!("dq: warning: {} path(s) could not be read; totals may be incomplete", scan.errors.len());
        if plan.presentation.show_errors { for error in &scan.errors { eprintln!("  {error}"); } }
    }
    let report = dq_report::build(&scan, &plan);
    let context = RenderContext::detect(plan.output);
    render::stdout("dq", |out| report.render(out, context));
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
}
