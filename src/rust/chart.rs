//! Shared chart building: turn (label, value) items into palette-colored segments, ready to render
//! as a donut (via `donut::slices`) or as bar/legend rows. Generalizes dq's folder/file legend
//! building so `pq` can reuse the same rendering primitives for process clusters.

use crate::donut;

const PALETTE: [(u8, u8, u8); 12] = crate::style::PALETTE;
const OTHER: (u8, u8, u8) = crate::style::OTHER_COLOR;
pub type Color = (u8, u8, u8);

/// Bar width (in characters) for the text-mode proportional bars, shared by dq and pq.
pub const BAR_WIDTH: usize = 20;
/// Width of the header's horizontal rule, shared by dq and pq.
pub const RULE_WIDTH: usize = 52;
/// Floor on a truncated label's width, shared by dq and pq.
pub const MIN_LABEL_WIDTH: usize = 12;
/// Square pixel canvas a donut is rasterized into (viuer scales it to the cell box).
pub const DONUT_PX: u32 = 600;

/// One chart entry: a label, its value, and the color it should be drawn with.
pub struct Segment {
    pub label: String,
    pub value: u64,
    pub color: Color
}

/// Give ordered process identities stable preferred palette slots, resolving collisions by moving
/// the lower-ranked identity to the next free slot. This is deliberately stateless: absent
/// identities reserve nothing, the first palette-sized set always gets distinct colors, and the
/// same identity keeps its preferred color across report types whenever that slot is uncontested.
pub fn identity_colors<'a>(identities: impl IntoIterator<Item = &'a str>) -> Vec<Color> {
    let mut used = [false; PALETTE.len()];
    identities.into_iter().enumerate().map(|(rank, identity)| {
        if rank >= PALETTE.len() { return OTHER; }
        let preferred = stable_identity_hash(identity) as usize % PALETTE.len();
        let slot = (0..PALETTE.len()).map(|offset| (preferred + offset) % PALETTE.len())
            .find(|slot| !used[*slot]).expect("a palette slot exists for every visible identity");
        used[slot] = true;
        PALETTE[slot]
    }).collect()
}

/// FNV-1a with an explicit seed and ASCII case folding. Unlike a hash-map hasher, this is stable
/// between processes and builds, matching pq's case-insensitive identity selection semantics.
fn stable_identity_hash(identity: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in identity.bytes() {
        hash ^= byte.to_ascii_lowercase() as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Process-identity variant of `segments`: preserve input rank (including zero-valued entries) so
/// the swatch in a report row and that identity's donut slice always use the same stable color.
pub fn identity_segments(items: &[(String, u64)], total: u64, max_slices: usize) -> Vec<Segment> {
    let k = items.len().min(max_slices).min(PALETTE.len());
    let colors = identity_colors(items[..k].iter().map(|(identity, _)| identity.as_str()));
    let mut out: Vec<Segment> = items[..k].iter().zip(colors).map(|((label, value), color)| {
        Segment { label: label.clone(), value: *value, color }
    }).collect();
    let kept: u64 = out.iter().map(|segment| segment.value).sum();
    if total > kept {
        out.push(Segment { label: "other".to_string(), value: total - kept, color: OTHER });
    }
    out
}

/**
 * Build palette-colored segments from (label, value) items, largest kept, remainder folded into
 * one gray "other" segment sized `other = total - kept_sum` (pass total==sum for a plain roll-up).
 */
pub fn segments(items: &[(String, u64)], total: u64, max_slices: usize) -> Vec<Segment> {
    segments_labeled(items, total, max_slices, |_folded| "other".to_string())
}

/// Like `segments`, but the "other" segment's label is computed from the number of items folded
/// into it (e.g. pq's "N smaller files"), instead of the plain "other".
pub fn segments_labeled(items: &[(String, u64)], total: u64, max_slices: usize, other_label: impl Fn(usize) -> String) -> Vec<Segment> {
    let visible: Vec<&(String, u64)> = items.iter().filter(|(_, v)| *v > 0).collect();
    let k = visible.len().min(max_slices).min(PALETTE.len());
    let mut out = Vec::new();
    let mut kept = 0u64;
    for i in 0..k {
        out.push(Segment { label: visible[i].0.clone(), value: visible[i].1, color: PALETTE[i] });
        kept += visible[i].1;
    }
    let other = total.saturating_sub(kept);
    if other > 0 {
        out.push(Segment { label: other_label(visible.len() - k), value: other, color: OTHER });
    }
    out
}

/// Convert segments into `donut::Slice`s for rasterizing.
pub fn slices(segments: &[Segment]) -> Vec<donut::Slice> {
    segments.iter().map(|s| (s.value, s.color)).collect()
}

/// How many of `width` bar cells should be filled for a `frac` (0.0-1.0, or slightly over from
/// independent rounding skew) proportion, rounded and clamped so it never exceeds the bar.
pub fn bar_fill(frac: f64, width: usize) -> usize {
    ((frac * width as f64).round() as usize).min(width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_fold_remainder_into_other() {
        let items: Vec<(String, u64)> = (0..20).map(|i| (format!("p{i}"), 100 - i as u64)).collect();
        let sum: u64 = items.iter().map(|(_, v)| v).sum();
        let segs = segments(&items, sum, 12);
        assert_eq!(segs.len(), 13); // 12 + other
        assert_eq!(segs.last().unwrap().label, "other");
        // other == total - kept
        let kept: u64 = segs[..12].iter().map(|s| s.value).sum();
        assert_eq!(segs.last().unwrap().value, sum - kept);
    }

    #[test]
    fn identity_colors_are_stable_unique_until_the_palette_is_full_and_then_gray() {
        let home = identity_colors(["chrome"])[0];
        assert_eq!(identity_colors(["CHROME"])[0], home);
        let non_conflicting = (0..100).map(|i| format!("candidate-{i}"))
            .find(|identity| identity_colors([identity.as_str()])[0] != home).unwrap();
        assert_eq!(identity_colors([non_conflicting.as_str(), "chrome"])[1], home);

        let names: Vec<String> = (0..PALETTE.len() + 2).map(|i| format!("process-{i}")).collect();
        let colors = identity_colors(names.iter().map(String::as_str));
        let unique: std::collections::HashSet<Color> = colors[..PALETTE.len()].iter().copied().collect();
        assert_eq!(unique.len(), PALETTE.len());
        assert!(colors[PALETTE.len()..].iter().all(|color| *color == OTHER));
    }
}
