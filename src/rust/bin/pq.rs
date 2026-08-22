//! pq: query and cluster the processes eating your CPU/memory. Companion to dq, sharing the same
//! rendering primitives (style/donut/term/graphics/chart) from the qtools library.
mod proc; mod identity; mod cluster; mod report; mod kill; mod net; mod net_report; mod netdiag; mod pq_query; mod io_report;
mod fd_report; mod blocked_report; mod pss_report; mod unix_report;
mod ranked_report; mod tree_report; mod oom_report; mod limits_report;

use std::collections::HashMap;
use std::io::{self, Write};
use std::time::Duration;

use clap::{Parser, ValueEnum};
use qtools::style;
use qtools::help::{self, Request, Topic};
use qtools::render::{self, RenderContext};

use pq_query::{Action, KillOptions, ProcessSelection, QueryPlan, View};

const EXAMPLES: &str = "\
Examples:
  pq                        what is eating cpu/memory right now
  pq -m -v chrome           memory view, clusters expanded, only chrome
  pq --net                  who is serving / talking on the network
  pq --net -v --listen      every listening socket, one row each
  pq --port 8080            who owns port 8080
  pq --port 8080 --kill     kill whatever is LISTENING on port 8080
  pq --kill gradle          kill the gradle tree (preview + confirm)";

const HELP: &str = "\
pq — query what is using your machine

Usage:
  pq [PATTERN]               CPU/memory report
  pq --net [PATTERN]         Network report
  pq --kill PATTERN          Stop matching process trees

Common options:
  -m, --memory               Sort by memory
  -s, --swap                 Sort by swap
  -v, --verbose              Show underlying processes/connections
  -n, --top N                Number of clusters to show
      --json                 Pretty machine-readable output

Focused help:
  pq help process | io       Resource reports
  pq help files | blocked | pss
                             Process internals
  pq help net | filters      Connections and selection
  pq help kill | output      Actions and machine output

Tip: pq --net --help and pq --kill --help open the matching topic.";

const HELP_PROCESS: &str = "\
pq process reports

Examples:
  pq                         CPU report
  pq --memory                Sort by resident memory
  pq --swap                  Sort by swap
  pq --tree                  Static parent/child table
  pq -v chrome               Expand matching clusters into processes
  pq -n 30 --interval 1000   Show 30 clusters, sampled for one second

Options:
  -c, --cpu                  Sort/chart by CPU (default)
  -m, --memory               Sort/chart by memory
  -s, --swap                 Sort/chart by swap
      --tree                 Show processes in a parent/child table
  -v, --verbose              Expand clusters to member processes
  -n, --top N                Clusters to show (default 15)
      --interval MS          CPU sampling interval (default 400)

See also: pq help filters, pq help output";

const HELP_FILTERS: &str = "\
pq filters

Filters compose: every supplied filter must match.

Examples:
  pq chrome                  Identity/command contains “chrome”
  pq --user postgres
  pq --pid 1234
  pq --min-cpu 10
  pq --min-memory 500M
  pq --user rafael --min-memory 2GiB

Options:
      --user USER            Username or numeric UID
      --pid PID              Exact process ID
      --min-cpu PERCENT      Minimum cluster CPU percentage
      --min-memory SIZE      Minimum cluster memory; accepts M, GiB, etc.

The same filters work in network mode.";

const HELP_IO: &str = "\
pq process I/O

Examples:
  pq --io                    Sort by combined read/write rate
  pq --read                  Sort by read rate
  pq --write                 Sort by write rate
  pq --io postgres           I/O for matching processes
  pq --user postgres --io    I/O owned by postgres
  pq --port 5432 --io        I/O for processes owning local port 5432
  pq --remote-port 443 --io  I/O for processes connected to peer port 443

Options:
      --io                   Combined process I/O view
      --read                 Process I/O view sorted by reads
      --write                Process I/O view sorted by writes
      --interval MS          Sampling interval (default 400)
  -v, --verbose              Expand clusters to processes

Values come from /proc/PID/io. They describe process I/O requests; caching means they are not
necessarily physical device throughput. All ordinary process and socket filters compose.";

const HELP_FILES: &str = "\
pq files and descriptors

Examples:
  pq --deleted               Deleted files still consuming space
  pq --fds                   Descriptor counts by process
  pq --mount /mnt/data       Why a filesystem is busy
  pq --file /var/log/app.log Who holds a path
  pq --mount /mnt/data --fds FD usage among processes holding that mount

Options:
      --deleted              Rank identities by deleted open-file bytes
      --fds                  Rank identities by open descriptor count
      --mount PATH           Select paths beneath a mount/directory
      --file PATH            Select one exact path
  -v, --verbose              Show descriptors or individual hold reasons

Path selectors examine open descriptors, cwd, root, executable, and mapped files.";

const HELP_BLOCKED: &str = "\
pq blocked processes

Examples:
  pq --blocked
  pq --blocked -v
  pq --user postgres --blocked

Shows tasks in uninterruptible D state and their kernel wait channel from /proc/PID/wchan.";

const HELP_PSS: &str = "\
pq proportional memory

Examples:
  pq --pss chrome
  pq --pid 1234 --pss
  pq --port 5432 --pss

Reads /proc/PID/smaps_rollup and reports PSS plus private/shared/anonymous memory. This is explicit
because it is more expensive and permission-sensitive than the default RSS report.";

const HELP_OOM: &str = "\
pq out-of-memory kill preference

Examples:
  pq --oom                   Rank processes by current kernel OOM score
  pq postgres --oom          Inspect matching processes
  pq --pid 1820 --oom -v     Include adjustment details

The score is the kernel's current relative victim preference, not a probability or guarantee.
Lower scores are less preferred; an adjustment of -1000 protects a process.";

const HELP_LIMITS: &str = "\
pq process resource limits

Examples:
  pq --limits                Show processes at least 75% into a measurable finite limit
  pq postgres --limits       Show all measurable finite limits for matching processes
  pq --pid 1820 --limits -v  Include soft/hard limit details

Measured resources include open files, address space, locked memory, and the real user's
process/thread allowance. Unlimited or non-measurable limits are not assigned fake utilization.";

const HELP_UNIX: &str = "\
pq Unix-domain sockets

Examples:
  pq --unix
  pq --unix /run/docker.sock
  pq --net --unix
  pq --user root --unix

Joins /proc/net/unix socket inodes to owning processes. An optional value filters socket paths.";

const HELP_NET: &str = "\
pq network queries

Examples:
  pq --net                   Network activity by application
  pq --net -v                Every matching connection
  pq --listen                Every listener
  pq --port 8080             Inspect a local port
  pq --remote-port 443       Connections to peer port 443
  pq --net --tcp --state established
  pq --public                Listeners bound to all interfaces

Filters:
      --port PORT            Local port
      --remote-port PORT     Peer port
      --listen               Listening sockets only
      --tcp / --udp          Protocol
      --state STATE          listen, established, bound, or other
      --public               Bound to 0.0.0.0 or :: (firewall not checked)

Presentation:
  -v, --verbose              Per-connection rows
      --services             Annotate ports from /etc/services
      --resolve              Reverse-resolve peers in connection rows; may be slow

Targeting and process filters can be combined. See: pq help filters";

const HELP_KILL: &str = "\
pq safe process termination

Examples:
  pq --kill gradle           Preview, confirm, TERM, then KILL survivors
  pq --kill chrome --dry-run Preview only
  pq --port 8080 --kill      Stop the process listening on local port 8080
  pq --net --udp --kill dns  Stop matching UDP socket owners

Options:
      --kill [PATTERN]       Select matching process trees/socket owners
      --dry-run              Preview without signaling
  -y, --yes                  Skip confirmation
  -x, --exact                Exact resolved-identity match
      --signal SIGNAL        Initial signal (default TERM)
      --grace SECONDS        Wait before SIGKILL (default 4)
  -9, --sigkill              SIGKILL immediately

pq excludes itself, PID 1, and the invoking shell/ancestor chain.";

const HELP_OUTPUT: &str = "\
pq output

Examples:
  pq --json                  Pretty JSON snapshot
  pq --jsonl                 One compact JSON object and newline
  pq --net --json | jq .clusters
  pq --no-color | less

Options:
      --json                 Pretty machine-readable JSON
      --jsonl                Compact one-object-per-line JSON
      --no-color             Disable ANSI color and terminal graphics
  -n, --top N                Also limits clusters in machine output

JSON includes generated_at_unix_ms and sample_interval_ms.";

static HELP_TOPICS: &[Topic] = &[
    Topic { name: "process", aliases: &["proc", "metrics"], text: HELP_PROCESS },
    Topic { name: "io", aliases: &["read", "write"], text: HELP_IO },
    Topic { name: "files", aliases: &["fds", "deleted", "mount", "file"], text: HELP_FILES },
    Topic { name: "blocked", aliases: &["wait", "stuck"], text: HELP_BLOCKED },
    Topic { name: "pss", aliases: &["smaps"], text: HELP_PSS },
    Topic { name: "oom", aliases: &["oom-score"], text: HELP_OOM },
    Topic { name: "limits", aliases: &["ulimit", "rlimit"], text: HELP_LIMITS },
    Topic { name: "unix", aliases: &["unix-sockets"], text: HELP_UNIX },
    Topic { name: "net", aliases: &["network", "ports"], text: HELP_NET },
    Topic { name: "filters", aliases: &["filter"], text: HELP_FILTERS },
    Topic { name: "kill", aliases: &["signal"], text: HELP_KILL },
    Topic { name: "output", aliases: &["json", "jsonl"], text: HELP_OUTPUT },
];

static HELP_CONTEXTS: &[(&[&str], &str)] = &[
    (&["--kill", "--dry-run", "--signal", "--grace", "--sigkill", "-9"], "kill"),
    (&["--io", "--read", "--write"], "io"),
    (&["--deleted", "--fds", "--mount", "--file"], "files"),
    (&["--blocked"], "blocked"),
    (&["--pss"], "pss"),
    (&["--oom"], "oom"),
    (&["--limits"], "limits"),
    (&["--unix"], "unix"),
    (&["--net", "--port", "--listen", "--remote-port", "--tcp", "--udp", "--state", "--public", "--services", "--resolve"], "net"),
    (&["--user", "--pid", "--min-cpu", "--min-memory"], "filters"),
    (&["--json", "--jsonl", "--no-color"], "output"),
    (&["--cpu", "-c", "--memory", "-m", "--swap", "-s", "--tree", "--verbose", "-v", "--top", "-n", "--interval"], "process"),
];

#[derive(Parser, Debug)]
#[command(name = "pq", about = "pq: query and cluster the processes eating your CPU/memory or talking on the network.", after_help = EXAMPLES)]
struct Args {
    /// Clusters to show (default 15; --listen defaults to all, so no listener is ever cut off)
    #[arg(short = 'n', long)] top: Option<usize>,
    /// Expand clusters to their member processes (per-connection rows in --net mode)
    #[arg(short, long)] verbose: bool,
    /// Show matching processes in a static parent/child table
    #[arg(long, help_heading = "View")] tree: bool,
    /// CPU sample interval in milliseconds
    #[arg(long, default_value_t = 400)] interval: u64,
    /// Emit machine-readable JSON
    #[arg(long)] json: bool,
    /// Emit one compact JSON object per line (useful for append-only logs)
    #[arg(long, conflicts_with = "json")] jsonl: bool,
    /// Disable ANSI color and terminal graphics
    #[arg(long)] no_color: bool,
    /// Only processes owned by this username or numeric uid
    #[arg(long, help_heading = "Filter")] user: Option<String>,
    /// Only this process (network mode: sockets owned by this pid)
    #[arg(long, help_heading = "Filter")] pid: Option<i32>,
    /// Only clusters using at least this percentage of total CPU
    #[arg(long, value_name = "PERCENT", help_heading = "Filter")] min_cpu: Option<f64>,
    /// Only clusters using at least this much memory (for example 500M or 2GiB)
    #[arg(long, value_parser = parse_bytes, value_name = "SIZE", help_heading = "Filter")] min_memory: Option<u64>,
    /// Sort and chart by memory instead of CPU
    #[arg(short = 'm', long, help_heading = "Metric (default report)")] memory: bool,
    /// Sort and chart by swap usage instead of CPU
    #[arg(short = 's', long, help_heading = "Metric (default report)")] swap: bool,
    /// Sort and chart by CPU (default)
    #[arg(short = 'c', long, help_heading = "Metric (default report)")] cpu: bool,
    /// Report process read/write rates, sorted by combined I/O
    #[arg(long, conflicts_with_all = ["read", "write"], help_heading = "Process I/O")] io: bool,
    /// Report process I/O sorted by read rate
    #[arg(long, conflicts_with_all = ["io", "write"], help_heading = "Process I/O")] read: bool,
    /// Report process I/O sorted by write rate
    #[arg(long, conflicts_with_all = ["io", "read"], help_heading = "Process I/O")] write: bool,
    /// Report deleted files still held open
    #[arg(long, help_heading = "Files")] deleted: bool,
    /// Report open file descriptor counts and classifications
    #[arg(long, help_heading = "Files")] fds: bool,
    /// Select processes holding paths beneath this mount/directory
    #[arg(long, value_name = "PATH", help_heading = "Files")] mount: Option<String>,
    /// Select processes holding this exact path
    #[arg(long, value_name = "PATH", conflicts_with = "mount", help_heading = "Files")] file: Option<String>,
    /// Report processes in uninterruptible D state
    #[arg(long, help_heading = "Process internals")] blocked: bool,
    /// Report proportional memory from smaps_rollup
    #[arg(long, help_heading = "Process internals")] pss: bool,
    /// Rank processes by the kernel's current out-of-memory kill preference
    #[arg(long, help_heading = "Process internals")] oom: bool,
    /// Report measurable finite process resource limits
    #[arg(long, help_heading = "Process internals")] limits: bool,
    /// Report Unix-domain sockets, optionally matching a socket path
    #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "PATH", help_heading = "Net mode")] unix: Option<String>,
    /// Report TCP/UDP connections instead of cpu/mem clusters (sorted by traffic, then
    /// connection count; metric flags do not change the sort). World-exposed listeners are
    /// highlighted
    #[arg(long, help_heading = "Net mode")] net: bool,
    /// Only sockets with this local port (implies --net)
    #[arg(long, help_heading = "Net mode")] port: Option<u16>,
    /// Only listening sockets (implies --net)
    #[arg(long, help_heading = "Net mode")] listen: bool,
    /// Only sockets connected to this peer port (implies --net)
    #[arg(long, help_heading = "Net mode")] remote_port: Option<u16>,
    /// Only TCP sockets (implies --net)
    #[arg(long, conflicts_with = "udp", help_heading = "Net mode")] tcp: bool,
    /// Only UDP sockets (implies --net)
    #[arg(long, conflicts_with = "tcp", help_heading = "Net mode")] udp: bool,
    /// Only sockets in this state (implies --net)
    #[arg(long, value_enum, help_heading = "Net mode")] state: Option<NetState>,
    /// Only listeners bound to all interfaces (implies --net; firewall not checked)
    #[arg(long = "public", help_heading = "Net mode")] public_only: bool,
    /// Show conventional names from /etc/services beside port numbers
    #[arg(long, help_heading = "Net mode")] services: bool,
    /// Reverse-resolve peer IPs in per-connection rows (may be slow)
    #[arg(long, help_heading = "Net mode")] resolve: bool,
    /// Kill matching process trees (preview + confirm); pattern optional with --port/--listen
    #[arg(long, num_args = 0..=1, default_missing_value = "", help_heading = "Kill")] kill: Option<String>,
    /// Preview only, do not signal
    #[arg(long, help_heading = "Kill")] dry_run: bool,
    /// Skip the confirmation prompt
    #[arg(short = 'y', long, help_heading = "Kill")] yes: bool,
    /// Match the identity exactly instead of substring
    #[arg(short = 'x', long, help_heading = "Kill")] exact: bool,
    /// Initial signal
    #[arg(long, default_value = "TERM", help_heading = "Kill")] signal: String,
    /// Seconds to wait before SIGKILL
    #[arg(long, default_value_t = 4, help_heading = "Kill")] grace: u64,
    /// SIGKILL immediately (no TERM, no grace)
    #[arg(short = '9', long = "sigkill", help_heading = "Kill")] sigkill: bool,
    /// Filter the report to matching clusters
    pattern: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum NetState { Listen, Established, Bound, Other }

fn main() {
    show_help_and_exit();
    let args = Args::parse();
    let plan = QueryPlan::from_args(&args).unwrap_or_else(|message| {
        eprintln!("pq: {message}");
        std::process::exit(1);
    });
    match (&plan.action, plan.view) {
        (Action::Kill(options), _) if plan.sockets.requested() => { run_net_kill(&plan, options); return; }
        (Action::Kill(options), _) => { run_kill(options.pattern.as_deref().unwrap(), &plan, options); return; }
        (Action::Report, View::Network) => { run_net_report(&plan); return; }
        (Action::Report, View::Io) => { run_io_report(&plan); return; }
        (Action::Report, View::Deleted | View::Fds | View::Holders | View::Blocked | View::Pss | View::Oom | View::Limits | View::Unix) => {
            run_diagnostic_report(&plan); return;
        }
        (Action::Report, View::Processes) => {}
    }

    let metric = plan.metric;

    // Always take the two-sample CPU delta (even in --memory mode) so both the cpu and memory
    // columns are real numbers, not a fake 0. The interval is small; --kill exits before here.
    // The per-core /proc/stat samples ride along at the same two points, for --cpu's per-core rings.
    let prev = proc::snapshot();
    let stat0 = proc::cpu_stat();
    std::thread::sleep(std::time::Duration::from_millis(plan.interval_ms));
    let cur = proc::snapshot();
    let stat1 = proc::cpu_stat();
    let procs = proc::cpu_percent(&prev, &cur, plan.interval_ms as f64 / 1000.0);
    let core_loads = proc::per_core_busy(&stat0, &stat1);
    // Which process(es) actually ran on each core, for the --cpu donut's process-colored rings.
    let attribution = proc::per_core_attribution(&procs);

    let all_clusters = cluster::cluster(&procs, metric);
    // System-wide, computed before any pattern filter: the header's cpu% is a system total, matching
    // the mem/swap totals beside it (which are always system-wide), so a filtered report never shows
    // a small filtered cpu% next to unfiltered mem/swap.
    let cpu_total: f64 = all_clusters.iter().map(|c| c.cpu).sum();
    let selected_procs: Vec<proc::Proc> = procs.iter().filter(|p| process_matches(p, &plan.process)).cloned().collect();
    let mut clusters = cluster::cluster(&selected_procs, metric);

    if let Some(pattern) = &plan.process.pattern {
        let needle = pattern.to_lowercase();
        clusters.retain(|c| {
            c.identity.to_lowercase().contains(&needle)
                || c.members.iter().any(|m| {
                    m.cmd.to_lowercase().contains(&needle) || m.comm.to_lowercase().contains(&needle)
                })
        });
    }
    clusters.retain(|c| {
        plan.process.min_cpu.is_none_or(|min| c.cpu >= min)
            && plan.process.min_memory.is_none_or(|min| c.rss >= min)
    });

    let (mem_used, mem_total) = proc::mem_info();
    let (swap_used, swap_total) = proc::swap_info();

    // Colors/width are only meaningful on an interactive terminal; JSON and piped output stay plain.
    let context = RenderContext::detect(plan.output);
    let opts = report::ReportOpts {
        metric,
        top: plan.output.top.unwrap_or(15),
        verbose: plan.output.verbose,
        colors: context.colors,
        width: context.width,
        compact_json: context.compact_json(),
        sample_interval_ms: plan.interval_ms
    };
    if plan.tree {
        render::stdout("pq", |out| tree_report::render(out, &procs, &clusters, &opts, context));
    } else {
        render::stdout("pq", |out| report::render(out, &clusters, mem_used, mem_total, swap_used,
            swap_total, cpu_total, &core_loads, &attribution, &opts, context));
    }
}

fn show_help_and_exit() {
    let args: Vec<String> = std::env::args().collect();
    match help::route(&args, HELP, HELP_TOPICS, HELP_CONTEXTS) {
        Request::None => {}
        Request::Show(text) => { println!("{}", text); std::process::exit(0); }
        Request::Unknown(topic) => {
            eprintln!("pq: unknown help topic '{topic}'\navailable topics: {}", help::topic_names(HELP_TOPICS));
            std::process::exit(2);
        }
    }
}

/// --port and --listen imply --net, so `pq --port 8080` works without the extra flag.
fn parse_bytes(raw: &str) -> Result<u64, String> {
    let s = raw.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let number: f64 = s[..split].parse().map_err(|_| format!("invalid size: {raw}"))?;
    let unit = s[split..].trim().to_ascii_lowercase();
    let multiplier = match unit.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1_000.0, "ki" | "kib" => 1_024.0,
        "m" | "mb" => 1_000_000.0, "mi" | "mib" => 1_048_576.0,
        "g" | "gb" => 1_000_000_000.0, "gi" | "gib" => 1_073_741_824.0,
        "t" | "tb" => 1_000_000_000_000.0, "ti" | "tib" => 1_099_511_627_776.0,
        _ => return Err(format!("unknown size unit in {raw}"))
    };
    let bytes = number * multiplier;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        Err(format!("size out of range: {raw}"))
    } else {
        Ok(bytes.round() as u64)
    }
}

fn user_matches(uid: u32, wanted: &str) -> bool {
    wanted.parse::<u32>().map_or_else(|_| kill::username(uid) == wanted, |n| uid == n)
}

fn process_matches(p: &proc::Proc, selection: &ProcessSelection) -> bool {
    selection.pid.is_none_or(|pid| p.pid == pid)
        && selection.user.as_deref().is_none_or(|user| user_matches(p.uid, user))
}

/// The --kill pattern, if a non-empty one was given (bare `--kill` parses as Some("")).
/// One socket sample: read the tables, attribute inodes to the sampled pids. Returns the
/// attributed sockets and how many /proc/net tables were readable (0 = not Linux / no procfs).
fn sample_sockets(procs: &[proc::Proc]) -> (Vec<(net::Sock, Option<i32>)>, usize) {
    let (socks, tables_read) = net::read_sockets();
    let pids: Vec<i32> = procs.iter().map(|p| p.pid).collect();
    let inodes = net::inode_map(&pids);
    (net::attribute(socks, &inodes), tables_read)
}

/// Apply the complete socket/process selector intersection. Reports and actions share this path;
/// kill-by-port adds its stricter listener/attribution safety check after selection.
fn select_sockets(attributed: Vec<(net::Sock, Option<i32>)>, plan: &QueryPlan) -> Vec<(net::Sock, Option<i32>)> {
    let filtered = attributed.into_iter().filter(|(sock, pid)| {
        plan.process.pid.is_none_or(|wanted| *pid == Some(wanted))
            && plan.process.user.as_deref().is_none_or(|wanted| user_matches(sock.uid, wanted))
    }).collect();
    net::filter_socks_extended(filtered, plan.sockets.remote_port, plan.sockets.protocol,
        plan.sockets.state, plan.sockets.public_only)
}

/// `--net` report: sample processes (same two-sample cpu delta as the normal report, so the cpu
/// column is real), read and attribute sockets, filter, cluster, render.
fn run_net_report(plan: &QueryPlan) {
    let prev = proc::snapshot();
    std::thread::sleep(Duration::from_millis(plan.interval_ms));
    let procs = proc::cpu_percent(&prev, &proc::snapshot(), plan.interval_ms as f64 / 1000.0);

    let (attributed, tables_read) = sample_sockets(&procs);
    if tables_read == 0 {
        eprintln!("pq: cannot read /proc/net (is this Linux?)");
        std::process::exit(1);
    }
    let filtered = net::filter_socks(select_sockets(attributed, plan), plan.sockets.local_port, plan.sockets.listen);
    // Per-socket byte counters from sock_diag (what ss -i reads); failure degrades to blanks.
    let traffic = netdiag::tcp_traffic();
    let mut report = net::net_clusters(&procs, filtered, plan.process.pattern.as_deref(), traffic);
    report.clusters.retain(|c| plan.process.min_cpu.is_none_or(|min| c.cpu >= min)
        && plan.process.min_memory.is_none_or(|min| c.rss >= min));

    let (mem_used, mem_total) = proc::mem_info();
    let context = RenderContext::detect(plan.output);
    let opts = net_report::NetOpts {
        // --listen means "show me what's serving": never cut a listener off by default.
        top: plan.output.top.unwrap_or(if plan.sockets.listen { usize::MAX } else { 15 }),
        verbose: plan.output.verbose,
        colors: context.colors,
        width: context.width,
        port: plan.sockets.local_port,
        compact_json: context.compact_json(),
        sample_interval_ms: plan.interval_ms,
        names: net_report::display_names(&report, plan.sockets.services, plan.sockets.resolve)
    };

    let detail = plan.sockets.local_port.is_some() || (plan.process.pattern.is_some() && !plan.output.verbose);
    render::stdout("pq", |out| net_report::render(out, &report, &procs, mem_used, mem_total,
        &opts, context, detail));
}

/// Process-I/O report. Process selectors establish the base set; when socket selectors are
/// present their attributed owner PIDs are intersected with it instead of forcing a net view.
fn run_io_report(plan: &QueryPlan) {
    use std::collections::HashSet;

    let before_procs = proc::snapshot();
    let before_io = io_report::snapshot(before_procs.iter().map(|p| p.pid));
    std::thread::sleep(Duration::from_millis(plan.interval_ms));
    let current = proc::snapshot();
    let after_io = io_report::snapshot(current.iter().map(|p| p.pid));
    let procs = proc::cpu_percent(&before_procs, &current, plan.interval_ms as f64 / 1000.0);

    let selected: Vec<proc::Proc> = procs.iter().filter(|p| process_matches(p, &plan.process)).cloned().collect();
    let mut base = cluster::cluster(&selected, plan.metric);
    if let Some(pattern) = &plan.process.pattern {
        let needle = pattern.to_lowercase();
        base.retain(|c| c.identity.to_lowercase().contains(&needle)
            || c.members.iter().any(|m| m.cmd.to_lowercase().contains(&needle) || m.comm.to_lowercase().contains(&needle)));
    }
    base.retain(|c| plan.process.min_cpu.is_none_or(|min| c.cpu >= min)
        && plan.process.min_memory.is_none_or(|min| c.rss >= min));
    let mut allowed: HashSet<i32> = base.iter().flat_map(|c| c.members.iter().map(|m| m.pid)).collect();

    if plan.sockets.selects_owners() {
        let (attributed, tables_read) = sample_sockets(&procs);
        if tables_read == 0 {
            eprintln!("pq: cannot read /proc/net for socket selection (is this Linux?)");
            std::process::exit(1);
        }
        let sockets = net::filter_socks(select_sockets(attributed, plan), plan.sockets.local_port, plan.sockets.listen);
        let owners: HashSet<i32> = sockets.into_iter().filter_map(|(_, pid)| pid).collect();
        allowed.retain(|pid| owners.contains(pid));
    }

    let mut clusters = io_report::aggregate(&procs, &allowed, &before_io, &after_io,
        plan.interval_ms as f64 / 1000.0, plan.io_metric);
    let targeted = plan.process.pattern.is_some() || plan.process.user.is_some() || plan.process.pid.is_some()
        || plan.sockets.selects_owners();
    if !targeted {
        clusters.retain(|c| c.read_bps + c.write_bps > 0);
    }
    let context = RenderContext::detect(plan.output);
    let opts = io_report::IoOpts {
        metric: plan.io_metric, top: plan.output.top.unwrap_or(15), verbose: plan.output.verbose,
        compact_json: context.compact_json(), sample_interval_ms: plan.interval_ms,
        colors: context.colors,
    };
    render::stdout("pq", |out| io_report::render(out, &clusters, &opts, context));
}

fn selected_process_pids(procs: &[proc::Proc], plan: &QueryPlan) -> std::collections::HashSet<i32> {
    let selected: Vec<proc::Proc> = procs.iter().filter(|p| process_matches(p, &plan.process)).cloned().collect();
    let mut groups = cluster::cluster(&selected, plan.metric);
    if let Some(pattern) = &plan.process.pattern {
        let needle = pattern.to_lowercase();
        groups.retain(|c| c.identity.to_lowercase().contains(&needle)
            || c.members.iter().any(|m| m.cmd.to_lowercase().contains(&needle) || m.comm.to_lowercase().contains(&needle)));
    }
    groups.retain(|c| plan.process.min_cpu.is_none_or(|min| c.cpu >= min)
        && plan.process.min_memory.is_none_or(|min| c.rss >= min));
    groups.iter().flat_map(|c| c.members.iter().map(|m| m.pid)).collect()
}

fn intersect_socket_owners(procs: &[proc::Proc], plan: &QueryPlan, allowed: &mut std::collections::HashSet<i32>) {
    if !plan.sockets.selects_owners() { return; }
    let (attributed, tables_read) = sample_sockets(procs);
    if tables_read == 0 {
        eprintln!("pq: cannot read /proc/net for socket selection (is this Linux?)");
        std::process::exit(1);
    }
    let sockets = net::filter_socks(select_sockets(attributed, plan), plan.sockets.local_port, plan.sockets.listen);
    let owners: std::collections::HashSet<i32> = sockets.into_iter().filter_map(|(_, pid)| pid).collect();
    allowed.retain(|pid| owners.contains(pid));
}

fn run_diagnostic_report(plan: &QueryPlan) {
    let previous = proc::snapshot();
    std::thread::sleep(Duration::from_millis(plan.interval_ms));
    let current = proc::snapshot();
    let procs = proc::cpu_percent(&previous, &current, plan.interval_ms as f64 / 1000.0);
    let mut allowed = selected_process_pids(&procs, plan);
    intersect_socket_owners(&procs, plan, &mut allowed);

    let needs_files = matches!(plan.view, View::Deleted | View::Fds | View::Holders)
        || plan.process.file.is_some() || plan.process.mount.is_some();
    let include_special_paths = plan.process.file.is_some() || plan.process.mount.is_some() || plan.view == View::Holders;
    let files = needs_files.then(|| fd_report::collect(&procs, include_special_paths));
    let normalized_file = plan.process.file.as_deref().map(fd_report::normalize_query_path);
    let normalized_mount = plan.process.mount.as_deref().map(fd_report::normalize_query_path);
    if let Some(files) = &files {
        if normalized_file.is_some() || normalized_mount.is_some() {
            let holders = fd_report::matching_pids(files, normalized_file.as_deref(), normalized_mount.as_deref());
            allowed.retain(|pid| holders.contains(pid));
        }
    }

    let report = match plan.view {
        View::Deleted => fd_report::deleted_report(files.as_ref().unwrap(), &allowed),
        View::Fds => fd_report::fds_report(files.as_ref().unwrap(), &allowed),
        View::Holders => fd_report::holders_report(files.as_ref().unwrap(), &allowed,
            normalized_file.as_deref(), normalized_mount.as_deref()),
        View::Blocked => blocked_report::build(&procs, &allowed),
        View::Pss => pss_report::build(&procs, &allowed),
        View::Oom => oom_report::build(&procs, &allowed),
        View::Limits => limits_report::build(&procs, &allowed,
            plan.output.verbose || plan.process.pid.is_some() || plan.process.pattern.is_some()),
        View::Unix => unix_report::build(&procs, &allowed, plan.sockets.unix_path.as_deref()),
        _ => unreachable!("non-diagnostic view routed to diagnostic renderer"),
    };
    let context = RenderContext::detect(plan.output);
    let show_details = plan.output.verbose || matches!(plan.view, View::Holders | View::Blocked)
        || (plan.view == View::Limits && (plan.process.pid.is_some() || plan.process.pattern.is_some()))
        || (plan.view == View::Unix && plan.sockets.unix_path.is_some());
    let options = ranked_report::Options { top: plan.output.top.unwrap_or(15), verbose: show_details,
        sample_interval_ms: Some(plan.interval_ms) };
    render::stdout("pq", |out| report.render(out, &options, context));
}

/// Net-mode `--kill`: with --port, only an attributed LISTENING socket on that port selects (the
/// spec's "kill what is listening" rule, with diagnostics for the no-listener and cannot-attribute
/// cases); otherwise the selection is every attributed socket passing --listen, narrowed by the
/// pattern if one was given.
fn run_net_kill(plan: &QueryPlan, options: &KillOptions) {
    let self_pid = std::process::id() as i32;
    let prev = proc::snapshot();
    std::thread::sleep(Duration::from_millis(plan.interval_ms));
    let procs = proc::cpu_percent(&prev, &proc::snapshot(), plan.interval_ms as f64 / 1000.0);

    let (attributed, tables_read) = sample_sockets(&procs);
    if tables_read == 0 {
        eprintln!("pq: cannot read /proc/net (is this Linux?)");
        std::process::exit(1);
    }

    let attributed = select_sockets(attributed, plan);

    let owners: Vec<(net::Sock, i32)> = match plan.sockets.local_port {
        Some(port) => match net::port_kill_candidates(&attributed, port) {
            net::PortCandidates::Listeners(owners) => owners,
            net::PortCandidates::Unattributable { uid } => {
                eprintln!(
                    "pq: port {} is held by uid {} ({}) but the process cannot be identified; re-run with sudo",
                    port, uid, kill::username(uid)
                );
                std::process::exit(1);
            }
            net::PortCandidates::NoListener(others) => {
                if others.is_empty() {
                    println!("pq: no sockets with local port {}", port);
                } else {
                    println!("pq: no listener on port {}; these sockets merely have it as their local port:", port);
                    for s in &others {
                        println!("  {}", s.label());
                    }
                    println!("pq: not signaling (--port --kill only targets listeners)");
                }
                return;
            }
        },
        None => net::filter_socks(attributed, None, plan.sockets.listen).into_iter()
            .filter_map(|(s, pid)| pid.map(|p| (s, p)))
            .collect()
    };

    let targets = kill::select_owning(&procs, &owners, options.pattern.as_deref(), options.exact, self_pid);
    if targets.is_empty() {
        println!("pq: no matching socket owners");
        return;
    }
    let headline = match plan.sockets.local_port {
        Some(port) => format!("own port {}", port),
        None => "own matching sockets".to_string()
    };
    preview_and_kill(&targets, &procs, options, &headline);
}

/// `--kill <pattern>`: sample, select the matching subtree(s), preview, confirm, then escalate
/// SIGTERM -> (wait `--grace`) -> SIGKILL. Never signals pq itself, its ancestor chain, or pid 1
/// (enforced by `kill::select`).
fn run_kill(pattern: &str, plan: &QueryPlan, options: &KillOptions) {
    if pattern.trim().is_empty() {
        eprintln!("pq: --kill needs a non-empty pattern");
        std::process::exit(1);
    }

    let self_pid = std::process::id() as i32;

    // A real two-sample delta so the preview's cpu% column is meaningful, same cadence as the
    // normal report.
    let prev = proc::snapshot();
    std::thread::sleep(Duration::from_millis(plan.interval_ms));
    let procs = proc::cpu_percent(&prev, &proc::snapshot(), plan.interval_ms as f64 / 1000.0);

    let targets = kill::select(&procs, pattern, options.exact, self_pid);

    if targets.is_empty() {
        println!("pq: no processes match \"{}\"", pattern);
        return;
    }
    preview_and_kill(&targets, &procs, options, &format!("match \"{}\"", pattern));
}

/// Preview the targets (with a socket column when net mode supplied one), confirm unless -y,
/// then escalate. Shared by pattern kill and net kill.
fn preview_and_kill(targets: &[kill::Target], procs: &[proc::Proc], options: &KillOptions, headline: &str) {
    let by_pid: HashMap<i32, &proc::Proc> = procs.iter().map(|p| (p.pid, p)).collect();

    let has_sock = targets.iter().any(|t| t.sock.is_some());

    println!("pq: {} process(es) {}:", targets.len(), headline);
    for t in targets {
        let (cpu, rss) = by_pid.get(&t.pid).map(|p| (p.cpu_pct, p.rss)).unwrap_or((0.0, 0));
        if has_sock {
            let sock = t.sock.as_deref().unwrap_or("");
            println!(
                "  {:>7} {:<10} {:>5.1}%  {:>8}  {:<14} {:<32} {}",
                t.pid, kill::username(t.uid), cpu, style::human_plain(rss), t.identity, sock, t.cmd
            );
        } else {
            println!(
                "  {:>7} {:<10} {:>5.1}%  {:>8}  {:<14} {}",
                t.pid, kill::username(t.uid), cpu, style::human_plain(rss), t.identity, t.cmd
            );
        }
    }

    if options.dry_run {
        return;
    }
    if !options.yes && !confirm() {
        eprintln!("pq: aborted");
        return;
    }
    let (sig, grace) = if options.sigkill {
        (libc::SIGKILL, 0)
    } else {
        (kill::parse_signal(&options.signal), options.grace)
    };
    let result = kill::escalate(&kill::RealSender, targets, sig, grace, &|d| std::thread::sleep(d));
    println!(
        "pq: terminated {}, killed {} via SIGKILL, {} not permitted",
        result.terminated.len(), result.killed.len(), result.failed.len()
    );
    if !result.failed.is_empty() {
        std::process::exit(1);
    }
}

/// Prompt `Proceed? [y/N]` on stderr and read a line from stdin; only "y"/"yes" (any case) proceeds.
fn confirm() -> bool {
    eprint!("Proceed? [y/N] ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn port_and_listen_imply_net_mode() {
        let a = Args::try_parse_from(["pq", "--port", "8080"]).unwrap();
        assert_eq!(QueryPlan::from_args(&a).unwrap().view, View::Network);
        let a = Args::try_parse_from(["pq", "--listen"]).unwrap();
        assert_eq!(QueryPlan::from_args(&a).unwrap().view, View::Network);
        let a = Args::try_parse_from(["pq"]).unwrap();
        assert_eq!(QueryPlan::from_args(&a).unwrap().view, View::Processes);
    }

    #[test]
    fn network_filters_imply_net_mode() {
        for flag in ["--tcp", "--udp", "--public", "--services", "--resolve"] {
            assert_eq!(QueryPlan::from_args(&Args::try_parse_from(["pq", flag]).unwrap()).unwrap().view, View::Network);
        }
        assert_eq!(QueryPlan::from_args(&Args::try_parse_from(["pq", "--remote-port", "443"]).unwrap()).unwrap().view, View::Network);
        assert_eq!(QueryPlan::from_args(&Args::try_parse_from(["pq", "--state", "established"]).unwrap()).unwrap().view, View::Network);
    }

    #[test]
    fn byte_sizes_accept_decimal_and_binary_units() {
        assert_eq!(parse_bytes("500M").unwrap(), 500_000_000);
        assert_eq!(parse_bytes("2GiB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert!(parse_bytes("lots").is_err());
    }

    #[test]
    fn bare_kill_parses_as_no_pattern() {
        let a = Args::try_parse_from(["pq", "--port", "8080", "--kill"]).unwrap();
        assert_eq!(a.kill.as_deref(), Some(""));
        let plan = QueryPlan::from_args(&a).unwrap();
        assert!(matches!(plan.action, Action::Kill(KillOptions { pattern: None, .. })));
    }

    #[test]
    fn kill_with_pattern_still_works() {
        let a = Args::try_parse_from(["pq", "--kill", "gradle"]).unwrap();
        let plan = QueryPlan::from_args(&a).unwrap();
        assert!(matches!(plan.action, Action::Kill(KillOptions { pattern: Some(ref p), .. }) if p == "gradle"));
    }

    #[test]
    fn port_rejects_non_numeric() {
        assert!(Args::try_parse_from(["pq", "--port", "http"]).is_err());
    }
}
