//! Watch-mode benchmark of the code map: edit-to-map latency on a copy of a real project.
//!
//! ```text
//! cargo run --release -p pymapper-gui --example watch_bench -- PATH [--deps] [--stdlib] [--edits N]
//! ```
//!
//! The project's Python files and `pyproject.toml` / `pymapper.toml` (default walk: ignore
//! files honoured, hidden directories skipped) are copied to a temporary directory; with
//! `--deps` its `.venv` is linked into the copy so the venv is loaded too (never written).
//! The map worker loads the copy in watch mode (no cache: the copy is new) and builds the
//! first scene, then files across the size distribution are edited (a function appended) and
//! restored, one write at a time, each timed from the write to the published scene. The
//! scene upload to the GPU (one band rebuild on the next frame) is not included; see
//! `codemap_bench` for those numbers.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use pymapper_core::walk::{WalkOptions, walk};
use pymapper_gui::codemap::MapParams;
use pymapper_gui::codemap::load::LoadParams;
use pymapper_gui::codemap::worker::{MapWorker, Published, Request};
use pymapper_gui::stats::process_usage;

struct Copy(PathBuf);

impl Drop for Copy {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn next_update(w: &MapWorker) -> Result<Published> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("no update within 60 s");
        }
        match w.wait_timeout(left) {
            Some(Ok(p)) if p.update.is_some() => return Ok(p),
            Some(Ok(_)) | None => {}
            Some(Err(e)) => bail!("worker failed: {e}"),
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// `(median, p90, worst)` in milliseconds.
fn stats(v: &mut [Duration]) -> (f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    v.sort_unstable();
    let at = |p: usize| ms(v[((v.len() - 1) * p / 100).min(v.len() - 1)]);
    (at(50), at(90), at(100))
}

fn main() -> Result<()> {
    let mut path: Option<PathBuf> = None;
    let (mut deps, mut stdlib, mut edits) = (false, false, 12usize);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--deps" => deps = true,
            "--stdlib" => stdlib = true,
            "--edits" => edits = args.next().context("--edits needs a value")?.parse()?,
            _ if a.starts_with('-') => bail!("unknown option {a}"),
            _ => path = Some(PathBuf::from(a)),
        }
    }
    let src = fs::canonicalize(path.context("usage: watch_bench PATH [--deps] [--stdlib] [--edits N]")?)?;
    let dir = std::env::temp_dir().join(format!("pymapper-watch-bench-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let copy = Copy(fs::canonicalize(std::env::temp_dir())?.join(dir.file_name().context("name")?));
    let dir = copy.0.clone();
    let w = walk(&[&src], &WalkOptions { projects: true, ..WalkOptions::default() });
    let mut py: Vec<PathBuf> = Vec::new();
    for p in w.files.iter().chain(w.projects.iter()) {
        let dst = dir.join(p.strip_prefix(&src)?);
        fs::create_dir_all(dst.parent().context("parent")?)?;
        fs::copy(p, &dst)?;
        if dst.extension().is_some_and(|e| e == "py") {
            py.push(dst);
        }
    }
    for name in ["pyproject.toml", "pymapper.toml"] {
        if src.join(name).is_file() {
            fs::copy(src.join(name), dir.join(name))?;
        }
    }
    if deps && src.join(".venv").is_dir() {
        std::os::unix::fs::symlink(src.join(".venv"), dir.join(".venv"))?;
    }
    println!("copied {} files to {}", w.files.len(), dir.display());

    let load = LoadParams { path: dir.clone(), isolated: false, deps: Some(deps), stdlib: Some(stdlib), cache_dir: None, no_cache: true, watch: true };
    let t = Instant::now();
    let worker = MapWorker::spawn(load, MapParams::default(), Request::default(), Arc::new(|| {}))?;
    let first = worker.wait().map_err(anyhow::Error::msg)?;
    let nodes = first.map.analysis.index().node_count();
    let files = first.map.analysis.index().file_count();
    let phases: Vec<String> = first.map.analysis.phases.iter().map(|(n, d)| format!("{n} {:.0} ms", ms(*d))).collect();
    println!("load + first scene: {:.0} ms ({}), {files} files, {nodes} nodes, {} rects", ms(t.elapsed()), phases.join(", "), first.scene.rects.len());
    drop(first);

    // Files to edit: quantiles of the size distribution.
    let mut by_size: Vec<(u64, &PathBuf)> = py.iter().map(|p| (fs::metadata(p).map_or(0, |m| m.len()), p)).collect();
    by_size.sort();
    let picks: Vec<&Path> = (0..edits.div_ceil(2)).map(|i| by_size[(by_size.len() - 1) * (i + 1) / edits.div_ceil(2)].1.as_path()).collect();

    let (mut total, mut latency, mut update, mut rebuild, mut release, mut layout, mut edges, mut scene) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut overlays = 0usize;
    for (k, p) in picks.iter().enumerate() {
        let original = fs::read(p)?;
        let mut edited = original.clone();
        edited.extend_from_slice(format!("\n\ndef _pymapper_watch_bench_{k}():\n    return {k}\n").as_bytes());
        for text in [&edited, &original] {
            std::thread::sleep(Duration::from_millis(200));
            let t0 = Instant::now();
            fs::write(p, text)?;
            let up = next_update(&worker)?;
            total.push(t0.elapsed());
            let u = up.update.as_ref().context("update")?;
            latency.push(u.latency);
            update.push(u.update);
            rebuild.push(u.rebuild);
            release.push(u.release);
            scene.push(up.scene_build);
            overlays = overlays.max(u.transition.items.len());
            for (name, d) in &up.map.phases {
                match *name {
                    "layout" => layout.push(*d),
                    "edges" => edges.push(*d),
                    _ => {}
                }
            }
        }
    }
    println!("{} edits; ms median / p90 / worst:", total.len());
    for (name, v) in [
        ("write -> scene published", &mut total),
        ("first event -> published", &mut latency),
        ("  incremental update", &mut update),
        ("  rebuild (rollup, map, scene)", &mut rebuild),
        ("    layout", &mut layout),
        ("    edges", &mut edges),
        ("    scene", &mut scene),
        ("  release handshake", &mut release),
    ] {
        let (a, b, c) = stats(v);
        println!("  {name:<32} {a:>8.1} {b:>8.1} {c:>8.1}");
    }
    println!("most overlay rects in a transition: {overlays}");
    println!("peak RSS: {:.0} MB", process_usage().peak_rss_bytes as f64 / 1e6);
    drop(worker);
    drop(copy);
    Ok(())
}
