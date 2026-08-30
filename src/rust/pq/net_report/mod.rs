//! Text + JSON reporting for pq's --net mode.
//!
//! Three human views, one JSON view:
//! * The overview grid (`write_text`/`write_graphics`): one row per identity cluster with its
//!   listening ports and outbound peer-port summary in aligned columns. No cpu/mem here, that
//!   is the default report's job; net mode answers "who serves what, who talks to what".
//! * The detail cards (`write_net_detail`): for targeted queries (`--port N`, or a PATTERN
//!   without -v), a plain-English answer plus one boxed card per owning cluster showing each
//!   process (user, command line) and its sockets with reachability arrows: WORLD (red,
//!   bound to all interfaces), local (loopback), iface (a specific address).
//! * `-v` expands the grid to raw per-connection rows.
//! * JSON always includes everything (machines make their own UIs).

use std::io::{self, Write};

use crate::style::{self, bold, dim, human_plain, paint};
use crate::{chart, donut, graphics};
use crate::render::{RenderContext, RenderTarget};

use crate::pq::net;
use crate::pq::proc;
pub(crate) use crate::pq::presentation::network::{display_names, write_json, DisplayNames};

use chart::{DONUT_PX, MIN_LABEL_WIDTH, RULE_WIDTH};

mod detail;

use detail::{conn_line, outbound_summary, paint_port, write_net_detail};

#[cfg(test)]
use detail::{listener_scope, port_sentence};

#[cfg(test)]
mod tests;

/// Rendering knobs, the net-mode analogue of report::ReportOpts (no metric: net mode always
/// sorts by connection count). `port` is the --port filter, echoed in the header and used by
/// the detail view's phrasing and hints.
pub struct NetOpts {
    pub top: usize,
    pub verbose: bool,
    pub colors: bool,
    pub width: Option<usize>,
    pub port: Option<u16>,
    pub compact_json: bool,
    pub sample_interval_ms: u64,
    pub names: DisplayNames
}

#[allow(clippy::too_many_arguments)]
pub fn render<W: Write>(out: &mut W, report: &net::NetReport, procs: &[proc::Proc], mem_used: u64,
    mem_total: u64, opts: &NetOpts, context: RenderContext, detail: bool) -> io::Result<()> {
    match context.target {
        RenderTarget::Json | RenderTarget::JsonLines => write_json(out, report, mem_used, mem_total, opts),
        RenderTarget::Graphics => if detail {
            write_net_detail(out, report, procs, mem_used, mem_total, opts)
        } else {
            write_graphics(out, report, mem_used, mem_total, opts)
        },
        RenderTarget::Text => if detail {
            write_net_detail(out, report, procs, mem_used, mem_total, opts)
        } else {
            write_text(out, report, mem_used, mem_total, opts)
        },
    }
}


const CONNS_WIDTH: usize = 5;
const TX_WIDTH: usize = 8;
const RX_WIDTH: usize = 8;
const PROC_WIDTH: usize = 26;
const LISTEN_WIDTH: usize = 26;
// Fixed part of a grid row before the talking column: conns, tx, rx, process, listening, each
// followed by a 2-space gutter.
const NET_ROW_PREFIX: usize = CONNS_WIDTH + 2 + TX_WIDTH + 2 + RX_WIDTH + 2 + PROC_WIDTH + 2 + LISTEN_WIDTH + 2;
/// SGR color codes (style::paint takes SGR codes, not 256-color indexes).
pub(super) const WORLD_COLOR: i32 = 91; // bright red: world-exposed listeners deserve the attention
pub(super) const LOCAL_COLOR: i32 = 32; // green: loopback-only, nothing to worry about
pub(super) const IFACE_COLOR: i32 = 33; // yellow: bound to one specific address
pub(super) const ESTAB_COLOR: i32 = 36; // cyan: established traffic lines in the detail cards

/// Header line: `pq | net [| port N] | N conns | N listening [| N world] | mem used / total`,
/// then a rule. The world segment (listeners bound to all interfaces) only appears when there
/// are any; the port segment echoes a --port filter.
pub(super) fn write_header<W: Write>(out: &mut W, r: &net::NetReport, mem_used: u64, mem_total: u64, port: Option<u16>, colors: bool) -> io::Result<()> {
    let sep = dim("\u{2502}", colors);
    write!(out, "{}  {}  net", bold("pq", colors), sep)?;
    if let Some(p) = port {
        write!(out, "  {}  port {}", sep, p)?;
    }
    write!(out, "  {}  {} conns  {}  {} listening", sep, r.total_conns, sep, r.total_listening)?;
    if r.total_world > 0 {
        write!(out, "  {}  {}", sep, paint(&format!("{} world", r.total_world), WORLD_COLOR, false, colors))?;
    }
    writeln!(out, "  {}  mem {} / {}", sep, human_plain(mem_used), human_plain(mem_total))?;
    writeln!(out, "{}", dim(&"-".repeat(RULE_WIDTH), colors))
}

/// Human overview: header, then the grid, `-v` expanding per-connection rows, then per-uid rows
/// for unattributed sockets, the sudo hint when attribution was incomplete, and a usage footer.
pub fn write_text<W: Write>(out: &mut W, r: &net::NetReport, mem_used: u64, mem_total: u64, opts: &NetOpts) -> io::Result<()> {
    write_header(out, r, mem_used, mem_total, opts.port, opts.colors)?;
    write_body(out, r, opts, false)
}

/// The overview grid beneath the header/rule; shared with write_graphics, which passes
/// `swatches: true` after drawing the donut so every row carries the color swatch of its slice
/// (stable identity color for the top clusters, gray for anything folded into the "other" arc:
/// rank-13+ clusters and unattributed sockets).
fn write_body<W: Write>(out: &mut W, r: &net::NetReport, opts: &NetOpts, swatches: bool) -> io::Result<()> {
    let colors = opts.colors;
    // A swatch occupies 2 cells plus the 2-space gutter that follows it.
    let indent = if swatches { "    " } else { "" };
    let row_prefix = NET_ROW_PREFIX + indent.len();
    let heading = format!("{}{:>cw$}  {:>tw$}  {:>rw$}  {:<pw$}  {:<lw$}  talking to (peer port x conns)",
        indent, "conns", "tx", "rx", "process", "listening on",
        cw = CONNS_WIDTH, tw = TX_WIDTH, rw = RX_WIDTH, pw = PROC_WIDTH, lw = LISTEN_WIDTH);
    writeln!(out, "{}", dim(&heading, colors))?;

    let top: Vec<&net::NetCluster> = r.clusters.iter().take(opts.top).collect();
    let row_colors = chart::identity_colors(top.iter().map(|cluster| cluster.identity.as_str()));

    for (rank, c) in top.iter().enumerate() {
        if swatches {
            // Same stable identity color the donut gave this cluster; ranks beyond the palette
            // are folded into the gray "other" arc.
            write!(out, "{}  ", style::color_swatch(row_colors[rank], colors))?;
        }
        let mut name = c.identity.clone();
        if c.proc_count > 1 {
            name.push_str(&format!(" ({})", c.proc_count));
        }
        let (tx, rx) = c.traffic(&r.traffic);
        write_grid_row(
            out,
            c.conns.len(),
            (tx, rx),
            &name,
            &c.listen_ports,
            &c.world_ports,
            &outbound_summary(c.conns.iter().map(|(s, _)| s), &r.listen_port_set, &opts.names),
            row_prefix,
            opts
        )?;
        if opts.verbose {
            for (sock, pid) in &c.conns {
                writeln!(out, "{}{:>cw$}  {}", indent, "",
                    conn_line(sock, Some(*pid), &r.listen_port_set, &r.traffic, &opts.names, colors), cw = CONNS_WIDTH)?;
            }
        }
    }

    // The cutoff: clusters beyond -n exist in the totals (and in the donut's gray arc) but have
    // no row of their own; say so instead of letting them silently disappear.
    if r.clusters.len() > top.len() {
        let hidden = &r.clusters[top.len()..];
        let hidden_conns: usize = hidden.iter().map(|c| c.conns.len()).sum();
        // Hidden LISTENERS are called out by port: a serving process must never vanish silently.
        let (hidden_listen, _) = listening_ports(hidden.iter().flat_map(|c| c.conns.iter().map(|(s, _)| s)));
        if swatches {
            write!(out, "{}  ", style::color_swatch(style::OTHER_COLOR, colors))?;
        }
        let msg = if hidden_listen.is_empty() {
            format!("+ {} more cluster(s), {} conns (raise -n to see them)", hidden.len(), hidden_conns)
        } else {
            format!("+ {} more cluster(s), {} conns, still LISTENING on {} (raise -n, or --listen for every listener)",
                hidden.len(), hidden_conns, listen_cell_named(&hidden_listen, 40, &opts.names))
        };
        writeln!(out, "{}", dim(&msg, colors))?;
    }

    for (uid, socks) in &r.unattributed {
        if swatches {
            write!(out, "{}  ", style::color_swatch(style::OTHER_COLOR, colors))?;
        }
        let (ports, world) = listening_ports(socks.iter());
        let traffic = socks.iter().fold((0u64, 0u64), |(tx, rx), s| {
            let (t, rr) = r.traffic.get(&s.inode).copied().unwrap_or((0, 0));
            (tx + t, rx + rr)
        });
        // Truncate the username, never the marker: "(unattributed)" must survive whole.
        let user = style::truncate_middle(&crate::pq::platform::users::username(*uid), PROC_WIDTH.saturating_sub(15));
        write_grid_row(
            out,
            socks.len(),
            traffic,
            &format!("{} (unattributed)", user),
            &ports,
            &world,
            &outbound_summary(socks.iter(), &r.listen_port_set, &opts.names),
            row_prefix,
            opts
        )?;
        if opts.verbose {
            for sock in socks {
                writeln!(out, "{}{:>cw$}  {}", indent, "",
                    conn_line(sock, None, &r.listen_port_set, &r.traffic, &opts.names, colors), cw = CONNS_WIDTH)?;
            }
        }
    }

    if r.unattributed_count > 0 {
        writeln!(out, "{}", dim(
            &format!("{} connection(s) not attributed to a process; run with sudo for full attribution",
                r.unattributed_count),
            colors))?;
    }

    writeln!(out, "{}", dim(
        "try: pq --net NAME (deep dive)    pq --net --listen (every listener)    pq --port N [--kill] (inspect / free a port)",
        colors))
}

/// One grid row: conns count, tx/rx bytes (blank when zero, so listeners stay quiet), process
/// name, listening ports (world ones painted red), outbound summary. Cells are padded plain and
/// painted afterward so column math never sees escape bytes.
#[allow(clippy::too_many_arguments)]
fn write_grid_row<W: Write>(out: &mut W, conns: usize, (tx, rx): (u64, u64), name: &str, listen_ports: &[u16], world_ports: &[u16], talking: &str, row_prefix: usize, opts: &NetOpts) -> io::Result<()> {
    let colors = opts.colors;
    let name_cell = format!("{:<w$}", style::truncate_middle(name, PROC_WIDTH), w = PROC_WIDTH);
    let listen_plain = format!("{:<w$}", listen_cell_named(listen_ports, LISTEN_WIDTH, &opts.names), w = LISTEN_WIDTH);
    let listen_colored = if colors {
        world_ports.iter().fold(listen_plain, |l, p| paint_port(&l, *p, WORLD_COLOR))
    } else {
        listen_plain
    };
    let talking = match opts.width {
        Some(w) => style::truncate_middle(talking, w.saturating_sub(row_prefix).max(MIN_LABEL_WIDTH)),
        None => talking.to_string()
    };
    writeln!(out, "{:>cw$}  {:>tw$}  {:>rw$}  {}  {}  {}",
        conns, bytes_cell(tx), bytes_cell(rx), name_cell, listen_colored, talking,
        cw = CONNS_WIDTH, tw = TX_WIDTH, rw = RX_WIDTH)
}

/// Human bytes, or blank for zero: a listener that never spoke should not shout "0".
fn bytes_cell(v: u64) -> String {
    if v == 0 { String::new() } else { human_plain(v) }
}

/// The listening-ports cell: as many `:port` tokens as fit the column, then a `(+n)` overflow
/// marker so nothing silently disappears.
#[cfg(test)]
fn listen_cell(ports: &[u16], width: usize) -> String {
    listen_cell_named(ports, width, &DisplayNames::default())
}

fn listen_cell_named(ports: &[u16], width: usize, names: &DisplayNames) -> String {
    let mut out = String::new();
    for (i, p) in ports.iter().enumerate() {
        let label = names.services.get(p).map_or_else(|| p.to_string(), |name| format!("{}({})", p, name));
        let tok = format!("{}:{}", if i > 0 { " " } else { "" }, label);
        let remaining = ports.len() - i;
        // Reserve room for the overflow marker unless everything left still fits.
        let reserve = if remaining > 1 { 6 } else { 0 };
        if !out.is_empty() && out.len() + tok.len() + reserve > width {
            out.push_str(&format!(" (+{})", remaining));
            break;
        }
        out.push_str(&tok);
    }
    out
}

/// Listening ports (sorted, deduped) and the world-exposed subset for a plain socket list,
/// mirroring what NetCluster precomputes for clusters.
fn listening_ports<'a>(socks: impl Iterator<Item = &'a net::Sock>) -> (Vec<u16>, Vec<u16>) {
    let mut ports = Vec::new();
    let mut world = Vec::new();
    for s in socks {
        if s.is_listening() {
            ports.push(s.local_port);
            if s.is_world_exposed() {
                world.push(s.local_port);
            }
        }
    }
    ports.sort_unstable();
    ports.dedup();
    world.sort_unstable();
    world.dedup();
    (ports, world)
}

// -------------------------------------------------------------------------------------------------
// Detail cards: the answer to a targeted question (--port N, or a PATTERN without -v).
// -------------------------------------------------------------------------------------------------

/// Detail view: a plain-English answer line, then one boxed card per owning cluster (each
/// process with its user and command line, then its sockets with reachability arrows), then
/// unattributed sockets, the sudo hint, and an actionable kill hint.
/// sums to `total`. No "unused" arc: unlike cpu/mem there is no capacity concept.
fn conn_segments(r: &net::NetReport, top: usize, count: impl Fn(&net::NetCluster) -> u64, total: u64) -> Vec<chart::Segment> {
    let items: Vec<(String, u64)> = r.clusters.iter().map(|cluster| (cluster.identity.clone(), count(cluster))).collect();
    let mut segments = chart::identity_segments(&items, total, top);
    // Network legends always carry an "other" row because it also represents unattributed
    // sockets, including the useful all-attributed/zero case.
    if segments.last().is_none_or(|segment| segment.label != "other") {
        segments.push(chart::Segment { label: "other".to_string(), value: 0, color: style::OTHER_COLOR });
    }
    segments
}

/// Draw the connections donut, then the grid beneath it as the legend (each row carries its
/// slice's color swatch). When the socket set has both listeners and established connections, a
/// two-ring donut separates the stories: outer ring = established (who talks), inner ring =
/// listeners (who serves), same cluster color on both, labeled beneath the image. Otherwise a
/// single ring of total connections says it all. Falls back to the plain grid (never re-writing
/// the header) when there is nothing to chart or the image fails to render.
pub fn write_graphics<W: Write>(out: &mut W, r: &net::NetReport, mem_used: u64, mem_total: u64, opts: &NetOpts) -> io::Result<()> {
    write_graphics_with(out, r, mem_used, mem_total, opts, |image, config| {
        match viuer::print(image, config) {
            Ok(_) => true,
            Err(error) => { graphics::debug_print_failure(&error); false }
        }
    })
}

fn write_graphics_with<W: Write>(out: &mut W, r: &net::NetReport, mem_used: u64, mem_total: u64,
    opts: &NetOpts, mut draw: impl FnMut(&image::DynamicImage, &viuer::Config) -> bool) -> io::Result<()> {
    write_header(out, r, mem_used, mem_total, opts.port, opts.colors)?;
    if r.total_conns == 0 {
        return write_body(out, r, opts, false);
    }
    let cols = opts.width.unwrap_or(80).clamp(16, 40) as u32;
    let config = viuer::Config {
        absolute_offset: false,
        width: Some(cols),
        height: Some((cols / 2).max(8)),
        ..Default::default()
    };
    let total_lsn = r.total_listening as u64;
    let total_est = (r.total_conns - r.total_listening) as u64;
    let two_ring = total_est > 0 && total_lsn > 0;
    // Flush before viuer::print: it writes straight to the terminal, bypassing out's buffer.
    out.flush()?;
    let image = if two_ring {
        let outer = conn_segments(r, opts.top, |c| c.established() as u64, total_est);
        let inner = conn_segments(r, opts.top, |c| c.listeners() as u64, total_lsn);
        donut::build_two_ring_image(&chart::slices(&outer), &chart::slices(&inner), DONUT_PX)
    } else {
        let segs = conn_segments(r, opts.top, |c| c.conns.len() as u64, r.total_conns as u64);
        donut::build_donut_image(&chart::slices(&segs), DONUT_PX)
    };
    let drew = draw(&image, &config);
    if drew && two_ring {
        writeln!(out, "{}", dim("outer ring: established (talking)    inner ring: listening (serving)", opts.colors))?;
    }
    // Swatches only make sense beneath a donut that actually rendered.
    write_body(out, r, opts, drew)
}
