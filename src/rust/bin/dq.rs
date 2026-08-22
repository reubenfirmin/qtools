mod view;
mod progress;
mod dq_query;
use qtools::term;
use qtools::help::{self, Request, Topic};

use std::error::Error;
use std::fs;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::os::unix::fs::MetadataExt;
use threadpool::ThreadPool;
use clap::Parser;
use crate::view::report;
use crate::view::FormatOptions;
use crate::progress::Progress;
use dq_query::DiskQuery;

const HELP: &str = "\
dq — query what is using your disk

Usage:
  dq [DIR]                   Scan a directory (default: current directory)

Common options:
  -v, --nosummary            Show every non-empty directory
  -V, --zeroes               Also show empty directories
  -t, --threads N            Worker threads (default 50)
      --json                 Machine-readable output

Focused help:
  dq help scan               Traversal, detail, and progress
  dq help output             Human and JSON output

Examples:
  dq
  dq /var
  dq -v --threads 100 /tmp
  dq --json /var > sizes.json";

const HELP_SCAN: &str = "\
dq scanning

Examples:
  dq .                       Scan the current directory
  dq /var                    Scan /var
  dq -v /var                 Show every non-empty directory
  dq -V /tmp                 Include zero-size directories
  dq --threads 100 /data     Increase traversal concurrency
  dq --noprogress /var       Suppress the progress indicator

Options:
  -t, --threads N            Worker threads (default 50)
  -v, --nosummary            Disable the normal 1% summary cutoff
  -V, --zeroes               Include empty directories
      --noprogress           Disable progress

dq stays on the starting filesystem and skips symlinks, /proc, and /sys.";

const HELP_OUTPUT: &str = "\
dq output

Examples:
  dq /var                    Rich terminal report, plain text when piped
  dq --json /var             Machine-readable JSON
  dq --json /var | jq .entries

Options:
      --json                 Emit JSON instead of the human report
  -v, --nosummary            Include every non-empty directory
  -V, --zeroes               Also include zero-size entries

Colors and graphics are automatically disabled when stdout is redirected.";

static HELP_TOPICS: &[Topic] = &[
    Topic { name: "scan", aliases: &["scanning", "walk"], text: HELP_SCAN },
    Topic { name: "output", aliases: &["json"], text: HELP_OUTPUT },
];

static HELP_CONTEXTS: &[(&[&str], &str)] = &[
    (&["--json"], "output"),
    (&["--threads", "-t", "--nosummary", "-v", "--zeroes", "-V", "--noprogress"], "scan"),
];

// A smarter, faster alternative to du.
#[derive(Parser, Debug)]
#[command(name = "dq", about = "dq: quantify what's eating your disk (a faster du).")]
struct Args {
    /// Emit machine-readable JSON instead of the human report
    #[arg(long)]
    json: bool,

    /// Number of worker threads used to walk the tree
    #[arg(short, long, default_value_t = 50)]
    threads: i32,

    /// Verbose: list every non-empty directory, not just those over 1%
    #[arg(short = 'v', long)]
    nosummary: bool,

    /// Extra verbose: also include zero-size directories
    #[arg(short = 'V', long)]
    zeroes: bool,

    /// Disable the progress indicator
    #[arg(long = "noprogress")]
    noprogress: bool,

    /// Directory to scan (defaults to the current directory)
    #[arg(default_value = ".")]
    dir: String,
}

fn main() {
    show_help_and_exit();
    let args = Args::parse();
    let plan = DiskQuery::from_args(&args);
    let dir = plan.root.clone();
    debug_assert!(plan.traversal.one_file_system && !plan.traversal.follow_links);
    debug_assert_eq!(plan.accounting, dq_query::SizeAccounting::Apparent);
    debug_assert_eq!(plan.view, dq_query::DiskView::Summary);

    let metadata = match fs::metadata(&dir) {
        Ok(metadata) => metadata,
        Err(e) => {
            eprintln!("dq: cannot access '{}': {}", dir, e);
            std::process::exit(1);
        }
    };
    if !metadata.is_dir() {
        eprintln!("dq: '{}' is not a directory", dir);
        std::process::exit(1);
    }

    let all_results = scan_path(&dir, plan.traversal.threads, metadata.dev(), HashSet::from([Path::new("/proc"), Path::new("/sys")]), plan.traversal.show_progress);

    if !all_results.contains_key(&dir) {
        eprintln!("dq: could not read '{}' (permission denied?)", dir);
        std::process::exit(1);
    }

    // A one-line usage hint on stderr, so it's visible on every human run without ever landing in
    // stdout (pipes, redirects, JSON stay clean). Dimmed when stderr is a terminal.
    if !plan.output.machine() {
        let usage = "usage: dq [OPTIONS] [DIR]   (--help for all options)";
        if std::io::stderr().is_terminal() {
            eprintln!("\x1B[2m{}\x1B[0m", usage);
        } else {
            eprintln!("{}", usage);
        }
    }

    // Colors and path truncation are only meaningful on an interactive terminal; when piped or
    // asked for JSON we emit plain, untruncated output that's easy to redirect or parse.
    let interactive = std::io::stdout().is_terminal() && !plan.output.machine();

    // Query the terminal for a graphics protocol (kitty / iTerm2 / sixel). Only when interactive,
    // since detection writes escape queries and reads the replies. Falls back to text when absent.
    let graphics = interactive && qtools::graphics::supported();
    if std::env::var_os("QTOOLS_DEBUG").is_some() {
        eprintln!("dq[debug]: interactive={} graphics={}", interactive, graphics);
    }

    // The files sitting directly in the scanned directory, so we can break down the "in this dir"
    // total into its biggest offenders when it's a meaningful chunk of the tree.
    let loose_files = list_direct_files(&dir);

    report(dir, all_results, loose_files, FormatOptions {
        json: plan.output.machine(),
        nosummary: plan.selection.include_all_nonempty,
        zeroes: plan.selection.include_zeroes,
        colors: interactive,
        width: if interactive { term::stdout_width() } else { None },
        graphics
    });
}

fn show_help_and_exit() {
    let args: Vec<String> = std::env::args().collect();
    match help::route(&args, HELP, HELP_TOPICS, HELP_CONTEXTS) {
        Request::None => {}
        Request::Show(text) => { println!("{}", text); std::process::exit(0); }
        Request::Unknown(topic) => {
            eprintln!("dq: unknown help topic '{topic}'\navailable topics: {}", help::topic_names(HELP_TOPICS));
            std::process::exit(2);
        }
    }
}

/**
 * Iterate a path and its subdirectories, collecting the size of each directory by summing files
 * within it.
 */
fn scan_path(dir: &str, threads: usize, device: u64, blacklist: HashSet<&Path>, show_progress: bool) -> HashMap<String, u64> {
    // set up a channel to receive results back from threads
    let (tx, rx) = std::sync::mpsc::channel();

    let pool = ThreadPool::new(threads);

    let mut results = HashMap::new();
    let mut progress = Progress::new(show_progress);

    let path = PathBuf::from(dir);

    // Run until we've received/handled the same number of results as we've submitted. Ideal world
    // we could get more state from the threadpool and not have to track this. We block on recv()
    // rather than spinning: every outstanding task sends exactly one result, so while pending > 0
    // there is always a result on the way.
    let mut pending = 1;
    submit(path, device, &pool, tx.clone());

    while pending > 0 {
        let result = rx.recv().expect("worker channel closed unexpectedly");
        pending -= 1;
        let mut scanned_bytes = 0;
        match result {
            Ok(it) => {
                scanned_bytes = it.size;
                let displayed = it.path.display().to_string();
                results.insert(displayed, it.size);
                for subpath in it.paths {
                    if !blacklist.contains(subpath.as_path()) {
                        pending += 1;
                        submit(subpath, device, &pool, tx.clone());
                    }
                }
            },
            Err(_) => {
                // we suppress errors because we don't care about folders we don't have
                // access to. TODO add flag to show these
            }
        }
        progress.update(pending, scanned_bytes);
    }
    progress.finish();
    results
}

/**
 * Submits a directory iteration to the worker pool.
 */
fn submit(path: PathBuf, device: u64, pool: &ThreadPool, tx: Sender<Result<DirMetadata, Box<dyn Error + Send + Sync>>>) {
    pool.execute (move || {
        let result = process_directory(&path, device);
        tx.send(result).expect("Couldn't send!");
    });
}

/**
 * Sum the sizes of files in this directory, and collect any direct subpaths.
 */
fn process_directory(dir_path: &Path, device: u64) -> Result<DirMetadata, Box<dyn Error + Send + Sync>> {
    let mut result = DirMetadata {
        path: dir_path.to_path_buf(),
        size: 0,
        paths: Vec::new()
    };

    let mut size = 0;

    for entry in fs::read_dir(dir_path)? {
        let entry = entry?;

        // file_type() reads the kernel's d_type from the directory listing itself (no extra
        // syscall on most Linux filesystems), letting us skip symlinks without stat'ing them.
        let is_symlink = match entry.file_type() {
            Ok(ft) => ft.is_symlink(),
            Err(_) => continue
        };
        if is_symlink {
            continue;
        }

        // entry.metadata() stats relative to the already-open directory (fstatat), which is
        // faster than symlink_metadata(entry.path()) re-resolving the whole path from scratch.
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue
        };

        if metadata.is_dir() && metadata.dev() == device {
            result.paths.push(entry.path());
        } else {
            size += metadata.len();
        }
    }

    result.size = size;
    Ok(result)
}

/**
 * The regular files sitting directly in `dir` (not in subdirectories), largest first. Used to
 * break down the "in this dir" total into its biggest files.
 */
fn list_direct_files(dir: &str) -> Vec<(String, u64)> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.path().symlink_metadata() {
                if metadata.is_file() {
                    files.push((entry.file_name().to_string_lossy().into_owned(), metadata.len()));
                }
            }
        }
    }
    files.sort_by_key(|b| std::cmp::Reverse(b.1));
    files
}

struct DirMetadata {
    path: PathBuf,
    paths: Vec<PathBuf>,
    size: u64
}
