use std::collections::HashMap;
use std::io::{self, Write};
use std::time::Duration;

use crate::render::{self, RenderContext};
use crate::style;

use super::super::{
    blocked_report, fd_report, io_report, kill, limits_report, maps_report, net, net_report, netdiag,
    oom_report, platform, proc, pss_report, ranked_report, selection, unix_report,
};
use super::super::pq_query::{KillOptions, QueryPlan, View};

fn sample_sockets(procs: &[proc::Proc]) -> (Vec<(net::Sock, Option<i32>)>, usize) {
    let (socks, tables_read) = net::read_sockets();
    let pids: Vec<i32> = procs.iter().map(|p| p.pid).collect();
    let inodes = net::inode_map(&pids);
    (net::attribute(socks, &inodes), tables_read)
}

fn sample_processes(interval_ms: u64) -> Vec<proc::Proc> {
    let previous = proc::snapshot();
    let started = std::time::Instant::now();
    std::thread::sleep(Duration::from_millis(interval_ms));
    let current = proc::snapshot();
    proc::cpu_percent(&previous, &current, started.elapsed().as_secs_f64())
}

/// Apply the complete socket/process selector intersection. Reports and actions share this path;
/// kill-by-port adds its stricter listener/attribution safety check after selection.
fn select_sockets(attributed: Vec<(net::Sock, Option<i32>)>, plan: &QueryPlan) -> Vec<(net::Sock, Option<i32>)> {
    let filtered = attributed.into_iter().filter(|(sock, pid)| {
        plan.process.pid.is_none_or(|wanted| *pid == Some(wanted))
            && plan.process.user.as_deref().is_none_or(|wanted| selection::user_matches(sock.uid, wanted))
    }).collect();
    net::filter_socks_extended(filtered, plan.sockets.remote_port, plan.sockets.protocol,
        plan.sockets.state, plan.sockets.public_only)
}

/// `--net` report: sample processes (same two-sample cpu delta as the normal report, so the cpu
/// column is real), read and attribute sockets, filter, cluster, render.
pub(super) fn run_net_report(plan: &QueryPlan) -> Result<(), String> {
    let procs = sample_processes(plan.interval_ms);

    let (attributed, tables_read) = sample_sockets(&procs);
    if tables_read == 0 {
        return Err("cannot read /proc/net (is this Linux?)".into());
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
    Ok(())
}

/// Process-I/O report. Process selectors establish the base set; when socket selectors are
/// present their attributed owner PIDs are intersected with it instead of forcing a net view.
pub(super) fn run_io_report(plan: &QueryPlan) -> Result<(), String> {
    use std::collections::HashSet;

    let before_procs = proc::snapshot();
    let before_io = io_report::snapshot(before_procs.iter().map(|p| p.pid));
    let sample_started = std::time::Instant::now();
    std::thread::sleep(Duration::from_millis(plan.interval_ms));
    let current = proc::snapshot();
    let after_io = io_report::snapshot(current.iter().map(|p| p.pid));
    let elapsed = sample_started.elapsed().as_secs_f64();
    let procs = proc::cpu_percent(&before_procs, &current, elapsed);

    let base = selection::clusters(&procs, plan);
    let mut allowed: HashSet<i32> = base.iter().flat_map(|c| c.members.iter().map(|m| m.pid)).collect();

    if plan.sockets.selects_owners() {
        let (attributed, tables_read) = sample_sockets(&procs);
        if tables_read == 0 {
            return Err("cannot read /proc/net for socket selection (is this Linux?)".into());
        }
        let sockets = net::filter_socks(select_sockets(attributed, plan), plan.sockets.local_port, plan.sockets.listen);
        let owners: HashSet<i32> = sockets.into_iter().filter_map(|(_, pid)| pid).collect();
        allowed.retain(|pid| owners.contains(pid));
    }

    let mut clusters = io_report::aggregate(&procs, &allowed, &before_io, &after_io,
        elapsed, plan.io_metric);
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
        width: context.width,
    };
    render::stdout("pq", |out| io_report::render(out, &clusters, &opts, context));
    Ok(())
}

fn selected_process_pids(procs: &[proc::Proc], plan: &QueryPlan) -> std::collections::HashSet<i32> {
    selection::clusters(procs, plan).iter()
        .flat_map(|group| group.members.iter().map(|member| member.pid)).collect()
}

fn intersect_socket_owners(procs: &[proc::Proc], plan: &QueryPlan, allowed: &mut std::collections::HashSet<i32>) -> Result<(), String> {
    if !plan.sockets.selects_owners() { return Ok(()); }
    let (attributed, tables_read) = sample_sockets(procs);
    if tables_read == 0 {
        return Err("cannot read /proc/net for socket selection (is this Linux?)".into());
    }
    let sockets = net::filter_socks(select_sockets(attributed, plan), plan.sockets.local_port, plan.sockets.listen);
    let owners: std::collections::HashSet<i32> = sockets.into_iter().filter_map(|(_, pid)| pid).collect();
    allowed.retain(|pid| owners.contains(pid));
    Ok(())
}

pub(super) fn run_diagnostic_report(plan: &QueryPlan) -> Result<(), String> {
    let procs = if plan.process.min_cpu.is_some() { sample_processes(plan.interval_ms) }
        else { proc::cpu_percent(&[], &proc::snapshot(), 0.0) };
    let mut allowed = selected_process_pids(&procs, plan);
    intersect_socket_owners(&procs, plan, &mut allowed)?;

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

    let access_sample = if plan.view == View::Access {
        let sampled = maps_report::begin_access_sample(&allowed);
        if !sampled.is_empty() { std::thread::sleep(Duration::from_millis(plan.interval_ms)); }
        Some(sampled)
    } else { None };

    let report = match plan.view {
        View::Deleted => fd_report::deleted_report(files.as_ref().unwrap(), &allowed),
        View::Fds => fd_report::fds_report(files.as_ref().unwrap(), &allowed),
        View::Holders => fd_report::holders_report(files.as_ref().unwrap(), &allowed,
            normalized_file.as_deref(), normalized_mount.as_deref()),
        View::Blocked => blocked_report::build(&procs, &allowed),
        View::Pss => pss_report::build(&procs, &allowed),
        View::Maps => maps_report::maps(&procs, &allowed),
        View::Access => maps_report::access(&procs, access_sample.as_ref().unwrap()),
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
    Ok(())
}

/// Net-mode `--kill`: with --port, only an attributed LISTENING socket on that port selects (the
/// spec's "kill what is listening" rule, with diagnostics for the no-listener and cannot-attribute
/// cases); otherwise the selection is every attributed socket passing --listen, narrowed by the
/// pattern if one was given.
pub(super) fn run_net_kill(plan: &QueryPlan, options: &KillOptions) -> Result<(), String> {
    let self_pid = std::process::id() as i32;
    let procs = sample_processes(plan.interval_ms);

    let (attributed, tables_read) = sample_sockets(&procs);
    if tables_read == 0 {
        return Err("cannot read /proc/net (is this Linux?)".into());
    }

    let attributed = select_sockets(attributed, plan);

    let owners: Vec<(net::Sock, i32)> = match plan.sockets.local_port {
        Some(port) => match net::port_kill_candidates(&attributed, port) {
            net::PortCandidates::Listeners(owners) => owners,
            net::PortCandidates::Unattributable { uid } => {
                return Err(format!(
                    "port {} is held by uid {} ({}) but the process cannot be identified; re-run with sudo",
                    port, uid, crate::pq::platform::users::username(uid)
                ));
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
                return Ok(());
            }
        },
        None => net::filter_socks(attributed, None, plan.sockets.listen).into_iter()
            .filter_map(|(s, pid)| pid.map(|p| (s, p)))
            .collect()
    };

    let targets = kill::select_owning(&procs, &owners, options.pattern.as_deref(), options.exact, self_pid);
    if targets.is_empty() {
        println!("pq: no matching socket owners");
        return Ok(());
    }
    let headline = match plan.sockets.local_port {
        Some(port) => format!("own port {}", port),
        None => "own matching sockets".to_string()
    };
    preview_and_kill(&targets, &procs, options, &headline)
}

/// `--kill <pattern>`: sample, select the matching subtree(s), preview, confirm, then escalate
/// SIGTERM -> (wait `--grace`) -> SIGKILL. Never signals pq itself, its ancestor chain, or pid 1
/// (enforced by `kill::select`).
pub(super) fn run_kill(pattern: &str, plan: &QueryPlan, options: &KillOptions) -> Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("--kill needs a non-empty pattern".into());
    }

    let self_pid = std::process::id() as i32;

    // A real two-sample delta so the preview's cpu% column is meaningful, same cadence as the
    // normal report.
    let procs = sample_processes(plan.interval_ms);

    let targets = kill::select(&procs, pattern, options.exact, self_pid);

    if targets.is_empty() {
        println!("pq: no processes match \"{}\"", pattern);
        return Ok(());
    }
    preview_and_kill(&targets, &procs, options, &format!("match \"{}\"", pattern))
}

/// Preview the targets (with a socket column when net mode supplied one), confirm unless -y,
/// then escalate. Shared by pattern kill and net kill.
fn preview_and_kill(targets: &[kill::Target], procs: &[proc::Proc], options: &KillOptions, headline: &str) -> Result<(), String> {
    let by_pid: HashMap<i32, &proc::Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let (sig, grace) = if options.sigkill {
        (libc::SIGKILL, 0)
    } else {
        let sig = kill::parse_signal(&options.signal)?;
        (sig, options.grace)
    };

    let has_sock = targets.iter().any(|t| t.sock.is_some());

    println!("pq: {} process(es) {}:", targets.len(), headline);
    for t in targets {
        let (cpu, rss) = by_pid.get(&t.pid).map(|p| (p.cpu_pct, p.rss)).unwrap_or((0.0, 0));
        if has_sock {
            let sock = t.sock.as_deref().unwrap_or("");
            println!(
                "  {:>7} {:<10} {:>5.1}%  {:>8}  {:<14} {:<32} {}",
                t.pid, crate::pq::platform::users::username(t.uid), cpu, style::human_plain(rss), t.identity, sock, t.cmd
            );
        } else {
            println!(
                "  {:>7} {:<10} {:>5.1}%  {:>8}  {:<14} {}",
                t.pid, crate::pq::platform::users::username(t.uid), cpu, style::human_plain(rss), t.identity, t.cmd
            );
        }
    }

    if options.dry_run {
        return Ok(());
    }
    if !options.yes && !confirm() {
        eprintln!("pq: aborted");
        return Ok(());
    }
    let sender = platform::linux::signals::RealSender::new(targets);
    let result = kill::escalate(&sender, targets, sig, grace, &|d| std::thread::sleep(d));
    println!(
        "pq: terminated {}, killed {} via SIGKILL, {} not permitted",
        result.terminated.len(), result.killed.len(), result.failed.len()
    );
    if !result.failed.is_empty() {
        return Err(format!("could not signal {} process(es)", result.failed.len()));
    }
    Ok(())
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
