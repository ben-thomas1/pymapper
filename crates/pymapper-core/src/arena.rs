//! Per-thread bump arenas with merge by concatenation + id remap (design: Performance rules).
//!
//! Three building blocks:
//!
//! - [`Bump`]: a chunked bump allocator for `Copy` data. Allocation is a pointer bump in the
//!   common case; memory is released all at once on [`Bump::reset`] or drop. It is `Send` but not
//!   `Sync`: each thread owns its own arena.
//! - [`Pool`] + [`Slice`]: a typed, index-addressed slice store. A [`Slice`] is an 8-byte
//!   `(start, len)` handle instead of a pointer, so per-thread pools can be merged by plain
//!   concatenation ([`Pool::append`]) followed by shifting handles ([`Slice::shift`]). This is the
//!   storage behind struct-of-arrays fact tables.
//! - [`PerThread`]: one value per rayon worker (plus one shared overflow slot), used to give each
//!   worker its own arena / interner / pool without contention, and to collect them afterwards
//!   for the merge.
//!
//! ```text
//!   worker 0: Pool [a a a b b]        worker 1: Pool [c c d]
//!   handles:  A=(0,3) B=(3,2)         handles:  C=(0,2) D=(2,1)
//!                         \             /
//!            merged Pool [a a a b b c c d]   base of worker 1 = 5
//!            handles:    A=(0,3) B=(3,2) C=(5,2) D=(7,1)
//! ```

use std::alloc::{self, Layout};
use std::cell::{Cell, RefCell};
use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::ops::Range;
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

// ---------------------------------------------------------------------------------------------
// Bump
// ---------------------------------------------------------------------------------------------

/// Alignment of every chunk. Covers all primitive types (incl. `u128` on aarch64); types with a
/// larger alignment are still handled by padding inside the chunk.
const CHUNK_ALIGN: usize = 16;
/// Size of the first chunk when none was requested.
const DEFAULT_FIRST_CHUNK: usize = 64 * 1024;
/// Chunks double in size up to this limit; larger single requests get a dedicated chunk.
const MAX_GROWTH_CHUNK: usize = 16 * 1024 * 1024;

/// One heap block owned by a [`Bump`].
struct Chunk {
    ptr: NonNull<u8>,
    size: usize,
}

impl Chunk {
    fn layout(size: usize) -> Layout {
        // `size` is at least 1 and was already validated against `isize::MAX` by the caller.
        Layout::from_size_align(size, CHUNK_ALIGN).expect("chunk size validated by caller")
    }

    fn new(size: usize) -> Chunk {
        debug_assert!(size > 0);
        let layout = Chunk::layout(size);
        // SAFETY: `layout` has a non-zero size (`size > 0`, checked above in debug builds and
        // guaranteed by `Bump::alloc_slow`, which never requests fewer than `CHUNK_ALIGN` bytes).
        let raw = unsafe { alloc::alloc(layout) };
        let Some(ptr) = NonNull::new(raw) else { alloc::handle_alloc_error(layout) };
        Chunk { ptr, size }
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        // SAFETY: `ptr` was returned by `alloc::alloc` with exactly this layout in `Chunk::new`
        // and is freed only here, once.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), Chunk::layout(self.size)) }
    }
}

/// Chunked bump allocator for `Copy` values and slices.
///
/// Allocation methods take `&self` and hand out references that live as long as the borrow of
/// the arena; [`Bump::reset`] takes `&mut self`, so it cannot run while any allocation is still
/// referenced. Only `Copy` types are accepted, which guarantees no destructor ever needs to run
/// for arena contents.
///
/// `Bump` is `Send` (it can move to the thread that merges results) but not `Sync`: use one
/// arena per thread, e.g. through [`PerThread`].
pub struct Bump {
    /// Next free byte in the current chunk (null before the first chunk exists).
    cur: Cell<*mut u8>,
    /// One past the last byte of the current chunk (null before the first chunk exists).
    end: Cell<*mut u8>,
    /// Owned chunks; the last one is the current chunk. Only touched on the slow path.
    chunks: RefCell<Vec<Chunk>>,
    /// Size of the next chunk to allocate when growing.
    next_size: Cell<usize>,
}

// SAFETY: a `Bump` exclusively owns all of its chunks; the raw pointers only point into those
// chunks and carry no thread affinity. References handed out borrow the `Bump`, so the arena
// cannot be moved to another thread while any of them is alive. `Bump` stays `!Sync` because of
// its `Cell`s, so it is never accessed from two threads at once.
unsafe impl Send for Bump {}

impl Default for Bump {
    fn default() -> Self {
        Bump::new()
    }
}

impl fmt::Debug for Bump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bump")
            .field("chunks", &self.chunks.borrow().len())
            .field("capacity_bytes", &self.capacity_bytes())
            .finish()
    }
}

// Handing out `&mut` from `&self` is the point of an arena: every allocation is a fresh,
// disjoint region, so the returned exclusive borrows never alias (same contract as bumpalo).
#[allow(clippy::mut_from_ref)]
impl Bump {
    /// Creates an empty arena. No memory is allocated until the first allocation.
    pub fn new() -> Bump {
        Bump::with_capacity(DEFAULT_FIRST_CHUNK)
    }

    /// Creates an empty arena whose first chunk will hold at least `bytes` bytes.
    pub fn with_capacity(bytes: usize) -> Bump {
        Bump {
            cur: Cell::new(ptr::null_mut()),
            end: Cell::new(ptr::null_mut()),
            chunks: RefCell::new(Vec::new()),
            next_size: Cell::new(bytes.max(CHUNK_ALIGN)),
        }
    }

    /// Total bytes of chunk memory currently owned by the arena.
    pub fn capacity_bytes(&self) -> usize {
        let mut total = 0;
        for c in self.chunks.borrow().iter() {
            total += c.size;
        }
        total
    }

    /// Frees all allocations at once. Keeps the largest chunk so a reused arena does not
    /// allocate again for a similar workload.
    pub fn reset(&mut self) {
        let chunks = self.chunks.get_mut();
        if chunks.is_empty() {
            return;
        }
        let mut largest = 0;
        for (i, c) in chunks.iter().enumerate() {
            if c.size > chunks[largest].size {
                largest = i;
            }
        }
        let keep = chunks.swap_remove(largest);
        chunks.clear();
        let base = keep.ptr.as_ptr();
        self.cur.set(base);
        // `wrapping_add` keeps provenance; `size` is exactly the chunk length.
        self.end.set(base.wrapping_add(keep.size));
        chunks.push(keep);
    }

    /// Reserves `layout.size()` bytes aligned to `layout.align()`; never returns for size 0
    /// callers (handled by the typed wrappers).
    #[inline]
    fn alloc_layout(&self, layout: Layout) -> NonNull<u8> {
        let cur = self.cur.get();
        let end = self.end.get();
        let addr = cur.addr();
        // `align` is a power of two; `aligned` may overflow only for absurd addresses, in which
        // case `checked_add` routes us to the slow path.
        if let Some(aligned) = addr.checked_add(layout.align() - 1).map(|a| a & !(layout.align() - 1))
            && let Some(new_end) = aligned.checked_add(layout.size())
            && new_end <= end.addr()
            && !cur.is_null()
        {
            let start = cur.wrapping_add(aligned - addr);
            self.cur.set(cur.wrapping_add(new_end - addr));
            debug_assert!(start.addr().is_multiple_of(layout.align()));
            // SAFETY: `start` lies inside the current chunk (between `cur` and `end`), which is a
            // live non-null allocation, so it is non-null.
            return unsafe { NonNull::new_unchecked(start) };
        }
        self.alloc_slow(layout)
    }

    #[cold]
    #[inline(never)]
    fn alloc_slow(&self, layout: Layout) -> NonNull<u8> {
        let need = layout
            .size()
            .checked_add(layout.align().saturating_sub(CHUNK_ALIGN))
            .filter(|&n| n <= isize::MAX as usize - CHUNK_ALIGN)
            .unwrap_or_else(|| alloc::handle_alloc_error(layout));
        let next = self.next_size.get();
        let size = chunk_size(next, need).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        // Growth follows the regular schedule; one oversized request does not inflate it.
        self.next_size.set(next.saturating_mul(2).min(MAX_GROWTH_CHUNK).max(next));
        let chunk = Chunk::new(size);
        let base = chunk.ptr.as_ptr();
        self.chunks.borrow_mut().push(chunk);
        self.cur.set(base);
        self.end.set(base.wrapping_add(size));
        // The fresh chunk is big enough for `layout` by construction, so this takes the fast path.
        let p = self.alloc_layout(layout);
        debug_assert!(p.as_ptr().addr() >= base.addr());
        debug_assert!(p.as_ptr().addr() + layout.size() <= base.addr() + size);
        p
    }

    /// Moves `val` into the arena.
    #[inline]
    pub fn alloc<T: Copy>(&self, val: T) -> &mut T {
        if mem::size_of::<T>() == 0 {
            // SAFETY: for a zero-sized type any aligned non-null pointer is valid for reads and
            // writes, and `dangling` is aligned; `T: Copy` means nothing is leaked or dropped.
            return unsafe { NonNull::<T>::dangling().as_mut() };
        }
        let p = self.alloc_layout(Layout::new::<T>()).cast::<T>();
        // SAFETY: `p` points to a fresh, exclusively owned, properly aligned region of
        // `size_of::<T>()` bytes inside a live chunk; writing initialises it, and the returned
        // `&mut` cannot alias any other allocation from this arena.
        unsafe {
            p.as_ptr().write(val);
            &mut *p.as_ptr()
        }
    }

    /// Copies `src` into the arena.
    #[inline]
    pub fn alloc_slice_copy<T: Copy>(&self, src: &[T]) -> &mut [T] {
        let dst = self.alloc_uninit_slice::<T>(src.len());
        if dst.is_null() {
            return &mut [];
        }
        // SAFETY: `dst` is a fresh, aligned region for `src.len()` values of `T` in a live chunk,
        // which cannot overlap `src` (a borrow from elsewhere, or an earlier arena allocation).
        // After the copy all elements are initialised.
        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
            slice::from_raw_parts_mut(dst, src.len())
        }
    }

    /// Allocates `len` copies of `val`.
    #[inline]
    pub fn alloc_slice_fill<T: Copy>(&self, len: usize, val: T) -> &mut [T] {
        let dst = self.alloc_uninit_slice::<T>(len);
        if dst.is_null() {
            return &mut [];
        }
        for i in 0..len {
            // SAFETY: `i < len` and `dst` is valid for `len` writes of `T`.
            unsafe { dst.add(i).write(val) };
        }
        // SAFETY: all `len` elements were initialised by the loop above.
        unsafe { slice::from_raw_parts_mut(dst, len) }
    }

    /// Collects an exact-size iterator into the arena. If the iterator yields fewer items than
    /// it promised, the returned slice is truncated to what it yielded; extra items are ignored.
    pub fn alloc_slice_from_iter<T, I>(&self, iter: I) -> &mut [T]
    where
        T: Copy,
        I: IntoIterator<Item = T>,
        I::IntoIter: ExactSizeIterator,
    {
        let mut iter = iter.into_iter();
        let len = iter.len();
        // The region is reserved before iterating, so an iterator that itself allocates from
        // this arena gets a disjoint region.
        let dst = self.alloc_uninit_slice::<T>(len);
        if dst.is_null() {
            return &mut [];
        }
        let mut n = 0;
        while n < len {
            let Some(v) = iter.next() else { break };
            // SAFETY: `n < len` and `dst` is valid for `len` writes of `T`.
            unsafe { dst.add(n).write(v) };
            n += 1;
        }
        // SAFETY: exactly the first `n` elements were initialised.
        unsafe { slice::from_raw_parts_mut(dst, n) }
    }

    /// Copies a byte string into the arena.
    #[inline]
    pub fn alloc_bytes(&self, src: &[u8]) -> &mut [u8] {
        self.alloc_slice_copy(src)
    }

    /// Copies a string into the arena.
    #[inline]
    pub fn alloc_str(&self, src: &str) -> &mut str {
        let bytes = self.alloc_slice_copy(src.as_bytes());
        // SAFETY: `bytes` is a byte-for-byte copy of a valid `str`.
        unsafe { std::str::from_utf8_unchecked_mut(bytes) }
    }

    /// Reserves room for `len` values of `T` and returns its start, or null when no memory is
    /// needed (`len == 0` or zero-sized `T`), in which case callers return an empty slice.
    #[inline]
    fn alloc_uninit_slice<T>(&self, len: usize) -> *mut T {
        if len == 0 || mem::size_of::<T>() == 0 {
            return ptr::null_mut();
        }
        let layout = Layout::array::<T>(len).unwrap_or_else(|_| capacity_overflow());
        self.alloc_layout(layout).cast::<T>().as_ptr()
    }
}

/// Size of a new chunk: at least `next` (the growth schedule) and `need` (the request plus
/// alignment padding), rounded up to [`CHUNK_ALIGN`]. `None` if that exceeds `isize::MAX`.
/// Checked because `next` comes from [`Bump::with_capacity`] and may be arbitrarily large; a
/// wrapped size of 0 would be an invalid zero-size allocation.
fn chunk_size(next: usize, need: usize) -> Option<usize> {
    let size = next.max(need).max(CHUNK_ALIGN).checked_next_multiple_of(CHUNK_ALIGN)?;
    (size <= isize::MAX as usize).then_some(size)
}

#[cold]
fn capacity_overflow() -> ! {
    panic!("arena capacity overflow")
}

// ---------------------------------------------------------------------------------------------
// Pool + Slice
// ---------------------------------------------------------------------------------------------

/// An 8-byte handle to a run of elements in a [`Pool<T>`]: `(start, len)` in elements.
///
/// Handles are plain indices, so they survive pool reallocation and can be rebased after a
/// merge with [`Slice::shift`].
pub struct Slice<T> {
    start: u32,
    len: u32,
    _ty: PhantomData<fn() -> T>,
}

impl<T> Clone for Slice<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Slice<T> {}
impl<T> PartialEq for Slice<T> {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start && self.len == other.len
    }
}
impl<T> Eq for Slice<T> {}
impl<T> std::hash::Hash for Slice<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64((u64::from(self.start) << 32) | u64::from(self.len));
    }
}
impl<T> Default for Slice<T> {
    fn default() -> Self {
        Slice::EMPTY
    }
}
impl<T> fmt::Debug for Slice<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Slice({}..{})", self.start, self.start + self.len)
    }
}

impl<T> Slice<T> {
    /// The empty slice at offset 0.
    pub const EMPTY: Slice<T> = Slice { start: 0, len: 0, _ty: PhantomData };

    /// Builds a handle from raw parts (e.g. when decoding a cache file).
    pub const fn from_raw(start: u32, len: u32) -> Slice<T> {
        Slice { start, len, _ty: PhantomData }
    }

    /// First element index.
    pub const fn start(self) -> u32 {
        self.start
    }

    /// Number of elements.
    pub const fn len(self) -> u32 {
        self.len
    }

    /// Whether the slice has no elements.
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Element index range.
    pub fn range(self) -> Range<usize> {
        self.start as usize..self.start as usize + self.len as usize
    }

    /// Rebases the handle after its pool was appended at `base` (see [`Pool::append`]).
    ///
    /// # Panics
    /// If the shifted end does not fit in `u32` (impossible for handles from a pool that holds
    /// at most `u32::MAX` elements).
    #[inline]
    pub fn shift(self, base: u32) -> Slice<T> {
        let start = self.start.checked_add(base).unwrap_or_else(|| capacity_overflow());
        debug_assert!(start.checked_add(self.len).is_some());
        Slice { start, len: self.len, _ty: PhantomData }
    }
}

/// Typed, index-addressed, append-only slice store with `u32` element indices.
///
/// Per-thread pools are merged by [`Pool::append`], which returns the base offset to apply to
/// the appended pool's handles with [`Slice::shift`]. Element indices returned by
/// [`Pool::push`] are rebased the same way (add the base).
pub struct Pool<T> {
    items: Vec<T>,
}

impl<T> Default for Pool<T> {
    fn default() -> Self {
        Pool::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for Pool<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.items.iter()).finish()
    }
}

impl<T> Pool<T> {
    /// Creates an empty pool.
    pub const fn new() -> Pool<T> {
        Pool { items: Vec::new() }
    }

    /// Creates an empty pool with room for `n` elements.
    pub fn with_capacity(n: usize) -> Pool<T> {
        Pool { items: Vec::with_capacity(n) }
    }

    /// Number of elements stored.
    pub fn len(&self) -> u32 {
        // Every insertion path checks the `u32` bound, so this cast is lossless.
        self.items.len() as u32
    }

    /// Whether the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Bytes of heap memory reserved by the pool.
    pub fn heap_bytes(&self) -> usize {
        self.items.capacity() * mem::size_of::<T>()
    }

    /// Reserves room for `n` more elements.
    pub fn reserve(&mut self, n: usize) {
        self.items.reserve(n);
    }

    /// Removes all elements, keeping the allocation.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    #[inline]
    fn check_room(&self, extra: usize) {
        if self.items.len().checked_add(extra).is_none_or(|n| n > u32::MAX as usize) {
            capacity_overflow();
        }
    }

    /// Appends one element and returns its index.
    #[inline]
    pub fn push(&mut self, v: T) -> u32 {
        self.check_room(1);
        let i = self.items.len() as u32;
        self.items.push(v);
        i
    }

    /// Current end index, to start a slice built by repeated [`Pool::push`].
    #[inline]
    pub fn mark(&self) -> u32 {
        self.len()
    }

    /// The slice of everything pushed since `mark` (see [`Pool::mark`]).
    #[inline]
    pub fn since(&self, mark: u32) -> Slice<T> {
        debug_assert!(mark <= self.len());
        Slice::from_raw(mark, self.len() - mark)
    }

    /// Appends all items of `iter` and returns their slice.
    pub fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) -> Slice<T> {
        let mark = self.mark();
        let iter = iter.into_iter();
        self.check_room(iter.size_hint().0);
        self.items.extend(iter);
        self.check_room(0);
        self.since(mark)
    }

    /// Copies `src` into the pool and returns its slice.
    pub fn push_slice(&mut self, src: &[T]) -> Slice<T>
    where
        T: Copy,
    {
        self.check_room(src.len());
        let mark = self.mark();
        self.items.extend_from_slice(src);
        self.since(mark)
    }

    /// Element by index.
    #[inline]
    pub fn get(&self, i: u32) -> &T {
        &self.items[i as usize]
    }

    /// Mutable element by index.
    #[inline]
    pub fn get_mut(&mut self, i: u32) -> &mut T {
        &mut self.items[i as usize]
    }

    /// Elements of a slice handle.
    ///
    /// # Panics
    /// If the handle is out of bounds for this pool (it came from another pool).
    #[inline]
    pub fn slice(&self, s: Slice<T>) -> &[T] {
        &self.items[s.range()]
    }

    /// Mutable elements of a slice handle (e.g. to remap ids in place).
    #[inline]
    pub fn slice_mut(&mut self, s: Slice<T>) -> &mut [T] {
        &mut self.items[s.range()]
    }

    /// All elements.
    pub fn as_slice(&self) -> &[T] {
        &self.items
    }

    /// All elements, mutably.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.items
    }

    /// Moves all elements of `other` to the end of `self` (leaving `other` empty but keeping its
    /// allocation) and returns the base offset to add to `other`'s indices and handles.
    pub fn append(&mut self, other: &mut Pool<T>) -> u32 {
        self.check_room(other.items.len());
        let base = self.len();
        self.items.append(&mut other.items);
        base
    }

    /// Consumes the pool, returning its elements.
    pub fn into_vec(self) -> Vec<T> {
        self.items
    }
}

// ---------------------------------------------------------------------------------------------
// PerThread
// ---------------------------------------------------------------------------------------------

/// Pads a value to its own cache line (128 bytes covers Apple M-series and x86 adjacent-line
/// prefetch) so per-thread slots do not false-share.
#[repr(align(128))]
struct CachePadded<T>(T);

/// One `T` per rayon worker thread plus one shared overflow slot.
///
/// [`PerThread::with`] gives the calling worker exclusive access to its own slot. The slot lock
/// is uncontended in normal use (only its worker touches it), so access costs one uncontended
/// atomic pair. Threads outside the rayon pool, and a worker that re-enters `with` while already
/// inside it (via nested rayon work stealing), use the overflow slot, which blocks.
///
/// Afterwards [`PerThread::into_vec`] yields every slot for the merge step.
pub struct PerThread<T> {
    slots: Box<[CachePadded<Mutex<T>>]>,
}

impl<T> PerThread<T> {
    /// Creates `rayon::current_num_threads() + 1` slots, each initialised with `init()`.
    ///
    /// Create it from the pool that will use it (or the global pool); workers of a larger pool
    /// fall back to the overflow slot.
    pub fn new(mut init: impl FnMut() -> T) -> PerThread<T> {
        let n = rayon::current_num_threads() + 1;
        let mut slots = Vec::with_capacity(n);
        for _ in 0..n {
            slots.push(CachePadded(Mutex::new(init())));
        }
        PerThread { slots: slots.into_boxed_slice() }
    }

    /// Number of slots (workers + overflow).
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Always false: there is at least the overflow slot.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Runs `f` with exclusive access to the calling thread's slot; `f` also receives the slot
    /// index, which callers record to know which slot's ids/handles their results refer to.
    ///
    /// `f` should not itself wait on nested rayon work that calls `with` on the same
    /// `PerThread` more than one level deep (the overflow slot is not re-entrant).
    #[inline]
    pub fn with<R>(&self, f: impl FnOnce(usize, &mut T) -> R) -> R {
        let overflow = self.slots.len() - 1;
        let own = match rayon::current_thread_index() {
            Some(i) if i < overflow => i,
            _ => overflow,
        };
        if own != overflow {
            match self.slots[own].0.try_lock() {
                Ok(mut g) => return f(own, &mut g),
                Err(TryLockError::Poisoned(e)) => return f(own, &mut e.into_inner()),
                // Re-entered from nested work on the same worker: use the overflow slot.
                Err(TryLockError::WouldBlock) => {}
            }
        }
        let mut g: MutexGuard<'_, T> =
            self.slots[overflow].0.lock().unwrap_or_else(PoisonError::into_inner);
        f(overflow, &mut g)
    }

    /// Mutable access to every slot (no locking needed with `&mut self`).
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.slots.iter_mut().map(|s| s.0.get_mut().unwrap_or_else(PoisonError::into_inner))
    }

    /// Consumes the set, returning the slot values in slot-index order.
    pub fn into_vec(self) -> Vec<T> {
        let mut out = Vec::with_capacity(self.slots.len());
        for s in self.slots.into_vec() {
            out.push(s.0.into_inner().unwrap_or_else(PoisonError::into_inner));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayon::prelude::*;

    #[test]
    fn bump_basic_types_and_alignment() {
        let b = Bump::with_capacity(64);
        let x = b.alloc(1u8);
        let y = b.alloc(0x1122_3344_5566_7788u64);
        let z = b.alloc(7u16);
        let w = b.alloc(9u128);
        assert_eq!((*x, *y, *z, *w), (1, 0x1122_3344_5566_7788, 7, 9));
        assert_eq!((y as *mut u64).addr() % mem::align_of::<u64>(), 0);
        assert_eq!((w as *mut u128).addr() % mem::align_of::<u128>(), 0);
        *x = 2;
        *y += 1;
        assert_eq!((*x, *y), (2, 0x1122_3344_5566_7789));
    }

    #[test]
    fn bump_over_aligned_type() {
        #[derive(Clone, Copy)]
        #[repr(align(64))]
        struct Big(u8);
        let b = Bump::with_capacity(16);
        let _ = b.alloc(1u8);
        for i in 0..100u8 {
            let p = b.alloc(Big(i));
            assert_eq!((p as *mut Big).addr() % 64, 0);
            assert_eq!(p.0, i);
        }
    }

    #[test]
    fn bump_slices_strings_and_growth() {
        let b = Bump::with_capacity(32);
        let mut kept = Vec::new();
        for i in 0..2000u32 {
            let s = b.alloc_slice_copy(&[i, i + 1, i + 2]);
            kept.push(s);
        }
        for (i, s) in kept.iter().enumerate() {
            let i = i as u32;
            assert_eq!(**s, [i, i + 1, i + 2]);
        }
        let st = b.alloc_str("héllo");
        assert_eq!(st, "héllo");
        let by = b.alloc_bytes(b"\xff\x00abc");
        assert_eq!(by, b"\xff\x00abc");
        assert!(b.chunks.borrow().len() > 1);
        // A request larger than the growth limit gets its own chunk.
        let big = b.alloc_slice_fill(MAX_GROWTH_CHUNK + 1, 3u8);
        assert_eq!(big.len(), MAX_GROWTH_CHUNK + 1);
        assert!(big.iter().all(|&v| v == 3));
    }

    #[test]
    fn bump_empty_and_zero_sized() {
        let b = Bump::new();
        let e: &mut [u64] = b.alloc_slice_copy(&[]);
        assert!(e.is_empty());
        let z = b.alloc_slice_fill(10, ());
        assert!(z.is_empty());
        let unit = b.alloc(());
        assert_eq!(*unit, ());
        assert_eq!(b.capacity_bytes(), 0, "nothing should have been allocated");
        assert_eq!(b.alloc_str(""), "");
    }

    #[test]
    fn bump_from_iter_handles_lying_iterators() {
        struct Liar(u32, usize);
        impl Iterator for Liar {
            type Item = u32;
            fn next(&mut self) -> Option<u32> {
                if self.0 == 0 {
                    return None;
                }
                self.0 -= 1;
                Some(self.0)
            }
            fn size_hint(&self) -> (usize, Option<usize>) {
                (self.1, Some(self.1))
            }
        }
        impl ExactSizeIterator for Liar {}
        let b = Bump::new();
        assert_eq!(b.alloc_slice_from_iter(Liar(2, 5)), &[1, 0]);
        assert_eq!(b.alloc_slice_from_iter(Liar(9, 3)), &[8, 7, 6]);
        assert_eq!(b.alloc_slice_from_iter([1u16, 2, 3]), &[1, 2, 3]);
    }

    #[test]
    fn bump_iterator_may_allocate_from_same_arena() {
        let b = Bump::with_capacity(16);
        let out = b.alloc_slice_from_iter((0..50u32).map(|i| *b.alloc(u64::from(i) * 2)));
        assert_eq!(out.len(), 50);
        for (i, v) in out.iter().enumerate() {
            assert_eq!(*v, i as u64 * 2);
        }
    }

    #[test]
    fn bump_reset_keeps_largest_chunk() {
        let mut b = Bump::with_capacity(32);
        for i in 0..10_000u64 {
            b.alloc(i);
        }
        let largest = b.chunks.borrow().iter().map(|c| c.size).max().unwrap();
        b.reset();
        assert_eq!(b.capacity_bytes(), largest);
        let v = b.alloc_slice_fill(largest / 8, 5u64);
        assert!(v.iter().all(|&x| x == 5));
        assert_eq!(b.chunks.borrow().len(), 1, "reset chunk should be reused");
        let mut empty = Bump::new();
        empty.reset();
        assert_eq!(empty.capacity_bytes(), 0);
    }

    #[test]
    fn chunk_size_is_checked_and_aligned() {
        assert_eq!(chunk_size(0, 1), Some(CHUNK_ALIGN));
        assert_eq!(chunk_size(100, 3), Some(112));
        assert_eq!(chunk_size(64, 1000), Some(1008));
        assert_eq!(chunk_size(usize::MAX, 1), None);
        assert_eq!(chunk_size(isize::MAX as usize, 1), None);
        // A huge requested first chunk allocates nothing up front.
        let b = Bump::with_capacity(usize::MAX);
        assert_eq!(b.capacity_bytes(), 0);
    }

    #[test]
    fn bump_is_send_across_threads() {
        let b = Bump::new();
        let h = std::thread::spawn(move || {
            let s = b.alloc_slice_copy(&[1u32, 2, 3]).len();
            (b, s)
        });
        let (b, s) = h.join().unwrap();
        assert_eq!(s, 3);
        assert!(b.capacity_bytes() > 0);
    }

    #[test]
    fn pool_slices_and_merge() {
        let mut a: Pool<u32> = Pool::new();
        let s1 = a.push_slice(&[1, 2, 3]);
        let m = a.mark();
        a.push(4);
        a.push(5);
        let s2 = a.since(m);
        let mut b: Pool<u32> = Pool::new();
        let s3 = b.extend([10, 11]);
        let s4 = b.push_slice(&[]);
        let i12 = b.push(12);
        assert_eq!(a.slice(s1), &[1, 2, 3]);
        assert_eq!(a.slice(s2), &[4, 5]);
        assert!(s4.is_empty());

        let base = a.append(&mut b);
        assert_eq!(base, 5);
        assert!(b.is_empty());
        assert_eq!(a.slice(s3.shift(base)), &[10, 11]);
        assert_eq!(*a.get(i12 + base), 12);
        assert_eq!(a.slice(s1), &[1, 2, 3], "existing handles stay valid");
        for v in a.slice_mut(s2) {
            *v *= 10;
        }
        assert_eq!(a.as_slice(), &[1, 2, 3, 40, 50, 10, 11, 12]);
        assert_eq!(Slice::<u32>::default(), Slice::EMPTY);
        assert_eq!(mem::size_of::<Slice<u64>>(), 8);
    }

    #[test]
    fn per_thread_parallel_collect_and_merge() {
        let pt: PerThread<Pool<u32>> = PerThread::new(Pool::new);
        let handles: Vec<(usize, Slice<u32>)> = (0..10_000u32)
            .into_par_iter()
            .map(|i| pt.with(|slot, pool| (slot, pool.push_slice(&[i, i ^ 0xffff]))))
            .collect();
        let pools = pt.into_vec();
        let mut merged = Pool::new();
        let mut bases = Vec::new();
        for mut p in pools {
            bases.push(merged.append(&mut p));
        }
        assert_eq!(merged.len(), 20_000);
        for (i, (slot, s)) in handles.into_iter().enumerate() {
            let i = i as u32;
            assert_eq!(merged.slice(s.shift(bases[slot])), &[i, i ^ 0xffff]);
        }
    }

    #[test]
    fn per_thread_outside_pool_uses_overflow_and_reentry_works() {
        let pt: PerThread<Vec<u32>> = PerThread::new(Vec::new);
        let slot = pt.with(|slot, v| {
            v.push(1);
            // Re-entry on the same (non-pool) thread would block on the overflow slot, so only
            // check the pool path re-enters: run it inside the pool.
            slot
        });
        assert_eq!(slot, pt.len() - 1);
        rayon::scope(|_| {
            pt.with(|outer, _| {
                let inner = pt.with(|inner, v| {
                    v.push(2);
                    inner
                });
                if rayon::current_thread_index().is_some_and(|i| i < pt.len() - 1) {
                    assert_ne!(outer, inner, "re-entry must not alias the held slot");
                }
            });
        });
        let all: Vec<u32> = pt.into_vec().into_iter().flatten().collect();
        assert_eq!(all.len(), 2);
    }
}
