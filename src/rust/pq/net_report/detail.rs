use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use crate::pq::{net, proc};
use crate::style::{self, bold, dim, human_plain, paint};

use super::{DisplayNames, ESTAB_COLOR, IFACE_COLOR, LOCAL_COLOR, NetOpts, WORLD_COLOR, write_header};

pub(super) fn write_net_detail<W: Write>(out: &mut W, r: &net::NetReport, procs: &[proc::Proc], mem_used: u64, mem_total: u64, opts: &NetOpts) -> io::Result<()> {
    let colors = opts.colors;
    write_header(out, r, mem_used, mem_total, opts.port, colors)?;

    if r.clusters.is_empty() && r.unattributed.is_empty() {
        match opts.port {
            Some(port) => writeln!(out, "nothing on this host has local port {}", port)?,
            None => writeln!(out, "nothing matches")?
        }
        return Ok(());
    }

    if let Some(port) = opts.port {
        writeln!(out, "{}", port_sentence(r, port, colors))?;
        writeln!(out)?;
    }

    let by_pid: HashMap<i32, &proc::Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let width = opts.width.unwrap_or(76).clamp(44, 100);

    for c in r.clusters.iter().take(opts.top) {
        write_cluster_card(out, c, &by_pid, &r.listen_port_set, &r.traffic, width, colors)?;
    }

    for (uid, socks) in &r.unattributed {
        writeln!(out, "{}", dim(&format!("user {} (unattributed)", crate::pq::platform::users::username(*uid)), colors))?;
        for sock in socks {
            writeln!(out, "    {}", conn_line(sock, None, &r.listen_port_set, &r.traffic, &opts.names, colors))?;
        }
    }

    if r.unattributed_count > 0 {
        writeln!(out, "{}", dim(
            &format!("{} connection(s) not attributed to a process; run with sudo for full attribution",
                r.unattributed_count),
            colors))?;
    }

    if r.total_listening > 0 {
        let hint = match opts.port {
            Some(port) => format!("free the port: pq --port {} --kill    every connection: pq --net -v", port),
            None => {
                // Name the actual match when there is exactly one, so the hint is paste-ready.
                let name = if r.clusters.len() == 1 { r.clusters[0].identity.as_str() } else { "NAME" };
                format!("kill it: pq --net --kill {}    every connection: pq --net {} -v", name, name)
            }
        };
        writeln!(out, "{}", dim(&hint, colors))?;
    }
    Ok(())
}

/// The plain-English answer for a port query: who holds it and how exposed it is.
pub(super) fn port_sentence(r: &net::NetReport, port: u16, colors: bool) -> String {
    let mut holders: Vec<String> = r.clusters.iter().map(|c| c.identity.clone()).collect();
    for (uid, _) in &r.unattributed {
        holders.push(format!("an unidentified {} process", crate::pq::platform::users::username(*uid)));
    }
    let holders = holders.join(" and ");
    let est = r.total_conns - r.total_listening;
    let any_world = r.clusters.iter().any(|c| !c.world_ports.is_empty())
        || r.unattributed.iter().any(|(_, ss)| ss.iter().any(net::Sock::is_world_exposed));
    let exposure = if any_world {
        paint("open to the WORLD (bound to all interfaces)", WORLD_COLOR, false, colors)
    } else if r.total_listening > 0 {
        "bound to local/specific addresses only".to_string()
    } else {
        "no listeners, only established traffic".to_string()
    };
    format!(
        "port {} is held by {}: {} listening, {} established, {}",
        port, bold(&holders, colors), r.total_listening, est, exposure
    )
}

/// One boxed card: the cluster identity in the border, then per-pid blocks (bold meta line,
/// dimmed command line, socket rows with scope arrows and plain-language notes). Pid blocks are
/// capped so a 45-process browser does not produce a wall; the busiest pids (most sockets) win.
fn write_cluster_card<W: Write>(out: &mut W, c: &net::NetCluster, by_pid: &HashMap<i32, &proc::Proc>, listen_ports: &HashSet<u16>, traffic: &HashMap<u64, (u64, u64)>, width: usize, colors: bool) -> io::Result<()> {
    const MAX_PID_BLOCKS: usize = 4;
    let inner = width.saturating_sub(4); // "│ " and " │"

    // Group sockets per pid, busiest pid first.
    let mut per_pid: HashMap<i32, Vec<&net::Sock>> = HashMap::new();
    for (s, pid) in &c.conns {
        per_pid.entry(*pid).or_default().push(s);
    }
    let mut pids: Vec<(i32, Vec<&net::Sock>)> = per_pid.into_iter().collect();
    pids.sort_by_key(|(pid, socks)| (std::cmp::Reverse(socks.len()), *pid));

    // Content lines as (colored, visible-width) pairs; pad_visible closes the box later.
    let mut lines: Vec<(String, usize)> = Vec::new();
    for (i, (pid, socks)) in pids.iter().take(MAX_PID_BLOCKS).enumerate() {
        if i > 0 {
            lines.push((String::new(), 0));
        }
        let meta = match by_pid.get(pid) {
            Some(p) => format!("pid {}  user {}  {:.0}% cpu  {} mem",
                pid, crate::pq::platform::users::username(p.uid), p.cpu_pct, human_plain(p.rss)),
            None => format!("pid {}", pid)
        };
        lines.push((bold(&meta, colors), meta.chars().count()));
        if let Some(p) = by_pid.get(pid) {
            // A kernel thread or vanished process has no cmdline; fall back to comm.
            let cmd = if p.cmdline.is_empty() { format!("[{}]", p.comm) } else { p.cmdline.join(" ") };
            let cmd = style::truncate_middle(&cmd, inner);
            lines.push((dim(&cmd, colors), cmd.chars().count()));
        }
        socket_lines(socks, listen_ports, traffic, inner, colors, &mut lines);
    }
    if pids.len() > MAX_PID_BLOCKS {
        let hidden: usize = pids[MAX_PID_BLOCKS..].iter().map(|(_, s)| s.len()).sum();
        lines.push((String::new(), 0));
        let note = format!("+ {} more procs holding {} socket(s); -v lists every connection",
            pids.len() - MAX_PID_BLOCKS, hidden);
        lines.push((dim(&note, colors), note.chars().count()));
    }

    // The box itself.
    let title = format!(" {} ", c.identity);
    let fill = width.saturating_sub(3 + title.chars().count());
    writeln!(out, "{}{}{}",
        dim("\u{250c}\u{2500}", colors),
        bold(&title, colors),
        dim(&format!("{}\u{2510}", "\u{2500}".repeat(fill)), colors))?;
    for (colored, visible) in lines {
        writeln!(out, "{} {} {}",
            dim("\u{2502}", colors),
            style::pad_visible(&colored, visible, inner),
            dim("\u{2502}", colors))?;
    }
    writeln!(out, "{}", dim(&format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(width.saturating_sub(2))), colors))
}

/// Socket rows for one pid block: aligned scope/direction arrows into the endpoint, with a
/// plain-language reachability note (plus traffic counters for established connections).
/// Listeners first (the input is already sorted that way).
fn socket_lines(socks: &[&net::Sock], listen_ports: &HashSet<u16>, traffic: &HashMap<u64, (u64, u64)>, inner: usize, colors: bool, lines: &mut Vec<(String, usize)>) {
    let mut rows: Vec<(String, i32, String)> = Vec::new();
    for s in socks {
        let proto = match s.proto { net::Proto::Tcp => "tcp", net::Proto::Udp => "udp" };
        if s.is_listening() {
            let (tag, color, arrow, note) = listener_scope(s);
            rows.push((format!("{:<5} {} {} {}", tag, arrow, proto, s.local_str()), color, note.to_string()));
        } else {
            let dirn = s.direction(listen_ports);
            let note = match traffic.get(&s.inode) {
                Some(&(tx, rx)) if tx + rx > 0 =>
                    format!("{} tx / {} rx", human_plain(tx), human_plain(rx)),
                _ => "established".to_string()
            };
            // The peer is the story; an outbound socket's ephemeral local address is noise.
            // Inbound keeps the local port so you can see which listener accepted it.
            let lead = if dirn == "in" {
                format!("{:<5} \u{25c0}\u{2500}\u{2500} {} {} -> :{}", dirn, proto, s.peer_str(), s.local_port)
            } else {
                format!("{:<5} \u{2500}\u{2500}\u{25b6} {} {}", dirn, proto, s.peer_str())
            };
            rows.push((lead, ESTAB_COLOR, note));
        }
    }
    let lead_w = rows.iter().map(|(l, _, _)| l.chars().count()).max().unwrap_or(0);
    for (lead, color, note) in rows {
        let with_note = lead_w + 3 + note.len() <= inner;
        let visible = if with_note { lead_w + 3 + note.len() } else { lead.chars().count().min(inner) };
        let padded = format!("{:<w$}", lead, w = if with_note { lead_w } else { 0 });
        let colored = if with_note {
            format!("{}   {}", paint(&padded, color, false, colors), dim(&note, colors))
        } else {
            paint(&padded, color, false, colors)
        };
        lines.push((colored, visible));
    }
}

/// Reachability scope of a listener's bind address: (tag, SGR color, arrow, note).
pub(super) fn listener_scope(s: &net::Sock) -> (&'static str, i32, &'static str, &'static str) {
    if s.local.is_unspecified() {
        ("WORLD", WORLD_COLOR, "\u{2550}\u{2550}\u{25b6}", "reachable from anywhere")
    } else if s.local.is_loopback() {
        ("local", LOCAL_COLOR, "\u{2500}\u{2500}\u{25b6}", "this machine only")
    } else {
        ("iface", IFACE_COLOR, "\u{2500}\u{2500}\u{25b6}", "only via this specific address")
    }
}

/// One per-connection line for -v and unattributed listings: direction tag (listen/in/out),
/// owning pid (when known), the socket label, and a [world] marker on listeners bound to all
/// interfaces. The marker is textual so piped output keeps it; it turns red when colors are on.
pub(super) fn conn_line(sock: &net::Sock, pid: Option<i32>, listen_ports: &HashSet<u16>, traffic: &HashMap<u64, (u64, u64)>, names: &DisplayNames, colors: bool) -> String {
    let pid_part = pid.map(|p| format!("pid {}  ", p)).unwrap_or_default();
    let mut text = format!("{:<6}  {}{}", sock.direction(listen_ports), pid_part, sock.label());
    let relevant_port = if sock.is_listening() { sock.local_port } else { sock.peer_port };
    if let Some(service) = names.services.get(&relevant_port) {
        text.push_str(&format!("  [{}]", service));
    }
    if let Some(host) = names.hosts.get(&sock.peer) {
        text.push_str(&format!("  [{}]", host));
    }
    if let Some(&(tx, rx)) = traffic.get(&sock.inode) {
        if tx + rx > 0 {
            text.push_str(&format!("  tx {} rx {}", human_plain(tx), human_plain(rx)));
        }
    }
    let mut line = dim(&text, colors);
    if sock.is_world_exposed() {
        line.push_str("  ");
        line.push_str(&paint("[world]", WORLD_COLOR, false, colors));
    }
    line
}

/// Compact outbound summary: the top peer ports of connections dialed out, e.g.
/// ":443 x28 :53 x2 (+3)". Inbound established connections are excluded: they are already
/// represented by the listening port. Empty when nothing dialed.
pub(super) fn outbound_summary<'a>(socks: impl Iterator<Item = &'a net::Sock>, listen_ports: &HashSet<u16>, names: &DisplayNames) -> String {
    let mut counts: HashMap<u16, usize> = HashMap::new();
    for s in socks {
        if s.direction(listen_ports) == "out" {
            *counts.entry(s.peer_port).or_insert(0) += 1;
        }
    }
    if counts.is_empty() {
        return String::new();
    }
    let mut peers: Vec<(u16, usize)> = counts.into_iter().collect();
    peers.sort_by_key(|&(port, n)| (std::cmp::Reverse(n), port));
    let shown = peers.iter().take(3)
        .map(|(port, n)| {
            let port = names.services.get(port).map_or_else(|| format!(":{}", port), |name| format!(":{}({})", port, name));
            if *n > 1 { format!("{} x{}", port, n) } else { port }
        })
        .collect::<Vec<_>>().join(" ");
    let extra = peers.len().saturating_sub(3);
    if extra > 0 {
        format!("{} (+{})", shown, extra)
    } else {
        shown
    }
}

/// Paint occurrences of `:port` in `label` that end at a digit boundary, so painting ":53"
/// never recolors the front of ":5432".
pub(super) fn paint_port(label: &str, port: u16, color: i32) -> String {
    let token = format!(":{}", port);
    let mut out = String::with_capacity(label.len() + 16);
    let mut rest = label;
    while let Some(idx) = rest.find(&token) {
        let end = idx + token.len();
        let boundary = rest[end..].chars().next().is_none_or(|c| !c.is_ascii_digit());
        out.push_str(&rest[..idx]);
        if boundary {
            out.push_str(&paint(&token, color, false, true));
        } else {
            out.push_str(&token);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}
