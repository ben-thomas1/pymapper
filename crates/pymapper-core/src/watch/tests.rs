use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use super::queue::{Queue, ready_at};
use super::*;
use crate::incremental::Program;
use crate::index::{self, Inputs};
use crate::resolve::Attribution;
use crate::roots::Origin;
use crate::walk::{WalkOptions, walk};

/// A fresh temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-watch-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, contents).expect("write");
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn opts(quiet_ms: u64, max_ms: u64) -> WatchOptions {
    WatchOptions { quiet: Duration::from_millis(quiet_ms), max_delay: Duration::from_millis(max_ms), max_pending: 1 << 20 }
}

/// A batch of `paths` (with flags) through a queue with no debounce delay.
fn batch_of(paths: &[(&[u8], u8)]) -> Batch {
    let q = Queue::new(opts(0, 0));
    let parts: Vec<[&[u8]; 1]> = paths.iter().map(|(p, _)| [*p]).collect();
    q.push(parts.iter().zip(paths).map(|(p, &(_, f))| (&p[..], f)));
    let mut b = Batch::new();
    assert_eq!(q.wait(&mut b, Some(Duration::from_secs(1))), Wait::Ready);
    b
}

fn rescan_batch() -> Batch {
    let q = Queue::new(opts(0, 0));
    q.push_rescan();
    let mut b = Batch::new();
    assert_eq!(q.wait(&mut b, Some(Duration::from_secs(1))), Wait::Ready);
    b
}

#[test]
fn batches_are_sorted_deduplicated_and_flags_merged() {
    let q = Queue::new(opts(0, 0));
    q.push([(&[&b"/r/b.py"[..]][..], 0), (&[&b"/r/a"[..], &b"/x.py"[..]][..], 0), (&[&b"/r/b.py"[..]][..], 0)]);
    q.push([(&[&b"/r/dir"[..]][..], FLAG_DIR), (&[&b"/r/a/x.py"[..]][..], 0), (&[&b"/r/dir"[..]][..], 0)]);
    let mut b = Batch::new();
    assert_eq!(q.wait(&mut b, Some(Duration::from_secs(1))), Wait::Ready);
    let got: Vec<&[u8]> = (0..b.len()).map(|i| b.path_bytes(i)).collect();
    assert_eq!(got, [&b"/r/a/x.py"[..], b"/r/b.py", b"/r/dir"]);
    assert_eq!(b.flags(2), FLAG_DIR);
    assert_eq!(b.flags(0), 0);
    assert_eq!(b.events(), 6);
    assert!(!b.rescan());
    assert!(b.first_event().is_some() && b.first_event() <= b.last_event());
    // The queue is empty again; the batch's buffers went back to it.
    assert_eq!(q.wait(&mut b, Some(Duration::from_millis(10))), Wait::Timeout);
    assert_eq!(b.len(), 3, "a timeout leaves the caller's batch alone");
}

#[test]
fn ready_at_is_quiet_after_last_but_capped_by_max_delay() {
    let o = opts(30, 300);
    let t0 = Instant::now();
    assert_eq!(ready_at(t0, t0, &o), t0 + Duration::from_millis(30));
    let last = t0 + Duration::from_millis(100);
    assert_eq!(ready_at(t0, last, &o), last + Duration::from_millis(30));
    let late = t0 + Duration::from_millis(290);
    assert_eq!(ready_at(t0, late, &o), t0 + Duration::from_millis(300));
}

#[test]
fn wait_debounces_until_quiet() {
    let q = Arc::new(Queue::new(opts(40, 2000)));
    let q2 = Arc::clone(&q);
    let t0 = Instant::now();
    let producer = std::thread::spawn(move || {
        for i in 0..5u8 {
            let name = [b'a' + i];
            q2.push([(&[&name[..]][..], 0)]);
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let mut b = Batch::new();
    assert_eq!(q.wait(&mut b, Some(Duration::from_secs(5))), Wait::Ready);
    producer.join().expect("producer");
    // Five events 10 ms apart, closed 40 ms after the last: one batch with all of them.
    assert_eq!(b.len(), 5, "batch closed early: {:?}", b.paths().collect::<Vec<_>>());
    assert!(t0.elapsed() >= Duration::from_millis(80));
}

#[test]
fn wait_caps_a_continuous_stream_at_max_delay() {
    let q = Arc::new(Queue::new(opts(50, 100)));
    let q2 = Arc::clone(&q);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let producer = std::thread::spawn(move || {
        while !stop2.load(Ordering::Relaxed) {
            q2.push([(&[&b"x.py"[..]][..], 0)]);
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let mut b = Batch::new();
    let t0 = Instant::now();
    assert_eq!(q.wait(&mut b, Some(Duration::from_secs(5))), Wait::Ready);
    let took = t0.elapsed();
    stop.store(true, Ordering::Relaxed);
    producer.join().expect("producer");
    assert!(took < Duration::from_millis(1000), "never closed under a stream: {took:?}");
    assert!(b.events() > 1);
    assert_eq!(b.len(), 1);
}

#[test]
fn stop_wakes_waiters() {
    let q = Arc::new(Queue::new(opts(10, 100)));
    let q2 = Arc::clone(&q);
    let t = std::thread::spawn(move || {
        let mut b = Batch::new();
        q2.wait(&mut b, None)
    });
    std::thread::sleep(Duration::from_millis(20));
    q.stop();
    assert_eq!(t.join().expect("waiter"), Wait::Stopped);
    assert!(q.stopped());
}

#[test]
fn too_many_paths_degrade_to_rescan() {
    let q = Queue::new(WatchOptions { max_pending: 3, ..opts(0, 0) });
    let names: Vec<[u8; 1]> = (0..10u8).map(|i| [b'a' + i]).collect();
    let parts: Vec<[&[u8]; 1]> = names.iter().map(|n| [&n[..]]).collect();
    q.push(parts.iter().map(|p| (&p[..], 0)));
    let mut b = Batch::new();
    assert_eq!(q.wait(&mut b, Some(Duration::from_secs(1))), Wait::Ready);
    assert!(b.rescan());
    assert_eq!(b.len(), 0);
    assert_eq!(b.events(), 10);
    assert!(!b.is_empty());
}

fn load(root: &Path) -> Program {
    let w = walk(&[root], &WalkOptions::everything());
    Program::load(Inputs::from_root_dir(root, &w.files, Origin::FirstParty, b"proj"), Attribution::new(), true)
}

fn rewalk(root: &Path) -> Inputs {
    let w = walk(&[root], &WalkOptions::everything());
    Inputs::from_root_dir(root, &w.files, Origin::FirstParty, b"proj")
}

fn bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

#[test]
fn classify_sorts_paths_into_changed_rewalk_reload() {
    let d = TempDir::new("classify");
    let a = d.write("pkg/a.py", "def f():\n    pass\n");
    let b = d.write("pkg/b.py", "import pkg.a\n");
    let p = load(d.path());
    let root = d.path();
    let ix = p.index();

    // A content edit of a loaded file.
    let bt = batch_of(&[(&bytes(&a), 0)]);
    let c = classify(&bt, ix, root);
    assert_eq!(c.changed, [a.as_path()]);
    assert!(!c.rewalk && !c.reload && !c.all);

    // Unrelated files, caches, hidden directories, files that came and went.
    let tmp = d.write("pkg/a.py.swp", "");
    let pyc = d.write("pkg/__pycache__/a.cpython-313.pyc", "");
    let hidden = d.write(".venv/lib/x.py", "");
    let gone = root.join("pkg/tmp_123.py");
    let git = d.write(".git/index", "");
    let bt = batch_of(&[(&bytes(&tmp), 0), (&bytes(&pyc), 0), (&bytes(&hidden), 0), (&bytes(&gone), 0), (&bytes(&git), 0), (&bytes(&root.join("pkg/__pycache__")), FLAG_DIR)]);
    let c = classify(&bt, ix, root);
    assert!(c.is_empty(), "{c:?}");
    assert_eq!(c.ignored, 6);

    // A new Python file, a deleted loaded file, a new directory: walk again.
    let new = d.write("pkg/c.py", "x = 1\n");
    assert!(classify(&batch_of(&[(&bytes(&new), 0)]), ix, root).rewalk);
    fs::remove_file(&b).expect("rm");
    let bt = batch_of(&[(&bytes(&b), 0)]);
    let c = classify(&bt, ix, root);
    assert!(c.rewalk && c.changed.is_empty());
    fs::create_dir_all(root.join("sub")).expect("mkdir");
    assert!(classify(&batch_of(&[(&bytes(&root.join("sub")), FLAG_DIR)]), ix, root).rewalk);
    // A directory event without the flag (inotify on the directory itself) is still a directory.
    assert!(classify(&batch_of(&[(&bytes(&root.join("sub")), 0)]), ix, root).rewalk);
    assert!(classify(&batch_of(&[(&bytes(&root.join(".gitignore")), 0)]), ix, root).rewalk);
    assert!(classify(&batch_of(&[(&bytes(&root.join(".git/info/exclude")), 0)]), ix, root).rewalk);

    // Configuration.
    let bt = batch_of(&[(&bytes(&root.join("pyproject.toml")), 0), (&bytes(&a), 0)]);
    let c = classify(&bt, ix, root);
    assert!(c.reload);
    assert_eq!(c.changed, [a.as_path()]);
    assert!(classify(&batch_of(&[(&bytes(&root.join("sub/pymapper.toml")), 0)]), ix, root).reload);

    // Lost events.
    let bt = rescan_batch();
    let c = classify(&bt, ix, root);
    assert!(c.rewalk && c.all);
}

#[test]
fn classify_inspects_only_components_below_the_root() {
    let d = TempDir::new("hiddenroot");
    let root = d.path().join(".config/proj");
    fs::create_dir_all(&root).expect("mkdir");
    fs::write(root.join("m.py"), "x = 1\n").expect("write");
    let p = load(&root);
    let new = root.join("n.py");
    fs::write(&new, "y = 2\n").expect("write");
    let bt = batch_of(&[(&bytes(&root.join("m.py")), 0), (&bytes(&new), 0)]);
    let c = classify(&bt, p.index(), &root);
    assert_eq!(c.changed, [root.join("m.py").as_path()]);
    assert!(c.rewalk, "a new file under a root inside a hidden directory counts");
}

#[test]
fn inputs_of_reproduces_the_loaded_inputs() {
    let d = TempDir::new("inputs");
    d.write("pkg/__init__.py", "");
    d.write("pkg/a.py", "def f():\n    pass\n");
    d.write("pkg/a.pyi", "def f() -> None: ...\n");
    d.write("tool-x.py", "print(1)\n");
    let original = rewalk(d.path());
    let ix = index::load(rewalk(d.path()));
    let again = inputs_of(&ix);
    assert_eq!(again.len(), original.len());
    assert_eq!(again.groups(), original.groups());
    for f in 0..original.len() as u32 {
        assert_eq!(again.path(f), original.path(f));
        assert_eq!(again.module(f), original.module(f));
        assert_eq!(again.status(f), original.status(f));
        assert_eq!(again.group(f), original.group(f));
    }
    // Updating with them and nothing changed is a no-op.
    let mut p = Program::from_parts(ix, Attribution::new(), true);
    let (d0, st) = p.update(again, &[] as &[PathBuf]);
    assert_eq!(st.reextracted, 0);
    assert!(d0.is_empty(), "{d0:?}");
}

#[test]
fn update_applies_edits_additions_and_rescans() {
    let d = TempDir::new("update");
    let a = d.write("pkg/a.py", "def f():\n    pass\n");
    d.write("pkg/b.py", "from pkg.a import f\n\ndef g():\n    f()\n");
    let root = d.path().to_path_buf();
    let mut p = load(&root);
    let nodes0 = p.index().node_count();

    // Content edit: one more function, no rewalk.
    fs::write(&a, "def f():\n    pass\n\ndef h():\n    pass\n").expect("write");
    let bt = batch_of(&[(&bytes(&a), 0)]);
    let c = classify(&bt, p.index(), &root);
    let (diff, st) = update(&mut p, &c, || panic!("no rewalk for a content edit"));
    assert_eq!(st.reextracted, 1);
    assert_eq!(p.index().node_count(), nodes0 + 1);
    assert!(!diff.added.is_empty());

    // A new file: rewalk, then the update loads it.
    let new = d.write("pkg/c.py", "def k():\n    pass\n");
    let bt = batch_of(&[(&bytes(&new), 0)]);
    let c = classify(&bt, p.index(), &root);
    assert!(c.rewalk);
    let (_, st) = update(&mut p, &c, || rewalk(&root));
    assert_eq!(st.added, 1);
    assert_eq!(p.index().file_count(), 3);

    // Lost events: every first-party file is re-extracted.
    let bt = rescan_batch();
    let c = classify(&bt, p.index(), &root);
    let (_, st) = update(&mut p, &c, || rewalk(&root));
    assert_eq!(st.reextracted, 3);
}

/// Waits for a batch containing `want` (batches without it are skipped: editors and the
/// OS may report unrelated paths, and FSEvents may split a sequence over batches).
fn wait_for(w: &Watcher, want: &Path) -> Batch {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut b = Batch::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "no event for {} within 10 s", want.display());
        if w.wait(&mut b, Some(left)) == Wait::Ready && (b.paths().any(|p| p == want) || b.rescan()) {
            return b;
        }
    }
}

#[test]
fn watcher_reports_file_and_directory_changes() {
    let d = TempDir::new("live");
    let a = d.write("pkg/a.py", "x = 1\n");
    let w = Watcher::new(&[d.path()], opts(20, 200)).expect("start watcher");
    // FSEvents may report events from before the stream started; let them pass.
    std::thread::sleep(Duration::from_millis(50));
    w.flush();
    let mut b = Batch::new();
    let _ = w.wait(&mut b, Some(Duration::from_millis(100)));

    fs::write(&a, "x = 2\n").expect("write");
    let b = wait_for(&w, &a);
    assert!(!b.rescan());

    let new = d.write("pkg/sub/new.py", "y = 1\n");
    let b = wait_for(&w, &new);
    assert!(b.paths().all(|p| p.starts_with(d.path())), "paths are spelled as the root was given: {:?}", b.paths().collect::<Vec<_>>());

    // A file in a directory created after the start (inotify adds the watch on the fly).
    std::thread::sleep(Duration::from_millis(50));
    let later = d.write("pkg/sub/later.py", "z = 1\n");
    wait_for(&w, &later);

    let moved = d.path().join("pkg/moved.py");
    fs::rename(&new, &moved).expect("rename");
    wait_for(&w, &moved);

    fs::remove_file(&a).expect("rm");
    wait_for(&w, &a);
}

#[test]
fn watcher_stops_on_waker_and_rejects_missing_roots() {
    let d = TempDir::new("stop");
    let w = Watcher::new(&[d.path()], WatchOptions::default()).expect("start watcher");
    let waker = w.waker();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        waker.stop();
    });
    let mut b = Batch::new();
    assert_eq!(w.wait(&mut b, None), Wait::Stopped);
    t.join().expect("waker");
    assert!(w.stopped());
    drop(w);
    assert!(matches!(Watcher::new(&[d.path().join("missing")], WatchOptions::default()), Err(WatchError::Root(..))));
}
