//! Own Barnes-Hut quadtree: a flat, pointer-free region quadtree over weighted points, rebuilt
//! from scratch every layout iteration with reused buffers (no allocation once warm).
//!
//! ```text
//!   cells (SoA, creation order: a parent always precedes its children)
//!     ox, oy, half   square cell: centre and half side
//!     child          first of 4 consecutive children (quadrant = (x >= ox) | (y >= oy) << 1),
//!                    NONE for a leaf
//!     body           leaf: first body of its chain (NONE: empty); bodies chain through
//!                    `next` when they coincide or the cell is at MAX_DEPTH
//!     mass, mx, my   total mass and centre of mass (filled bottom-up by `finish`)
//! ```
//!
//! Build: O(n log(extent / spacing)) insertions, each descending to its leaf and splitting an
//! occupied leaf. Query ([`QuadTree::repulsion`]): the usual Barnes-Hut opening test
//! `side / distance < theta`, an explicit fixed-size stack (no recursion, no allocation), so
//! queries of different bodies run in parallel on a shared tree.

const NONE: u32 = u32::MAX;
/// Cells at this depth are never split (bodies closer than `extent / 2^24` share a leaf).
pub const MAX_DEPTH: u32 = 24;
/// Query stack: at most 3 pending siblings per level plus the root.
const STACK: usize = 3 * MAX_DEPTH as usize + 4;

/// A Barnes-Hut quadtree over bodies with positions and masses.
#[derive(Debug, Default, Clone)]
pub struct QuadTree {
    ox: Vec<f32>,
    oy: Vec<f32>,
    half: Vec<f32>,
    child: Vec<u32>,
    body: Vec<u32>,
    mass: Vec<f32>,
    mx: Vec<f32>,
    my: Vec<f32>,
    depth: Vec<u8>,
    /// Per body: next body in the same leaf.
    next: Vec<u32>,
    bx: Vec<f32>,
    by: Vec<f32>,
    bm: Vec<f32>,
}

impl QuadTree {
    /// Rebuilds the tree over `pos` with masses `mass` (same length). Non-finite positions
    /// are left out.
    pub fn build(&mut self, pos: &[[f32; 2]], mass: &[f32]) {
        assert_eq!(pos.len(), mass.len(), "one mass per body");
        for v in [&mut self.ox, &mut self.oy, &mut self.half, &mut self.mass, &mut self.mx, &mut self.my] {
            v.clear();
        }
        self.child.clear();
        self.body.clear();
        self.depth.clear();
        self.next.clear();
        self.next.resize(pos.len(), NONE);
        self.bx.clear();
        self.by.clear();
        self.bm.clear();
        self.bx.extend(pos.iter().map(|p| p[0]));
        self.by.extend(pos.iter().map(|p| p[1]));
        self.bm.extend_from_slice(mass);
        let (mut lo, mut hi) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
        for p in pos.iter().filter(|p| p[0].is_finite() && p[1].is_finite()) {
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
        }
        if lo[0] > hi[0] {
            return;
        }
        // A square root cell, slightly enlarged so the maximum lands strictly inside.
        let half = ((hi[0] - lo[0]).max(hi[1] - lo[1]) * 0.5).max(1e-3) * 1.001;
        self.push_cell((lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5, half, 0);
        for (i, p) in pos.iter().enumerate() {
            if p[0].is_finite() && p[1].is_finite() {
                self.insert(i as u32);
            }
        }
        self.finish();
    }

    /// Number of cells.
    #[inline]
    pub fn cells(&self) -> usize {
        self.ox.len()
    }

    /// Total mass and centre of mass of the root (`None`: empty tree).
    pub fn root(&self) -> Option<(f32, [f32; 2])> {
        (!self.mass.is_empty()).then(|| (self.mass[0], [self.mx[0], self.my[0]]))
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        4 * (self.ox.capacity()
            + self.oy.capacity()
            + self.half.capacity()
            + self.child.capacity()
            + self.body.capacity()
            + self.mass.capacity()
            + self.mx.capacity()
            + self.my.capacity()
            + self.next.capacity()
            + self.bx.capacity()
            + self.by.capacity()
            + self.bm.capacity())
            + self.depth.capacity()
    }

    fn push_cell(&mut self, ox: f32, oy: f32, half: f32, depth: u8) -> u32 {
        self.ox.push(ox);
        self.oy.push(oy);
        self.half.push(half);
        self.child.push(NONE);
        self.body.push(NONE);
        self.depth.push(depth);
        (self.ox.len() - 1) as u32
    }

    #[inline]
    fn quadrant(&self, c: u32, x: f32, y: f32) -> u32 {
        let c = c as usize;
        u32::from(x >= self.ox[c]) | u32::from(y >= self.oy[c]) << 1
    }

    fn insert(&mut self, i: u32) {
        let (x, y) = (self.bx[i as usize], self.by[i as usize]);
        let mut c = 0u32;
        loop {
            let ci = c as usize;
            if self.child[ci] != NONE {
                c = self.child[ci] + self.quadrant(c, x, y);
                continue;
            }
            let occ = self.body[ci];
            if occ == NONE {
                self.body[ci] = i;
                return;
            }
            let (ax, ay) = (self.bx[occ as usize], self.by[occ as usize]);
            if u32::from(self.depth[ci]) >= MAX_DEPTH || (ax == x && ay == y) {
                // Chain: coincident bodies, or the finest cell.
                self.next[i as usize] = occ;
                self.body[ci] = i;
                return;
            }
            // Split: four children, the occupant chain moves to its quadrant, then retry here.
            let (ox, oy, h, d) = (self.ox[ci], self.oy[ci], self.half[ci] * 0.5, self.depth[ci] + 1);
            let first = self.push_cell(ox - h, oy - h, h, d);
            self.push_cell(ox + h, oy - h, h, d);
            self.push_cell(ox - h, oy + h, h, d);
            self.push_cell(ox + h, oy + h, h, d);
            self.child[ci] = first;
            self.body[ci] = NONE;
            let q = first + self.quadrant(c, ax, ay);
            self.body[q as usize] = occ;
        }
    }

    /// Masses and centres of mass, bottom-up (children were created after their parent).
    fn finish(&mut self) {
        let n = self.ox.len();
        self.mass.resize(n, 0.0);
        self.mx.resize(n, 0.0);
        self.my.resize(n, 0.0);
        for c in (0..n).rev() {
            let (mut m, mut sx, mut sy) = (0.0f32, 0.0f32, 0.0f32);
            if self.child[c] == NONE {
                let mut b = self.body[c];
                while b != NONE {
                    let bi = b as usize;
                    m += self.bm[bi];
                    sx += self.bm[bi] * self.bx[bi];
                    sy += self.bm[bi] * self.by[bi];
                    b = self.next[bi];
                }
            } else {
                let f = self.child[c] as usize;
                for k in f..f + 4 {
                    m += self.mass[k];
                    sx += self.mass[k] * self.mx[k];
                    sy += self.mass[k] * self.my[k];
                }
            }
            self.mass[c] = m;
            if m > 0.0 {
                self.mx[c] = sx / m;
                self.my[c] = sy / m;
            } else {
                self.mx[c] = self.ox[c];
                self.my[c] = self.oy[c];
            }
        }
    }

    /// Repulsion on body `i` (at its build position, with its build mass): the sum over the
    /// other bodies of `k * m_i * m_j / d` along `p_i - p_j` (magnitude inversely
    /// proportional to the distance, as ForceAtlas2), cells with `side / d < theta`
    /// approximated by their centre of mass. Coincident bodies push apart along a fixed
    /// direction derived from their indices (deterministic).
    pub fn repulsion(&self, i: u32, theta: f32, k: f32) -> [f32; 2] {
        if self.mass.is_empty() {
            return [0.0; 2];
        }
        let (x, y, mi) = (self.bx[i as usize], self.by[i as usize], self.bm[i as usize]);
        let theta2 = theta * theta;
        let (mut fx, mut fy) = (0.0f32, 0.0f32);
        let mut stack = [0u32; STACK];
        let mut top = 1usize;
        while top > 0 {
            top -= 1;
            let c = stack[top] as usize;
            let m = self.mass[c];
            if m == 0.0 {
                continue;
            }
            if self.child[c] == NONE {
                let mut b = self.body[c];
                while b != NONE {
                    if b != i {
                        let bi = b as usize;
                        let (dx, dy) = (x - self.bx[bi], y - self.by[bi]);
                        let d2 = dx * dx + dy * dy;
                        let f = k * mi * self.bm[bi];
                        if d2 > 1e-12 {
                            fx += f * dx / d2;
                            fy += f * dy / d2;
                        } else {
                            let [ux, uy] = jitter(i, b);
                            fx += f * ux;
                            fy += f * uy;
                        }
                    }
                    b = self.next[b as usize];
                }
                continue;
            }
            let (dx, dy) = (x - self.mx[c], y - self.my[c]);
            let d2 = dx * dx + dy * dy;
            let side = 2.0 * self.half[c];
            if side * side < theta2 * d2 {
                let f = k * mi * m / d2;
                fx += f * dx;
                fy += f * dy;
                continue;
            }
            let f = self.child[c];
            for q in 0..4 {
                stack[top] = f + q;
                top += 1;
            }
        }
        [fx, fy]
    }
}

/// A unit direction for two coincident bodies, antisymmetric in (a, b).
#[inline]
fn jitter(a: u32, b: u32) -> [f32; 2] {
    let (lo, hi, s) = if a < b { (a, b, 1.0) } else { (b, a, -1.0) };
    let h = (u64::from(lo) << 32 | u64::from(hi)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let ang = (h >> 40) as f32 * (std::f32::consts::TAU / (1u64 << 24) as f32);
    [s * ang.cos(), s * ang.sin()]
}
