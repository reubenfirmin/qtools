//! Current-scan yield hints shared by suffixes of directory paths.
//! These predictions affect ordering only; they never credit allocation.
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use super::super::directory_tree::{DirectoryTree, NodeId};

// Back off from five path components to shorter suffixes when samples are
// sparse. Eight observed work units are required before using a prediction.
const CONTEXT_LENGTH: usize = 5;
pub(super) const MIN_OBSERVATION_COST: u64 = 8;

#[derive(Default)]
struct Observation { bytes: u64, cost: u64 }

pub(super) struct PathYields {
    names: HashMap<Vec<u8>, usize>,
    contexts: HashMap<(usize, usize), usize>,
    observations: Vec<Observation>,
    nodes: Vec<[usize; CONTEXT_LENGTH]>,
}

impl PathYields {
    pub fn new() -> Self {
        let mut result = Self { names: HashMap::new(), contexts: HashMap::new(),
            observations: vec![Observation::default()], nodes: Vec::new() };
        result.add_node(0, None, OsStr::new("."));
        result
    }

    pub fn add_node(&mut self, node: NodeId, parent: Option<NodeId>, name: &OsStr) {
        assert_eq!(node, self.nodes.len());
        let name = normalize(name);
        let next_name = self.names.len() + 1;
        let name_id = *self.names.entry(name).or_insert(next_name);
        let mut keys = [0; CONTEXT_LENGTH];
        for (length, key) in keys.iter_mut().enumerate() {
            let prefix = if length == 0 { 0 } else {
                let Some(parent) = parent else { break };
                let prefix = self.nodes[parent][length - 1];
                if prefix == 0 { break; }
                prefix
            };
            let next_context = self.observations.len();
            *key = *self.contexts.entry((prefix, name_id)).or_insert(next_context);
            if *key == next_context { self.observations.push(Observation::default()); }
        }
        self.nodes.push(keys);
    }

    pub fn observe(&mut self, mut node: NodeId, bytes: u64, cost: u64, tree: &DirectoryTree) {
        loop {
            for &key in &self.nodes[node] {
                if key == 0 { break; }
                let observation = &mut self.observations[key];
                observation.bytes = observation.bytes.saturating_add(bytes);
                observation.cost = observation.cost.saturating_add(cost);
            }
            if node == 0 { break; }
            node = tree.nodes[node].parent;
        }
    }

    pub fn prediction(&self, node: NodeId) -> Option<u64> {
        self.nodes[node].iter().rev().filter(|&&key| key != 0).find_map(|&key| {
            let observation = &self.observations[key];
            (observation.cost >= MIN_OBSERVATION_COST).then(|| observation.bytes / observation.cost)
        })
    }
}

fn normalize(name: &OsStr) -> Vec<u8> {
    let name = name.as_bytes();
    if name.len() >= 2 && name.iter().all(u8::is_ascii_hexdigit) {
        format!("[hex:{}]", name.len().ilog2() + 1).into_bytes()
    } else { name.to_vec() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn add(tree: &mut DirectoryTree, hints: &mut PathYields, parent: NodeId, name: &str) -> NodeId {
        let node = tree.add_child(parent, Path::new(name));
        hints.add_node(node, Some(parent), OsStr::new(name));
        node
    }

    #[test]
    fn longer_supported_context_distinguishes_repeated_layouts_from_common_names() {
        let mut tree = DirectoryTree::new(3);
        let mut hints = PathYields::new();
        let project = add(&mut tree, &mut hints, 0, "project");
        let build = add(&mut tree, &mut hints, project, "build");
        let lib = add(&mut tree, &mut hints, build, "lib");
        hints.observe(lib, 8 * 1024 * 1024, 8, &tree);
        let installed = add(&mut tree, &mut hints, 0, "installed");
        let small_lib = add(&mut tree, &mut hints, installed, "lib");
        hints.observe(small_lib, 8192, 8, &tree);
        let other_project = add(&mut tree, &mut hints, 0, "other-project");
        let other_build = add(&mut tree, &mut hints, other_project, "build");
        let other_lib = add(&mut tree, &mut hints, other_build, "lib");
        assert_eq!(hints.prediction(other_lib), Some(1024 * 1024));
        let other_installed = add(&mut tree, &mut hints, 0, "installed");
        let other_small_lib = add(&mut tree, &mut hints, other_installed, "lib");
        assert_eq!(hints.prediction(other_small_lib), Some(1024));
        assert_eq!(tree.nodes[0].bytes, 0, "predictions must never enter size accounting");
    }

    #[test]
    fn unknown_contexts_have_no_prediction_and_hex_names_share_only_a_hint() {
        let mut tree = DirectoryTree::new(3);
        let mut hints = PathYields::new();
        let first = add(&mut tree, &mut hints, 0, "0123456789abcdef");
        assert_eq!(hints.prediction(first), None);
        hints.observe(first, 4096, 7, &tree);
        assert_eq!(hints.prediction(first), None);
        hints.observe(first, 4096, 1, &tree);
        let second = add(&mut tree, &mut hints, 0, "fedcba9876543210");
        assert_eq!(hints.prediction(second), Some(1024));
        assert_eq!(tree.nodes[0].bytes, 0);
    }
}
