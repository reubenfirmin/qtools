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
  dq --precise [DIR]         Scan every file (may take longer)
  dq --files [DIR]           Largest files
  dq --owners [DIR]          Usage by owner
  dq --age [DIR]             Usage by modification age

Focused help:
  dq help views              Files, owners, and age
  dq help filters            Exclusions, age, and size selection
  dq help scan               Scan speed, --precise, and disk accounting
  dq help output             Display limits, JSON, and errors";

const HELP_VIEWS: &str = "\
dq views

  dq DIR                     Parent directories with significant subdirectories
  dq --files DIR             Largest files recursively
  dq --owners DIR            Usage grouped by owner
  dq --age DIR               Bytes grouped into modification-age buckets

Primary views conflict. Combine a view with filters instead, for example:
  dq --files --older-than 90d --top 30 /var";

const HELP_FILTERS: &str = "\
dq filters

  --exclude PATTERN          Skip matching files/directories; repeatable; * and ? supported
  --exclude-from FILE        Load exclusion patterns from a text file
  --older-than AGE           Only files modified at least AGE ago
  --newer-than AGE           Only files modified within the last AGE
  --min-size SIZE            Only files at least this large: 10M, 2GiB

Patterns match a name or a path relative to DIR. An exclusion file contains one pattern
per line; blank lines and lines starting with # are ignored. For example, exclusions.txt:
  # Skip dependencies and cache files
  node_modules
  *.cache
  build/*

AGE accepts s, m, h, d, w, or y (for example, 12h, 30d, 6w, 1y).
Age filters use modification times. Combine --older-than and --newer-than for an age range.

Examples:
  dq --exclude node_modules --exclude '*.cache' .
  dq --exclude-from exclusions.txt .
  dq --files --older-than 90d --min-size 1G /var
  dq --files --newer-than 24h /var
  dq --files --older-than 7d --newer-than 30d /var";

const HELP_SCAN: &str = "\
dq scanning

  --allocated               Count allocated filesystem blocks (default)
  --apparent                Count logical file lengths
  --one-file-system         Stay on the starting filesystem (default)
  --cross-filesystems       Traverse mounted filesystems too
  --precise                 Scan every file (may take longer)
  --max-error PERCENT       Space left unaccounted for (% of disk capacity; default 3)
  -t, --threads N           Scan threads (default 32)
  --noprogress              Disable progress

Reading every file can be slow. For a quick overview, dq can stop with a small amount of
disk space still unaccounted for: at most 3% of the disk's total capacity by default.
Use --precise when you need complete totals. It checks every file and may take longer.

This shortcut applies to directory summaries of whole supported filesystems.
Subdirectories, filtered scans, --all, JSON, and other views already get a full scan.
Lower --max-error to leave less space unaccounted for; --max-error 0 scans everything.

Hard links are counted once by device/inode. Symlinks are not followed.

Examples:
  dq --precise /data
  dq --max-error 1 /data";

const HELP_OUTPUT: &str = "\
dq presentation and output

  --top N                   Limit parent groups, or rows in other views
  --depth N                 Directory grouping: 0 = total, 1 = tree, 2+ = flat list
  --min-percent PERCENT     Hide groups smaller than PERCENT of the total
                            Default: 1 for directories, 0 for other views
  --all                     Show all top-level groups (-v is an alias)
  -V, --zeroes              Include empty groups; use with --all
  --errors                  List unreadable paths
  --json                    Pretty-printed JSON report
  --jsonl                   The JSON report on a single line
  --no-color                Disable color and terminal graphics

By default, directories show up to 15 parent groups with significant children and
grandchildren beneath them. Other views show up to 15 rows. --all removes the count
and percentage limits on top-level groups; an explicit --top still applies.

Examples:
  dq --top 20 /data
  dq --all --depth 2 /data
  dq --json --all /data
  dq --no-color /data";

static HELP_TOPICS: &[Topic] = &[
    Topic { name: "views", aliases: &["files", "owners", "age"], text: HELP_VIEWS },
    Topic { name: "filters", aliases: &["exclude", "selection"], text: HELP_FILTERS },
    Topic { name: "scan", aliases: &["accounting", "traversal"], text: HELP_SCAN },
    Topic { name: "output", aliases: &["json", "jsonl", "presentation"], text: HELP_OUTPUT },
];
static HELP_CONTEXTS: &[(&[&str], &str)] = &[
    (&["--files", "--owners", "--age"], "views"),
    (&["--exclude", "--exclude-from", "--older-than", "--newer-than", "--min-size"], "filters"),
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
