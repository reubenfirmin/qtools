//! Typed interpretation of pq's CLI. Collection and rendering consume this plan rather than
//! independently inferring modes from raw flags.

use crate::output::OutputOptions;

use crate::pq::cluster::Metric;
use crate::pq::net;

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessSelection {
    pub pattern: Option<String>,
    pub user: Option<String>,
    pub pid: Option<i32>,
    pub min_cpu: Option<f64>,
    pub min_memory: Option<u64>,
    pub mount: Option<String>,
    pub file: Option<String>,
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
    pub unix_path: Option<String>,
}

impl SocketSelection {
    pub fn requested(&self) -> bool {
        self.network_requested
    }

    pub fn selects_owners(&self) -> bool { self.owner_selection_requested }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View { Processes, Network, Io, Deleted, Fds, Holders, Blocked, Pss, Oom, Limits, Unix }

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
    pub tree: bool,
}


#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use crate::pq::cli::Args;
    use crate::output::OutputFormat;

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
        assert!(plan(&["pq", "--tree"]).tree);
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

    #[test]
    fn diagnostic_views_and_path_selectors_plan_cleanly() {
        assert_eq!(plan(&["pq", "--deleted"]).view, View::Deleted);
        assert_eq!(plan(&["pq", "--fds"]).view, View::Fds);
        assert_eq!(plan(&["pq", "--blocked"]).view, View::Blocked);
        assert_eq!(plan(&["pq", "--pss"]).view, View::Pss);
        assert_eq!(plan(&["pq", "--oom"]).view, View::Oom);
        assert_eq!(plan(&["pq", "--limits"]).view, View::Limits);
        assert_eq!(plan(&["pq", "--unix"]).view, View::Unix);
        let p = plan(&["pq", "--mount", "/mnt/data", "--fds"]);
        assert_eq!(p.view, View::Fds);
        assert_eq!(p.process.mount.as_deref(), Some("/mnt/data"));
        assert!(QueryPlan::from_args(&Args::try_parse_from(["pq", "--fds", "--pss"]).unwrap()).is_err());
    }
}
