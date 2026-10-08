//! Read-only file mappings (libc `mmap`) and the aligned byte buffer the readers share with
//! tests.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;

/// A read-only, private mapping of a whole file.
///
/// Cache files are only ever replaced by `rename` (never written in place), so a mapping keeps
/// seeing the bytes it was opened on. Another process truncating the file under us would
/// raise `SIGBUS`; nothing in pymapper does that.
pub(crate) struct Mmap {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is read-only and owned by this value; sharing `&[u8]` views of it
// across threads is like sharing an immutable heap buffer.
unsafe impl Send for Mmap {}
// SAFETY: see `Send`: no interior mutability, the bytes never change while mapped.
unsafe impl Sync for Mmap {}

impl Mmap {
    /// Maps `file` (its current length). Empty files cannot be mapped: `InvalidData`.
    pub fn map(file: &File) -> io::Result<Mmap> {
        let len = file.metadata()?.len();
        let len = usize::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::FileTooLarge))?;
        if len == 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        // SAFETY: a fresh private read-only mapping of an open descriptor at a kernel-chosen
        // address; no existing memory is affected. The result is checked below.
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0)
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(p.cast::<u8>()).ok_or_else(|| io::Error::from(io::ErrorKind::Other))?;
        Ok(Mmap { ptr, len })
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr..ptr + len` is a live read-only mapping owned by `self` (unmapped only
        // in `drop`), and mapped file bytes are always initialised.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly the region returned by `mmap`; no borrow of it outlives
        // `self` (`as_slice` borrows `self`).
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

/// Bytes of a cache file: mapped, or owned in an 8-byte aligned buffer (small files, tests).
/// Either way the start is 8-byte aligned, so aligned sections can be viewed as `&[u32]` etc.
pub(crate) enum Bytes {
    Mapped(Mmap),
    Owned { words: Vec<u64>, len: usize },
}

impl Bytes {
    /// Copies `b` into an aligned buffer.
    pub fn copy_from(b: &[u8]) -> Bytes {
        let mut words = vec![0u64; b.len().div_ceil(8)];
        for (w, c) in words.iter_mut().zip(b.chunks(8)) {
            let mut a = [0u8; 8];
            a[..c.len()].copy_from_slice(c);
            *w = u64::from_ne_bytes(a);
        }
        Bytes::Owned { words, len: b.len() }
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Bytes::Mapped(m) => m.as_slice(),
            Bytes::Owned { words, len } => &as_bytes(words.as_slice())[..*len],
        }
    }
}

/// Plain little-endian scalar types that can be viewed in place.
///
/// # Safety
/// Implementors must have no padding and accept every bit pattern.
pub(crate) unsafe trait Pod: Copy + Default + 'static {}
// SAFETY: primitive integers: no padding, every bit pattern valid.
unsafe impl Pod for u8 {}
// SAFETY: as above.
unsafe impl Pod for u16 {}
// SAFETY: as above.
unsafe impl Pod for u32 {}
// SAFETY: as above.
unsafe impl Pod for u64 {}

/// The bytes of a scalar slice (native order; the format is little-endian and the cache is
/// only used on little-endian hosts, see [`super::NATIVE_OK`]).
#[inline]
pub(crate) fn as_bytes<T: Pod>(s: &[T]) -> &[u8] {
    // SAFETY: `T: Pod` has no padding, so all `size_of_val(s)` bytes are initialised; `u8` has
    // alignment 1 and the lifetime is tied to `s`.
    unsafe { std::slice::from_raw_parts(s.as_ptr().cast::<u8>(), size_of_val(s)) }
}

/// Views `b` as a slice of `T`; `None` if misaligned or not a whole number of elements.
#[inline]
pub(crate) fn cast<T: Pod>(b: &[u8]) -> Option<&[T]> {
    let size = size_of::<T>();
    if !b.len().is_multiple_of(size) || !(b.as_ptr() as usize).is_multiple_of(align_of::<T>()) {
        return None;
    }
    // SAFETY: checked alignment and length above; `T: Pod` accepts every bit pattern; the
    // lifetime is tied to `b`.
    Some(unsafe { std::slice::from_raw_parts(b.as_ptr().cast::<T>(), b.len() / size) })
}
