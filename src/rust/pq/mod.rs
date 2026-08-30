//! Query and cluster the processes consuming system resources.

mod application;
mod blocked_report;
mod cli;
mod cluster;
mod fd_report;
mod identity;
mod io_report;
mod kill;
mod limits_report;
mod maps_report;
mod net;
mod net_report;
mod oom_report;
mod platform;
mod pq_query;
mod presentation;
mod pss_report;
mod ranked_report;
mod report;
mod selection;
mod tree_report;
mod unix_report;

pub(crate) use platform::linux::{netdiag, proc};

use clap::Parser;

use cli::Args;
use pq_query::QueryPlan;

pub fn main() {
    if crate::update::run_internal_check() {
        return;
    }

    let invocation: Vec<String> = std::env::args().collect();
    let machine_output = invocation
        .iter()
        .any(|arg| arg == "--json" || arg == "--jsonl");
    let colors = !invocation.iter().any(|arg| arg == "--no-color");

    cli::help::show_and_exit();
    let args = Args::parse();
    if let Some(shell) = args.generate_completion {
        cli::generate_completion(shell);
        return;
    }
    let plan = QueryPlan::from_args(&args).unwrap_or_else(|message| exit_with_error(message));
    if let Err(message) = application::execute(&plan) {
        exit_with_error(message);
    }
    crate::update::after_invocation(machine_output, colors);
}

fn exit_with_error(message: String) -> ! {
    eprintln!("pq: {message}");
    std::process::exit(1);
}
