//! Band-filtered view of a batch: the instances visible in one LOD band, in batch order,
//! with per-bin slot tables so the grid can turn a query into slot ranges.
//!
//! Rebuilt only when the camera crosses a band boundary that changes membership (see
//! [`super::scene::ChangePoints`]); all buffers are reused across rebuilds.

use std::ops::Range;

use super::grid::{item_ranges, limit_ranges};
use super::scene::{Band, Batch, WorldRect};

#[derive(Debug, Default)]
pub struct BandBatch {
    /// Band the slots were built for (`None`: never built).
    pub band: Option<Band>,
    /// `slots[s]` is the batch index drawn at instance slot `s`.
    pub slots: Vec<u32>,
    /// Per scene layer: `table[b]..table[b + 1]` are the slots of bin `b`.
    pub tables: Vec<Vec<u32>>,
}

impl BandBatch {
    /// Recomputes the visible set for `band`.
    pub fn rebuild<T>(&mut self, batch: &Batch<T>, band: Band) {
        self.band = Some(band);
        self.slots.clear();
        self.tables.resize_with(batch.layers.len(), Vec::new);
        for (layer, table) in batch.layers.iter().zip(&mut self.tables) {
            table.clear();
            table.reserve(layer.bin_start.len());
            for w in layer.bin_start.windows(2) {
                table.push(self.slots.len() as u32);
                for i in w[0]..w[1] {
                    if batch.lod[i as usize].contains(band) {
                        self.slots.push(i);
                    }
                }
            }
            table.push(self.slots.len() as u32);
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Calls `f(first_slot, batch_range)` for each maximal run of consecutive batch indices
    /// (uploads copy whole runs at once).
    pub fn for_each_run(&self, mut f: impl FnMut(u32, Range<u32>)) {
        let mut i = 0usize;
        while i < self.slots.len() {
            let start = self.slots[i];
            let mut j = i + 1;
            while j < self.slots.len() && self.slots[j] == start + (j - i) as u32 {
                j += 1;
            }
            f(i as u32, start..start + (j - i) as u32);
            i = j;
        }
    }

    /// Slot ranges of layer `li` whose items may intersect `q`, at most `max_ranges` of them
    /// (merging across small gaps). Results go to `out`.
    pub fn query<T>(&self, batch: &Batch<T>, li: usize, q: &WorldRect, max_ranges: usize, scratch: &mut QueryScratch, out: &mut Vec<Range<u32>>) {
        out.clear();
        let (Some(layer), Some(table)) = (batch.layers.get(li), self.tables.get(li)) else { return };
        scratch.bins.clear();
        layer.grid.bin_ranges(q, &mut scratch.bins);
        item_ranges(&scratch.bins, table, out);
        limit_ranges(out, max_ranges, &mut scratch.gaps);
    }
}

/// Reusable buffers for [`BandBatch::query`].
#[derive(Debug, Default)]
pub struct QueryScratch {
    bins: Vec<Range<u32>>,
    gaps: Vec<(u32, u32)>,
}
