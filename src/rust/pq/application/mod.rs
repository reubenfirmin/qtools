use crate::render::{self, RenderContext};

use super::{cluster, proc, report, selection, tree_report};
use super::pq_query::{Action, QueryPlan, View};

mod commands;

use commands::{run_diagnostic_report, run_io_report, run_kill, run_net_kill, run_net_report};

pub(super) fn execute(plan: &QueryPlan) -> Result<(), String> {
    match (&plan.action, plan.view) {
        (Action::Kill(options), _) if plan.sockets.requested() => return run_net_kill(plan, options),
        (Action::Kill(options), _) => return run_kill(options.pattern.as_deref().unwrap(), plan, options),
        (Action::Report, View::Network) => return run_net_report(plan),
        (Action::Report, View::Io) => return run_io_report(plan),
        (Action::Report, View::Deleted | View::Fds | View::Holders | View::Blocked | View::Pss | View::Oom | View::Limits | View::Unix) => {
            return run_diagnostic_report(plan);
        }
        (Action::Report, View::Processes) => {}
    }

    let metric = plan.metric;

    // Always take the two-sample CPU delta (even in --memory mode) so both the cpu and memory
    // columns are real numbers, not a fake 0. The interval is small; --kill exits before here.
    // The per-core /proc/stat samples ride along at the same two points, for --cpu's per-core rings.
    let prev = proc::snapshot();
    let stat0 = proc::cpu_stat();
    let sample_started = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(plan.interval_ms));
    let cur = proc::snapshot();
    let stat1 = proc::cpu_stat();
    let procs = proc::cpu_percent(&prev, &cur, sample_started.elapsed().as_secs_f64());
    let core_loads = proc::per_core_busy(&stat0, &stat1);
    // Which process(es) actually ran on each core, for the --cpu donut's process-colored rings.
    let attribution = proc::per_core_attribution(&procs);

    let all_clusters = cluster::cluster(&procs, metric);
    // System-wide, computed before any pattern filter: the header's cpu% is a system total, matching
    // the mem/swap totals beside it (which are always system-wide), so a filtered report never shows
    // a small filtered cpu% next to unfiltered mem/swap.
    let cpu_total: f64 = all_clusters.iter().map(|c| c.cpu).sum();
    let clusters = selection::clusters(&procs, plan);

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
    Ok(())
}
