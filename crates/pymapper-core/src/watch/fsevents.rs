//! macOS backend: one FSEvents stream over every root with per-file events, delivered on a
//! private serial dispatch queue (no run loop thread of our own).
//!
//! FSEvents reports canonical paths (`/private/var/...` for `/var/...`); the callback maps
//! them back to the roots as the caller spelled them, so paths match the walker's.

use std::ffi::{CStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;

use super::queue::{FLAG_DIR, Queue};
use super::{WatchError, WatchOptions};

type CFIndex = isize;
type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type FSEventStreamRef = *mut c_void;
type ConstFSEventStreamRef = *const c_void;
type DispatchQueue = *mut c_void;

#[repr(C)]
struct CFArrayCallBacks {
    version: CFIndex,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

#[repr(C)]
struct FSEventStreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

type FSEventStreamCallback = extern "C" fn(
    stream: ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    event_ids: *const u64,
);

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const SINCE_NOW: u64 = u64::MAX;
const CREATE_NO_DEFER: u32 = 0x02;
const CREATE_WATCH_ROOT: u32 = 0x04;
const CREATE_FILE_EVENTS: u32 = 0x10;
const EVENT_MUST_SCAN_SUBDIRS: u32 = 0x01;
const EVENT_USER_DROPPED: u32 = 0x02;
const EVENT_KERNEL_DROPPED: u32 = 0x04;
const EVENT_ROOT_CHANGED: u32 = 0x20;
const EVENT_IS_DIR: u32 = 0x0002_0000;
/// Seconds FSEvents may coalesce events for; with `NoDefer` the first event of a quiet
/// period is delivered at once and only bursts wait this long.
const LATENCY_S: f64 = 0.005;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: CFArrayCallBacks;
    fn CFStringCreateWithBytes(alloc: CFAllocatorRef, bytes: *const u8, len: CFIndex, encoding: u32, external: u8) -> CFStringRef;
    fn CFArrayCreate(alloc: CFAllocatorRef, values: *const CFTypeRef, len: CFIndex, callbacks: *const CFArrayCallBacks) -> CFArrayRef;
    fn CFRelease(obj: CFTypeRef);
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        alloc: CFAllocatorRef,
        callback: FSEventStreamCallback,
        context: *const FSEventStreamContext,
        paths: CFArrayRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: DispatchQueue);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventStreamFlushSync(stream: FSEventStreamRef);
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
}

// libdispatch is part of libSystem: no framework to link.
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_release(obj: *mut c_void);
}

/// A root prefix: `(canonical, as given)`.
type Prefix = (Box<[u8]>, Box<[u8]>);

/// What the callback reads: the queue and the root prefixes to translate.
struct Ctx {
    queue: Arc<Queue>,
    /// `(canonical, as given)` root prefixes, longest canonical first.
    roots: Vec<Prefix>,
}

/// A running stream. Dropping it stops the stream and frees the context.
pub(super) struct Backend {
    stream: FSEventStreamRef,
    dispatch: DispatchQueue,
    ctx: *mut Ctx,
}

// SAFETY: the stream and the dispatch queue are thread-safe CoreFoundation / libdispatch
// objects; the context is only read by the callback (on the dispatch queue) and freed in
// `Drop` after the stream is invalidated, when no callback can run any more.
unsafe impl Send for Backend {}

impl Backend {
    /// Starts watching `roots` (directories that exist), reporting into `queue`.
    pub fn start(roots: &[PathBuf], queue: Arc<Queue>, _opts: &WatchOptions) -> Result<Backend, WatchError> {
        let mut prefixes = Vec::with_capacity(roots.len());
        for r in roots {
            let canon = std::fs::canonicalize(r).map_err(|e| WatchError::Root(r.clone(), e))?;
            prefixes.push((canon.as_os_str().as_bytes().into(), r.as_os_str().as_bytes().into()));
        }
        prefixes.sort_by_key(|p: &Prefix| std::cmp::Reverse(p.0.len()));
        let ctx = Box::into_raw(Box::new(Ctx { queue, roots: prefixes }));
        // SAFETY: `ctx` was just leaked from a Box and is non-null; on failure it is reclaimed
        // below exactly once, on success by `Drop`.
        match unsafe { create(&(*ctx).roots, ctx) } {
            Ok((stream, dispatch)) => Ok(Backend { stream, dispatch, ctx }),
            Err(e) => {
                // SAFETY: no stream refers to `ctx` (creation failed or the stream was released).
                drop(unsafe { Box::from_raw(ctx) });
                Err(e)
            }
        }
    }

    /// Delivers every event that happened before this call to the queue (blocks).
    pub fn flush(&self) {
        // SAFETY: `self.stream` is a started, valid stream until `Drop`.
        unsafe { FSEventStreamFlushSync(self.stream) };
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        // SAFETY: the stream is valid and started; after Stop + Invalidate no callback runs or
        // is queued, so releasing the stream, the dispatch queue and the context is sound.
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            dispatch_release(self.dispatch);
            drop(Box::from_raw(self.ctx));
        }
    }
}

/// Creates and starts the stream over the canonical root paths.
///
/// # Safety
/// `ctx` must stay valid until the returned stream is invalidated.
unsafe fn create(roots: &[Prefix], ctx: *mut Ctx) -> Result<(FSEventStreamRef, DispatchQueue), WatchError> {
    let mut strings: Vec<CFTypeRef> = Vec::with_capacity(roots.len());
    for (canon, _) in roots {
        // SAFETY: the pointer and length describe the live `canon` slice.
        let s = unsafe { CFStringCreateWithBytes(std::ptr::null(), canon.as_ptr(), canon.len() as CFIndex, K_CF_STRING_ENCODING_UTF8, 0) };
        if s.is_null() {
            release_all(&strings);
            return Err(WatchError::Backend("a root path is not valid UTF-8 (FSEvents needs UTF-8 paths)"));
        }
        strings.push(s);
    }
    // SAFETY: `strings` holds `len` valid CFStrings; the array retains them.
    let array = unsafe { CFArrayCreate(std::ptr::null(), strings.as_ptr(), strings.len() as CFIndex, &raw const kCFTypeArrayCallBacks) };
    release_all(&strings);
    if array.is_null() {
        return Err(WatchError::Backend("CFArrayCreate failed"));
    }
    let context = FSEventStreamContext {
        version: 0,
        info: ctx.cast(),
        retain: std::ptr::null(),
        release: std::ptr::null(),
        copy_description: std::ptr::null(),
    };
    let flags = CREATE_NO_DEFER | CREATE_WATCH_ROOT | CREATE_FILE_EVENTS;
    // SAFETY: the array is a valid CFArray of CFStrings and the context outlives the call
    // (FSEvents copies it); `ctx` stays valid while the stream lives (caller's contract).
    let stream = unsafe { FSEventStreamCreate(std::ptr::null(), callback, &context, array, SINCE_NOW, LATENCY_S, flags) };
    // SAFETY: the stream retains the paths it needs; our reference is no longer used.
    unsafe { CFRelease(array) };
    if stream.is_null() {
        return Err(WatchError::Backend("FSEventStreamCreate failed"));
    }
    // SAFETY: the label is a nul-terminated static string; a null attribute makes a serial queue.
    let dispatch = unsafe { dispatch_queue_create(c"pymapper.watch".as_ptr(), std::ptr::null()) };
    if dispatch.is_null() {
        // SAFETY: the stream was created above and never started.
        unsafe { FSEventStreamRelease(stream) };
        return Err(WatchError::Backend("dispatch_queue_create failed"));
    }
    // SAFETY: valid stream and queue; the stream retains the queue.
    let started = unsafe {
        FSEventStreamSetDispatchQueue(stream, dispatch);
        FSEventStreamStart(stream)
    };
    if started == 0 {
        // SAFETY: the stream was scheduled but not started: invalidate before releasing.
        unsafe {
            FSEventStreamInvalidate(stream);
            FSEventStreamRelease(stream);
            dispatch_release(dispatch);
        }
        return Err(WatchError::Backend("FSEventStreamStart failed"));
    }
    Ok((stream, dispatch))
}

fn release_all(objs: &[CFTypeRef]) {
    for &o in objs {
        // SAFETY: each object was created by us with a +1 reference that we own.
        unsafe { CFRelease(o) };
    }
}

/// Maps a canonical path reported by FSEvents back to the roots as given.
fn translate<'a>(roots: &'a [Prefix], path: &'a [u8]) -> (&'a [u8], &'a [u8]) {
    for (canon, given) in roots {
        if let Some(rest) = path.strip_prefix(&canon[..])
            && (rest.is_empty() || rest[0] == b'/')
        {
            return (given, rest);
        }
    }
    (&[], path)
}

extern "C" fn callback(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    _event_ids: *const u64,
) {
    if info.is_null() || num_events == 0 {
        return;
    }
    // SAFETY: `info` is the `Ctx` passed at creation, valid until the stream is invalidated
    // (see `Backend`); FSEvents passes `num_events` C strings (no `UseCFTypes` flag) and as many
    // flags.
    let (ctx, paths, flags) = unsafe {
        (
            &*(info as *const Ctx),
            std::slice::from_raw_parts(event_paths as *const *const c_char, num_events),
            std::slice::from_raw_parts(event_flags, num_events),
        )
    };
    if flags.iter().any(|&f| f & (EVENT_MUST_SCAN_SUBDIRS | EVENT_USER_DROPPED | EVENT_KERNEL_DROPPED | EVENT_ROOT_CHANGED) != 0) {
        ctx.queue.push_rescan();
        return;
    }
    // Parts per event: the root as given + the rest of the canonical path (one small
    // allocation per delivery, not per event).
    let mut parts: Vec<[&[u8]; 2]> = Vec::with_capacity(num_events);
    for &p in paths {
        // SAFETY: FSEvents passes valid nul-terminated strings for the duration of the call.
        let bytes = unsafe { CStr::from_ptr(p) }.to_bytes();
        let (a, b) = translate(&ctx.roots, bytes);
        parts.push([a, b]);
    }
    ctx.queue.push(parts.iter().zip(flags).map(|(p, &f)| (&p[..], if f & EVENT_IS_DIR != 0 { FLAG_DIR } else { 0 })));
}

#[cfg(test)]
mod tests {
    use super::{Prefix, translate};

    #[test]
    fn translate_maps_canonical_prefixes_back() {
        let roots: Vec<Prefix> = vec![
            ((&b"/private/var/x/proj"[..]).into(), (&b"/var/x/proj"[..]).into()),
            ((&b"/private/var/x"[..]).into(), (&b"/var/x"[..]).into()),
        ];
        assert_eq!(translate(&roots, b"/private/var/x/proj/a.py"), (&b"/var/x/proj"[..], &b"/a.py"[..]));
        assert_eq!(translate(&roots, b"/private/var/x/other.py"), (&b"/var/x"[..], &b"/other.py"[..]));
        assert_eq!(translate(&roots, b"/private/var/x/projector.py"), (&b"/var/x"[..], &b"/projector.py"[..]));
        assert_eq!(translate(&roots, b"/private/var/x/proj"), (&b"/var/x/proj"[..], &b""[..]));
        assert_eq!(translate(&roots, b"/elsewhere/y.py"), (&b""[..], &b"/elsewhere/y.py"[..]));
    }
}
