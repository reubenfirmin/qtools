//! Queue ordering from fresh byte yield. Priorities never certify a size bound.
use std::collections::VecDeque;

use super::super::directory_tree::DirectoryTree;
use super::traversal::{Work, WorkKind};

pub(super) type Priority = u8;

// Logarithmic buckets keep push/pop independent of the frontier size.
struct PriorityBuckets {
    buckets: [VecDeque<Work>; 256], occupied: [u64; 4], len: usize,
}

impl PriorityBuckets {
    fn new() -> Self { Self { buckets: std::array::from_fn(|_| VecDeque::new()), occupied: [0; 4], len: 0 } }
    fn push(&mut self, work: Work, priority: Priority, preserve_order: bool) {
        let index = usize::from(priority);
        if preserve_order { self.buckets[index].push_front(work); }
        else { self.buckets[index].push_back(work); }
        self.occupied[index / 64] |= 1 << (index % 64);
        self.len += 1;
    }
    fn pop(&mut self) -> Option<Work> {
        for word in (0..4).rev() {
            if self.occupied[word] == 0 { continue; }
            let bit = 63 - self.occupied[word].leading_zeros() as usize;
            let index = word * 64 + bit;
            let work = self.buckets[index].pop_back();
            self.len -= 1;
            if self.buckets[index].is_empty() { self.occupied[word] &= !(1 << bit); }
            return work;
        }
        None
    }
}

pub(super) struct WorkQueue { fifo: VecDeque<Work>, buckets: Option<Box<PriorityBuckets>> }

impl WorkQueue {
    pub fn new(adaptive: bool) -> Self {
        Self { fifo: VecDeque::new(), buckets: adaptive.then(|| Box::new(PriorityBuckets::new())) }
    }
    pub fn adaptive(&self) -> bool { self.buckets.is_some() }
    pub fn len(&self) -> usize { self.buckets.as_ref().map_or(self.fifo.len(), |buckets| buckets.len) }
    pub fn is_empty(&self) -> bool { self.len() == 0 }
    pub fn clear(&mut self) {
        self.fifo.clear();
        if let Some(buckets) = &mut self.buckets { **buckets = PriorityBuckets::new(); }
    }
    pub fn pop_front(&mut self) -> Option<Work> {
        match &mut self.buckets { Some(buckets) => buckets.pop(), None => self.fifo.pop_front() }
    }
    pub fn push_ranked(&mut self, work: Work, priority: Priority) {
        if let Some(buckets) = &mut self.buckets { buckets.push(work, priority, false); }
        else { self.fifo.push_back(work); }
    }
    pub fn extend(&mut self, work: impl IntoIterator<Item = Work>) {
        for work in work { self.push_ranked(work, 0); }
    }
    pub fn update(&mut self, mut update: impl FnMut(Work) -> Option<(Work, Priority)>) {
        if let Some(mut old) = self.buckets.take() {
            self.buckets = Some(Box::new(PriorityBuckets::new()));
            while let Some(work) = old.pop() {
                if let Some((work, priority)) = update(work) {
                    self.buckets.as_mut().unwrap().push(work, priority, true);
                }
            }
        } else {
            let old = std::mem::take(&mut self.fifo);
            for work in old {
                if let Some((work, priority)) = update(work) { self.push_ranked(work, priority); }
            }
        }
    }
}

pub(super) fn priority(work: &Work, tree: &DirectoryTree) -> Priority {
    if let WorkKind::Files { bytes_per_file, .. } = &work.kind {
        // Tiny-file tails stay behind unopened directories.
        if *bytes_per_file <= 4096 { return 8; }
        return (16 + (*bytes_per_file).max(1).ilog2().saturating_sub(12)).min(63) as Priority;
    }
    let depth = tree.nodes[work.node].depth;
    if depth == 0 { return 255; }
    // Establish the three chart levels before pursuing deeper detail.
    let shallow = if depth <= 3 { 4 - depth } else { 0 };
    let mut ancestor = tree.nodes[work.node].parent;
    let mut score = 24;
    while ancestor != 0 {
        let node = &tree.nodes[ancestor];
        // Nearby samples keep a productive subtree from boosting tiny siblings.
        if node.yield_cost >= 8 {
            let average = node.yield_bytes / node.yield_cost;
            score = (16 + average.max(1).ilog2().saturating_sub(12) as usize).min(63);
            break;
        }
        ancestor = node.parent;
    }
    (shallow * 64 + score) as Priority
}
