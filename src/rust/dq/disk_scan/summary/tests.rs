use super::*;
use clap::Parser;
use crate::dq::cli::Args;

fn fixture() -> (PathBuf, DiskQuery) {
    let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("qtools-dq-resume-{}-{unique}", std::process::id()));
    let leaf = root.join("code/sites/main");
    fs::create_dir_all(&leaf).unwrap();
    fs::write(leaf.join("large"), vec![1_u8; 16 * 1024 * 1024]).unwrap();
    fs::File::open(leaf.join("large")).unwrap().sync_all().unwrap();
    for number in 0..128 { fs::write(root.join(format!("tiny-{number}")), b"tiny").unwrap(); }
    let query = DiskQuery::from_args(&Args::try_parse_from(["dq", "--threads", "1", "--noprogress"]).unwrap()).unwrap();
    (root, query)
}

#[test]
fn growth_at_the_final_check_resumes_without_remeasuring_completed_files() {
    let (root, query) = fixture();
    let exact = summarize_with_bound(&root, &query, None);
    let bytes = exact.levels[0].values().map(|usage| usage.0).sum::<u64>();
    let mut state = SummaryState::new(&root, &query, Some(Bound::new(bytes, bytes * 2)));
    let mut grew = false;
    let options = WalkOptions { threads: 1, one_file_system: true, excludes: &[], show_progress: false, adaptive: true };
    let (errors, _) = walk(&root, options, state.collector(), |event, queue| {
        if matches!(event, WalkEvent::Idle) && !grew {
            assert!(!state.deferred.is_empty(), "fixture must first meet the stopping budget");
            // Model a usage refresh above the budget while keeping all prior
            // measurements valid. A subdirectory fixture has no real refresh.
            state.bound.as_mut().unwrap().used += bytes;
            grew = true;
        }
        state.receive(event, queue);
    });
    assert!(grew);
    let resumed = state.finish(errors).unwrap();
    assert_eq!(resumed.pruned_directories, 0);
    assert_eq!(resumed.scanned_files, exact.scanned_files);
    assert_eq!(resumed.directories, exact.directories);
    assert_eq!(resumed.levels, exact.levels);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_allocation_accounting_uses_one_complete_fallback() {
    let (root, query) = fixture();
    let exact = summarize_with_bound(&root, &query, None);
    // A zero usage bound becomes invalid on the first directory allocation.
    // Retrying another bounded scan would hit exactly the same condition.
    let fallback = summarize_with_bound(&root, &query, Some(Bound::new(0, 1)));
    assert_eq!(fallback.pruned_directories, 0);
    assert_eq!(fallback.levels, exact.levels);
    assert_eq!(fallback.scanned_files, exact.scanned_files);
    assert_eq!(fallback.directories, exact.directories);
    fs::remove_dir_all(root).unwrap();
}
