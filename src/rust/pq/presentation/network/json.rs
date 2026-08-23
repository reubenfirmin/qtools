use std::io::{self, Write};
use serde::Serialize;

use crate::pq::net;
use crate::pq::net_report::NetOpts;

/// Machine-readable JSON. Connections are always included (JSON is for machines; no -v gating),
/// each as {pid, proto, state, listening, direction, world, local, peer} with peer empty for
/// listeners.
pub fn write_json<W: Write>(out: &mut W, r: &net::NetReport, mem_used: u64, mem_total: u64, opts: &NetOpts) -> io::Result<()> {
    #[derive(Serialize)]
    struct Connection {
        pid: Option<i32>, proto: &'static str, state: String, listening: bool,
        direction: &'static str, world: bool, tx: u64, rx: u64, local: String, peer: String
    }
    #[derive(Serialize)]
    struct NetCluster<'a> {
        identity: &'a str, conns: usize, listeners: usize, established: usize,
        listening: &'a [u16], world_ports: &'a [u16], bytes_tx: u64, bytes_rx: u64,
        cpu: f64, mem: u64, proc_count: usize, connections: Vec<Connection>
    }
    #[derive(Serialize)]
    struct Unattributed { uid: u32, user: String, conns: usize, connections: Vec<Connection> }
    #[derive(Serialize)]
    struct NetReport<'a> {
        mode: &'static str, generated_at_unix_ms: u128, sample_interval_ms: u64,
        total_conns: usize, total_listening: usize, total_world: usize,
        unattributed_conns: usize, mem_used: u64, mem_total: u64,
        clusters: Vec<NetCluster<'a>>, unattributed: Vec<Unattributed>
    }

    let connection = |s: &net::Sock, pid| {
        let (tx, rx) = r.traffic.get(&s.inode).copied().unwrap_or((0, 0));
        Connection {
            pid, proto: match s.proto { net::Proto::Tcp => "tcp", net::Proto::Udp => "udp" },
            state: s.state_label().to_string(), listening: s.is_listening(),
            direction: s.direction(&r.listen_port_set), world: s.is_world_exposed(), tx, rx,
            local: s.local_str(), peer: s.peer_str()
        }
    };
    let clusters = r.clusters.iter().take(opts.top).map(|c| {
        let (bytes_tx, bytes_rx) = c.traffic(&r.traffic);
        NetCluster {
            identity: &c.identity, conns: c.conns.len(), listeners: c.listeners(), established: c.established(),
            listening: &c.listen_ports, world_ports: &c.world_ports, bytes_tx, bytes_rx,
            cpu: round2(c.cpu), mem: c.rss, proc_count: c.proc_count,
            connections: c.conns.iter().map(|(s, pid)| connection(s, Some(*pid))).collect()
        }
    }).collect();
    let unattributed = r.unattributed.iter().map(|(uid, socks)| Unattributed {
        uid: *uid, user: crate::pq::platform::users::username(*uid), conns: socks.len(),
        connections: socks.iter().map(|s| connection(s, None)).collect()
    }).collect();
    let generated_at_unix_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_millis();
    let report = NetReport {
        mode: "net", generated_at_unix_ms, sample_interval_ms: opts.sample_interval_ms,
        total_conns: r.total_conns, total_listening: r.total_listening,
        total_world: r.total_world, unattributed_conns: r.unattributed_count,
        mem_used, mem_total, clusters, unattributed
    };
    if opts.compact_json {
        serde_json::to_writer(&mut *out, &report).map_err(io::Error::other)?;
    } else {
        serde_json::to_writer_pretty(&mut *out, &report).map_err(io::Error::other)?;
    }
    writeln!(out)
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
