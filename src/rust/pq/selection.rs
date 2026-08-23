use super::{cluster, proc};
use super::pq_query::{ProcessSelection, QueryPlan};

pub fn process_matches(process: &proc::Proc, selection: &ProcessSelection) -> bool {
    selection.pid.is_none_or(|pid| process.pid == pid)
        && selection.user.as_deref().is_none_or(|wanted| user_matches(process.uid, wanted))
}

pub fn user_matches(uid: u32, wanted: &str) -> bool {
    wanted.parse::<u32>().map_or_else(
        |_| crate::pq::platform::users::username(uid) == wanted,
        |expected| uid == expected,
    )
}

pub fn clusters(procs: &[proc::Proc], plan: &QueryPlan) -> Vec<cluster::Cluster> {
    let selected: Vec<proc::Proc> = procs.iter()
        .filter(|process| process_matches(process, &plan.process)).cloned().collect();
    let mut groups = cluster::cluster(&selected, plan.metric);
    if let Some(pattern) = &plan.process.pattern {
        let needle = pattern.to_lowercase();
        groups.retain(|group| group.identity.to_lowercase().contains(&needle)
            || group.members.iter().any(|member| member.cmd.to_lowercase().contains(&needle)
                || member.comm.to_lowercase().contains(&needle)));
    }
    groups.retain(|group| plan.process.min_cpu.is_none_or(|minimum| group.cpu >= minimum)
        && plan.process.min_memory.is_none_or(|minimum| group.rss >= minimum));
    groups
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::pq::cli::Args;

    fn process(pid: i32, uid: u32, cpu: f64, rss: u64, command: &str) -> proc::Proc {
        proc::Proc {
            pid, ppid: 1, comm: command.into(), cmdline: vec![command.into()], rss,
            swap: 0, cpu_pct: cpu, uid,
        }
    }

    fn plan(arguments: &[&str]) -> QueryPlan {
        QueryPlan::from_args(&Args::try_parse_from(arguments).unwrap()).unwrap()
    }

    #[test]
    fn process_selection_composes_pid_and_numeric_user() {
        let selected = ProcessSelection {
            pattern: None, user: Some("1000".into()), pid: Some(7), min_cpu: None,
            min_memory: None, mount: None, file: None,
        };
        assert!(process_matches(&process(7, 1000, 0.0, 0, "worker"), &selected));
        assert!(!process_matches(&process(8, 1000, 0.0, 0, "worker"), &selected));
        assert!(!process_matches(&process(7, 1001, 0.0, 0, "worker"), &selected));
        assert!(user_matches(1000, "1000"));
        assert!(!user_matches(1000, "1001"));
    }

    #[test]
    fn cluster_selection_applies_pattern_and_resource_floors() {
        let processes = vec![
            process(7, 1000, 12.0, 2_000_000, "alpha-worker"),
            process(8, 1000, 2.0, 100, "beta-worker"),
        ];
        let selected = clusters(&processes, &plan(&["pq", "alpha", "--min-cpu", "10", "--min-memory", "1M"]));
        assert_eq!(selected.len(), 1);
        assert!(selected[0].members.iter().any(|member| member.pid == 7));
        assert!(clusters(&processes, &plan(&["pq", "missing"])).is_empty());
    }
}
