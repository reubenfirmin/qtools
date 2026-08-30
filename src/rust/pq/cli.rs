use clap::{CommandFactory, Parser, ValueEnum, ValueHint};
use clap_complete::{generate, Shell};

pub(crate) mod help;

use help::EXAMPLES;
use super::{cluster::Metric, net};
use super::pq_query::{Action, IoMetric, KillOptions, ProcessSelection, QueryPlan, SocketSelection, View};
use crate::output::{OutputFormat, OutputOptions};

#[derive(Parser, Debug)]
#[command(name = "pq", about = "pq: query and cluster the processes eating your CPU/memory or talking on the network.", after_help = EXAMPLES)]
pub(crate) struct Args {
    /// Print a completion script for the selected shell
    #[arg(long, value_enum, value_name = "SHELL", exclusive = true)] pub(super) generate_completion: Option<CompletionShell>,
    /// Clusters to show (default 15; --listen defaults to all, so no listener is ever cut off)
    #[arg(short = 'n', long, help_heading = "Reporting")] pub(super) top: Option<usize>,
    /// Expand clusters to their member processes (per-connection rows in --net mode)
    #[arg(short, long, help_heading = "Reporting")] pub(super) verbose: bool,
    /// Show matching processes in a static parent/child table
    #[arg(long, help_heading = "Reporting")] pub(super) tree: bool,
    /// CPU sample interval in milliseconds
    #[arg(long, default_value_t = 400, help_heading = "Selection and refinement")] pub(super) interval: u64,
    /// Emit machine-readable JSON
    #[arg(long, help_heading = "Reporting")] pub(super) json: bool,
    /// Emit one compact JSON object per line (useful for append-only logs)
    #[arg(long, conflicts_with = "json", help_heading = "Reporting")] pub(super) jsonl: bool,
    /// Disable ANSI color and terminal graphics
    #[arg(long, help_heading = "Reporting")] pub(super) no_color: bool,
    /// Only processes owned by this username or numeric uid
    #[arg(long, help_heading = "Selection and refinement")] pub(super) user: Option<String>,
    /// Only this process (network mode: sockets owned by this pid)
    #[arg(long, help_heading = "Selection and refinement")] pub(super) pid: Option<i32>,
    /// Only clusters using at least this percentage of total CPU
    #[arg(long, value_name = "PERCENT", help_heading = "Selection and refinement")] pub(super) min_cpu: Option<f64>,
    /// Only clusters using at least this much memory (for example 500M or 2GiB)
    #[arg(long, value_parser = parse_bytes, value_name = "SIZE", help_heading = "Selection and refinement")] pub(super) min_memory: Option<u64>,
    /// Sort and chart by memory instead of CPU
    #[arg(short = 'm', long, help_heading = "Queries")] pub(super) memory: bool,
    /// Sort and chart by swap usage instead of CPU
    #[arg(short = 's', long, help_heading = "Queries")] pub(super) swap: bool,
    /// Sort and chart by CPU (default)
    #[arg(short = 'c', long, help_heading = "Queries")] pub(super) cpu: bool,
    /// Report process read/write rates, sorted by combined I/O
    #[arg(long, conflicts_with_all = ["read", "write"], help_heading = "Queries")] pub(super) io: bool,
    /// Report process I/O sorted by read rate
    #[arg(long, conflicts_with_all = ["io", "write"], help_heading = "Queries")] pub(super) read: bool,
    /// Report process I/O sorted by write rate
    #[arg(long, conflicts_with_all = ["io", "read"], help_heading = "Queries")] pub(super) write: bool,
    /// Report deleted files still held open
    #[arg(long, help_heading = "Queries")] pub(super) deleted: bool,
    /// Report open file descriptor counts and classifications
    #[arg(long, help_heading = "Queries")] pub(super) fds: bool,
    /// Select processes holding paths beneath this mount/directory
    #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath, help_heading = "Selection and refinement")] pub(super) mount: Option<String>,
    /// Select processes holding this exact path
    #[arg(long, value_name = "PATH", value_hint = ValueHint::FilePath, conflicts_with = "mount", help_heading = "Selection and refinement")] pub(super) file: Option<String>,
    /// Report processes in uninterruptible D state
    #[arg(long, help_heading = "Queries")] pub(super) blocked: bool,
    /// Report proportional memory from smaps_rollup
    #[arg(long, help_heading = "Queries")] pub(super) pss: bool,
    /// Report mapped address-space breadth from /proc/PID/maps
    #[arg(long, help_heading = "Queries")] pub(super) maps: bool,
    /// Sample recently referenced memory regions (resets selected processes' page-reference bits)
    #[arg(long, help_heading = "Queries")] pub(super) access: bool,
    /// Rank processes by the kernel's current out-of-memory kill preference
    #[arg(long, help_heading = "Queries")] pub(super) oom: bool,
    /// Report measurable finite process resource limits
    #[arg(long, help_heading = "Queries")] pub(super) limits: bool,
    /// Report Unix-domain sockets, optionally matching a socket path
    #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "PATH", value_hint = ValueHint::FilePath, help_heading = "Queries")] pub(super) unix: Option<String>,
    /// Report TCP/UDP connections instead of cpu/mem clusters (sorted by traffic, then
    /// connection count; metric flags do not change the sort). World-exposed listeners are
    /// highlighted
    #[arg(long, help_heading = "Queries")] pub(super) net: bool,
    /// Only sockets with this local port (implies --net)
    #[arg(long, help_heading = "Selection and refinement")] pub(super) port: Option<u16>,
    /// Only listening sockets (implies --net)
    #[arg(long, help_heading = "Selection and refinement")] pub(super) listen: bool,
    /// Only sockets connected to this peer port (implies --net)
    #[arg(long, help_heading = "Selection and refinement")] pub(super) remote_port: Option<u16>,
    /// Only TCP sockets (implies --net)
    #[arg(long, conflicts_with = "udp", help_heading = "Selection and refinement")] pub(super) tcp: bool,
    /// Only UDP sockets (implies --net)
    #[arg(long, conflicts_with = "tcp", help_heading = "Selection and refinement")] pub(super) udp: bool,
    /// Only sockets in this state (implies --net)
    #[arg(long, value_enum, help_heading = "Selection and refinement")] pub(super) state: Option<NetState>,
    /// Only listeners bound to all interfaces (implies --net; firewall not checked)
    #[arg(long = "public", help_heading = "Selection and refinement")] pub(super) public_only: bool,
    /// Show conventional names from /etc/services beside port numbers
    #[arg(long, help_heading = "Reporting")] pub(super) services: bool,
    /// Reverse-resolve peer IPs in per-connection rows (may be slow)
    #[arg(long, help_heading = "Reporting")] pub(super) resolve: bool,
    /// Kill matching process trees (preview + confirm); pattern optional with --port/--listen
    #[arg(long, num_args = 0..=1, default_missing_value = "", help_heading = "Actions")] pub(super) kill: Option<String>,
    /// Preview only, do not signal
    #[arg(long, help_heading = "Actions")] pub(super) dry_run: bool,
    /// Skip the confirmation prompt
    #[arg(short = 'y', long, help_heading = "Actions")] pub(super) yes: bool,
    /// Match the identity exactly instead of substring
    #[arg(short = 'x', long, help_heading = "Actions")] pub(super) exact: bool,
    /// Initial signal
    #[arg(long, default_value = "TERM", help_heading = "Actions")] pub(super) signal: String,
    /// Seconds to wait before SIGKILL
    #[arg(long, default_value_t = 4, help_heading = "Actions")] pub(super) grace: u64,
    /// SIGKILL immediately (no TERM, no grace)
    #[arg(short = '9', long = "sigkill", help_heading = "Actions")] pub(super) sigkill: bool,
    /// Filter the report to matching clusters
    pub(super) pattern: Option<String>,
}

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
    generate(Shell::from(shell), &mut Args::command(), "pq", &mut std::io::stdout());
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum NetState { Listen, Established, Bound, Other }

fn parse_bytes(raw: &str) -> Result<u64, String> {
    let value = raw.trim();
    let split = value.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(value.len());
    let number: f64 = value[..split].parse().map_err(|_| format!("invalid size: {raw}"))?;
    let unit = value[split..].trim().to_ascii_lowercase();
    let multiplier = match unit.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1_000.0, "ki" | "kib" => 1_024.0,
        "m" | "mb" => 1_000_000.0, "mi" | "mib" => 1_048_576.0,
        "g" | "gb" => 1_000_000_000.0, "gi" | "gib" => 1_073_741_824.0,
        "t" | "tb" => 1_000_000_000_000.0, "ti" | "tib" => 1_099_511_627_776.0,
        _ => return Err(format!("unknown size unit in {raw}")),
    };
    let bytes = number * multiplier;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        Err(format!("size out of range: {raw}"))
    } else {
        Ok(bytes.round() as u64)
    }
}

impl QueryPlan {
    pub fn from_args(args: &Args) -> Result<Self, String> {
        let socket_selection_requested = args.net || args.port.is_some() || args.listen || args.remote_port.is_some()
            || args.tcp || args.udp || args.state.is_some() || args.public_only || args.services || args.resolve;
        let sockets = SocketSelection {
            network_requested: socket_selection_requested,
            owner_selection_requested: args.port.is_some() || args.listen || args.remote_port.is_some()
                || args.tcp || args.udp || args.state.is_some() || args.public_only,
            local_port: args.port,
            remote_port: args.remote_port,
            listen: args.listen,
            protocol: if args.tcp { Some(net::Proto::Tcp) } else if args.udp { Some(net::Proto::Udp) } else { None },
            state: args.state.map(|state| match state {
                NetState::Listen => "listen", NetState::Established => "established",
                NetState::Bound => "bound", NetState::Other => "other"
            }),
            public_only: args.public_only,
            services: args.services,
            resolve: args.resolve,
            unix_path: args.unix.as_ref().and_then(|path| (!path.is_empty()).then(|| path.clone())),
        };
        let process = ProcessSelection {
            pattern: args.pattern.clone(), user: args.user.clone(), pid: args.pid,
            min_cpu: args.min_cpu, min_memory: args.min_memory,
            mount: args.mount.clone(), file: args.file.clone(),
        };
        let output = OutputOptions {
            format: if args.jsonl { OutputFormat::JsonLines } else if args.json { OutputFormat::Json } else { OutputFormat::Rich },
            top: args.top, verbose: args.verbose, no_color: args.no_color,
        };
        let action = match &args.kill {
            None => Action::Report,
            Some(pattern) => {
                let pattern = pattern.trim();
                if pattern.is_empty() && args.port.is_none() && !args.listen {
                    return Err("--kill needs a pattern or a --net filter (--port / --listen)".into());
                }
                Action::Kill(KillOptions {
                    pattern: (!pattern.is_empty()).then(|| pattern.to_string()),
                    dry_run: args.dry_run, yes: args.yes, exact: args.exact,
                    signal: args.signal.clone(), grace: args.grace, sigkill: args.sigkill,
                })
            }
        };
        let metric = if args.swap { Metric::Swap } else if args.memory { Metric::Memory } else { Metric::Cpu };
        let io_metric = if args.read { IoMetric::Read } else if args.write { IoMetric::Write } else { IoMetric::Combined };
        let explicit = [args.io || args.read || args.write, args.deleted, args.fds, args.blocked, args.pss,
            args.maps, args.access, args.oom, args.limits, args.unix.is_some()];
        if explicit.into_iter().filter(|active| *active).count() > 1 {
            return Err("choose only one report view (--io/--read/--write, --deleted, --fds, --blocked, --pss, --maps, --access, --oom, --limits, or --unix)".into());
        }
        let view = if args.io || args.read || args.write { View::Io }
            else if args.deleted { View::Deleted }
            else if args.fds { View::Fds }
            else if args.blocked { View::Blocked }
            else if args.pss { View::Pss }
            else if args.maps { View::Maps }
            else if args.access { View::Access }
            else if args.oom { View::Oom }
            else if args.limits { View::Limits }
            else if args.unix.is_some() { View::Unix }
            else if args.mount.is_some() || args.file.is_some() { View::Holders }
            else if socket_selection_requested { View::Network } else { View::Processes };
        if args.tree && view != View::Processes {
            return Err("--tree currently applies to the CPU/memory/swap process report".into());
        }
        Ok(Self { process, sockets, view, action, output, metric, io_metric, interval_ms: args.interval, tree: args.tree })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn byte_sizes_accept_decimal_and_binary_units() {
        assert_eq!(parse_bytes("500M").unwrap(), 500_000_000);
        assert_eq!(parse_bytes("2GiB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert!(parse_bytes("lots").is_err());
    }

    #[test]
    fn socket_options_select_the_network_view() {
        for args in [
            vec!["pq", "--port", "8080"],
            vec!["pq", "--listen"],
            vec!["pq", "--tcp"],
            vec!["pq", "--udp"],
            vec!["pq", "--public"],
            vec!["pq", "--services"],
            vec!["pq", "--resolve"],
            vec!["pq", "--remote-port", "443"],
            vec!["pq", "--state", "established"],
        ] {
            let args = Args::try_parse_from(args).unwrap();
            assert_eq!(QueryPlan::from_args(&args).unwrap().view, View::Network);
        }
        let args = Args::try_parse_from(["pq"]).unwrap();
        assert_eq!(QueryPlan::from_args(&args).unwrap().view, View::Processes);
    }

    #[test]
    fn bare_kill_is_valid_with_a_port() {
        let args = Args::try_parse_from(["pq", "--port", "8080", "--kill"]).unwrap();
        let plan = QueryPlan::from_args(&args).unwrap();
        assert!(matches!(plan.action, Action::Kill(KillOptions { pattern: None, .. })));
    }

    #[test]
    fn kill_pattern_and_invalid_port_parsing_are_preserved() {
        let args = Args::try_parse_from(["pq", "--kill", "gradle"]).unwrap();
        let plan = QueryPlan::from_args(&args).unwrap();
        assert!(matches!(plan.action, Action::Kill(KillOptions { pattern: Some(ref value), .. }) if value == "gradle"));
        assert!(Args::try_parse_from(["pq", "--port", "http"]).is_err());
    }
}
