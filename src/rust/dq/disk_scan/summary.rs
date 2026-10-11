//! Streaming accounting and a resumable allocation-bound scan controller.
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::super::directory_tree::{DirectoryTree, NodeId};
use super::super::dq_query::{DiskQuery, SizeAccounting};
use super::super::pruning::{self, Bound};
use super::scheduling::{priority, WorkQueue};
use super::traversal::{walk, Batch, CollectFiles, EntryMetadata, WalkEvent, WalkOptions, Work};
use super::{FileFact, SummaryScan};

#[derive(Clone, Copy)]
struct FileSelection { accounting: SizeAccounting, min_size: Option<u64>, older_than: Option<u64>, now: i64 }
impl FileSelection {
    fn matches(&self, bytes: u64, modified: i64) -> bool {
        !self.min_size.is_some_and(|minimum| bytes < minimum)
            && !self.older_than.is_some_and(|age| self.now.saturating_sub(modified) < age as i64)
    }
    fn bytes(&self, fact: &FileFact) -> u64 {
        match self.accounting { SizeAccounting::Allocated => fact.allocated, SizeAccounting::Apparent => fact.apparent }
    }
}

#[derive(Clone)]
struct SummaryFiles {
    selection: FileSelection, bytes: u64, selected: u64, scanned: usize, hard_links: Vec<FileFact>,
    bound_device: Option<u64>, check_file_inodes: bool, bound_bytes: u64, allocations: Vec<(u64, u64)>,
}
impl CollectFiles for SummaryFiles {
    fn push(&mut self, path: impl FnOnce() -> PathBuf, metadata: &EntryMetadata) {
        self.scanned += 1;
        // Only multiply-linked inodes need paths and deterministic representatives.
        if metadata.links > 1 { self.hard_links.push(metadata.file_fact(path())); return; }
        if self.bound_device == Some(metadata.device) {
            if self.check_file_inodes { self.allocations.push((metadata.inode, metadata.allocated)); }
            else { self.bound_bytes += metadata.allocated; }
        }
        let bytes = match self.selection.accounting {
            SizeAccounting::Allocated => metadata.allocated, SizeAccounting::Apparent => metadata.apparent,
        };
        if self.selection.matches(bytes, metadata.modified) { self.bytes += bytes; self.selected += 1; }
    }
}

struct HardLinkGroup { fact: FileFact, directory: NodeId, observed: u64, finalized: bool }

struct SummaryState<'a> {
    root: &'a Path, query: &'a DiskQuery, bound: Option<Bound>, bound_device: Option<u64>,
    selection: FileSelection, depths: Vec<usize>, tree: DirectoryTree,
    links: HashMap<(u64, u64), HardLinkGroup>, pending_link_bytes: u64,
    scanned_files: usize, selected_files: usize, directories: usize, duplicate_hard_links: usize,
    deferred: Vec<Work>, branch_pruning: bool, invalid: bool,
    previous_remaining: u64, checkpoint_directories: usize, ranked_directories: usize,
    started: Instant, profiling: bool, next_profile: Duration,
    accounting_time: Duration, scheduling_time: Duration, batches: usize, continuations: usize,
}

impl<'a> SummaryState<'a> {
    fn new(root: &'a Path, query: &'a DiskQuery, bound: Option<Bound>) -> Self {
        let depths = if query.presentation.depth == 1 { vec![1, 2, 3] } else { vec![query.presentation.depth] };
        let selection = FileSelection { accounting: query.accounting, min_size: query.selection.min_size,
            older_than: query.selection.older_than.map(|age| age.as_secs()),
            now: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64 };
        let bound_device = bound.as_ref().and_then(|_| fs::metadata(root).ok().map(|item| item.dev()));
        Self { root, query, bound, bound_device, selection, tree: DirectoryTree::new(*depths.iter().max().unwrap()), depths,
            links: HashMap::new(), pending_link_bytes: 0, scanned_files: 0, selected_files: 0, directories: 0, duplicate_hard_links: 0,
            deferred: Vec::new(), branch_pruning: true, invalid: false, previous_remaining: u64::MAX,
            checkpoint_directories: 0, ranked_directories: 0, started: Instant::now(),
            profiling: std::env::var_os("QTOOLS_PROFILE").is_some(), next_profile: Duration::from_secs(1),
            accounting_time: Duration::ZERO, scheduling_time: Duration::ZERO, batches: 0, continuations: 0 }
    }

    fn collector(&self) -> SummaryFiles {
        SummaryFiles { selection: self.selection, bytes: 0, selected: 0, scanned: 0, hard_links: Vec::new(),
            bound_device: self.bound_device, check_file_inodes: self.bound.as_ref().is_some_and(Bound::check_file_inodes),
            bound_bytes: 0, allocations: Vec::new() }
    }

    fn receive(&mut self, event: WalkEvent<SummaryFiles>, queue: &mut WorkQueue) {
        if self.invalid { return; }
        let batch = match event {
            WalkEvent::Batch(batch) => batch,
            WalkEvent::Idle => { self.resume(queue); return; }
        };
        let accounting_started = self.profiling.then(Instant::now);
        self.account(&batch);
        self.account_links(batch.files.hard_links, batch.node);
        if let Some(bound) = &mut self.bound {
            bound.account_file_bytes(batch.files.bound_bytes);
            for (inode, bytes) in batch.files.allocations { bound.record(inode, bytes); }
        }
        if let Some(started) = accounting_started { self.accounting_time += started.elapsed(); }
        let scheduling_started = self.profiling.then(Instant::now);
        self.schedule(batch.node, batch.subdirs, batch.continuations, queue);
        if batch.finished_job { self.check_budget(queue); }
        if let Some(started) = scheduling_started { self.scheduling_time += started.elapsed(); }
    }

    fn account(&mut self, batch: &Batch<SummaryFiles>) {
        self.batches += 1; self.continuations += usize::from(!batch.directory_visit);
        self.directories += usize::from(batch.directory_visit);
        self.scanned_files += batch.files.scanned; self.selected_files += batch.files.selected as usize;
        if self.bound.is_some() {
            self.tree.observe_yield(batch.node, batch.scanned, batch.files.scanned as u64
                + if batch.directory_visit { 2 + batch.subdirs.len() as u64 } else { 1 });
        }
        if batch.directory_visit {
            let directory_metadata = self.query.selection.min_size.is_none() && self.query.selection.older_than.is_none();
            let new_metadata = !self.tree.nodes[batch.node].metadata_counted;
            let bytes = if new_metadata && directory_metadata && self.query.accounting == SizeAccounting::Allocated { batch.directory.allocated } else { 0 };
            self.tree.observe(batch.node, batch.files.bytes + bytes, batch.files.selected,
                directory_metadata || self.query.presentation.include_zeroes || batch.files.selected > 0);
            if batch.directory_inode != 0 && new_metadata {
                if let Some(bound) = &mut self.bound { bound.record(batch.directory_inode, batch.directory.allocated); }
                self.tree.nodes[batch.node].metadata_counted = true;
            }
        } else if batch.files.selected > 0 {
            self.tree.add_usage(batch.node, batch.files.bytes, batch.files.selected);
        }
    }

    fn account_links(&mut self, facts: Vec<FileFact>, node: NodeId) {
        let mut settled_usage: Option<(NodeId, u64, u64)> = None;
        for fact in facts {
            let group = match self.links.entry((fact.device, fact.inode)) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    if self.bound_device == Some(fact.device) {
                        if let Some(bound) = &mut self.bound { bound.record(fact.inode, fact.allocated); }
                        self.pending_link_bytes += fact.allocated;
                    }
                    entry.insert(HardLinkGroup { fact, directory: node, observed: 1, finalized: false })
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    self.duplicate_hard_links += 1;
                    entry.get_mut().observed += 1;
                    if (fact.allocated, fact.links) != (entry.get().fact.allocated, entry.get().fact.links) {
                        if let Some(bound) = &mut self.bound { bound.invalidate(); }
                    }
                    if fact.path.as_os_str() < entry.get().fact.path.as_os_str() {
                        entry.get_mut().fact = fact; entry.get_mut().directory = node;
                    }
                    entry.into_mut()
                }
            };
            if let Some(bound) = &mut self.bound {
                if group.observed > group.fact.links && group.finalized { bound.invalidate(); }
                else if bound.can_settle_links() && group.observed == group.fact.links && !group.finalized {
                    // Until all links are seen, an earlier path can change attribution.
                    if self.bound_device == Some(group.fact.device) {
                        self.pending_link_bytes = self.pending_link_bytes.saturating_sub(group.fact.allocated);
                    }
                    if settled_usage.as_ref().is_some_and(|usage| usage.0 != group.directory) {
                        let (directory, bytes, files) = settled_usage.take().unwrap();
                        self.tree.add_usage(directory, bytes, files);
                    }
                    let usage = settled_usage.get_or_insert((group.directory, 0, 0));
                    usage.1 += group.fact.allocated; usage.2 += 1;
                    self.selected_files += 1; group.finalized = true;
                }
            }
        }
        if let Some((directory, bytes, files)) = settled_usage { self.tree.add_usage(directory, bytes, files); }
    }

    fn schedule(&mut self, node: NodeId, children: Vec<Work>, continuations: Vec<Work>, queue: &mut WorkQueue) {
        let report_depth = *self.depths.iter().max().unwrap();
        let parent_closed = self.tree.is_closed(node);
        let mut child_metadata_bytes = 0;
        for mut work in children {
            let relative = if self.tree.nodes[node].depth < report_depth {
                work.path.strip_prefix(self.root).unwrap_or(&work.path)
            } else { Path::new("") };
            work.node = self.tree.add_child(node, relative);
            if let (Some(bound), Some(metadata)) = (&mut self.bound, work.metadata()) {
                // A child with fresh metadata can be credited before opening it.
                bound.record(metadata.inode, metadata.allocated);
                self.tree.precount_child(work.node, metadata.allocated);
                child_metadata_bytes += metadata.allocated;
                self.tree.nodes[work.node].metadata_counted = true;
            }
            self.enqueue(work, parent_closed, queue);
        }
        if child_metadata_bytes > 0 { self.tree.add_descendant_usage(node, child_metadata_bytes, 0); }
        for mut work in continuations {
            work.node = node; self.enqueue(work, parent_closed, queue);
        }
    }

    fn enqueue(&mut self, work: Work, closed: bool, queue: &mut WorkQueue) {
        if closed { self.deferred.push(work); }
        else { let rank = if queue.adaptive() { priority(&work, &self.tree) } else { 0 }; queue.push_ranked(work, rank); }
    }

    fn invalidate(&mut self, queue: &mut WorkQueue) {
        self.invalid = true;
        queue.clear(); self.deferred.clear();
    }

    fn check_budget(&mut self, queue: &mut WorkQueue) {
        if self.profiling && self.started.elapsed() >= self.next_profile {
            eprintln!("dq[profile]: elapsed={}ms files={} directories={} measured={} remaining={:?} pending-links={} queued={}",
                self.started.elapsed().as_millis(), self.scanned_files, self.directories, self.tree.nodes[0].bytes,
                self.bound.as_ref().and_then(Bound::remaining), self.pending_link_bytes, queue.len());
            self.next_profile = self.started.elapsed() + Duration::from_secs(1);
        }
        let Some(bound) = &mut self.bound else { return; };
        if self.tree.is_closed(0) { return; }
        let remaining = bound.remaining().or_else(|| { bound.refresh(self.root); bound.remaining() });
        let Some(remaining) = remaining else { self.invalidate(queue); return; };
        let stop_at = bound.capacity as f64 * self.query.traversal.max_error_percent / 100.0 * 0.99;
        let within_budget = remaining.saturating_add(self.pending_link_bytes) as f64 <= stop_at;
        let tightened = remaining < self.previous_remaining.saturating_mul(4) / 5;
        let ranking_due = self.directories - self.ranked_directories >= 4096;
        if self.directories != 1 && !within_budget && !tightened && !ranking_due
            && self.directories - self.checkpoint_directories < 16_384 { return; }
        bound.refresh(self.root);
        let Some(remaining) = bound.remaining() else { self.invalidate(queue); return; };
        self.previous_remaining = remaining; self.checkpoint_directories = self.directories;
        let stop_at = bound.capacity as f64 * self.query.traversal.max_error_percent / 100.0 * 0.99;
        let mut changed = false;
        if remaining.saturating_add(self.pending_link_bytes) as f64 <= stop_at {
            changed = !self.tree.is_closed(0); self.tree.close(0);
        } else if self.branch_pruning {
            for id in pruning::closed_branches(&self.tree, self.query, remaining.saturating_add(self.pending_link_bytes)) {
                changed |= !self.tree.is_closed(id); self.tree.close(id);
            }
        }
        if changed || tightened || ranking_due {
            queue.update(|work| {
                if self.tree.is_closed(work.node) { self.deferred.push(work); None }
                else { let rank = priority(&work, &self.tree); Some((work, rank)) }
            });
            self.ranked_directories = self.directories;
        }
    }

    fn resume(&mut self, queue: &mut WorkQueue) {
        let Some(bound) = &mut self.bound else { return; };
        bound.refresh(self.root);
        let Some(remaining) = bound.remaining() else { self.invalidate(queue); return; };
        if self.deferred.is_empty() || remaining.saturating_add(self.pending_link_bytes) as f64
            <= bound.capacity as f64 * self.query.traversal.max_error_percent / 100.0 { return; }
        // Keep every measurement, node, link group and pending filename. Only
        // untouched work re-enters the queue; the worker pool remains live.
        self.tree.reopen(); self.branch_pruning = false;
        for work in self.deferred.drain(..) {
            let rank = priority(&work, &self.tree); queue.push_ranked(work, rank);
        }
    }

    fn finish(mut self, errors: Vec<String>) -> Result<SummaryScan, ()> {
        if self.invalid { return Err(()); }
        let finalization_started = self.profiling.then(Instant::now);
        let pruned_directories = self.deferred.len();
        let link_groups = self.links.len();
        for group in self.links.into_values() {
            if group.finalized || pruned_directories > 0 { continue; }
            let bytes = self.selection.bytes(&group.fact);
            if !self.selection.matches(bytes, group.fact.modified) { continue; }
            self.selected_files += 1; self.tree.add_usage(group.directory, bytes, 1);
        }
        self.tree.finish_deep_totals();
        let levels = self.tree.levels(&self.depths).into_iter().map(|groups|
            groups.into_iter().map(|(relative, usage)| (self.root.join(relative), usage)).collect()).collect();
        if let Some(started) = finalization_started {
            eprintln!("dq[profile]: accounting={}ms scheduling={}ms", self.accounting_time.as_millis(), self.scheduling_time.as_millis());
            eprintln!("dq[profile]: batches={} continuations={}", self.batches, self.continuations);
            eprintln!("dq[profile]: finalization={}ms hard-link-groups={link_groups}", started.elapsed().as_millis());
        }
        let unmeasured_bytes_upper_bound = if pruned_directories > 0 { self.bound.as_ref().and_then(Bound::remaining) } else { None };
        let known_unassigned_bytes = if pruned_directories > 0 { self.pending_link_bytes } else { 0 };
        Ok(SummaryScan { root: self.root.to_path_buf(), levels, scanned_files: self.scanned_files, selected_files: self.selected_files,
            directories: self.directories, duplicate_hard_links: self.duplicate_hard_links, errors, duration_ms: self.started.elapsed().as_millis(),
            pruned_directories, unmeasured_bytes_upper_bound, known_unassigned_bytes })
    }
}

fn attempt(root: &Path, query: &DiskQuery, bound: Option<Bound>) -> Result<SummaryScan, ()> {
    let mut state = SummaryState::new(root, query, bound);
    let options = WalkOptions { threads: query.traversal.threads, one_file_system: query.traversal.one_file_system,
        excludes: &query.selection.excludes, show_progress: query.traversal.show_progress && !query.output.machine(), adaptive: state.bound.is_some() };
    let (errors, _) = walk(root, options, state.collector(), |event, queue| state.receive(event, queue));
    state.finish(errors)
}

pub(super) fn summarize_with_bound(root: &Path, query: &DiskQuery, bound: Option<Bound>) -> SummaryScan {
    let started = Instant::now();
    let mut scan = match attempt(root, query, bound) {
        Ok(scan) => scan,
        Err(()) => {
            // The failed attempt and all its allocations have been dropped.
            // A full traversal has no allocation-bound retry path.
            if std::env::var_os("QTOOLS_PROFILE").is_some() { eprintln!("dq[profile]: invalid allocation bound; completing a full traversal"); }
            attempt(root, query, None).expect("a complete traversal does not require an allocation bound")
        }
    };
    scan.duration_ms = started.elapsed().as_millis();
    scan
}

pub(in crate::dq) fn summarize(root: &Path, query: &DiskQuery) -> SummaryScan {
    summarize_with_bound(root, query, Bound::for_query(root, query))
}

#[cfg(test)]
mod tests;
