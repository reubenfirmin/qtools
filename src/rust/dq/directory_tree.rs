//! In-memory scan tree. Chart ancestors stay cumulative during traversal; deeper
//! totals are accumulated once at the end instead of on every observation.
use std::path::{Path, PathBuf};

use super::hierarchy::Totals;

pub(super) type NodeId = usize;
const NONE: NodeId = usize::MAX;

pub(super) struct Node {
    pub parent: NodeId,
    pub depth: usize,
    pub bytes: u64,
    pub files: u64,
    pub yield_bytes: u64,
    pub yield_cost: u64,
    pub metadata_counted: bool,
    pub path: Option<PathBuf>,
    direct: (u64, u64),
    direct_present: bool,
    present: bool,
    branch: NodeId,
    first_child: NodeId,
    next_sibling: NodeId,
    closed: bool,
}

pub(super) struct DirectoryTree {
    pub nodes: Vec<Node>,
    report_nodes: Vec<NodeId>,
    report_depth: usize,
}

impl DirectoryTree {
    pub fn new(report_depth: usize) -> Self {
        Self { nodes: vec![Node { parent: NONE, depth: 0, bytes: 0, files: 0, yield_bytes: 0, yield_cost: 0,
            metadata_counted: false, path: Some(PathBuf::new()), direct: (0, 0), direct_present: false, present: false,
            branch: 0, first_child: NONE, next_sibling: NONE, closed: false }],
            report_nodes: vec![0], report_depth }
    }

    pub fn add_child(&mut self, parent: NodeId, relative: &Path) -> NodeId {
        let id = self.nodes.len();
        let depth = self.nodes[parent].depth + 1;
        let displayed = depth <= self.report_depth;
        self.nodes.push(Node { parent, depth, bytes: 0, files: 0, yield_bytes: 0, yield_cost: 0,
            metadata_counted: false, path: displayed.then(|| relative.to_path_buf()), direct: (0, 0), direct_present: false, present: false,
            branch: if displayed { id } else { self.nodes[parent].branch },
            first_child: NONE, next_sibling: self.nodes[parent].first_child, closed: false });
        self.nodes[parent].first_child = id;
        if displayed { self.report_nodes.push(id); }
        id
    }

    pub fn observe(&mut self, id: NodeId, bytes: u64, files: u64, included: bool) {
        if included {
            self.nodes[id].direct.0 += bytes; self.nodes[id].direct.1 += files;
            self.nodes[id].direct_present = true;
        }
        let branch = self.nodes[id].branch;
        if branch != id {
            let node = &mut self.nodes[id];
            if included { node.bytes += bytes; node.files += files; node.present = true; }
        }
        let mut ancestor = branch;
        while ancestor != NONE {
            let node = &mut self.nodes[ancestor];
            if included { node.bytes += bytes; node.files += files; node.present = true; }
            ancestor = node.parent;
        }
    }

    pub fn add_usage(&mut self, id: NodeId, bytes: u64, files: u64) {
        self.nodes[id].direct.0 += bytes;
        self.nodes[id].direct.1 += files;
        self.nodes[id].direct_present = true;
        if self.nodes[id].branch != id {
            self.nodes[id].bytes += bytes; self.nodes[id].files += files; self.nodes[id].present = true;
        }
        self.add_descendant_usage(id, bytes, files);
    }

    pub fn precount_child(&mut self, id: NodeId, bytes: u64) {
        let node = &mut self.nodes[id];
        node.direct.0 += bytes; node.bytes += bytes;
        node.direct_present = true; node.present = true;
    }

    pub fn add_descendant_usage(&mut self, id: NodeId, bytes: u64, files: u64) {
        let mut ancestor = self.nodes[id].branch;
        while ancestor != NONE {
            let node = &mut self.nodes[ancestor];
            node.bytes += bytes; node.files += files; node.present = true;
            ancestor = node.parent;
        }
    }

    pub fn observe_yield(&mut self, mut id: NodeId, bytes: u64, cost: u64) {
        while id != NONE {
            let node = &mut self.nodes[id];
            node.yield_bytes += bytes;
            node.yield_cost += cost;
            id = node.parent;
        }
    }

    pub fn finish_deep_totals(&mut self) {
        // Parent IDs precede child IDs. Report nodes already include every
        // descendant; only the deeper nodes need this child-first accumulation.
        for id in (1..self.nodes.len()).rev() {
            let parent = self.nodes[id].parent;
            if self.nodes[parent].depth <= self.report_depth { continue; }
            let (bytes, files, present) = {
                let node = &self.nodes[id]; (node.bytes, node.files, node.present)
            };
            let node = &mut self.nodes[parent];
            node.bytes += bytes; node.files += files; node.present |= present;
        }
    }

    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut next = self.nodes[id].first_child;
        std::iter::from_fn(move || {
            if next == NONE { return None; }
            let current = next;
            next = self.nodes[current].next_sibling;
            Some(current)
        })
    }

    pub fn root_groups(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.children(0).chain(self.nodes[0].direct_present.then_some(0))
    }

    pub fn group_bytes(&self, id: NodeId) -> u64 {
        if id == 0 { self.nodes[0].direct.0 } else { self.nodes[id].bytes }
    }

    pub fn close(&mut self, id: NodeId) { self.nodes[id].closed = true; }
    pub fn reopen(&mut self) {
        for &id in &self.report_nodes { self.nodes[id].closed = false; }
    }

    pub fn is_closed(&self, id: NodeId) -> bool {
        let mut ancestor = self.nodes[id].branch;
        while ancestor != NONE {
            if self.nodes[ancestor].closed { return true; }
            ancestor = self.nodes[ancestor].parent;
        }
        false
    }

    pub fn levels(&self, depths: &[usize]) -> Vec<Totals> {
        depths.iter().map(|depth| {
            self.report_nodes.iter().filter_map(|id| {
                let node = &self.nodes[*id];
                let usage = if node.depth == *depth && node.present { (node.bytes, node.files) }
                    else if node.depth < *depth && node.direct_present { node.direct }
                    else { return None; };
                Some((node.path.as_ref().unwrap().clone(), usage))
            }).collect()
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cumulative_sizes_and_direct_allocations_match_each_report_depth() {
        let mut tree = DirectoryTree::new(3);
        let code = tree.add_child(0, Path::new("code"));
        let sites = tree.add_child(code, Path::new("code/sites"));
        let leaf = tree.add_child(sites, Path::new("code/sites/coalicionfloresta"));
        let deep = tree.add_child(leaf, Path::new("code/sites/coalicionfloresta/deep"));
        tree.observe(0, 4, 0, true); tree.observe(code, 10, 1, true);
        tree.observe(sites, 20, 1, true); tree.observe(leaf, 30, 1, true);
        tree.observe(deep, 100, 2, true);
        assert_eq!(tree.nodes[0].bytes, 164);
        assert_eq!(tree.nodes[code].bytes, 160);
        assert_eq!(tree.nodes[sites].bytes, 150);
        assert_eq!(tree.nodes[leaf].bytes, 130);
        let levels = tree.levels(&[1, 2, 3]);
        assert_eq!(levels[0][Path::new("code")], (160, 5));
        assert_eq!(levels[1][Path::new("code")], (10, 1));
        assert_eq!(levels[1][Path::new("code/sites")], (150, 4));
        assert_eq!(levels[2][Path::new("code/sites")], (20, 1));
        assert_eq!(levels[2][Path::new("code/sites/coalicionfloresta")], (130, 3));
        tree.close(sites); assert!(tree.is_closed(deep)); assert!(!tree.is_closed(code));
        tree.reopen(); assert!(!tree.is_closed(deep));
        tree.close(0); assert!(tree.is_closed(code));
    }

    #[test]
    fn deep_totals_finish_without_double_counting_live_chart_ancestors() {
        let mut tree = DirectoryTree::new(1);
        let shown = tree.add_child(0, Path::new("shown"));
        let deep = tree.add_child(shown, Path::new("shown/deep"));
        let leaf = tree.add_child(deep, Path::new("shown/deep/leaf"));
        tree.precount_child(deep, 4);
        tree.add_descendant_usage(shown, 4, 0);
        tree.precount_child(leaf, 8);
        tree.add_descendant_usage(deep, 8, 0);
        tree.observe(deep, 20, 1, true);
        tree.observe(leaf, 30, 2, true);
        tree.add_usage(leaf, 40, 1);
        assert_eq!((tree.nodes[shown].bytes, tree.nodes[shown].files), (102, 4));
        tree.finish_deep_totals();
        assert_eq!((tree.nodes[deep].bytes, tree.nodes[deep].files), (102, 4));
        assert_eq!((tree.nodes[leaf].bytes, tree.nodes[leaf].files), (78, 3));
        assert_eq!((tree.nodes[shown].bytes, tree.nodes[shown].files), (102, 4));
        assert_eq!(tree.nodes[0].bytes, 102);
    }
}
