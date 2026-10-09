//! Deterministic squarified treemap layout of the hierarchy (design: GUI, "code map"). GUI
//! agnostic: plain rects in world units, testable without a GPU.
//!
//! ```text
//!   Index + Aggregates ──> LayoutTree        pre-order shape (end, depth), effective weight per
//!   (or a synthetic tree)   │                node, nodes grouped by depth, origin of each root,
//!                           │                64-bit hierarchy hash
//!                           └─> Layout       levels of rects, computed lazily one depth at a
//!                                │           time: level d+1 from the rects of level d
//!                                ├─> LayoutWorker   the same on a background thread: requests
//!                                │                  (depth), published levels (Arc), notify
//!                                └─> store          on-disk cache keyed by hierarchy hash +
//!                                                   config fingerprint
//! ```
//!
//! # World and regions
//!
//! The world is [`LayoutConfig::world`]. Distributions (depth 0) are split by origin: first
//! party gets the main region; third-party and stdlib share a separate, muted region along the
//! long side of the world (right of a landscape world), at most
//! [`LayoutConfig::external_share`] of it, less if they are smaller than that share by weight
//! (a venv of millions of lines never squeezes the project into a corner). Inside the external
//! region, third-party comes before stdlib (top, or left), split by weight. Each region is
//! squarified on its own, so toggling the venv or the stdlib only changes the width of the
//! main region: first-party code never mixes with external code and keeps its relative areas. [`Layout::muted`] tells the renderer which nodes to
//! draw muted.
//!
//! # Weights
//!
//! A node's weight is its [`SizeMetric`] value (LOC by default), at least
//! [`TreeOptions::min_weight`] and at least the sum of its children's weights (metrics like
//! LOC count the defs of a module inside the module's lines, while sums like `cc_sum` skip
//! classes: taking the max keeps every parent large enough for its children). What a parent
//! has beyond its children (module-level code, a package's `__init__` lines) is an unlabelled
//! filler cell laid out with the children, so areas stay proportional to weight within a
//! parent.
//!
//! # Per parent: padding, order, squarify
//!
//! A node with children gets a content rect: its rect inset by a padding on the sides and
//! bottom and a header band on top for the label. Both are absolute world sizes capped by a
//! fraction of the rect ([`LayoutConfig::pad`], [`LayoutConfig::header`]), so deep levels keep
//! most of their area. When the inset leaves no area, the content rect is the whole rect.
//! Children (and the filler) are ordered by weight descending, ties by pre-order (node id), or
//! kept in pre-order ([`Order::Preorder`], stable sibling order under edits at the cost of
//! aspect ratios), then squarified into the content rect.
//!
//! # Determinism and stability
//!
//! The layout is a pure function of the tree (shape, weights, root origins) and the config:
//! no hashing order, no thread-count dependence (levels are split over threads by parent,
//! every parent writes only its children's cells). The same hierarchy gives bit-identical
//! rects on every run and machine with IEEE `f64`. [`LayoutTree::hash`] covers shape, names,
//! kinds, weights and origins; with [`LayoutConfig::fingerprint`] it keys the on-disk cache
//! ([`cache_path`], [`load_levels`], [`save_levels`]), so a restart with an unchanged
//! hierarchy loads the levels it had instead of computing them. Any change of a weight
//! changes the hash: the cache never serves positions of another hierarchy.
//!
//! # Levels and laziness
//!
//! Level `d` holds the rects of every node at depth `d`, in pre-order: a parent's children
//! are a contiguous run of the next level, so level `d + 1` is one pass over level `d`.
//! [`Layout::ensure`] computes levels up to a depth; [`LayoutWorker`] does the same on its own
//! thread for the GUI, publishing each finished level as an `Arc<[Rect]>` (no copy on read)
//! and calling a notify hook (the repaint request). Semantic zoom asks only for the depths it
//! shows, so function-level rects of a venv are computed only when someone zooms that far.
//!
//! Cost: O(n log d) per level (sorting each parent's `d` children), parallel over parents for
//! levels above 32k nodes. Memory: 16 bytes per laid-out node (rects) plus 18 bytes per node
//! in the tree (end, depth, weight, slot, by-level order). Measured (`benches/layout.rs`, see
//! docs/benchmarks.md): the venv + stdlib (912k nodes) in 5.8 ms on 14 cores.

mod engine;
mod squarify;
mod store;
mod tree;
mod worker;

#[cfg(test)]
mod tests;

pub use engine::Layout;
pub use store::{StoreError, cache_key, cache_path, load_levels, save_levels};
pub use tree::{LayoutTree, SizeMetric, TreeOptions};
pub use worker::LayoutWorker;

use crate::roots::Origin;

/// An axis-aligned rect in world units: `x0 <= x1`, `y0 <= y1` (y grows downwards).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Rect {
    pub const fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect { x0, y0, x1, y1 }
    }

    #[inline]
    pub fn w(&self) -> f32 {
        self.x1 - self.x0
    }

    #[inline]
    pub fn h(&self) -> f32 {
        self.y1 - self.y0
    }

    #[inline]
    pub fn area(&self) -> f32 {
        self.w() * self.h()
    }

    /// `other` lies inside `self` (edges may touch).
    #[inline]
    pub fn contains(&self, other: &Rect) -> bool {
        other.x0 >= self.x0 && other.y0 >= self.y0 && other.x1 <= self.x1 && other.y1 <= self.y1
    }
}

/// Where a distribution goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Region {
    /// First-party code: the main region.
    Main = 0,
    /// Third-party distributions (muted region).
    ThirdParty = 1,
    /// The stdlib (muted region).
    Stdlib = 2,
}

impl Region {
    pub fn of(origin: Origin) -> Region {
        match origin {
            Origin::FirstParty => Region::Main,
            Origin::ThirdParty => Region::ThirdParty,
            Origin::Stdlib => Region::Stdlib,
        }
    }

    #[inline]
    pub(crate) fn from_u8(v: u8) -> Region {
        match v {
            0 => Region::Main,
            1 => Region::ThirdParty,
            _ => Region::Stdlib,
        }
    }
}

/// Order of siblings before squarifying.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Order {
    /// Weight descending, ties by node id: the squarified order (best aspect ratios).
    #[default]
    Size,
    /// Pre-order (source order for defs, name order for modules): siblings keep their place
    /// when weights change; aspect ratios are worse.
    Preorder,
}

/// Layout parameters. All sizes are world units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutConfig {
    /// The whole map. Keep coordinates within about +-2^14 (f32 precision of the renderer).
    pub world: Rect,
    /// Largest share of the world's long side given to third-party + stdlib together (0..1).
    pub external_share: f32,
    /// Gap between the main and the external region.
    pub region_gap: f32,
    /// Padding inside a parent on the left, right and bottom ...
    pub pad: f32,
    /// ... capped at this fraction of the parent's shorter side.
    pub pad_frac: f32,
    /// Header band at the top of a parent (label space) ...
    pub header: f32,
    /// ... capped at this fraction of the parent's height.
    pub header_frac: f32,
    pub order: Order,
}

impl Default for LayoutConfig {
    fn default() -> LayoutConfig {
        LayoutConfig {
            world: Rect::new(0.0, 0.0, 16000.0, 10000.0),
            external_share: 0.3,
            region_gap: 80.0,
            pad: 8.0,
            pad_frac: 0.02,
            header: 40.0,
            header_frac: 0.08,
            order: Order::Size,
        }
    }
}

impl LayoutConfig {
    /// A 64-bit fingerprint of every field (part of [`cache_key`]).
    pub fn fingerprint(&self) -> u64 {
        let mut h = crate::intern::FxHasher::default();
        let f = [
            self.world.x0,
            self.world.y0,
            self.world.x1,
            self.world.y1,
            self.external_share,
            self.region_gap,
            self.pad,
            self.pad_frac,
            self.header,
            self.header_frac,
        ];
        for v in f {
            std::hash::Hasher::write_u32(&mut h, v.to_bits());
        }
        std::hash::Hasher::write_u8(&mut h, self.order as u8);
        std::hash::Hasher::finish(&h)
    }

    /// The content rect of a parent with rect `r`: inset by the padding and the header, or
    /// `r` itself when the inset leaves no area.
    pub fn content(&self, r: &Rect) -> Rect {
        let pad = self.pad.min(self.pad_frac * r.w().min(r.h())).max(0.0);
        let head = self.header.min(self.header_frac * r.h()).max(pad);
        let c = Rect::new(r.x0 + pad, r.y0 + head, r.x1 - pad, r.y1 - pad);
        if c.w() > 0.0 && c.h() > 0.0 { c } else { *r }
    }

    /// The header band of a parent with rect `r` (empty when [`LayoutConfig::content`] has no
    /// inset).
    pub fn header_band(&self, r: &Rect) -> Rect {
        let c = self.content(r);
        Rect::new(r.x0, r.y0, r.x1, c.y0)
    }
}
