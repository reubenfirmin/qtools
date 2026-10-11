//! Conservative unseen-allocation bounds and chart-significance decisions.
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::directory_tree::{DirectoryTree, NodeId};
use super::dq_query::{DiskQuery, SizeAccounting};
use super::hierarchy::{MAX_CHILDREN, MIN_PARENT_SHARE};
use super::space::Space;

pub(super) struct Bound {
    pub used: u64,
    pub capacity: u64,
    accounted: u64,
    // A sparse inode bitmap avoids retaining millions of file records. One
    // page covers 65,536 inode numbers in 8 KiB, regardless of their paths.
    inodes: Vec<Option<Box<[u64; 1024]>>>,
    large_inodes: HashMap<u64, Box<[u64; 1024]>>,
    valid: bool,
    check_file_inodes: bool,
    trust_link_counts: bool,
    mount_layout: Option<Vec<u8>>,
}

impl Bound {
    pub fn for_query(root: &Path, query: &DiskQuery) -> Option<Self> {
        if query.traversal.precise || query.traversal.max_error_percent == 0.0 || query.output.machine() || !query.traversal.one_file_system
            || query.accounting != SizeAccounting::Allocated || query.presentation.depth != 1
            || query.presentation.all || query.presentation.include_zeroes || query.presentation.min_percent == 0.0
            || query.selection.min_size.is_some() || query.selection.older_than.is_some()
            || !query.selection.excludes.is_empty() { return None; }
        let space = Space::allocation_bound(root)?;
        let mut bound = Self::new(space.used_bytes, space.capacity_bytes);
        // A file bind mount can expose one link at several paths. Until the
        // layout excludes aliases, nlink cannot certify complete attribution.
        bound.trust_link_counts = false;
        if let (Ok(canonical), Ok(metadata), Ok(layout)) =
            (fs::canonicalize(root), fs::metadata(root), fs::read("/proc/self/mountinfo")) {
            let device = format!("{}:{}", libc::major(metadata.dev()), libc::minor(metadata.dev()));
            if file_aliases_possible(&canonical, device.as_bytes(), &layout) == Some(false) {
                // A singly-linked regular file has one directory entry.
                // Without bind aliases, its allocation can be summed per
                // batch rather than recorded individually in the bitmap.
                bound.check_file_inodes = false;
                bound.trust_link_counts = true;
                bound.mount_layout = Some(layout);
            }
        }
        Some(bound)
    }

    pub fn new(used: u64, capacity: u64) -> Self {
        Self { used, capacity, accounted: 0, inodes: Vec::new(), large_inodes: HashMap::new(), valid: true,
            check_file_inodes: true, trust_link_counts: true, mount_layout: None }
    }

    pub fn check_file_inodes(&self) -> bool { self.check_file_inodes }
    pub fn can_settle_links(&self) -> bool { self.trust_link_counts }
    pub fn account_file_bytes(&mut self, bytes: u64) { self.accounted = self.accounted.saturating_add(bytes); }
    #[cfg(test)]
    pub fn with_file_inode_checks(mut self, enabled: bool) -> Self { self.check_file_inodes = enabled; self }
    #[cfg(test)]
    pub fn without_trusted_link_counts(mut self) -> Self { self.trust_link_counts = false; self }

    pub fn record(&mut self, inode: u64, bytes: u64) {
        if !self.check_file_inodes {
            // The verified mount layout excludes directory aliases as well
            // as file aliases. Hard links are deduplicated by their groups.
            self.account_file_bytes(bytes);
            return;
        }
        let page_number = inode / 65_536;
        // Ext4 inode numbers fit in 32 bits. Index their sparse bitmap pages
        // directly rather than hashing every file; retain a bounded fallback
        // for unusual inode numbers so a bad input cannot grow a huge vector.
        let page = if page_number < 65_536 {
            let index = page_number as usize;
            if self.inodes.len() <= index { self.inodes.resize_with(index + 1, || None); }
            self.inodes[index].get_or_insert_with(|| Box::new([0; 1024]))
        } else {
            self.large_inodes.entry(page_number).or_insert_with(|| Box::new([0; 1024]))
        };
        let index = (inode % 65_536 / 64) as usize;
        let bit = 1 << (inode % 64);
        if page[index] & bit != 0 {
            // A repeated singly-linked file or directory can indicate a bind
            // alias. Its additive chart totals cannot bound physical usage.
            self.valid = false;
            return;
        }
        page[index] |= bit;
        self.accounted = self.accounted.saturating_add(bytes);
    }

    pub fn remaining(&self) -> Option<u64> {
        self.valid.then(|| self.used.checked_sub(self.accounted)).flatten()
    }

    pub fn invalidate(&mut self) { self.valid = false; }

    pub fn refresh(&mut self, root: &Path) {
        if let Some(space) = Space::read(root) {
            self.used = self.used.max(space.used_bytes);
            self.capacity = space.capacity_bytes;
        } else if self.mount_layout.is_some() {
            // A checked filesystem root must remain readable for the final
            // allocation certificate to refer to the same filesystem.
            self.valid = false;
        }
        if self.mount_layout.as_ref().is_some_and(|layout|
            fs::read("/proc/self/mountinfo").ok().as_ref() != Some(layout)) { self.valid = false; }
    }
}

/// Same-device mounts are harmless when their source root is exactly the
/// directory already present at that path. Any alias (including a file bind)
/// requires individual inode checks. Unparseable layouts use that fallback.
fn file_aliases_possible(root: &Path, device: &[u8], layout: &[u8]) -> Option<bool> {
    let mut mounts = Vec::new();
    for line in layout.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
        let fields = line.split(|byte| *byte == b' ').take(6).collect::<Vec<_>>();
        if fields.len() != 6 { return None; }
        if fields[2] != device { continue; }
        let source = mount_path(fields[3])?;
        let target = mount_path(fields[4])?;
        if !source.is_absolute() || !target.is_absolute() { return None; }
        mounts.push((source, target));
    }
    let base = mounts.iter().rev().find(|(_, target)| target == root)?.0.as_path();
    Some(mounts.iter().any(|(source, target)| {
        target.strip_prefix(root).ok().is_some_and(|relative|
            !relative.as_os_str().is_empty() && *source != base.join(relative))
    }))
}

fn mount_path(field: &[u8]) -> Option<PathBuf> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        if field[index] == b'\\' {
            let mut value = 0_u16;
            for digit in field.get(index + 1..index + 4)? {
                if !(b'0'..=b'7').contains(digit) { return None; }
                value = value * 8 + u16::from(*digit - b'0');
            }
            decoded.push(u8::try_from(value).ok()?); index += 4;
        } else { decoded.push(field[index]); index += 1; }
    }
    Some(PathBuf::from(OsString::from_vec(decoded)))
}

/// Only close a branch when even assigning every remaining byte to it cannot
/// introduce a displayed directory within it. For a displayed third-ring leaf,
/// additionally require that at least 90% of its possible allocation is known.
/// This preserves significant directory choices, not exact arc angles: sizes
/// remain lower bounds and all unresolved allocation stays in the outer ring.
pub(super) fn closed_branches(tree: &DirectoryTree, query: &DiskQuery, remaining: u64) -> Vec<NodeId> {
    let total = tree.nodes[0].bytes;
    if total == 0 { return Vec::new(); }
    let groups = tree.root_groups().collect::<Vec<_>>();
    let floor = ranking_floor(tree, &groups, query.presentation.top);
    let roots = selected(tree, &groups, true, total, total, query.presentation.min_percent, query.presentation.top);
    let mut closed = Vec::new();
    for &id in &groups {
        if id == 0 { continue; }
        if !roots.contains(&id) {
            if cannot_enter(tree, floor, true, id, total, total, query.presentation.min_percent,
                query.presentation.top, remaining) { closed.push(id); }
            continue;
        }
        // Growth of this root must not push another currently shown root
        // below the requested display cutoff.
        if roots.iter().any(|other| super::dq_report::percent(tree.group_bytes(*other), total.saturating_add(remaining))
            < query.presentation.min_percent) { continue; }
        close_children(tree, query, id, total, remaining, &mut closed);
    }
    closed
}

fn close_children(tree: &DirectoryTree, query: &DiskQuery, parent: NodeId, total: u64,
    remaining: u64, closed: &mut Vec<NodeId>) {
    let parent_bytes = tree.nodes[parent].bytes;
    let groups = tree.children(parent).collect::<Vec<_>>();
    let floor = ranking_floor(tree, &groups, MAX_CHILDREN);
    let min_percent = query.presentation.min_percent.max(1.0);
    let children = selected(tree, &groups, false, parent_bytes, total, min_percent, MAX_CHILDREN);
    for &id in &groups {
        if !children.contains(&id) {
            if cannot_enter(tree, floor, false, id, parent_bytes, total, min_percent, MAX_CHILDREN, remaining) {
                closed.push(id);
            }
            continue;
        }
        if children.iter().any(|other| !meets_share(tree.nodes[*other].bytes, parent_bytes.saturating_add(remaining), MIN_PARENT_SHARE)
            || !meets_share(tree.nodes[*other].bytes, total.saturating_add(remaining), min_percent / 100.0)) { continue; }
        if tree.nodes[id].depth == 3 {
            if remaining as f64 <= tree.nodes[id].bytes as f64 * MIN_PARENT_SHARE { closed.push(id); }
        } else {
            close_children(tree, query, id, total, remaining, closed);
        }
    }
}

fn selected(tree: &DirectoryTree, groups: &[NodeId], roots: bool, parent_bytes: u64, total: u64,
    min_percent: f64, limit: usize) -> Vec<NodeId> {
    let mut rows = groups.iter().copied().filter(|id| {
        let bytes = tree.group_bytes(*id);
        bytes > 0 && (roots || meets_share(bytes, parent_bytes, MIN_PARENT_SHARE))
            && if roots { super::dq_report::percent(bytes, total) >= min_percent }
                else { meets_share(bytes, total, min_percent / 100.0) }
    }).collect::<Vec<_>>();
    rows.sort_by(|a, b| tree.group_bytes(*b).cmp(&tree.group_bytes(*a))
        .then_with(|| tree.nodes[*a].path.cmp(&tree.nodes[*b].path)));
    rows.truncate(limit);
    rows
}

#[allow(clippy::too_many_arguments)]
fn cannot_enter(tree: &DirectoryTree, floor: Option<u64>, roots: bool, id: NodeId, parent_bytes: u64, total: u64,
    min_percent: f64, limit: usize, remaining: u64) -> bool {
    let upper = tree.group_bytes(id).saturating_add(remaining);
    let below_total = if roots { super::dq_report::percent(upper, total.saturating_add(remaining)) < min_percent }
        else { !meets_share(upper, total.saturating_add(remaining), min_percent / 100.0) };
    if (!roots && !meets_share(upper, parent_bytes.saturating_add(remaining), MIN_PARENT_SHARE))
        || below_total { return true; }
    limit == 0 || floor.is_some_and(|floor| floor > upper)
}

fn ranking_floor(tree: &DirectoryTree, groups: &[NodeId], limit: usize) -> Option<u64> {
    if limit == 0 || groups.len() < limit { return None; }
    let mut bytes = groups.iter().map(|id| tree.group_bytes(*id)).collect::<Vec<_>>();
    // The candidate's current bytes cannot exceed its own upper bound, so it
    // never contributes to the count of strictly larger siblings. One order
    // statistic replaces rescanning every sibling for every candidate.
    Some(*bytes.select_nth_unstable_by(limit - 1, |a, b| b.cmp(a)).1)
}

fn meets_share(bytes: u64, total: u64, share: f64) -> bool {
    total != 0 && bytes as f64 >= total as f64 * share
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use crate::dq::cli::Args;

    fn query() -> DiskQuery { DiskQuery::from_args(&Args::try_parse_from(["dq"]).unwrap()).unwrap() }
    #[test] fn mount_alias_detection_distinguishes_overmounts_files_and_other_filesystems() {
        let normal = b"1 0 8:2 / /data rw - ext4 /dev/sda2 rw\n2 1 8:2 /code /data/code rw - ext4 /dev/sda2 rw\n3 1 0:5 / /data/memory rw - tmpfs tmpfs rw\n";
        assert_eq!(file_aliases_possible(Path::new("/data"), b"8:2", normal), Some(false));
        let mut alias = normal.to_vec(); alias.extend_from_slice(b"4 1 8:2 /code/file /data/file-alias rw - ext4 /dev/sda2 rw\n");
        assert_eq!(file_aliases_possible(Path::new("/data"), b"8:2", &alias), Some(true));
        assert_eq!(file_aliases_possible(Path::new("/missing"), b"8:2", normal), None);
        assert_eq!(file_aliases_possible(Path::new("/data"), b"8:2", b"malformed"), None);
        let spaced = b"1 0 8:2 / /data\\040disk rw - ext4 /dev/sda2 rw\n2 1 8:2 /code\\040files /data\\040disk/code\\040files rw - ext4 /dev/sda2 rw\n";
        assert_eq!(file_aliases_possible(Path::new("/data disk"), b"8:2", spaced), Some(false));
    }
    fn tree(sites_bytes: u64, oss_bytes: u64) -> DirectoryTree {
        let mut tree = DirectoryTree::new(3);
        let code = tree.add_child(0, Path::new("code"));
        let documents = tree.add_child(0, Path::new("documents"));
        let sites = tree.add_child(code, Path::new("code/sites"));
        let oss = tree.add_child(code, Path::new("code/oss"));
        let small = tree.add_child(code, Path::new("code/small"));
        let leaf = tree.add_child(sites, Path::new("code/sites/coalicionfloresta"));
        let other = tree.add_child(sites, Path::new("code/sites/other"));
        tree.observe(code, 320 - sites_bytes - oss_bytes - 1, 1, true);
        tree.observe(documents, 60, 1, true); tree.observe(sites, sites_bytes - 180, 1, true);
        tree.observe(oss, oss_bytes, 1, true); tree.observe(small, 1, 1, true);
        tree.observe(leaf, 179, 1, true); tree.observe(other, 1, 1, true);
        tree
    }
    fn is_closed(tree: &DirectoryTree, path: &str) -> bool {
        tree.nodes.iter().enumerate().find(|(_, node)| node.path.as_deref() == Some(Path::new(path)))
            .is_some_and(|(id, _)| tree.is_closed(id))
    }
    fn close(tree: &mut DirectoryTree, remaining: u64) {
        for id in closed_branches(tree, &query(), remaining) { tree.close(id); }
    }

    #[test] fn remaining_allocation_prevents_hiding_an_unseen_large_file() {
        let mut tree = tree(182, 100); close(&mut tree, 200);
        assert!(!is_closed(&tree, "code/sites/other"));
        assert!(!is_closed(&tree, "code/small"));
        assert!(!is_closed(&tree, "code/sites/coalicionfloresta"));
    }

    #[test] fn dominant_leaf_and_provably_hidden_branches_can_stop() {
        let mut tree = tree(182, 100); close(&mut tree, 3);
        assert!(is_closed(&tree, "code/sites/coalicionfloresta"));
        assert!(is_closed(&tree, "code/sites/other"));
        assert!(is_closed(&tree, "code/small"));
        assert!(!is_closed(&tree, "code/oss"));
    }

    #[test] fn growth_must_not_hide_a_sibling_near_the_ten_percent_threshold() {
        let mut tree = tree(282, 33); close(&mut tree, 15);
        assert!(!is_closed(&tree, "code/sites/coalicionfloresta"));
    }

    #[test] fn only_one_bound_is_used_for_all_skipped_branches() {
        let mut bound = Bound::new(100, 200);
        bound.record(2, 10); bound.record(31_000_000, 50);
        assert_eq!(bound.remaining(), Some(40));
        let _ = closed_branches(&tree(182, 100), &query(), bound.remaining().unwrap());
        assert_eq!(bound.remaining(), Some(40));
        // A bind alias must not make the remaining bound artificially small.
        bound.record(31_000_000, 50);
        assert_eq!(bound.remaining(), None);
    }

    #[test] fn sparse_inode_tracking_handles_large_inode_numbers() {
        let mut bound = Bound::new(100, 200);
        bound.record(1 << 26, 10);
        bound.record(u64::MAX, 50);
        assert_eq!(bound.remaining(), Some(40));
        bound.record(u64::MAX, 50);
        assert_eq!(bound.remaining(), None);
    }
}
