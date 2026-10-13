use super::*;
use clap::Parser;
use crate::dq::cli::Args;

fn fixture() -> (PathBuf, DiskQuery) {
    static NEXT_FIXTURE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let fixture = NEXT_FIXTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("qtools-dq-resume-{}-{unique}-{fixture}", std::process::id()));
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

#[test]
fn large_file_tails_preserve_every_name_and_canonical_link_allocation() {
    let (root, mut query) = fixture();
    let flat = root.join("flat");
    fs::create_dir(&flat).unwrap();
    for number in 0..5103 {
        fs::write(flat.join(format!("file-{number}-{}", "x".repeat(number % 13))), b"data").unwrap();
    }
    let branches = root.join("branches");
    fs::create_dir(&branches).unwrap();
    for number in 0..530 {
        let child = branches.join(format!("dir-{number}-{}", "x".repeat(160)));
        fs::create_dir(&child).unwrap();
        fs::write(child.join("tiny"), b"data").unwrap();
    }
    fs::hard_link(flat.join("file-0-"), flat.join("a-link")).unwrap();
    fs::File::create(flat.join("sparse")).unwrap().set_len(1 << 30).unwrap();
    std::os::unix::fs::symlink("file-0-", flat.join("symlink")).unwrap();
    let exact = summarize_with_bound(&root, &query, None);
    let bytes = exact.levels[0].values().map(|usage| usage.0).sum::<u64>();
    // Keep the bound above every available allocation so every tail drains.
    query.traversal.max_error_percent = 0.0;
    query.traversal.threads = 4;
    let bounded = summarize_with_bound(&root, &query, Some(Bound::new(bytes + 1, bytes * 2)));
    assert_eq!(bounded.pruned_directories, 0);
    assert_eq!(bounded.errors, exact.errors);
    assert_eq!(bounded.scanned_files, exact.scanned_files);
    assert_eq!(bounded.duplicate_hard_links, exact.duplicate_hard_links);
    assert_eq!(bounded.levels, exact.levels);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn hard_link_groups_preserve_canonical_attribution_and_one_allocation() {
    let root = Path::new("/fixture");
    let query = DiskQuery::from_args(&Args::try_parse_from(["dq", "--noprogress"]).unwrap()).unwrap();
    let mut state = SummaryState::new(root, &query, None);
    let metadata = EntryMetadata { apparent: 1024, allocated: 4096, device: 1, inode: 7,
        links: 2, uid: 0, modified: 0, mode: libc::S_IFREG };
    state.account_links(vec![metadata.file_fact(root.join("z"))], 0);
    state.account_links(vec![metadata.file_fact(root.join("a"))], 0);
    assert_eq!(state.links.len(), 1);
    let group = &state.links[&(1, 7)];
    assert_eq!(group.fact.path, root.join("a"));
    assert_eq!(group.fact.allocated, 4096);
    assert_eq!(group.observed, 2);
    assert_eq!(state.duplicate_hard_links, 1);
    let result = state.finish(Vec::new()).unwrap();
    assert_eq!(result.selected_files, 1);
    assert_eq!(result.levels[0].values().map(|usage| usage.0).sum::<u64>(), 4096);
}

#[test]
fn allocation_histogram_keeps_boundary_values_in_their_bucket() {
    let mut histogram = super::AllocationHistogram::new();
    histogram.record(0); histogram.record(1);
    for bits in [12, 16, 20, 24, 28, 32] {
        histogram.record(1_u64 << bits);
        histogram.record((1_u64 << bits) + 1);
    }
    histogram.record(64_u64 << 30);
    assert_eq!(histogram.buckets.map(|bucket| bucket.0), [1, 2, 2, 2, 2, 2, 2, 2]);
    assert_eq!(histogram.buckets[1].1, 4097);
    assert_eq!(histogram.buckets[6].1, (1_u64 << 28) + 1 + (1_u64 << 32));
    let mut merged = super::AllocationHistogram::new();
    merged.merge(&histogram);
    assert_eq!(merged.buckets, histogram.buckets);
}
