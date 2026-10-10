//! A bounded directory breakdown using the same additive totals as the flat report.
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{chart, donut, style};
use super::dq_report::Row;

pub(super) type Totals = HashMap<PathBuf, (u64, u64)>;
pub(super) const MAX_CHILDREN: usize = 3;
pub(super) const MIN_PARENT_SHARE: f64 = 0.10;

pub(super) fn directory_prefix(relative: &Path, depth: usize) -> &OsStr {
    if depth == 0 { return OsStr::new(""); }
    let mut components = relative.components();
    for _ in 0..depth {
        if components.next().is_none() { return relative.as_os_str(); }
    }
    let rest = components.as_path().as_os_str();
    if rest.is_empty() { return relative.as_os_str(); }
    let bytes = relative.as_os_str().as_bytes();
    let mut end = bytes.len() - rest.len();
    while end > 1 && bytes[end - 1] == b'/' { end -= 1; }
    OsStr::from_bytes(&bytes[..end])
}

/// A continuous part of the hue wheel, subdivided according to the displayed tree.
#[derive(Clone, Copy)]
struct ColorSpace { start: f64, width: f64 }

impl ColorSpace {
    fn hue(self) -> f64 { (self.start + self.width / 2.0).rem_euclid(360.0) }

    fn divide(self, weights: &[usize]) -> Vec<Self> {
        let total = weights.iter().sum::<usize>() as f64;
        let mut start = self.start;
        weights.iter().map(|weight| {
            let width = self.width * *weight as f64 / total;
            let space = Self { start, width };
            start += width;
            space
        }).collect()
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Remainder {
    pub bytes: u64,
    pub files: u64,
    pub percent: f64,
    pub color: chart::Color,
}

#[derive(Debug, Serialize)]
pub(super) struct Node {
    pub id: String,
    pub path: Option<PathBuf>,
    pub label: String,
    pub bytes: u64,
    pub files: u64,
    pub percent: f64,
    pub color: chart::Color,
    pub children: Vec<Node>,
    pub remainder: Option<Remainder>,
}

#[derive(Debug, Serialize)]
pub(super) struct Breakdown {
    pub nodes: Vec<Node>,
    pub remainder: Option<Remainder>,
}

pub(super) fn build(root: &Path, rows: &[Row], total: u64, total_files: u64, levels: &[Totals], min_percent: f64) -> Breakdown {
    let mut nodes = rows.iter().enumerate().map(|(rank, row)| {
        let path = (row.label != "[root files and metadata]").then(|| PathBuf::from(&row.label));
        let label = path.as_ref().map_or_else(|| "[files here + metadata]".into(), |path| relative_label(root, path));
        let mut node = Node { id: (rank + 1).to_string(), path, label, bytes: row.bytes, files: row.files,
            percent: row.percent, color: style::OTHER_COLOR, children: Vec::new(), remainder: None };
        expand(&mut node, root, total, levels, min_percent.max(1.0));
        node
    }).collect::<Vec<_>>();
    color_tree(&mut nodes);
    let shown_bytes: u64 = nodes.iter().map(|node| node.bytes).sum();
    let shown_files: u64 = nodes.iter().map(|node| node.files).sum();
    Breakdown { nodes, remainder: (total > shown_bytes).then(|| Remainder {
        bytes: total - shown_bytes, files: total_files.saturating_sub(shown_files),
        percent: super::dq_report::percent(total - shown_bytes, total),
        color: style::OTHER_COLOR,
    }) }
}

fn expand(node: &mut Node, root: &Path, total: u64, levels: &[Totals], min_percent: f64) {
    let (Some(path), Some(totals)) = (&node.path, levels.first()) else { return };
    if node.bytes == 0 { return; }
    let mut children: Vec<_> = totals.iter().filter(|(child, (bytes, _))| {
        child.parent() == Some(path.as_path()) && *bytes > 0
            && *bytes as f64 / node.bytes as f64 >= MIN_PARENT_SHARE
            && *bytes as f64 / total as f64 * 100.0 >= min_percent
    }).collect();
    children.sort_by(|(a, av), (b, bv)| bv.0.cmp(&av.0).then_with(|| a.cmp(b)));
    children.truncate(MAX_CHILDREN);
    for (rank, (path, (bytes, files))) in children.into_iter().enumerate() {
        let mut child = Node { id: format!("{}.{}", node.id, rank + 1), path: Some(path.clone()),
            label: relative_label(root, path), bytes: *bytes, files: *files,
            percent: super::dq_report::percent(*bytes, total), color: node.color,
            children: Vec::new(), remainder: None };
        expand(&mut child, root, total, &levels[1..], min_percent);
        node.children.push(child);
    }
    if !node.children.is_empty() {
        let kept_bytes: u64 = node.children.iter().map(|child| child.bytes).sum();
        let kept_files: u64 = node.children.iter().map(|child| child.files).sum();
        node.remainder = (node.bytes > kept_bytes).then(|| Remainder {
            bytes: node.bytes - kept_bytes, files: node.files.saturating_sub(kept_files),
            percent: super::dq_report::percent(node.bytes - kept_bytes, total),
            color: node.color,
        });
    }
}

fn relative_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

fn color_weight(node: &Node) -> usize {
    node.children.iter().map(color_weight).sum::<usize>().max(1)
}

fn color_spaces(nodes: &[Node]) -> Vec<(usize, ColorSpace)> {
    // Interleave ranks around the wheel, so adjacent chart hierarchies do not
    // simply receive adjacent hues. Root files are a named category too.
    // Complex trees reserve room for more colors.
    let order: Vec<_> = (0..nodes.len()).step_by(2).chain((1..nodes.len()).step_by(2)).collect();
    let weights: Vec<_> = order.iter().map(|rank| color_weight(&nodes[*rank])).collect();
    order.into_iter().zip(ColorSpace { start: 0.0, width: 360.0 }.divide(&weights)).collect()
}

fn color_tree(nodes: &mut [Node]) {
    for (rank, space) in color_spaces(nodes) {
        let node = &mut nodes[rank];
        node.color = from_hsl(space.hue(), 0.70, 0.52);
        color_children(node, space, 1);
    }
}

fn color_children(node: &mut Node, space: ColorSpace, level: usize) {
    if let Some(remainder) = &mut node.remainder { remainder.color = remainder_color(node.color); }
    let lightness = to_hsl(node.color).2;
    let shades = if level == 1 { [0.64, 0.38, 0.76] }
        else if lightness >= 0.70 { [0.47, 0.64, 0.35] }
        else if lightness >= 0.60 { [0.46, 0.78, 0.35] }
        else { [0.63, 0.48, 0.76] };
    let weights: Vec<_> = node.children.iter().map(color_weight).collect();
    for (rank, (child, child_space)) in node.children.iter_mut().zip(space.divide(&weights)).enumerate() {
        child.color = from_hsl(child_space.hue(), [0.72, 0.78, 0.68][rank], shades[rank]);
        color_children(child, child_space, level + 1);
    }
}

fn to_hsl(color: chart::Color) -> (f64, f64, f64) {
    let (r, g, b) = (color.0 as f64 / 255.0, color.1 as f64 / 255.0, color.2 as f64 / 255.0);
    let high = r.max(g).max(b);
    let low = r.min(g).min(b);
    let delta = high - low;
    let lightness = (high + low) / 2.0;
    if delta == 0.0 { return (0.0, 0.0, lightness); }
    let hue = if high == r { ((g - b) / delta).rem_euclid(6.0) }
        else if high == g { (b - r) / delta + 2.0 } else { (r - g) / delta + 4.0 };
    (hue * 60.0, delta / (1.0 - (2.0 * lightness - 1.0).abs()), lightness)
}

fn from_hsl(hue: f64, saturation: f64, lightness: f64) -> chart::Color {
    let hue = hue.rem_euclid(360.0) / 60.0;
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let x = chroma * (1.0 - (hue.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match hue as u8 {
        0 => (chroma, x, 0.0), 1 => (x, chroma, 0.0), 2 => (0.0, chroma, x),
        3 => (0.0, x, chroma), 4 => (x, 0.0, chroma), _ => (chroma, 0.0, x),
    };
    let channel = |value: f64| ((value + lightness - chroma / 2.0) * 255.0).round() as u8;
    (channel(r), channel(g), channel(b))
}

fn remainder_color(color: chart::Color) -> chart::Color {
    let (hue, saturation, lightness) = to_hsl(color);
    // Keep the parent's hue while crossing its lightness, so an attributed
    // remainder reads as part of that hierarchy rather than unidentified space.
    from_hsl(hue, saturation.max(0.65), if lightness < 0.60 { 0.78 } else { 0.30 })
}

impl Breakdown {
    pub fn rings(&self) -> Vec<Vec<donut::LabeledSlice>> {
        let depth = self.nodes.iter().map(Node::depth).max().unwrap_or(1);
        (0..depth).map(|level| {
            let mut slices = Vec::new();
            for node in &self.nodes { node.slices(level, &mut slices); }
            if let Some(remainder) = &self.remainder {
                slices.push(donut::LabeledSlice { value: remainder.bytes, color: (level == 0).then_some(remainder.color),
                    label: if level == 0 { "*".into() } else { String::new() } });
            }
            slices
        }).collect()
    }

    pub fn next_path(&self) -> Option<&Path> {
        let mut node = self.nodes.iter().find(|node| node.path.is_some() && node.bytes > 0)?;
        while let Some(child) = node.children.first() { node = child; }
        node.path.as_deref()
    }
}

impl Node {
    fn depth(&self) -> usize { 1 + self.children.iter().map(Node::depth).max().unwrap_or(0) }

    fn slices(&self, level: usize, slices: &mut Vec<donut::LabeledSlice>) {
        if level == 0 || self.children.is_empty() {
            // Reserve the terminal branch's angle without inventing deeper detail.
            slices.push(donut::LabeledSlice { value: self.bytes, color: (level == 0).then_some(self.color),
                label: if level == 0 { self.id.clone() } else { String::new() } });
            return;
        }
        for child in &self.children { child.slices(level - 1, slices); }
        if let Some(remainder) = &self.remainder {
            slices.push(donut::LabeledSlice { value: remainder.bytes, color: (level == 1).then_some(remainder.color),
                label: if level == 1 { format!("{}.*", self.id) } else { String::new() } });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(children: Vec<Node>) -> Node {
        Node { id: String::new(), path: Some(PathBuf::from("directory")), label: String::new(), bytes: 0, files: 0,
            percent: 0.0, color: style::OTHER_COLOR, children, remainder: None }
    }

    fn hue_distance(a: chart::Color, b: chart::Color) -> f64 {
        let delta = (to_hsl(a).0 - to_hsl(b).0).rem_euclid(360.0);
        delta.min(360.0 - delta)
    }

    #[test]
    fn one_busy_parent_can_use_triadic_child_colors() {
        let mut nodes = vec![node((0..3).map(|_| node(Vec::new())).collect())];
        color_tree(&mut nodes);
        let children = &nodes[0].children;
        for (rank, child) in children.iter().enumerate() {
            for sibling in &children[..rank] {
                assert!(hue_distance(child.color, sibling.color) >= 119.0,
                    "a single hierarchy should use the whole color wheel");
            }
            assert!((to_hsl(child.color).2 - to_hsl(nodes[0].color).2).abs() >= 0.10);
        }
    }

    #[test]
    fn complex_hierarchies_receive_broad_distinct_hue_ranges() {
        let branch = |leaves| node((0..leaves).map(|_| node(Vec::new())).collect());
        let mut nodes = vec![node(vec![branch(1), branch(2), branch(3)]),
            node(vec![branch(3), branch(1)]), node(Vec::new()), node(Vec::new())];
        let spaces = color_spaces(&nodes);
        let busy_space = spaces.iter().find(|(rank, _)| *rank == 0).unwrap().1;
        let small_space = spaces.iter().find(|(rank, _)| *rank == 2).unwrap().1;
        assert!(busy_space.width >= 180.0 && busy_space.width >= 6.0 * small_space.width);
        color_tree(&mut nodes);
        for neighbors in nodes.windows(2) {
            assert!(hue_distance(neighbors[0].color, neighbors[1].color) >= 74.0,
                "adjacent hierarchies must have separated hues");
        }
        assert!(hue_distance(nodes[0].color, nodes[3].color) >= 74.0);
        assert!(hue_distance(nodes[0].children[0].color, nodes[0].children[2].color) >= 119.0,
            "a busy green parent must allow contrasting child hues");
        for (rank, space) in spaces {
            let parent = &nodes[rank];
            for descendant in parent.children.iter().flat_map(|child| std::iter::once(child).chain(&child.children)) {
                let offset = (to_hsl(descendant.color).0 - space.start).rem_euclid(360.0);
                assert!(offset <= space.width + 1.0, "descendants must stay in their hierarchy's allocated hue range");
            }
        }
    }

    #[test]
    fn root_files_and_metadata_have_their_own_color() {
        let mut files = node(Vec::new());
        files.path = None;
        let mut nodes = vec![files, node(Vec::new())];
        color_tree(&mut nodes);
        assert_ne!(nodes[0].color, style::OTHER_COLOR);
        assert!(hue_distance(nodes[0].color, nodes[1].color) >= 179.0);
        assert_eq!(color_spaces(&nodes).len(), 2);
    }

    #[test]
    fn attributed_remainders_are_colored_and_match_the_legend_at_their_own_level() {
        let mut parent = node(vec![node(vec![node(Vec::new())])]);
        parent.bytes = 100;
        parent.children[0].bytes = 70;
        parent.children[0].children[0].bytes = 40;
        parent.remainder = Some(Remainder { bytes: 30, files: 1, percent: 30.0, color: style::OTHER_COLOR });
        parent.children[0].remainder = Some(Remainder { bytes: 30, files: 1, percent: 30.0, color: style::OTHER_COLOR });
        parent.id = "1".into();
        parent.children[0].id = "1.1".into();
        let mut tree = Breakdown { nodes: vec![parent], remainder: None };
        color_tree(&mut tree.nodes);
        for branch in [&tree.nodes[0], &tree.nodes[0].children[0]] {
            let color = branch.remainder.as_ref().unwrap().color;
            assert_ne!(color, style::OTHER_COLOR);
            assert!(to_hsl(color).1 >= 0.60, "an attributed remainder must not look gray");
            assert!(hue_distance(color, branch.color) < 1.0);
            assert!((to_hsl(color).2 - to_hsl(branch.color).2).abs() >= 0.18);
        }
        let rings = tree.rings();
        let parent_remainder = tree.nodes[0].remainder.as_ref().unwrap().color;
        let child_remainder = tree.nodes[0].children[0].remainder.as_ref().unwrap().color;
        assert_eq!(rings[1].last().unwrap().color, Some(parent_remainder));
        assert_eq!(rings[2].last().unwrap().color, None);
        assert_eq!(rings[2][1].color, Some(child_remainder));
        for ring in rings { assert_eq!(ring.iter().map(|slice| slice.value).sum::<u64>(), 100); }
    }

    #[test]
    fn terminal_branches_leave_transparent_angles_without_shifting_deeper_siblings() {
        let root = Path::new("/scan");
        let rows = [("reference", 50), ("research", 30), ("img", 10), ("[root files and metadata]", 5)]
            .into_iter().map(|(label, bytes)| Row {
                label: if label.starts_with('[') { label.into() } else { root.join(label).display().to_string() },
                bytes, files: 1, percent: bytes as f64, uid: None, modified_unix: None,
                apparent_bytes: None, allocated_bytes: None, links: None,
            }).collect::<Vec<_>>();
        let levels = [
            [(root.join("reference/files"), (45, 1)), (root.join("research/data"), (30, 1))].into(),
            [(root.join("research/data/archive"), (30, 1))].into(),
        ];
        let tree = build(root, &rows, 100, 5, &levels, 1.0);
        let rings = tree.rings();
        let intervals = |ring: &[donut::LabeledSlice]| {
            let mut offset = 0;
            ring.iter().map(|slice| {
                let start = offset;
                offset += slice.value;
                (start, offset, slice.color.is_some())
            }).collect::<Vec<_>>()
        };
        assert_eq!(intervals(&rings[0]), [(0, 50, true), (50, 80, true), (80, 90, true), (90, 95, true), (95, 100, true)]);
        assert_eq!(intervals(&rings[1]), [(0, 45, true), (45, 50, true), (50, 80, true), (80, 90, false), (90, 95, false), (95, 100, false)]);
        assert_eq!(intervals(&rings[2]), [(0, 45, false), (45, 50, false), (50, 80, true), (80, 90, false), (90, 95, false), (95, 100, false)]);
        assert_eq!(rings[1][0].label, "1.1");
        assert_eq!(rings[2][2].label, "2.1.1");

        let image = donut::build_sunburst_image(&rings, 600).to_rgba8();
        let alpha = |fraction: f64, radius: f64| {
            let angle = fraction * std::f64::consts::TAU - std::f64::consts::FRAC_PI_2;
            image.get_pixel((300.0 + radius * angle.cos()).round() as u32,
                (300.0 + radius * angle.sin()).round() as u32).0[3]
        };
        assert_eq!(alpha(0.225, 189.0), 255, "reference/files is visible in the child ring");
        assert_eq!(alpha(0.225, 127.0), 0, "reference/files has no grandchild arc");
        assert_eq!(alpha(0.475, 127.0), 0, "an aggregate has no invented grandchildren");
        assert_eq!(alpha(0.65, 127.0), 255, "research's grandchild retains its original angle");
        for fraction in [0.85, 0.925, 0.975] {
            assert_eq!(alpha(fraction, 251.0), 255);
            assert_eq!(alpha(fraction, 189.0), 0);
            assert_eq!(alpha(fraction, 127.0), 0);
        }
    }
}
