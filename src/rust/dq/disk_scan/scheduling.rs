//! Queue ordering from fresh byte yield. Priorities never certify a size bound.
use std::collections::VecDeque;

use super::super::directory_tree::DirectoryTree;
use super::path_yield::{PathYields, MIN_OBSERVATION_COST};
use super::traversal::{Work, WorkKind};

pub(super) type Priority = u8;

// Half-octave yield ranks occupy 0..=63. The first three chart levels have
// separate ranges above them; low ranks explore depths uniformly.
const MAX_YIELD_PRIORITY: Priority = 63;
const LOW_YIELD_MAX_PRIORITY: Priority = 16;
const DEPTH_BUCKET_COUNT: usize = 64;
const SHALLOW_LEVELS: usize = 3;
const SHALLOW_PRIORITY_STRIDE: usize = 64;
const UNKNOWN_YIELD_BYTES: u64 = 1024 * 1024;
// A fixed seed makes a given queue history reproducible, including reranking.
const EXPLORATION_SEED: u64 = 0x42d0_51a7_b831_f607;

// Logarithmic buckets keep push/pop independent of the frontier size.
struct PriorityBuckets {
    buckets: [VecDeque<Work>; 256], occupied: [u64; 4], len: usize,
    low_depths: [Vec<Work>; DEPTH_BUCKET_COUNT], low_occupied: u64, random: u64,
}

impl PriorityBuckets {
    fn new() -> Self {
        Self { buckets: std::array::from_fn(|_| VecDeque::new()), occupied: [0; 4], len: 0,
            low_depths: std::array::from_fn(|_| Vec::new()), low_occupied: 0, random: EXPLORATION_SEED }
    }
    fn push(&mut self, work: Work, priority: Priority, preserve_order: bool) {
        if priority <= LOW_YIELD_MAX_PRIORITY {
            // Choose depths uniformly rather than letting a populous, low-yield
            // subtree monopolize the frontier. Estimates never credit bytes.
            let depth = work.depth.min(DEPTH_BUCKET_COUNT - 1);
            self.low_depths[depth].push(work);
            self.low_occupied |= 1 << depth;
            self.len += 1;
            return;
        }
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
        if self.low_occupied == 0 { return None; }
        let mut ordinal = self.random_index(self.low_occupied.count_ones() as usize);
        let mut depths = self.low_occupied;
        while ordinal > 0 { depths &= depths - 1; ordinal -= 1; }
        let depth = depths.trailing_zeros() as usize;
        let index = self.random_index(self.low_depths[depth].len());
        let work = self.low_depths[depth].swap_remove(index);
        self.len -= 1;
        if self.low_depths[depth].is_empty() { self.low_occupied &= !(1 << depth); }
        Some(work)
    }
    fn random_index(&mut self, limit: usize) -> usize {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        self.random as usize % limit
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

pub(super) fn priority(work: &Work, tree: &DirectoryTree, hints: Option<&PathYields>) -> Priority {
    if let WorkKind::Files { bytes_per_file, .. } = &work.kind {
        return yield_priority(*bytes_per_file);
    }
    let depth = tree.nodes[work.node].depth;
    if depth == 0 { return 255; }
    // Establish the three chart levels before pursuing deeper detail.
    let shallow = if depth <= SHALLOW_LEVELS { SHALLOW_LEVELS + 1 - depth } else { 0 };
    let mut ancestor = tree.nodes[work.node].parent;
    let mut score = yield_priority(UNKNOWN_YIELD_BYTES);
    while ancestor != 0 {
        let node = &tree.nodes[ancestor];
        if node.yield_cost >= MIN_OBSERVATION_COST { score = yield_priority(node.yield_bytes / node.yield_cost); break; }
        ancestor = node.parent;
    }
    if let Some(prediction) = hints.and_then(|hints| hints.prediction(work.node)) { score = yield_priority(prediction); }
    // A gradual depth penalty lets productive file tails compete with long
    // runs of directory discovery while preserving measured-yield ordering.
    let score = score.saturating_sub(depth.saturating_sub(SHALLOW_LEVELS).min(usize::from(MAX_YIELD_PRIORITY)) as u8);
    (shallow * SHALLOW_PRIORITY_STRIDE + usize::from(score)) as Priority
}

fn yield_priority(average: u64) -> Priority {
    let average = average.max(1);
    let exponent = average.ilog2();
    let half = u32::from(exponent > 0 && average & (1 << (exponent - 1)) != 0);
    (exponent * 2 + half).min(u32::from(MAX_YIELD_PRIORITY)) as Priority
}
