//! In-place replacement of row runs in a column (incremental updates of the global tables).
//!
//! A [`Splice`] replaces rows `at..at + old` of a destination column by rows
//! `src..src + new` of a source column. [`splice_col`] applies a sorted list of splices in one
//! pass, in place: the rows between splices are moved once (`copy_within`, a memmove) and the
//! column is resized once. No temporary column is allocated, so patching a large table costs a
//! memmove of the rows behind the first splice and no extra memory beyond its growth.
//!
//! ```text
//!   before:  [ a a | x x x | b b b b | y | c ]      splices: x x x -> X, y -> Y Y Y
//!   after:   [ a a | X | b b b b | Y Y Y | c ]      segments a, b, c moved by the deltas
//! ```

/// Rows `at..at + old` of the destination become rows `src..src + new` of the source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Splice {
    pub at: u32,
    pub old: u32,
    pub src: u32,
    pub new: u32,
}

/// Applies `plan` to `col`, taking new rows from `src`.
///
/// `plan` must be sorted and non-overlapping in the destination (`at >= prev.at + prev.old`;
/// splices at the same position apply in order, so an insertion (`old == 0`) followed by a
/// removal at the same `at` replaces the removed rows). Every source run must be in bounds.
///
/// # Panics
/// If the plan is unsorted, overlapping or out of bounds.
pub(crate) fn splice_col<T: Copy>(col: &mut Vec<T>, plan: &[Splice], src: &[T]) {
    if plan.is_empty() {
        return;
    }
    let old_len = col.len();
    let mut new_len = old_len as i64;
    let mut prev_end = 0usize;
    for s in plan {
        assert!(s.at as usize >= prev_end, "splices out of order");
        prev_end = (s.at + s.old) as usize;
        new_len += i64::from(s.new) - i64::from(s.old);
    }
    assert!(prev_end <= old_len, "splice beyond the column");
    let new_len = new_len as usize;
    if new_len > old_len {
        // Any value works as filler: every new slot is written below.
        let fill = match plan.iter().find(|s| s.new > 0) {
            Some(s) => src[s.src as usize],
            None => unreachable!("a growing plan inserts rows"),
        };
        col.resize(new_len, fill);
    }
    // Segment k is the run of kept rows after splice k - 1 (k = 0: before the first splice) and
    // moves by the sum of the deltas of the splices before it. Left movers are moved left to
    // right, then right movers right to left: no move overwrites a segment not yet moved.
    let n = plan.len();
    let seg = |k: usize| -> (usize, usize) {
        let start = if k == 0 { 0 } else { (plan[k - 1].at + plan[k - 1].old) as usize };
        let end = if k == n { old_len } else { plan[k].at as usize };
        (start, end)
    };
    let delta = |k: usize| i64::from(plan[k].new) - i64::from(plan[k].old);
    let mut shift = 0i64;
    for k in 0..=n {
        let (s, e) = seg(k);
        if shift < 0 && s < e {
            col.copy_within(s..e, (s as i64 + shift) as usize);
        }
        if k < n {
            shift += delta(k);
        }
    }
    // `shift` is now the total delta (the shift of the last segment).
    for k in (0..=n).rev() {
        let (s, e) = seg(k);
        if shift > 0 && s < e {
            col.copy_within(s..e, (s as i64 + shift) as usize);
        }
        if k > 0 {
            shift -= delta(k - 1);
        }
    }
    debug_assert_eq!(shift, 0);
    for sp in plan {
        if sp.new > 0 {
            let at = (i64::from(sp.at) + shift) as usize;
            let from = sp.src as usize;
            col[at..at + sp.new as usize].copy_from_slice(&src[from..from + sp.new as usize]);
        }
        shift += i64::from(sp.new) - i64::from(sp.old);
    }
    col.truncate(new_len);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference: rebuild the column by concatenation.
    fn naive(col: &[u32], plan: &[Splice], src: &[u32]) -> Vec<u32> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        for s in plan {
            out.extend_from_slice(&col[pos..s.at as usize]);
            out.extend_from_slice(&src[s.src as usize..(s.src + s.new) as usize]);
            pos = (s.at + s.old) as usize;
        }
        out.extend_from_slice(&col[pos..]);
        out
    }

    #[test]
    fn grow_shrink_and_mixed() {
        let col: Vec<u32> = (0..20).collect();
        let src: Vec<u32> = (100..120).collect();
        let plans: Vec<Vec<Splice>> = vec![
            vec![Splice { at: 3, old: 2, src: 0, new: 5 }],
            vec![Splice { at: 3, old: 5, src: 0, new: 1 }],
            vec![Splice { at: 0, old: 0, src: 0, new: 3 }],
            vec![Splice { at: 20, old: 0, src: 4, new: 2 }],
            vec![Splice { at: 2, old: 4, src: 0, new: 0 }, Splice { at: 10, old: 0, src: 1, new: 7 }],
            vec![Splice { at: 2, old: 0, src: 0, new: 6 }, Splice { at: 10, old: 8, src: 1, new: 1 }],
            vec![
                Splice { at: 1, old: 3, src: 0, new: 1 },
                Splice { at: 5, old: 1, src: 2, new: 6 },
                Splice { at: 9, old: 4, src: 9, new: 0 },
                Splice { at: 15, old: 0, src: 3, new: 3 },
            ],
            // Insertion then removal at the same position.
            vec![Splice { at: 4, old: 0, src: 0, new: 2 }, Splice { at: 4, old: 3, src: 0, new: 0 }],
            vec![Splice { at: 0, old: 20, src: 0, new: 0 }],
        ];
        for plan in &plans {
            let mut c = col.clone();
            splice_col(&mut c, plan, &src);
            assert_eq!(c, naive(&col, plan, &src), "{plan:?}");
        }
    }

    #[test]
    fn random_plans_match_naive() {
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut next = |m: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % m.max(1)
        };
        for _ in 0..2000 {
            let len = next(40) as u32;
            let col: Vec<u32> = (0..len).collect();
            let src: Vec<u32> = (1000..1040).collect();
            let mut plan = Vec::new();
            let mut pos = 0u32;
            while pos <= len && next(3) != 0 {
                let at = pos + next(u64::from(len - pos) + 1) as u32;
                let old = next(u64::from(len - at) + 1) as u32;
                let new = next(6) as u32;
                let s = next(u64::from(40 - new) + 1) as u32;
                plan.push(Splice { at, old, src: s, new });
                pos = at + old;
            }
            let mut c = col.clone();
            splice_col(&mut c, &plan, &src);
            assert_eq!(c, naive(&col, &plan, &src), "{plan:?}");
        }
    }
}
