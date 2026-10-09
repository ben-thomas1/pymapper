//! Watch mode: incremental analysis updates against fresh loads, transitions, and the map
//! worker reacting to real file changes.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use pymapper_core::index::{Index, NONE};
use pymapper_core::layout::Order;
use pymapper_core::watch::Changes;

use super::load::{Analysis, LoadParams, load};
use super::transition::{FLASH, Kind, Transition};
use super::worker::{MapWorker, Published, Request};
use super::{CodeMap, MapParams};
use crate::views::Views;

struct TempProject(PathBuf);

impl TempProject {
    fn new(files: &[(&str, &str)]) -> TempProject {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-gui-watch-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        fs::write(dir.join("pyproject.toml"), "[project]\nname = \"proj\"\nversion = \"0\"\n").expect("pyproject");
        let p = TempProject(fs::canonicalize(&dir).expect("canonical"));
        for (rel, src) in files {
            p.write(rel, src);
        }
        p
    }

    fn write(&self, rel: &str, src: &str) -> PathBuf {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, src).expect("write");
        p
    }

    fn params(&self, watch: bool) -> LoadParams {
        LoadParams { path: self.0.clone(), isolated: true, deps: Some(false), stdlib: Some(false), cache_dir: None, no_cache: true, watch }
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const PROJ: &[(&str, &str)] = &[
    ("app/__init__.py", ""),
    ("app/main.py", "from app import util\n\ndef run():\n    util.log('x')\n    return helper()\n\ndef helper():\n    return 1\n\nrun()\n"),
    ("app/util.py", "def log(msg):\n    for c in msg:\n        if c:\n            print(c)\n    return msg\n\ndef gone():\n    return 2\n"),
    ("app/big.py", "def a():\n    return 1\n\ndef b():\n    return 2\n\ndef c():\n    return 3\n"),
];

const UTIL_EDITED: &str = "def log(msg):\n    for c in msg:\n        if c:\n            print(c)\n    return msg\n\ndef fresh():\n    x = 1\n    y = 2\n    z = 3\n    return x + y + z\n";

fn qids(ix: &Index) -> Vec<String> {
    let mut v: Vec<String> = (0..ix.node_count()).map(|n| ix.qid_string(n)).collect();
    v.sort();
    v
}

fn find(ix: &Index, qid: &str) -> Option<u32> {
    (0..ix.node_count()).find(|&n| ix.qid_string(n) == qid)
}

#[test]
fn analysis_update_matches_a_fresh_load() {
    let p = TempProject::new(PROJ);
    let mut a = load(&p.params(true), &|_| {}).expect("load");
    assert!(a.watchable());
    let util = p.write("app/util.py", UTIL_EDITED);
    let changes = Changes { changed: vec![util.as_path()], ..Changes::default() };
    let mut warnings = Vec::new();
    let (diff, st, _) = a.update(&changes, &mut warnings);
    assert_eq!(st.reextracted, 1);
    assert!(!diff.added.is_empty() && !diff.removed.is_empty());

    // A new file: the rewalk finds it.
    p.write("app/extra.py", "def e():\n    return 0\n");
    let changes = Changes { rewalk: true, ..Changes::default() };
    let (_, st, _) = a.update(&changes, &mut warnings);
    assert_eq!(st.added, 1);

    let fresh: Analysis = load(&p.params(false), &|_| {}).expect("fresh load");
    assert_eq!(qids(a.index()), qids(fresh.index()));
    assert_eq!(a.agg.len(), fresh.agg.len());
    let root_loc = |x: &Analysis| (0..x.index().node_count()).filter(|&v| x.index().parent(v).is_none()).map(|v| x.agg.get(v).loc).sum::<u32>();
    assert_eq!(root_loc(&a), root_loc(&fresh));
    let pairs = |x: &Analysis| {
        let ix = x.index();
        let mut v: Vec<(String, String, u32)> = x.rollup.call_pairs().map(|(s, d, n)| (ix.qid_string(s), ix.qid_string(d), n)).collect();
        v.sort();
        v
    };
    assert_eq!(pairs(&a), pairs(&fresh));
}

fn preorder() -> MapParams {
    let mut m = MapParams::default();
    m.layout.order = Order::Preorder;
    m
}

#[test]
fn transition_marks_changed_added_removed_and_moved_cells() {
    let p = TempProject::new(PROJ);
    let a = Arc::new(load(&p.params(true), &|_| {}).expect("load"));
    let old = CodeMap::build(a, preorder());
    let gone = find(old.analysis.index(), "app.util:gone").expect("gone");
    let CodeMap { analysis, layout: old_layout, .. } = old;
    let mut a = Arc::try_unwrap(analysis).expect("unique");
    let util = p.write("app/util.py", UTIL_EDITED);
    let changes = Changes { changed: vec![util.as_path()], ..Changes::default() };
    let (diff, _, _) = a.update(&changes, &mut Vec::new());
    let new = CodeMap::build(Arc::new(a), preorder());
    let ix = new.analysis.index();
    let fresh = find(ix, "app.util:fresh").expect("fresh");
    let module = find(ix, "app.util").expect("module");

    let mut t = Transition::build(&old_layout, &new.layout, &diff);
    let has = |kind: Kind, r: Option<pymapper_core::layout::Rect>| t.items.iter().any(|i| i.kind == kind && Some(i.to) == r);
    assert!(has(Kind::Added, new.layout.rect(fresh)), "{:?}", t.items);
    assert!(has(Kind::Changed, new.layout.rect(module)), "{:?}", t.items);
    let old_gone = old_layout.rect(gone).expect("old rect");
    assert!(t.items.iter().any(|i| i.kind == Kind::Removed && i.from == old_gone));
    // `fresh` is bigger than `gone`: the cells around it moved.
    assert!(t.items.iter().any(|i| i.kind == Kind::Moved));
    assert!(diff.node_map[gone as usize] == NONE);

    // Animation: nothing before the start, overlays while running, done after the flash.
    let mut out = Vec::new();
    assert!(t.frame(Instant::now(), &mut out) && out.is_empty());
    let t0 = Instant::now();
    t.start(t0);
    assert!(t.frame(t0, &mut out));
    assert!(!out.is_empty() && out.len() <= t.items.len());
    assert!(t.frame(t0 + FLASH / 2, &mut out));
    assert!(!t.frame(t0 + FLASH, &mut out));
    assert!(out.is_empty());
}

/// Next published update (skipping scenes published for other reasons).
fn next_update(w: &MapWorker) -> Published {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "no update within 20 s");
        match w.wait_timeout(left) {
            Some(Ok(p)) if p.update.is_some() => return p,
            Some(Ok(_)) | None => {}
            Some(Err(e)) => panic!("worker failed: {e}"),
        }
    }
}

#[test]
fn worker_applies_file_changes_and_reloads_on_config_changes() {
    let p = TempProject::new(PROJ);
    let w = MapWorker::spawn(p.params(true), MapParams::default(), Request::default(), Arc::new(|| {})).expect("spawn");
    let first = w.wait().expect("first scene");
    assert_eq!(first.map.layout.config().order, Order::Preorder, "watch mode lays out in pre-order");
    let old_nodes = first.map.analysis.index().node_count();
    let helper = find(first.map.analysis.index(), "app.main:helper").expect("helper");
    // The UI's reference must go for the update to proceed (the release handshake).
    drop(first);
    std::thread::sleep(Duration::from_millis(100));

    p.write("app/util.py", UTIL_EDITED);
    let up = next_update(&w);
    let u = up.update.as_ref().expect("update");
    assert_eq!(u.node_map.len(), old_nodes as usize);
    let ix = up.map.analysis.index();
    assert!(find(ix, "app.util:fresh").is_some());
    assert!(find(ix, "app.util:gone").is_none());
    let h = u.node_map[helper as usize];
    assert_ne!(h, NONE);
    assert_eq!(ix.qid_string(h), "app.main:helper");
    assert!(!u.transition.is_empty());
    assert_eq!(u.files.0, 1);
    assert!(u.latency >= u.update);
    drop(up);

    // A configuration change reloads: nothing carries over.
    std::thread::sleep(Duration::from_millis(100));
    p.write("pyproject.toml", "[project]\nname = \"proj\"\nversion = \"1\"\n");
    let up = next_update(&w);
    let u = up.update.as_ref().expect("update");
    assert!(u.node_map.is_empty());
    assert!(u.transition.is_empty());
    assert!(find(up.map.analysis.index(), "app.util:fresh").is_some());
}

#[test]
fn views_carry_the_selection_over_an_update() {
    let mut v = Views::new(Arc::new(|| {}));
    v.selection = Some(2);
    v.remap(&[0, 1, 5, NONE]);
    assert_eq!(v.selection, Some(5));
    v.selection = Some(3);
    v.remap(&[0, 1, 5, NONE]);
    assert_eq!(v.selection, None, "a removed node clears the selection");
    v.selection = Some(1);
    v.remap(&[]);
    assert_eq!(v.selection, None, "a reload clears the selection");
    v.release();
}
