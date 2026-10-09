//! Squarified treemap of one rectangle (Bruls, Huizing, van Wijk 2000), in `f64`.
//!
//! Items are laid out in the order given (the caller sorts them). Rows are filled along the
//! shorter side of the remaining rectangle while the worst aspect ratio of the row does not
//! get worse; then the row is fixed and the rectangle shrinks. Every item gets exactly its
//! share of the area (up to rounding), rows tile the rectangle without gaps, and the last row
//! takes whatever is left so rounding never leaves a sliver. O(n), no allocation.

/// A rectangle as `x0, y0, x1, y1` in `f64` (layout arithmetic; stored rects are `f32`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct R64 {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl R64 {
    #[inline]
    pub fn w(&self) -> f64 {
        self.x1 - self.x0
    }

    #[inline]
    pub fn h(&self) -> f64 {
        self.y1 - self.y0
    }

    /// A zero-area rect at the top-left corner of `self`.
    #[inline]
    pub fn corner(&self) -> R64 {
        R64 { x0: self.x0, y0: self.y0, x1: self.x0, y1: self.y0 }
    }
}

/// Lays out `weights` (in order, all `>= 0`) in `r`, writing one rect per weight to `out`
/// (same length). Zero weights get a zero-area rect at the corner of the remaining space.
/// A degenerate `r` (zero width or height) gives every item a degenerate rect inside `r`.
///
/// # Panics
/// If `out.len() != weights.len()`.
pub(crate) fn squarify(weights: &[f64], r: R64, out: &mut [R64]) {
    assert_eq!(weights.len(), out.len(), "one rect per weight");
    let n = weights.len();
    let mut total = 0.0;
    for &w in weights {
        total += w;
    }
    let (rw, rh) = (r.w().max(0.0), r.h().max(0.0));
    if n == 0 {
        return;
    }
    if total <= 0.0 || rw <= 0.0 || rh <= 0.0 {
        for o in out.iter_mut() {
            *o = r.corner();
        }
        return;
    }
    // Areas are weights scaled by `k`; the remaining rect always has area `rest * k`.
    let k = rw * rh / total;
    // The row holding the last positive weight takes everything that is left.
    let mut last_pos = 0;
    for (q, &w) in weights.iter().enumerate() {
        if w > 0.0 {
            last_pos = q;
        }
    }
    let mut rest = r;
    let mut i = 0;
    while i < n {
        let (w, h) = (rest.w(), rest.h());
        let side = w.min(h);
        // Grow the row i..j while its worst aspect ratio improves.
        let (mut j, mut sum, mut lo, mut hi) = (i, 0.0f64, f64::INFINITY, 0.0f64);
        let mut best = f64::INFINITY;
        let s2 = side * side;
        while j < n {
            let a = weights[j] * k;
            if a <= 0.0 {
                // Zero weights ride along with the row (zero-length cells).
                if j == i {
                    out[j] = rest.corner();
                    i += 1;
                }
                j += 1;
                continue;
            }
            let (ns, nlo, nhi) = (sum + a, lo.min(a), hi.max(a));
            let worst = (s2 * nhi / (ns * ns)).max(ns * ns / (s2 * nlo));
            if sum > 0.0 && worst > best {
                break;
            }
            best = worst;
            sum = ns;
            lo = nlo;
            hi = nhi;
            j += 1;
        }
        if i >= j {
            continue;
        }
        let last = j > last_pos;
        place_row(&weights[i..j], k, sum, last, &mut rest, &mut out[i..j]);
        i = j;
    }
}

/// Places one row along the shorter side of `rest` and shrinks `rest` past it. `sum` is the
/// row's area; with `last` the row takes all of `rest`.
fn place_row(ws: &[f64], k: f64, sum: f64, last: bool, rest: &mut R64, out: &mut [R64]) {
    let (w, h) = (rest.w(), rest.h());
    let vertical = w >= h; // a column on the left edge, cells stacked top to bottom
    let len = if vertical { h } else { w };
    let thick = if last { if vertical { w } else { h } } else { (sum / len).min(if vertical { w } else { h }) };
    let mut pos = if vertical { rest.y0 } else { rest.x0 };
    let end = if vertical { rest.y1 } else { rest.x1 };
    // The last non-empty cell of the row ends exactly at the far edge.
    let mut last_nz = ws.len();
    for (q, &wt) in ws.iter().enumerate().rev() {
        if wt > 0.0 {
            last_nz = q;
            break;
        }
    }
    for (q, (&wt, o)) in ws.iter().zip(out.iter_mut()).enumerate() {
        let step = if wt > 0.0 { wt * k / sum * len } else { 0.0 };
        let next = if q == last_nz { end } else { (pos + step).min(end) };
        *o = if vertical {
            R64 { x0: rest.x0, y0: pos, x1: rest.x0 + thick, y1: next }
        } else {
            R64 { x0: pos, y0: rest.y0, x1: next, y1: rest.y0 + thick }
        };
        pos = next;
    }
    if vertical {
        rest.x0 = if last { rest.x1 } else { rest.x0 + thick };
    } else {
        rest.y0 = if last { rest.y1 } else { rest.y0 + thick };
    }
}
