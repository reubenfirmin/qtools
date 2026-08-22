//! Typed interpretation of pq's CLI. Collection and rendering consume this plan rather than
//! independently inferring modes from raw flags.

use qtools::output::{OutputFormat, OutputOptions};

use crate::cluster::Metric;
use crate::net;
use crate::{Args, NetState};

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessSelection {
    pub pattern: Option<String>,
    pub user: Option<String>,
    pub pid: Option<i32>,
    pub min_cpu: Option<f64>,
    pub min_memory: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SocketSelection {
    pub network_requested: bool,
    pub owner_selection_requested: bool,
    pub local_port: Option<u16>,
    pub remote_port: Option<u16>,
    pub listen: bool,
    pub protocol: Option<net::Proto>,
    pub state: Option<&'static str>,
    pub public_only: bool,
    pub services: bool,
    pub resolve: bool,
}

impl SocketSelection {
    pub fn requested(&self) -> bool {
        self.network_requested
    }

    pub fn selects_owners(&self) -> bool { self.owner_selection_requested }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View { Processes, Network, Io }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoMetric { Combined, Read, Write }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillOptions {
    pub pattern: Option<String>,
    pub dry_run: bool,
    pub yes: bool,
    pub exact: bool,
    pub signal: String,
    pub grace: u64,
    pub sigkill: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action { Report, Kill(KillOptions) }

#[derive(Debug, Clone, PartialEq)]
pub struct QueryPlan {
    pub process: ProcessSelection,
    pub sockets: SocketSelection,
    pub view: View,
    pub action: Action,
    pub output: OutputOptions,
    pub metric: Metric,
    pub io_metric: IoMetric,
    pub interval_ms: u64,
}

impl QueryPlan {
    pub fn from_args(args: &Args) -> Result<Self, String> {
        let socket_selection_requested = args.net || args.port.is_some() || args.listen || args.remote_port.is_some()
            || args.tcp || args.udp || args.state.is_some() || args.public_only || args.services || args.resolve;
        let sockets = SocketSelection {
            network_requested: socket_selection_requested,
            owner_selection_requested: args.net || args.port.is_some() || args.listen || args.remote_port.is_some()
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
        };
        let process = ProcessSelection {
            pattern: args.pattern.clone(), user: args.user.clone(), pid: args.pid,
            min_cpu: args.min_cpu, min_memory: args.min_memory,
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
        let view = if args.io || args.read || args.write { View::Io }
            else if socket_selection_requested { View::Network } else { View::Processes };
        Ok(Self { process, sockets, view, action, output, metric, io_metric, interval_ms: args.interval })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn plan(words: &[&str]) -> QueryPlan {
        QueryPlan::from_args(&Args::try_parse_from(words).unwrap()).unwrap()
    }

    #[test]
    fn existing_cli_maps_to_explicit_views() {
        assert_eq!(plan(&["pq"]).view, View::Processes);
        assert_eq!(plan(&["pq", "--memory"]).view, View::Processes);
        assert_eq!(plan(&["pq", "--net"]).view, View::Network);
        assert_eq!(plan(&["pq", "--port", "5432"]).view, View::Network);
        assert_eq!(plan(&["pq", "--remote-port", "443"]).view, View::Network);
    }

    #[test]
    fn selectors_and_output_are_retained_in_the_plan() {
        let p = plan(&["pq", "--user", "postgres", "--min-memory", "2GiB", "--tcp", "--jsonl"]);
        assert_eq!(p.process.user.as_deref(), Some("postgres"));
        assert_eq!(p.process.min_memory, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(p.sockets.protocol, Some(net::Proto::Tcp));
        assert_eq!(p.output.format, OutputFormat::JsonLines);
    }

    #[test]
    fn action_validation_is_centralized() {
        let args = Args::try_parse_from(["pq", "--kill"]).unwrap();
        assert!(QueryPlan::from_args(&args).is_err());
        let p = plan(&["pq", "--port", "5432", "--kill"]);
        assert!(matches!(p.action, Action::Kill(KillOptions { pattern: None, .. })));
    }

    #[test]
    fn explicit_io_view_wins_while_socket_selector_is_retained() {
        let p = plan(&["pq", "--port", "5432", "--io"]);
        assert_eq!(p.view, View::Io);
        assert_eq!(p.sockets.local_port, Some(5432));
        assert!(p.sockets.selects_owners());
        assert_eq!(p.io_metric, IoMetric::Combined);

        let p = plan(&["pq", "--remote-port", "443", "--write"]);
        assert_eq!(p.view, View::Io);
        assert_eq!(p.io_metric, IoMetric::Write);
    }
}
