//! /proc/net socket tables: pure parsing of the kernel's hex-encoded tcp/tcp6/udp/udp6 format,
//! socket-inode to pid attribution via /proc/<pid>/fd, and the net-mode clustering that joins
//! sockets onto identity clusters. Mirrors proc.rs's pattern: impure /proc reads are isolated in
//! thin readers; everything else takes plain data in and out and is unit tested.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::pq::cluster;
use crate::pq::proc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto { Tcp, Udp }

/// Kernel socket states we distinguish (values from include/net/tcp_states.h: 0A=LISTEN,
/// 01=ESTABLISHED). UDP reuses the same field: a bound-but-unconnected UDP socket reads
/// TCP_CLOSE (07), surfaced here as Other(7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SockState { Listen, Established, Other(u8) }

/// One socket table row. `inode` joins it to an owning process via /proc/<pid>/fd.
#[derive(Debug, Clone)]
pub struct Sock {
    pub proto: Proto,
    pub state: SockState,
    pub local: IpAddr,
    pub local_port: u16,
    pub peer: IpAddr,
    pub peer_port: u16,
    pub uid: u32,
    pub inode: u64
}

impl Sock {
    /// "Listening": a TCP socket in LISTEN, or a bound-but-unconnected UDP socket (state
    /// TCP_CLOSE with no peer), which is what serves inbound datagrams.
    pub fn is_listening(&self) -> bool {
        match self.proto {
            Proto::Tcp => self.state == SockState::Listen,
            Proto::Udp => self.state == SockState::Other(7) && self.peer_port == 0
        }
    }

    /// Listening on an unspecified address (0.0.0.0 or ::): reachable on every interface, i.e.
    /// world-exposed as far as this host's binding goes (the firewall may still block it; pq
    /// does not interrogate firewall rules).
    pub fn is_world_exposed(&self) -> bool {
        self.is_listening() && self.local.is_unspecified()
    }

    /// Traffic direction relative to this host: "listen" for listeners, "in" for an established
    /// connection accepted on a port this host listens on, "out" for everything else (ephemeral
    /// local port, so this process initiated the connection). A heuristic, but a solid one: the
    /// only miss is a client that deliberately binds its source port to a locally-listened port.
    pub fn direction(&self, listen_ports: &HashSet<u16>) -> &'static str {
        if self.is_listening() {
            "listen"
        } else if listen_ports.contains(&self.local_port) {
            "in"
        } else {
            "out"
        }
    }

    /// Short human label for previews and -v rows, e.g. "tcp LISTEN  0.0.0.0:8080" or
    /// "tcp ESTAB  192.168.1.5:44322 -> 34.107.243.93:443".
    pub fn label(&self) -> String {
        let proto = match self.proto { Proto::Tcp => "tcp", Proto::Udp => "udp" };
        if self.peer_port == 0 {
            format!("{} {}  {}", proto, self.state_label(), self.local_str())
        } else {
            format!("{} {}  {} -> {}", proto, self.state_label(), self.local_str(), self.peer_str())
        }
    }

    /// Short state string: "LISTEN"/"ESTAB"/"OTHER" for TCP, "BOUND"/"UDP" for UDP (bound vs.
    /// connected), matching what the text report prints for BOUND UDP listeners.
    pub fn state_label(&self) -> &'static str {
        match (self.proto, self.state) {
            (Proto::Tcp, SockState::Listen) => "LISTEN",
            (Proto::Tcp, SockState::Established) => "ESTAB",
            (Proto::Tcp, SockState::Other(_)) => "OTHER",
            (Proto::Udp, _) => if self.is_listening() { "BOUND" } else { "UDP" }
        }
    }

    /// Local endpoint as "ip:port", always present (every socket row has a local address).
    pub fn local_str(&self) -> String {
        format!("{}:{}", self.local, self.local_port)
    }

    /// Peer as "ip:port", or empty when there is no peer (listeners, unconnected UDP), so JSON
    /// and table cells can render it directly.
    pub fn peer_str(&self) -> String {
        if self.peer_port == 0 { String::new() } else { format!("{}:{}", self.peer, self.peer_port) }
    }
}

/// Parse one /proc/net/{tcp,tcp6,udp,udp6} table. Line layout (header skipped):
/// `sl local_address rem_address st tx:rx tr:tm->when retrnsmt uid timeout inode ...`,
/// i.e. fields[1]=local, [2]=peer, [3]=state, [7]=uid, [9]=inode. A line that fails to parse is
/// skipped rather than failing the table. Rows with inode 0 (TIME_WAIT and other kernel-held
/// sockets not attached to any process) are dropped: they cannot be attributed or killed.
pub fn parse_net_table(raw: &str, proto: Proto) -> Vec<Sock> {
    raw.lines().skip(1).filter_map(|line| parse_net_line(line, proto)).collect()
}

fn parse_net_line(line: &str, proto: Proto) -> Option<Sock> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let (local, local_port) = split_addr(fields.get(1)?)?;
    let (peer, peer_port) = split_addr(fields.get(2)?)?;
    let state = match u8::from_str_radix(fields.get(3)?, 16).ok()? {
        0x0A => SockState::Listen,
        0x01 => SockState::Established,
        n => SockState::Other(n)
    };
    let uid: u32 = fields.get(7)?.parse().ok()?;
    let inode: u64 = fields.get(9)?.parse().ok()?;
    if inode == 0 {
        return None;
    }
    Some(Sock { proto, state, local, local_port, peer, peer_port, uid, inode })
}

/// Split "0100007F:1F90" into (address, port). The port is 4 hex chars in natural (big-endian)
/// reading order.
fn split_addr(field: &str) -> Option<(IpAddr, u16)> {
    let (addr_hex, port_hex) = field.split_once(':')?;
    Some((parse_addr(addr_hex)?, u16::from_str_radix(port_hex, 16).ok()?))
}

/// Decode a hex-encoded address as the kernel prints it: 8 chars for IPv4 (one little-endian
/// u32), 32 chars for IPv6 (four little-endian u32 groups). A v4-mapped v6 address collapses to
/// its v4 form so ::ffff:127.0.0.1 and 127.0.0.1 filter and display identically.
fn parse_addr(hex: &str) -> Option<IpAddr> {
    match hex.len() {
        8 => {
            let n = u32::from_str_radix(hex, 16).ok()?;
            Some(IpAddr::V4(Ipv4Addr::from(n.to_le_bytes())))
        }
        32 => {
            let mut bytes = [0u8; 16];
            for i in 0..4 {
                let n = u32::from_str_radix(&hex[i * 8..(i + 1) * 8], 16).ok()?;
                bytes[i * 4..(i + 1) * 4].copy_from_slice(&n.to_le_bytes());
            }
            let v6 = Ipv6Addr::from(bytes);
            Some(match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => IpAddr::V6(v6)
            })
        }
        _ => None
    }
}

/// All TCP/UDP sockets from the four /proc/net tables, plus how many tables were actually
/// readable. An unreadable table contributes nothing; `tables_read == 0` means /proc/net itself
/// is unavailable and the caller decides how loudly to fail.
pub fn read_sockets() -> (Vec<Sock>, usize) {
    let tables = [
        ("/proc/net/tcp", Proto::Tcp), ("/proc/net/tcp6", Proto::Tcp),
        ("/proc/net/udp", Proto::Udp), ("/proc/net/udp6", Proto::Udp)
    ];
    let mut out = Vec::new();
    let mut tables_read = 0;
    for (path, proto) in tables {
        if let Ok(raw) = fs::read_to_string(path) {
            tables_read += 1;
            out.extend(parse_net_table(&raw, proto));
        }
    }
    (out, tables_read)
}

/// Map socket inodes to owning pids by walking /proc/<pid>/fd symlinks ("socket:[12345]").
/// Per-pid failures (permission denied for other users' processes, or the pid exiting mid-scan)
/// are skipped silently, same policy as proc::snapshot(): without root only the caller's own
/// processes attribute, and the report surfaces that as unattributed rows rather than an error.
/// A socket shared across forked processes maps to whichever pid was scanned last.
pub fn inode_map(pids: &[i32]) -> HashMap<u64, i32> {
    let mut map = HashMap::new();
    for &pid in pids {
        let entries = match fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(entries) => entries,
            Err(_) => continue
        };
        for entry in entries.flatten() {
            if let Ok(target) = fs::read_link(entry.path()) {
                if let Some(inode) = target.to_str().and_then(parse_socket_link) {
                    map.insert(inode, pid);
                }
            }
        }
    }
    map
}

/// "socket:[12345]" -> 12345.
fn parse_socket_link(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

/// Pure join of sockets onto owning pids (None = could not attribute).
pub fn attribute(socks: Vec<Sock>, inodes: &HashMap<u64, i32>) -> Vec<(Sock, Option<i32>)> {
    socks.into_iter().map(|s| {
        let pid = inodes.get(&s.inode).copied();
        (s, pid)
    }).collect()
}

/// Apply the report-mode filters: `port` keeps sockets whose LOCAL port matches (any state, so
/// the report answers "who owns port N" broadly; the stricter listeners-only rule for --kill
/// lives in `port_kill_candidates`), and `listen` keeps only listening sockets.
pub fn filter_socks(attributed: Vec<(Sock, Option<i32>)>, port: Option<u16>, listen: bool) -> Vec<(Sock, Option<i32>)> {
    attributed.into_iter()
        .filter(|(s, _)| port.is_none_or(|p| s.local_port == p))
        .filter(|(s, _)| !listen || s.is_listening())
        .collect()
}

/// Additional composable report filters. Local-port/listener filtering stays in `filter_socks`
/// because kill mode intentionally uses only those established semantics.
pub fn filter_socks_extended(
    attributed: Vec<(Sock, Option<i32>)>, remote_port: Option<u16>, proto: Option<Proto>,
    state: Option<&str>, public_only: bool
) -> Vec<(Sock, Option<i32>)> {
    attributed.into_iter()
        .filter(|(s, _)| remote_port.is_none_or(|p| s.peer_port == p))
        .filter(|(s, _)| proto.is_none_or(|p| s.proto == p))
        .filter(|(s, _)| state.is_none_or(|wanted| match wanted {
            "listen" => s.proto == Proto::Tcp && s.state == SockState::Listen,
            "established" => s.state == SockState::Established,
            "bound" => s.proto == Proto::Udp && s.is_listening(),
            "other" => !s.is_listening() && s.state != SockState::Established,
            _ => true
        }))
        .filter(|(s, _)| !public_only || s.is_world_exposed())
        .collect()
}

/// One identity cluster's network view: the base cluster's cpu/rss/member-count aggregates plus
/// every socket attributed to a member pid.
pub struct NetCluster {
    pub identity: String,
    pub cpu: f64,
    pub rss: u64,
    pub proc_count: usize,
    pub conns: Vec<(Sock, i32)>,
    pub listen_ports: Vec<u16>,
    /// The subset of listen_ports bound to an unspecified address (world-exposed).
    pub world_ports: Vec<u16>
}

impl NetCluster {
    /// Listening sockets in this cluster (TCP LISTEN + bound UDP).
    pub fn listeners(&self) -> usize {
        self.conns.iter().filter(|(s, _)| s.is_listening()).count()
    }

    /// Established (non-listening) sockets in this cluster.
    pub fn established(&self) -> usize {
        self.conns.len() - self.listeners()
    }

    /// Summed (tx, rx) bytes over this cluster's sockets, joined by inode against a sock_diag
    /// traffic map. Sockets without counters (UDP, very old kernels) contribute nothing.
    pub fn traffic(&self, traffic: &HashMap<u64, (u64, u64)>) -> (u64, u64) {
        self.conns.iter().fold((0, 0), |(tx, rx), (s, _)| {
            let (t, r) = traffic.get(&s.inode).copied().unwrap_or((0, 0));
            (tx + t, rx + r)
        })
    }
}

/// Everything the net report renders. Totals are computed over the full (post --port/--listen,
/// pre-pattern) socket set, mirroring how pq's header cpu% stays system-wide under a pattern
/// filter. `unattributed` is display data (hidden under a pattern, which sockets cannot match);
/// `unattributed_count` is always the true count, driving the sudo hint.
pub struct NetReport {
    pub clusters: Vec<NetCluster>,
    pub unattributed: Vec<(u32, Vec<Sock>)>,
    pub total_conns: usize,
    pub total_listening: usize,
    pub unattributed_count: usize,
    /// Every local port with a listener in the (pre-pattern) socket set, for direction tagging.
    pub listen_port_set: HashSet<u16>,
    /// Listeners bound to an unspecified address (world-exposed), pre-pattern.
    pub total_world: usize,
    /// Per-socket (tx, rx) byte counters from sock_diag, keyed by inode (TCP only).
    pub traffic: HashMap<u64, (u64, u64)>
}

/// Join attributed sockets onto identity clusters (reusing `cluster::cluster` for the cpu/rss
/// aggregates so a socket-owning child folds into the same cluster pq shows elsewhere), keeping
/// only clusters that own at least one socket. Sorted by traffic (tx+rx bytes) descending, then
/// connection count, then cpu, so the heaviest talkers lead when counters are available and the
/// order stays sane when they are not (UDP-only, counters unavailable). `pattern` filters
/// clusters exactly like pq's report filter (identity, or any member's comm/cmdline,
/// case-insensitive substring).
pub fn net_clusters(procs: &[proc::Proc], attributed: Vec<(Sock, Option<i32>)>, pattern: Option<&str>, traffic: HashMap<u64, (u64, u64)>) -> NetReport {
    let base = cluster::cluster(procs, cluster::Metric::Cpu);
    let identity_of = cluster::effective_identities(procs);

    let needle = pattern.map(str::to_lowercase);
    let matches: HashMap<&str, bool> = base.iter().map(|c| {
        let ok = needle.as_deref().is_none_or(|n| {
            c.identity.to_lowercase().contains(n)
                || c.members.iter().any(|m| m.cmd.to_lowercase().contains(n) || m.comm.to_lowercase().contains(n))
        });
        (c.identity.as_str(), ok)
    }).collect();

    let total_conns = attributed.len();
    let total_listening = attributed.iter().filter(|(s, _)| s.is_listening()).count();
    let listen_port_set: HashSet<u16> = attributed.iter()
        .filter(|(s, _)| s.is_listening()).map(|(s, _)| s.local_port).collect();
    let total_world = attributed.iter().filter(|(s, _)| s.is_world_exposed()).count();

    let mut by_identity: HashMap<String, Vec<(Sock, i32)>> = HashMap::new();
    let mut unattributed_map: HashMap<u32, Vec<Sock>> = HashMap::new();
    for (sock, pid) in attributed {
        match pid.and_then(|p| identity_of.get(&p).cloned().map(|id| (id, p))) {
            Some((id, p)) => by_identity.entry(id).or_default().push((sock, p)),
            None => unattributed_map.entry(sock.uid).or_default().push(sock)
        }
    }
    let unattributed_count: usize = unattributed_map.values().map(Vec::len).sum();

    let mut clusters: Vec<NetCluster> = base.iter().filter_map(|c| {
        if !matches.get(c.identity.as_str()).copied().unwrap_or(true) {
            return None;
        }
        let mut conns = by_identity.remove(&c.identity)?;
        conns.sort_by_key(|(s, _)| (!s.is_listening(), s.local_port, s.peer_port));
        let mut listen_ports: Vec<u16> = conns.iter()
            .filter(|(s, _)| s.is_listening()).map(|(s, _)| s.local_port).collect();
        listen_ports.sort_unstable();
        listen_ports.dedup();
        let mut world_ports: Vec<u16> = conns.iter()
            .filter(|(s, _)| s.is_world_exposed()).map(|(s, _)| s.local_port).collect();
        world_ports.sort_unstable();
        world_ports.dedup();
        Some(NetCluster {
            identity: c.identity.clone(), cpu: c.cpu, rss: c.rss,
            proc_count: c.members.len(), conns, listen_ports, world_ports
        })
    }).collect();
    clusters.sort_by(|a, b| {
        let (atx, arx) = a.traffic(&traffic);
        let (btx, brx) = b.traffic(&traffic);
        (btx + brx).cmp(&(atx + arx))
            .then(b.conns.len().cmp(&a.conns.len()))
            .then(b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal))
    });

    let unattributed = if pattern.is_some() {
        Vec::new()
    } else {
        let mut v: Vec<(u32, Vec<Sock>)> = unattributed_map.into_iter().collect();
        v.sort_by_key(|b| std::cmp::Reverse(b.1.len()));
        v
    };

    NetReport { clusters, unattributed, total_conns, total_listening, unattributed_count, listen_port_set, total_world, traffic }
}

/// The three outcomes of kill-by-port selection: attributed listeners ready to signal, a
/// listener whose owner could not be identified, or no listener at all (with any non-listening
/// local-port matches carried along for the diagnostic listing).
#[derive(Debug)]
pub enum PortCandidates {
    Listeners(Vec<(Sock, i32)>),
    Unattributable { uid: u32 },
    NoListener(Vec<Sock>)
}

/// Kill-by-port candidate selection, implementing the spec's "kill what is listening" rule:
/// only listening sockets (TCP LISTEN, bound UDP) on the local port are candidates, so a client
/// whose ephemeral local port happens to equal N is never swept in. Attribution is never guessed:
/// if ANY listener on the port could not be attributed to a pid, the whole port is a hard error,
/// because attribution is per-pid all-or-nothing, so an unattributed listener almost always
/// belongs to a different process entirely, one that would silently survive the kill.
pub fn port_kill_candidates(attributed: &[(Sock, Option<i32>)], port: u16) -> PortCandidates {
    let listeners: Vec<&(Sock, Option<i32>)> = attributed.iter()
        .filter(|(s, _)| s.local_port == port && s.is_listening()).collect();
    if listeners.is_empty() {
        return PortCandidates::NoListener(
            attributed.iter().filter(|(s, _)| s.local_port == port).map(|(s, _)| s.clone()).collect()
        );
    }
    if let Some((s, _)) = listeners.iter().find(|(_, pid)| pid.is_none()) {
        return PortCandidates::Unattributable { uid: s.uid };
    }
    let owned: Vec<(Sock, i32)> = listeners.iter()
        .filter_map(|(s, pid)| pid.map(|p| (s.clone(), p))).collect();
    PortCandidates::Listeners(owned)
}

#[cfg(test)]
mod tests;
