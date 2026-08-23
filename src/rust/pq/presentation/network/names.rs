use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::pq::net;

#[derive(Default)]
pub struct DisplayNames {
    pub(crate) services: HashMap<u16, String>,
    pub(crate) hosts: HashMap<IpAddr, String>
}

/// Resolve optional presentation-only names once per report. Numeric endpoints remain canonical
/// in JSON and are always shown in text; names are annotations, never query inputs.
pub fn display_names(r: &net::NetReport, services: bool, resolve: bool) -> DisplayNames {
    let service_names = if services { read_services() } else { HashMap::new() };
    let mut hosts = HashMap::new();
    if resolve {
        let ips: Vec<IpAddr> = r.clusters.iter().flat_map(|c| c.conns.iter().map(|(s, _)| s.peer))
            .chain(r.unattributed.iter().flat_map(|(_, socks)| socks.iter().map(|s| s.peer)))
            .filter(|ip| !ip.is_unspecified()).collect::<HashSet<_>>().into_iter().collect();
        let pool = threadpool::ThreadPool::new(ips.len().clamp(1, 8));
        let (tx, rx) = std::sync::mpsc::channel();
        for ip in &ips {
            let tx = tx.clone(); let ip = *ip;
            pool.execute(move || { let _ = tx.send(resolve_host(ip)); });
        }
        drop(tx);
        for (ip, name) in rx.into_iter().flatten() {
            hosts.insert(ip, name);
        }
    }
    DisplayNames { services: service_names, hosts }
}

fn resolve_host(ip: IpAddr) -> Option<(IpAddr, String)> {
    let mut child = Command::new("getent").args(["hosts", &ip.to_string()])
        .stdin(Stdio::null()).stderr(Stdio::null()).stdout(Stdio::piped()).spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().ok()? {
            if !status.success() { return None; }
            let output = child.wait_with_output().ok()?;
            let name = String::from_utf8_lossy(&output.stdout).split_whitespace().nth(1)?
                .trim_end_matches('.').to_string();
            return Some((ip, name));
        }
        if Instant::now() >= deadline { let _ = child.kill(); let _ = child.wait(); return None; }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_services() -> HashMap<u16, String> {
    std::fs::read_to_string("/etc/services")
        .map(|raw| parse_services(&raw))
        .unwrap_or_default()
}

fn parse_services(raw: &str) -> HashMap<u16, String> {
    let mut names = HashMap::new();
    for line in raw.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')) {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else { continue };
        let Some(port_proto) = fields.next() else { continue };
        if let Some((port, _)) = port_proto.split_once('/') {
            if let Ok(port) = port.parse() {
                names.entry(port).or_insert_with(|| name.to_string());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pq::net::{NetCluster, NetReport, Proto, Sock, SockState};

    #[test]
    fn services_parser_ignores_comments_garbage_and_duplicate_protocols() {
        let names = parse_services("# comment\nhttp 80/tcp\nhttp-alt 80/udp\nbad\ninvalid nope/tcp\n domain 53/udp # dns\n");
        assert_eq!(names.get(&80).map(String::as_str), Some("http"));
        assert_eq!(names.get(&53).map(String::as_str), Some("domain"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn disabled_annotations_do_no_io_and_return_empty_maps() {
        let report = NetReport {
            clusters: Vec::new(), unattributed: Vec::new(), total_conns: 0,
            total_listening: 0, unattributed_count: 0, listen_port_set: HashSet::new(),
            total_world: 0, traffic: HashMap::new(),
        };
        let names = display_names(&report, false, false);
        assert!(names.services.is_empty());
        assert!(names.hosts.is_empty());
    }

    #[test]
    fn enabled_annotations_read_services_and_resolve_unique_peers() {
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let socket = Sock {
            proto: Proto::Tcp, state: SockState::Established,
            local: "127.0.0.1".parse().unwrap(), local_port: 40000,
            peer, peer_port: 80, uid: 1000, inode: 1,
        };
        let report = NetReport {
            clusters: vec![NetCluster {
                identity: "client".into(), cpu: 0.0, rss: 0,
                conns: vec![(socket.clone(), 7), (socket, 7)], proc_count: 1,
                listen_ports: Vec::new(), world_ports: Vec::new(),
            }],
            unattributed: Vec::new(), total_conns: 2, total_listening: 0,
            unattributed_count: 0, listen_port_set: HashSet::new(), total_world: 0,
            traffic: HashMap::new(),
        };
        let names = display_names(&report, true, true);
        assert!(!names.services.is_empty());
        assert!(names.hosts.get(&peer).is_some_and(|name| !name.is_empty()));
    }
}
