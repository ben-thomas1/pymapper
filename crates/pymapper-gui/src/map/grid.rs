//! Own uniform spatial grid (a short pyramid of uniform grids) for culling and hit testing.
//!
//! Level 0 is a uniform grid sized for about [`ITEMS_PER_CELL`] items per cell. Level `l`
//! has cells [`LEVEL_FACTOR`]`^l` times larger, up to a top level with a single cell. An
//! item lives in exactly one bin: the cell containing its bbox centre, at the finest level
//! whose cells are at least as large as the item ("loose" grid). So every item lies inside
//! its cell expanded by half a cell, and a query only has to expand its rect by half a
//! cell per level; no item is stored twice and nothing is ever scanned linearly.
//!
//! Bins are numbered level by level, row-major within a level. Instances are stored sorted
//! by bin, so the items of a row of cells are one contiguous instance range: culling turns
//! into a handful of `draw(.., start..end)` calls over a static buffer.
//!
//! ```text
//! level 2  [            0            ]          bins: | L0 row0 | L0 row1 | ... | L1 | L2 |
//! level 1  [  0  |  1  ][  2  |  3  ]
//! level 0  [0|1|2|3|4|5|6|7] ...
//! ```

use std::ops::Range;

use super::scene::WorldRect;

/// Target average number of items per level-0 cell.
pub const ITEMS_PER_CELL: f32 = 8.0;
/// Cell size ratio between consecutive levels.
pub const LEVEL_FACTOR: u32 = 4;
/// Cap on level-0 cells per axis (memory of the bin table is ~cells * 4 bytes).
pub const MAX_DIM: u32 = 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    pub cols: u32,
    pub rows: u32,
    pub cell: [f32; 2],
    /// Index of this level's first bin.
    pub base: u32,
}

/// Geometry of one grid pyramid over `bounds`.
#[derive(Debug, Clone, PartialEq)]
pub struct GridGeom {
    pub origin: [f32; 2],
    pub levels: Vec<Level>,
    bins: u32,
}

impl GridGeom {
    /// A grid over `bounds` for about `items` items. Empty or degenerate bounds produce a
    /// single-cell grid (still correct, just unindexed).
    pub fn new(bounds: WorldRect, items: usize) -> Self {
        let finite = bounds.min.iter().chain(&bounds.max).all(|v| v.is_finite());
        if !finite || bounds.width() < 0.0 || bounds.height() < 0.0 {
            return Self::single([0.0, 0.0], [1.0, 1.0]);
        }
        // Zero-extent axes get a tiny positive extent so cell sizes stay positive.
        let w = bounds.width().max(f32::MIN_POSITIVE.sqrt());
        let h = bounds.height().max(f32::MIN_POSITIVE.sqrt());
        let cells = (items as f32 / ITEMS_PER_CELL).max(1.0);
        // Square cells: cell^2 * cells = w * h.
        let side = ((w * h) / cells).sqrt();
        let cols = ((w / side).ceil() as u32).clamp(1, MAX_DIM);
        let rows = ((h / side).ceil() as u32).clamp(1, MAX_DIM);
        let mut levels = Vec::new();
        let mut cell = [w / cols as f32, h / rows as f32];
        let (mut c, mut r) = (cols, rows);
        let mut base = 0u32;
        loop {
            levels.push(Level { cols: c, rows: r, cell, base });
            base += c * r;
            if c == 1 && r == 1 {
                break;
            }
            c = c.div_ceil(LEVEL_FACTOR);
            r = r.div_ceil(LEVEL_FACTOR);
            let f = LEVEL_FACTOR as f32;
            cell = [cell[0] * f, cell[1] * f];
            if c == 1 && r == 1 {
                // The top cell must cover everything, including items larger than bounds.
                cell = [f32::INFINITY, f32::INFINITY];
            }
        }
        Self { origin: bounds.min, levels, bins: base }
    }

    fn single(origin: [f32; 2], _cell: [f32; 2]) -> Self {
        let levels = vec![Level { cols: 1, rows: 1, cell: [f32::INFINITY; 2], base: 0 }];
        Self { origin, levels, bins: 1 }
    }

    pub const fn bins(&self) -> u32 {
        self.bins
    }

    /// Cell coordinate along one axis, clamped to the level (NaN -> 0).
    fn cell_coord(&self, v: f32, axis: usize, lv: &Level) -> u32 {
        let n = if axis == 0 { lv.cols } else { lv.rows };
        if n == 1 {
            return 0;
        }
        let c = ((v - self.origin[axis]) / lv.cell[axis]).floor();
        // `as` saturates: negatives and NaN become 0.
        (c as u32).min(n - 1)
    }

    /// The bin for an item with bounding box `r`.
    pub fn bin_of(&self, r: &WorldRect) -> u32 {
        let (w, h) = (r.width(), r.height());
        let c = r.center();
        let last = self.levels.len() - 1;
        let li = self.levels.iter().position(|lv| w <= lv.cell[0] && h <= lv.cell[1]).unwrap_or(last);
        let lv = &self.levels[li];
        lv.base + self.cell_coord(c[1], 1, lv) * lv.cols + self.cell_coord(c[0], 0, lv)
    }

    /// Pushes the bin ranges (half-open, ascending, non-overlapping) whose items may
    /// intersect `q`: per level, the cells overlapping `q` expanded by half a cell, one
    /// range per row of cells (rows merge when they span the full level width).
    pub fn bin_ranges(&self, q: &WorldRect, out: &mut Vec<Range<u32>>) {
        if !(q.max[0] >= q.min[0] && q.max[1] >= q.min[1]) {
            return;
        }
        for lv in &self.levels {
            let hx = lv.cell[0] * 0.5;
            let hy = lv.cell[1] * 0.5;
            if lv.cols == 1 && lv.rows == 1 {
                push_merged(out, lv.base..lv.base + 1);
                continue;
            }
            // Reject levels the query misses entirely (beyond the half-cell margin).
            let gx1 = self.origin[0] + lv.cell[0] * lv.cols as f32;
            let gy1 = self.origin[1] + lv.cell[1] * lv.rows as f32;
            if q.max[0] + hx < self.origin[0] || q.min[0] - hx > gx1 || q.max[1] + hy < self.origin[1] || q.min[1] - hy > gy1 {
                continue;
            }
            let x0 = self.cell_coord(q.min[0] - hx, 0, lv);
            let x1 = self.cell_coord(q.max[0] + hx, 0, lv);
            let y0 = self.cell_coord(q.min[1] - hy, 1, lv);
            let y1 = self.cell_coord(q.max[1] + hy, 1, lv);
            for y in y0..=y1 {
                let row = lv.base + y * lv.cols;
                push_merged(out, row + x0..row + x1 + 1);
            }
        }
    }
}

/// Appends `r`, merging with the last range when contiguous. Empty ranges are dropped.
pub fn push_merged(out: &mut Vec<Range<u32>>, r: Range<u32>) {
    if r.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.end == r.start
    {
        last.end = r.end;
        return;
    }
    out.push(r);
}

/// Maps bin ranges to item ranges through a bin start table (`starts[b]..starts[b + 1]`
/// are the items of bin `b`), merging contiguous results and dropping empty ones.
pub fn item_ranges(bins: &[Range<u32>], starts: &[u32], out: &mut Vec<Range<u32>>) {
    for b in bins {
        let r = starts[b.start as usize]..starts[b.end as usize];
        push_merged(out, r);
    }
}

/// Reduces ascending, disjoint ranges to at most `max` by merging across the smallest gaps
/// (the merged-in gap items are drawn too, which is harmless: they are just culled by the
/// GPU). Keeps draw calls bounded at a near-minimal overdraw cost. `gaps` is scratch.
pub fn limit_ranges(ranges: &mut Vec<Range<u32>>, max: usize, gaps: &mut Vec<(u32, u32)>) {
    let max = max.max(1);
    if ranges.len() <= max {
        return;
    }
    gaps.clear();
    for (i, w) in ranges.windows(2).enumerate() {
        gaps.push((w[1].start - w[0].end, i as u32));
    }
    // Keep the (max - 1) largest gaps as splits; merge across all others. Ties break by
    // index so the result is deterministic.
    let keep = max - 1;
    let cut = gaps.len() - keep;
    if keep > 0 {
        gaps.select_nth_unstable_by_key(cut, |g| (g.0, g.1));
    }
    let splits = &mut gaps[cut..];
    splits.sort_unstable_by_key(|g| g.1);
    let first = ranges[0].start;
    let last = ranges[ranges.len() - 1].end;
    // Range k of the result runs from just after split k-1 to split k.
    let mut start = first;
    let mut w = 0usize;
    for g in splits.iter() {
        let i = g.1 as usize;
        let end = ranges[i].end;
        let next = ranges[i + 1].start;
        ranges[w] = start..end;
        w += 1;
        start = next;
    }
    ranges[w] = start..last;
    ranges.truncate(w + 1);
}

/// Computes per-item bins for `bboxes` and the counting-sort permutation that orders the
/// items by bin (stable). Returns `(perm, starts)` where `perm[k]` is the original index of
/// the k-th sorted item and `starts` has `geom.bins() + 1` entries.
pub fn sort_by_bin(geom: &GridGeom, bboxes: impl Iterator<Item = WorldRect>, n: usize) -> (Vec<u32>, Vec<u32>) {
    let mut bin = Vec::with_capacity(n);
    let mut starts = vec![0u32; geom.bins() as usize + 1];
    for r in bboxes {
        let b = geom.bin_of(&r);
        starts[b as usize + 1] += 1;
        bin.push(b);
    }
    for i in 1..starts.len() {
        starts[i] += starts[i - 1];
    }
    let mut next = starts.clone();
    let mut perm = vec![0u32; bin.len()];
    for (i, &b) in bin.iter().enumerate() {
        let slot = &mut next[b as usize];
        perm[*slot as usize] = i as u32;
        *slot += 1;
    }
    (perm, starts)
}
