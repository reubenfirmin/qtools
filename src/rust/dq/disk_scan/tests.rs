use std::fs;
use std::path::PathBuf;
use clap::Parser;

use super::*;
use super::scheduling::{Priority, WorkQueue};
use super::summary::summarize_with_bound;
use super::traversal::{glob_match, read_directory, should_descend, Work};
use crate::dq::cli::Args;
use crate::dq::dq_query::DiskQuery;
use crate::dq::pruning::Bound;

fn query() -> DiskQuery {
    DiskQuery::from_args(&Args::try_parse_from(["dq", "--threads", "1", "--noprogress"]).unwrap()).unwrap()
}

fn root(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("qtools-dq-{name}-{}-{unique}", std::process::id()))
}

#[test]
fn untrusted_link_counts_keep_attribution_in_the_global_error_budget() {
    let root = root("link-counts");
    fs::create_dir_all(root.join("code/sites/main")).unwrap();
    fs::write(root.join("shared"), vec![1_u8; 1024 * 1024]).unwrap();
    fs::hard_link(root.join("shared"), root.join("alias")).unwrap();
    fs::write(root.join("code/sites/main/big"), vec![2_u8; 64 * 1024 * 1024]).unwrap();
    for number in 0..200 {
        let tail = root.join(format!("code/sites/main/tail/{number}/nested"));
        fs::create_dir_all(&tail).unwrap(); fs::write(tail.join("tiny"), b"tiny").unwrap();
    }
    let query = query();
    let exact = summarize_with_bound(&root, &query, None);
    let bytes = exact.levels[0].values().map(|usage| usage.0).sum::<u64>();
    let partial = summarize_with_bound(&root, &query,
        Some(Bound::new(bytes, bytes * 2).without_trusted_link_counts()));
    assert!(partial.pruned_directories > 0);
    assert_eq!(partial.known_unassigned_bytes, 1024 * 1024);
    assert_eq!(partial.levels[0][&root].1, 0);
    assert!(partial.levels[0][&root].0 < exact.levels[0][&root].0);
    let measured = partial.levels[0].values().map(|usage| usage.0).sum::<u64>();
    let uncertainty = partial.unmeasured_bytes_upper_bound.unwrap() + partial.known_unassigned_bytes;
    assert!(bytes - measured <= uncertainty);
    assert!(uncertainty as f64 <= bytes as f64 * 2.0 * query.traversal.max_error_percent / 100.0);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bucket_updates_preserve_every_eligible_directory_across_priority_boundaries() {
    let mut queue = WorkQueue::new(true);
    for node in 0..256 {
        let mut work = Work::directory(PathBuf::from(node.to_string()), None); work.node = node;
        queue.push_ranked(work, node as Priority);
    }
    assert_eq!(queue.len(), 256);
    assert_eq!(queue.pop_front().unwrap().node, 255);
    queue.update(|work| {
        if work.node % 3 == 0 { None }
        else { let rank = (255 - work.node) as Priority; Some((work, rank)) }
    });
    let expected = (0..255).filter(|node| node % 3 != 0).collect::<Vec<_>>();
    assert_eq!(queue.len(), expected.len());
    let mut actual = std::iter::from_fn(|| queue.pop_front()).map(|work| work.node).collect::<Vec<_>>();
    // Positive-yield work retains ranked ordering above the low frontier.
    let ranked = expected.iter().copied().filter(|node| *node < 239).collect::<Vec<_>>();
    assert_eq!(actual[..ranked.len()], ranked);
    actual.sort_unstable();
    assert_eq!(actual, expected);
    assert!(queue.is_empty());
    // Re-ranking must retain order among equally ranked work.
    for node in 0..10 {
        let mut work = Work::directory(PathBuf::new(), None); work.node = node;
        queue.push_ranked(work, 100);
    }
    queue.update(|work| Some((work, 100)));
    let actual = std::iter::from_fn(|| queue.pop_front()).map(|work| work.node).collect::<Vec<_>>();
    assert_eq!(actual, (0..10).rev().collect::<Vec<_>>());
}

#[test]
fn low_yield_depth_selection_preserves_all_work_and_does_not_weight_depth_by_population() {
    fn sequence() -> Vec<usize> {
        let mut queue = WorkQueue::new(true);
        for node in 0..1001 {
            let mut work = Work::directory(PathBuf::new(), None);
            work.node = node;
            work.depth = if node == 1000 { 6 } else { 5 };
            queue.push_ranked(work, 16);
        }
        std::iter::from_fn(|| queue.pop_front()).map(|work| work.node).collect()
    }
    let mut actual = sequence();
    assert_eq!(actual, sequence());
    assert!(actual[..20].contains(&1000));
    actual.sort_unstable();
    assert_eq!(actual, (0..1001).collect::<Vec<_>>());
}

#[test]
fn wildcard_matching() {
    assert!(glob_match("*.cache", "foo.cache"));
    assert!(glob_match("node_modules", "node_modules")); assert!(!glob_match("*.cache", "foo.log"));
}
#[test]
fn filesystem_boundary_is_enforced_only_when_requested() {
    assert!(!should_descend(true, 1, 2)); assert!(should_descend(true, 1, 1)); assert!(should_descend(false, 1, 2));
}
#[test]
fn missing_directory_becomes_a_scan_error() {
    let missing = root("missing");
    let batch = read_directory(missing.clone(), &missing, 0, true, &[]);
    assert_eq!(batch.errors.len(), 1); assert!(batch.files.is_empty());
}
#[test]
fn exclusions_prune_directories_during_traversal() {
    let root = root("scan");
    fs::create_dir_all(root.join("keep")).unwrap(); fs::create_dir_all(root.join("cache")).unwrap();
    fs::write(root.join("keep/file"), b"keep").unwrap(); fs::write(root.join("cache/file"), b"skip").unwrap();
    let result = scan(&root, 2, true, &["cache".into()], false);
    assert_eq!(result.files.len(), 1);
    assert!(result.files[0].path.ends_with("keep/file"));
    assert!(!result.directories.iter().any(|directory| directory.path.ends_with("cache")));
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn fresh_samples_defer_tiny_file_tails_and_keep_unseen_growth_in_the_bound() {
    let root = root("fresh-sample");
    let leaf = root.join("code/sites/main");
    fs::create_dir_all(&leaf).unwrap();
    fs::write(leaf.join("large"), vec![1_u8; 128 * 1024 * 1024]).unwrap();
    fs::File::open(leaf.join("large")).unwrap().sync_all().unwrap();
    for number in 0..1024 { fs::write(root.join(format!("tiny-{number}")), b"tiny").unwrap(); }
    std::os::unix::fs::symlink(leaf.join("large"), root.join("symlink")).unwrap();
    let query = query();
    let exact = summarize_with_bound(&root, &query, None);
    let bytes = exact.levels[0].values().map(|usage| usage.0).sum::<u64>();
    let sampled = summarize_with_bound(&root, &query, Some(Bound::new(bytes, bytes * 2)));
    assert!(sampled.pruned_directories > 0);
    assert!(sampled.scanned_files < exact.scanned_files / 8, "{} vs {} files", sampled.scanned_files, exact.scanned_files);
    let measured = sampled.levels[0].values().map(|usage| usage.0).sum::<u64>();
    assert!(measured <= bytes);
    assert!(bytes - measured <= sampled.unmeasured_bytes_upper_bound.unwrap());
    assert!(sampled.levels[2][&leaf].0 >= 128 * 1024 * 1024);

    // Even a directory that sampled tiny files can have a huge unmeasured file.
    fs::write(root.join("tiny-1000"), vec![2_u8; 256 * 1024 * 1024]).unwrap();
    fs::File::open(root.join("tiny-1000")).unwrap().sync_all().unwrap();
    fs::hard_link(root.join("tiny-1000"), leaf.join("alias")).unwrap();
    let exact = summarize_with_bound(&root, &query, None);
    let bytes = exact.levels[0].values().map(|usage| usage.0).sum::<u64>();
    let sampled = summarize_with_bound(&root, &query, Some(Bound::new(bytes, bytes * 2)));
    let measured = sampled.levels[0].values().map(|usage| usage.0).sum::<u64>();
    assert!(measured <= bytes);
    assert!(bytes - measured <= sampled.unmeasured_bytes_upper_bound.unwrap_or(0) + sampled.known_unassigned_bytes);

    // An unreachable budget must drain all continuations and match a full walk.
    let mut filtered = query.clone(); filtered.selection.excludes.push("tiny-10*".into());
    let exact = summarize_with_bound(&root, &filtered, None);
    let complete = summarize_with_bound(&root, &filtered, Some(Bound::new(bytes * 2, bytes * 2)));
    assert_eq!(complete.pruned_directories, 0);
    assert_eq!(complete.levels, exact.levels);
    assert_eq!(complete.scanned_files, exact.scanned_files);
    assert_eq!(complete.duplicate_hard_links, exact.duplicate_hard_links);
    assert!(complete.errors.is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn live_sampling_finds_a_productive_tail_before_draining_tiny_files() {
    let root = root("tail-sample");
    let leaf = root.join("code/project/assets");
    fs::create_dir_all(&leaf).unwrap();
    for number in 0..256 { fs::write(leaf.join(format!("file-{number}")), b"tiny").unwrap(); }
    // Grow the last listed inode without changing directory-entry order.
    let tail = fs::read_dir(&leaf).unwrap().map(|entry| entry.unwrap().path()).last().unwrap();
    fs::write(&tail, vec![1_u8; 128 * 1024 * 1024]).unwrap();
    fs::File::open(&tail).unwrap().sync_all().unwrap();
    let query = query();
    let exact = summarize_with_bound(&root, &query, None);
    let bytes = exact.levels[0].values().map(|usage| usage.0).sum::<u64>();
    let sampled = summarize_with_bound(&root, &query, Some(Bound::new(bytes, bytes * 2)));
    assert!(sampled.scanned_files <= 8, "measured {} files before finding the tail", sampled.scanned_files);
    assert!(sampled.levels[2][&leaf].0 >= 128 * 1024 * 1024);
    let measured = sampled.levels[0].values().map(|usage| usage.0).sum::<u64>();
    assert!(measured <= bytes);
    assert!(bytes - measured <= sampled.unmeasured_bytes_upper_bound.unwrap());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn shallow_small_files_cannot_hide_a_large_file_deep_in_the_tree() {
    let root = root("deep-unseen");
    fs::create_dir_all(&root).unwrap();
    for number in 0..256 { fs::write(root.join(format!("tiny-{number}")), b"tiny").unwrap(); }
    let mut leaf = root.join("code/project/assets");
    for _ in 0..40 { leaf.push("nested"); }
    fs::create_dir_all(&leaf).unwrap();
    fs::write(leaf.join("large"), vec![1_u8; 16 * 1024 * 1024]).unwrap();
    fs::File::open(leaf.join("large")).unwrap().sync_all().unwrap();
    let query = query();
    let exact = summarize_with_bound(&root, &query, None);
    let bytes = exact.levels[0].values().map(|usage| usage.0).sum::<u64>();
    let bounded = summarize_with_bound(&root, &query, Some(Bound::new(bytes, bytes * 2)));
    assert!(bounded.levels[2][&root.join("code/project/assets")].0 >= 16 * 1024 * 1024);
    let measured = bounded.levels[0].values().map(|usage| usage.0).sum::<u64>();
    assert!(bytes - measured <= bounded.unmeasured_bytes_upper_bound.unwrap_or(0));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bounded_walk_skips_a_dominant_leaf_tail_without_losing_its_allocation() {
    let root = root("bound");
    let leaf = root.join("code/sites/coalicionfloresta");
    fs::create_dir_all(&leaf).unwrap();
    fs::write(leaf.join("large"), vec![1_u8; 32 * 1024 * 1024]).unwrap();
    for number in 0..200 {
        let tail = leaf.join(format!("tail/{number}/nested"));
        fs::create_dir_all(&tail).unwrap(); fs::write(tail.join("small"), b"small").unwrap();
    }
    let query = query();
    let precise = summarize_with_bound(&root, &query, None);
    let exact_bytes = precise.levels[0].values().map(|usage| usage.0).sum::<u64>();
    let bounded = summarize_with_bound(&root, &query, Some(Bound::new(exact_bytes, exact_bytes * 2)));
    let measured = bounded.levels[0].values().map(|usage| usage.0).sum::<u64>();
    assert!(bounded.pruned_directories > 0);
    assert!(bounded.directories < precise.directories / 3, "{} vs {} directories", bounded.directories, precise.directories);
    assert!(measured <= exact_bytes);
    assert!(exact_bytes - measured <= bounded.unmeasured_bytes_upper_bound.unwrap());
    assert!(bounded.levels[2][&leaf].0 >= 32 * 1024 * 1024);

    let mut strict = query.clone(); strict.traversal.max_error_percent = 0.01;
    let bounded = summarize_with_bound(&root, &strict, Some(Bound::new(exact_bytes, exact_bytes * 2)));
    let measured = bounded.levels[0].values().map(|usage| usage.0).sum::<u64>();
    assert!((exact_bytes - measured) as f64 <= exact_bytes as f64 * 2.0 * 0.0001,
        "deferred branches must reopen when the global budget remains exceeded");
    let bounded = summarize_with_bound(&root, &strict, Some(Bound::new(exact_bytes + 10_000_000, exact_bytes * 2)));
    assert_eq!(bounded.pruned_directories, 0, "an unreachable budget must complete all accessible work");
    assert_eq!(bounded.levels, precise.levels);

    let hidden = root.join("code/sites/other/deep/nested");
    fs::create_dir_all(&hidden).unwrap();
    fs::write(hidden.join("unexpected"), vec![2_u8; 48 * 1024 * 1024]).unwrap();
    let precise = summarize_with_bound(&root, &query, None);
    let exact_bytes = precise.levels[0].values().map(|usage| usage.0).sum::<u64>();
    let bounded = summarize_with_bound(&root, &query, Some(Bound::new(exact_bytes, exact_bytes * 2)));
    assert!(bounded.levels[2][&root.join("code/sites/other")].0 >= 48 * 1024 * 1024);

    // Count links once, attributing them to the lexicographically first path.
    fs::create_dir_all(root.join("aaa/deep")).unwrap();
    fs::hard_link(hidden.join("unexpected"), root.join("aaa/deep/shared")).unwrap();
    let precise = summarize_with_bound(&root, &query, None);
    let exact_bytes = precise.levels[0].values().map(|usage| usage.0).sum::<u64>();
    let bounded = summarize_with_bound(&root, &query, Some(Bound::new(exact_bytes, exact_bytes * 2)));
    assert!(bounded.levels[0][&root.join("aaa")].0 >= 48 * 1024 * 1024);
    assert!(bounded.levels[2][&root.join("code/sites/other")].0 < 48 * 1024 * 1024);
    let measured = bounded.levels[0].values().map(|usage| usage.0).sum::<u64>();
    assert!(measured <= exact_bytes);
    if let Some(upper) = bounded.unmeasured_bytes_upper_bound { assert!(exact_bytes - measured <= upper); }
    let aggregated = summarize_with_bound(&root, &query,
        Some(Bound::new(exact_bytes, exact_bytes * 2).with_file_inode_checks(false)));
    assert_eq!(aggregated.levels, bounded.levels, "batch sums must preserve attribution and bounds");

    // An unseen earlier link must keep this allocation unresolved.
    fs::write(leaf.join("pending"), vec![3_u8; 4 * 1024 * 1024]).unwrap();
    let mut pending = root.join("aaa/pending");
    for _ in 0..30 { pending.push("deep"); }
    fs::create_dir_all(&pending).unwrap();
    fs::hard_link(leaf.join("pending"), pending.join("shared")).unwrap();
    let precise = summarize_with_bound(&root, &query, None);
    let exact_bytes = precise.levels[0].values().map(|usage| usage.0).sum::<u64>();
    let mut pending_query = query.clone(); pending_query.traversal.max_error_percent = 5.0;
    let bounded = summarize_with_bound(&root, &pending_query, Some(Bound::new(exact_bytes, exact_bytes * 2)));
    assert!(bounded.pruned_directories > 0);
    assert_eq!(bounded.levels[0][&root.join("aaa")].1, 1);
    assert!(bounded.levels[2][&leaf].0 <= precise.levels[2][&leaf].0);
    fs::remove_dir_all(&root).unwrap();
}
