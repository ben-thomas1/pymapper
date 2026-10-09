//! Animated transitions after a watch-mode update: overlay rects drawn over the new scene
//! for a moment, so changes are visible without rebuilding or re-uploading the scene per
//! frame (the scene switches at once; with the pre-order layout most cells stay put).
//!
//! ```text
//!   kind      from -> to                              look
//!   moved     old rect -> new rect (ease-out, 350 ms)  white outline morphing into place
//!   changed   new rect                                 amber fill + outline, fading (1.2 s)
//!   added     new rect                                 green fill + outline, fading
//!   removed   old rect -> its centre                   red outline shrinking away
//! ```
//!
//! Built once per update on the map thread from the old layout (its level arrays are kept,
//! not copied), the new code map and the index diff: O(nodes) for the moved-cell scan plus
//! a sort of the candidates. At most [`MAX_ITEMS`] overlays (the largest cells first); each
//! frame of the animation writes at most that many 32-byte instances.

use std::time::{Duration, Instant};

use pymapper_core::incremental::IndexDiff;
use pymapper_core::index::{NONE, NodeId};
use pymapper_core::layout::{Layout, Rect};

use crate::map::scene::{RectInstance, Rgba};

/// Most overlay rects per transition.
pub const MAX_ITEMS: usize = 4096;
/// Duration of the outline morph of moved cells.
pub const MOVE: Duration = Duration::from_millis(350);
/// Duration of the change / addition / removal highlight.
pub const FLASH: Duration = Duration::from_millis(1200);
/// A cell counts as moved when an edge shifts by more than this fraction of its larger side.
const MOVE_FRAC: f32 = 0.02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Moved,
    Changed,
    Added,
    Removed,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Item {
    pub from: Rect,
    pub to: Rect,
    pub kind: Kind,
}

/// One update's overlay animation (see the module docs).
#[derive(Debug, Default)]
pub struct Transition {
    pub items: Vec<Item>,
    start: Option<Instant>,
}

fn area(r: &Rect) -> f32 {
    r.w().max(0.0) * r.h().max(0.0)
}

fn visible(r: &Rect) -> bool {
    r.w() > 0.0 && r.h() > 0.0
}

impl Transition {
    /// The overlays for an update from the map laid out as `old` to the one laid out as
    /// `new`, with `diff` relating their nodes.
    pub fn build(old: &Layout, new: &Layout, diff: &IndexDiff) -> Transition {
        let mut items = Vec::new();
        // Highlights for nodes the diff names, largest first within the budget.
        let mut flashes: Vec<(f32, Item)> = Vec::with_capacity(diff.changed.len() + diff.added.len() + diff.removed.len());
        for (list, kind) in [(&diff.changed, Kind::Changed), (&diff.added, Kind::Added)] {
            for &v in list {
                if let Some(r) = new.rect(v).filter(visible) {
                    flashes.push((area(&r), Item { from: r, to: r, kind }));
                }
            }
        }
        for &v in &diff.removed {
            if let Some(r) = old.rect(v).filter(visible) {
                let (cx, cy) = ((r.x0 + r.x1) * 0.5, (r.y0 + r.y1) * 0.5);
                flashes.push((area(&r), Item { from: r, to: Rect::new(cx, cy, cx, cy), kind: Kind::Removed }));
            }
        }
        flashes.sort_by(|a, b| b.0.total_cmp(&a.0));
        items.extend(flashes.iter().take(MAX_ITEMS / 2).map(|f| f.1));
        // Cells that moved noticeably: one pass over the matched nodes.
        let mut moved: Vec<(f32, Item)> = Vec::new();
        for (v_old, &v_new) in diff.node_map.iter().enumerate() {
            if v_new == NONE {
                continue;
            }
            let (Some(a), Some(b)) = (old.rect(v_old as NodeId), new.rect(v_new)) else { continue };
            if !visible(&a) || !visible(&b) {
                continue;
            }
            let tol = MOVE_FRAC * a.w().max(a.h());
            let shift = (a.x0 - b.x0).abs().max((a.y0 - b.y0).abs()).max((a.x1 - b.x1).abs()).max((a.y1 - b.y1).abs());
            if shift > tol {
                moved.push((area(&b), Item { from: a, to: b, kind: Kind::Moved }));
            }
        }
        let room = MAX_ITEMS - items.len();
        if moved.len() > room {
            moved.select_nth_unstable_by(room, |a, b| b.0.total_cmp(&a.0));
            moved.truncate(room);
        }
        moved.sort_by(|a, b| b.0.total_cmp(&a.0));
        // Moved outlines first (drawn below the highlights).
        let mut out: Vec<Item> = moved.into_iter().map(|m| m.1).collect();
        out.append(&mut items);
        Transition { items: out, start: None }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Starts the clock (the first frame that shows the new scene).
    pub fn start(&mut self, now: Instant) {
        self.start.get_or_insert(now);
    }

    /// Writes the overlays at `now` to `out` (cleared). Returns false once the animation is
    /// over (and `out` is empty).
    pub fn frame(&self, now: Instant, out: &mut Vec<RectInstance>) -> bool {
        out.clear();
        let Some(start) = self.start else { return !self.items.is_empty() };
        let t = now.saturating_duration_since(start);
        if t >= FLASH.max(MOVE) {
            return false;
        }
        let tm = (t.as_secs_f32() / MOVE.as_secs_f32()).min(1.0);
        let tf = (t.as_secs_f32() / FLASH.as_secs_f32()).min(1.0);
        // Ease-out cubic for the morph, linear fade for the highlights.
        let em = 1.0 - (1.0 - tm).powi(3);
        let fade = 1.0 - tf;
        for it in &self.items {
            let (r, fill, stroke, width) = match it.kind {
                Kind::Moved if tm < 1.0 => (lerp(&it.from, &it.to, em), Rgba::TRANSPARENT, alpha(Rgba::new(255, 255, 255, 230), 1.0 - em * 0.6), 1.5),
                Kind::Moved => continue,
                Kind::Changed => (it.to, alpha(Rgba::new(255, 190, 40, 110), fade), alpha(Rgba::new(255, 190, 40, 255), fade), 2.0),
                Kind::Added => (it.to, alpha(Rgba::new(80, 220, 120, 110), fade), alpha(Rgba::new(80, 220, 120, 255), fade), 2.0),
                Kind::Removed => (lerp(&it.from, &it.to, em), Rgba::TRANSPARENT, alpha(Rgba::new(255, 80, 80, 255), fade), 2.0),
            };
            out.push(RectInstance { min: [r.x0, r.y0], max: [r.x1, r.y1], fill, stroke, radius: 2.0, stroke_width: width });
        }
        true
    }
}

fn lerp(a: &Rect, b: &Rect, t: f32) -> Rect {
    let m = |x: f32, y: f32| x + (y - x) * t;
    Rect::new(m(a.x0, b.x0), m(a.y0, b.y0), m(a.x1, b.x1), m(a.y1, b.y1))
}

fn alpha(c: Rgba, k: f32) -> Rgba {
    let [r, g, b, a] = c.0;
    Rgba([r, g, b, (f32::from(a) * k.clamp(0.0, 1.0)) as u8])
}
