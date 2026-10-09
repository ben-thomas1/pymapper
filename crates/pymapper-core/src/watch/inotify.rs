//! Linux backend: inotify with one watch per directory (inotify is not recursive), added
//! for the whole tree at start and for every directory created or moved in later. A reader
//! thread polls the inotify descriptor and a stop pipe.
//!
//! Hidden directories (`.git`, `.venv`, ...) and `__pycache__` are not watched: the walker
//! skips them by default and they are where most unrelated churn happens; each watch costs
//! kernel memory and counts against `fs.inotify.max_user_watches`.

use std::ffi::c_void;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use super::queue::{FLAG_DIR, Queue};
use super::{WatchError, WatchOptions};
use crate::intern::FxHashMap;

const MASK: u32 = libc::IN_MODIFY
    | libc::IN_CLOSE_WRITE
    | libc::IN_ATTRIB
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR
    | libc::IN_DONT_FOLLOW
    | libc::IN_EXCL_UNLINK;

/// Read buffer: room for hundreds of events per `read`, aligned for `inotify_event`.
#[repr(C, align(8))]
struct EventBuf([u8; 64 * 1024]);

/// A running watch. Dropping it stops and joins the reader thread and closes the
/// descriptors.
pub(super) struct Backend {
    stop_w: i32,
    thread: Option<JoinHandle<()>>,
}

/// The reader thread's state.
struct Reader {
    fd: i32,
    stop_r: i32,
    /// Watch descriptor -> directory path (as given: root + relative components).
    wds: FxHashMap<i32, Box<[u8]>>,
    /// Watch descriptors of the roots: losing one means rescanning.
    roots: Vec<i32>,
    queue: Arc<Queue>,
    /// Scratch for nul-terminated paths.
    cpath: Vec<u8>,
}

impl Backend {
    pub fn start(roots: &[PathBuf], queue: Arc<Queue>, _opts: &WatchOptions) -> Result<Backend, WatchError> {
        // SAFETY: plain syscall without pointers.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(WatchError::Io(std::io::Error::last_os_error()));
        }
        let mut pipe = [0i32; 2];
        // SAFETY: `pipe` has room for the two descriptors pipe2 writes.
        if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            let e = std::io::Error::last_os_error();
            close(fd);
            return Err(WatchError::Io(e));
        }
        // From here on the reader owns `fd` and the read end (closed by its `Drop`).
        let mut r = Reader { fd, stop_r: pipe[0], wds: FxHashMap::default(), roots: Vec::new(), queue, cpath: Vec::new() };
        for root in roots {
            match r.add(root.as_os_str().as_bytes()) {
                Ok(wd) => r.roots.push(wd),
                Err(e) => {
                    drop(r);
                    close(pipe[1]);
                    return Err(WatchError::Root(root.clone(), e));
                }
            }
            r.add_subdirs(root.as_os_str().as_bytes());
        }
        let thread = std::thread::Builder::new().name("pymapper-watch".to_owned()).spawn(move || r.run());
        match thread {
            Ok(t) => Ok(Backend { stop_w: pipe[1], thread: Some(t) }),
            Err(e) => {
                // The closure (and with it the reader's descriptors) was dropped unrun.
                close(pipe[1]);
                Err(WatchError::Io(e))
            }
        }
    }

    /// inotify delivers events to the queue as they happen: nothing to flush.
    pub fn flush(&self) {}
}

impl Drop for Backend {
    fn drop(&mut self) {
        let b = 1u8;
        // SAFETY: writes one byte from a live local to our own pipe.
        unsafe { libc::write(self.stop_w, (&raw const b).cast::<c_void>(), 1) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        close(self.stop_w);
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        close(self.fd);
        close(self.stop_r);
    }
}

fn close(fd: i32) {
    // SAFETY: closes a descriptor we own and never use again.
    unsafe { libc::close(fd) };
}

/// Directory names that are never watched (see the module docs).
fn skipped(name: &[u8]) -> bool {
    name.first() == Some(&b'.') || name == b"__pycache__"
}

impl Reader {
    /// Adds a watch on directory `dir`; returns its descriptor.
    fn add(&mut self, dir: &[u8]) -> std::io::Result<i32> {
        self.cpath.clear();
        self.cpath.extend_from_slice(dir);
        self.cpath.push(0);
        // SAFETY: `cpath` is nul-terminated and lives across the call.
        let wd = unsafe { libc::inotify_add_watch(self.fd, self.cpath.as_ptr().cast(), MASK) };
        if wd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        self.wds.insert(wd, dir.into());
        Ok(wd)
    }

    /// Watches every directory below `dir` (iteratively, no symlinks, skipping hidden
    /// directories and `__pycache__`). Directories that vanish or cannot be read are
    /// skipped; running out of watches asks for a rescan (the session then works from walks).
    fn add_subdirs(&mut self, dir: &[u8]) {
        let mut stack: Vec<Box<[u8]>> = vec![dir.into()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(std::ffi::OsStr::from_bytes(&d)) else { continue };
            for e in rd.flatten() {
                if !e.file_type().is_ok_and(|t| t.is_dir()) || skipped(e.file_name().as_bytes()) {
                    continue;
                }
                let mut p = Vec::with_capacity(d.len() + 1 + e.file_name().len());
                p.extend_from_slice(&d);
                p.push(b'/');
                p.extend_from_slice(e.file_name().as_bytes());
                match self.add(&p) {
                    Ok(_) => stack.push(p.into()),
                    Err(err) if err.raw_os_error() == Some(libc::ENOSPC) => {
                        self.queue.push_rescan();
                        return;
                    }
                    Err(_) => {}
                }
            }
        }
    }

    /// Removes the watches of `dir` and everything below it (a directory moved away: its
    /// watches would report under the old path). O(watches); directory moves are rare.
    fn remove_tree(&mut self, dir: &[u8]) {
        let fd = self.fd;
        self.wds.retain(|&wd, p| {
            let under = p.starts_with(dir) && (p.len() == dir.len() || p[dir.len()] == b'/');
            if under {
                // SAFETY: plain syscall on our descriptor; a stale wd only yields EINVAL.
                unsafe { libc::inotify_rm_watch(fd, wd) };
            }
            !under
        });
    }

    fn run(mut self) {
        let mut buf = Box::new(EventBuf([0; 64 * 1024]));
        // Events of one read: (wd, mask, name start, name end) into `buf`.
        let mut events: Vec<(i32, u32, usize, usize)> = Vec::new();
        let mut dirs: Vec<(u32, Box<[u8]>)> = Vec::new();
        loop {
            let mut fds = [
                libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.stop_r, events: libc::POLLIN, revents: 0 },
            ];
            // SAFETY: `fds` is a live array of two pollfd structs.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                self.queue.stop();
                return;
            }
            if fds[1].revents != 0 {
                return;
            }
            loop {
                // SAFETY: reads at most the buffer's length into the owned buffer.
                let got = unsafe { libc::read(self.fd, buf.0.as_mut_ptr().cast(), buf.0.len()) };
                if got <= 0 {
                    break;
                }
                parse(&buf.0[..got as usize], &mut events);
                self.deliver(&buf.0, &events, &mut dirs);
            }
        }
    }

    /// Pushes the events of one read to the queue, then updates the watches for
    /// directories created, moved in or moved away.
    fn deliver(&mut self, buf: &[u8], events: &[(i32, u32, usize, usize)], dirs: &mut Vec<(u32, Box<[u8]>)>) {
        if events.iter().any(|&(wd, mask, ..)| mask & libc::IN_Q_OVERFLOW != 0 || (self.roots.contains(&wd) && mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0)) {
            self.queue.push_rescan();
        }
        dirs.clear();
        let wds = &self.wds;
        let items = events.iter().filter_map(|&(wd, mask, s, e)| {
            if mask & libc::IN_IGNORED != 0 {
                return None;
            }
            let dir = wds.get(&wd)?;
            let flags = if mask & libc::IN_ISDIR != 0 { FLAG_DIR } else { 0 };
            Some(if s == e { ([&dir[..], &[][..], &[][..]], flags | FLAG_DIR) } else { ([&dir[..], &b"/"[..], &buf[s..e]], flags) })
        });
        let items: Vec<([&[u8]; 3], u8)> = items.collect();
        self.queue.push(items.iter().map(|(p, f)| (&p[..], *f)));
        for &(wd, mask, s, e) in events {
            if mask & libc::IN_ISDIR == 0 || s == e || skipped(&buf[s..e]) {
                continue;
            }
            if mask & (libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_MOVED_FROM) == 0 {
                continue;
            }
            if let Some(dir) = self.wds.get(&wd) {
                let mut p = Vec::with_capacity(dir.len() + 1 + e - s);
                p.extend_from_slice(dir);
                p.push(b'/');
                p.extend_from_slice(&buf[s..e]);
                dirs.push((mask, p.into()));
            }
        }
        for (mask, p) in dirs.drain(..) {
            if mask & libc::IN_MOVED_FROM != 0 {
                self.remove_tree(&p);
            } else if self.add(&p).is_ok() {
                self.add_subdirs(&p);
            }
        }
        // The kernel dropped these watches (directory deleted or unmounted): forget them.
        for &(wd, mask, ..) in events {
            if mask & libc::IN_IGNORED != 0 {
                self.wds.remove(&wd);
            }
        }
    }
}

/// Splits one `read` of the inotify descriptor into `(wd, mask, name start, name end)`
/// (the name without its nul padding; empty for events on the watched directory itself).
fn parse(buf: &[u8], out: &mut Vec<(i32, u32, usize, usize)>) {
    out.clear();
    let head = std::mem::size_of::<libc::inotify_event>();
    let mut off = 0;
    while off + head <= buf.len() {
        // SAFETY: `off + head <= buf.len()`, and read_unaligned has no alignment requirement.
        let ev: libc::inotify_event = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off).cast()) };
        let s = off + head;
        let e = (s + ev.len as usize).min(buf.len());
        let name = &buf[s..e];
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        out.push((ev.wd, ev.mask, s, s + len));
        off = e;
    }
}
