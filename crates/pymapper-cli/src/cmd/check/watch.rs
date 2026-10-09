//! `pymapper check --watch`: check once, then again after every change, updating the loaded
//! program incrementally instead of loading it again.
//!
//! ```text
//!   config ──> Setup ──> Watcher (project root) ──> load (cache) ──> Program (tracked resolution)
//!     ▲                                                                 │
//!     │ pyproject.toml / pymapper.toml changed                          ▼
//!     └──────── reload ◀── classify ◀── debounced batch ◀── wait     check + report
//!                              │                                        ▲
//!                              └─> rewalk? ──> Program::update ─────────┘
//! ```
//!
//! Each cycle prints a full report, like a fresh `check` (on a terminal the screen is cleared
//! first), followed by one status line on stderr with the edit-to-report latency. The exit
//! code is that of the last check when the output pipe closes; Ctrl-C ends the process.
//!
//! Limitations: duplication (`PM501`, `analysis.dupes`) is not run in watch mode (the update
//! does not maintain fingerprints); third-party and stdlib files are not watched; a changed
//! worker thread count (`threads`) needs a restart.

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::Context;
use pymapper_core::analysis::dupes::DupesParams;
use pymapper_core::incremental::{Program, UpdateStats};
use pymapper_core::intern::FxHashSet;
use pymapper_core::resolve::Attribution;
use pymapper_core::timing;
use pymapper_core::watch::{self, Batch, Wait, WatchOptions, Watcher};

use super::{Args, Done, Extra, Input, Setup, print_stats};
use crate::cmd::config::load_overrides;
use crate::pipeline::{self, warn};

/// Runs `check --watch` (see the module docs). Errors of the first load are fatal; later
/// configuration errors are printed and the next change is awaited.
pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    if args.stats {
        timing::enable(true);
    }
    let cwd = std::env::current_dir().context("cannot determine the working directory")?;
    let explicit = !args.paths.is_empty();
    let overrides = args.config.into_overrides(args.paths);
    let tty = is_tty();
    let mut threads = false;
    let mut first = true;
    let mut batch = Batch::new();
    loop {
        let t0 = Instant::now();
        let mut setup = match load_setup(&cwd, &overrides, explicit, args.baseline.as_deref()) {
            Ok(s) => s,
            Err(e) if !first => {
                let _ = writeln!(io::stderr().lock(), "pymapper: error: {e:#}");
                // Wait for the next change anywhere under the working directory.
                wait_for_change(&cwd, &mut batch)?;
                continue;
            }
            Err(e) => return Err(e),
        };
        if !threads {
            pipeline::configure_threads(setup.config().threads)?;
            threads = true;
        }
        if DupesParams::from_engine(&setup.engine).is_some() {
            warn(format_args!("duplication (PM501, `analysis.dupes`) is not checked in watch mode"));
        }
        let root = setup.root().to_path_buf();
        // Watch before loading: edits made during the load are picked up by the first update.
        let watcher = Watcher::new(&[&root], WatchOptions::default())?;
        let mut session = Session::load(&setup, &root);
        if tty {
            clear_screen();
        }
        let done = session.check(&setup, t0, args.stats)?;
        status_line(format_args!("watching {} for changes (Ctrl-C to stop); checked in {} ms", root.display(), t0.elapsed().as_millis()));
        if done.closed {
            return Ok(ExitCode::from(done.status.code()));
        }
        first = false;
        loop {
            match watcher.wait(&mut batch, None) {
                Wait::Ready => {}
                Wait::Timeout => continue,
                Wait::Stopped => anyhow::bail!("file watching stopped unexpectedly"),
            }
            let t0 = batch.first_event().unwrap_or_else(Instant::now);
            let changes = watch::classify(&batch, session.program.index(), &root);
            if changes.reload {
                break;
            }
            let extra = session.extra_inputs(&mut setup, &batch);
            if changes.is_empty() && !extra {
                continue;
            }
            let (st, n) = session.update(&setup, &root, &changes);
            if st.reextracted == 0 && st.added == 0 && st.removed == 0 && !extra {
                continue;
            }
            if tty {
                clear_screen();
            }
            let done = session.check(&setup, t0, args.stats)?;
            if args.stats {
                print_update(&st);
            }
            status_line(format_args!(
                "{n} file(s) changed; updated in {} ms, reported {} ms after the first event. Watching for changes",
                st.total.as_millis(),
                t0.elapsed().as_millis()
            ));
            if done.closed {
                return Ok(ExitCode::from(done.status.code()));
            }
        }
        // Configuration changed: drop everything (the watcher first) and load again.
        drop(watcher);
    }
}

fn load_setup(cwd: &Path, overrides: &pymapper_core::config::Overrides, explicit: bool, baseline: Option<&Path>) -> anyhow::Result<Setup> {
    let loaded = timing::time("config", || load_overrides(cwd, overrides))?;
    Setup::new(loaded, cwd, explicit, baseline, false)
}

/// Blocks until something changes under `dir` (after a failed reload).
fn wait_for_change(dir: &Path, batch: &mut Batch) -> anyhow::Result<()> {
    let w = Watcher::new(&[dir], WatchOptions::default())?;
    status_line(format_args!("fix the configuration; watching {} for changes", dir.display()));
    loop {
        match w.wait(batch, None) {
            Wait::Ready if !batch.is_empty() => return Ok(()),
            Wait::Stopped => anyhow::bail!("file watching stopped unexpectedly"),
            _ => {}
        }
    }
}

/// The loaded program and what the checks need besides it.
struct Session {
    program: Program,
    meta: Meta,
}

/// The parts of [`pipeline::Unresolved`] that outlive the index.
struct Meta {
    units: pymapper_core::roots::ProjectUnits,
    roots: pymapper_core::roots::Roots,
    cache: Option<pymapper_core::cache::CacheStats>,
    walk_errors: usize,
    config_evidence: Option<pymapper_core::extract::config::ConfigEvidence>,
    configs: pymapper_core::walk::PathList,
}

impl Session {
    fn load(setup: &Setup, root: &Path) -> Session {
        let u = pipeline::load_unresolved(setup.config(), root, None);
        let attr = Attribution::from_roots(&u.roots);
        let program = timing::time("incremental.load", || Program::from_parts(u.index, attr, setup.needs_calls()));
        let meta = Meta { units: u.units, roots: u.roots, cache: u.cache, walk_errors: u.walk_errors, config_evidence: u.config_evidence, configs: u.configs };
        Session { program, meta }
    }

    fn check(&self, setup: &Setup, t0: Instant, stats: bool) -> anyhow::Result<Done> {
        let m = &self.meta;
        let input = Input {
            ix: self.program.index(),
            imp: self.program.imports(),
            calls: self.program.calls(),
            units: &m.units,
            config_evidence: m.config_evidence.as_ref(),
        };
        let done = setup.check(&input, None, None, stats)?;
        if stats {
            let extra = Extra { cache: m.cache.as_ref(), walk_errors: m.walk_errors, config_evidence: m.config_evidence.as_ref() };
            print_stats(t0, setup, &input, &done, &extra);
        }
        Ok(done)
    }

    /// Whether `batch` touches a non-Python input of the checks: the coverage report (read by
    /// every check), the baseline or a config-evidence file (both read again here). A baseline
    /// that became unreadable or invalid is reported and the previous one kept.
    fn extra_inputs(&mut self, setup: &mut Setup, batch: &Batch) -> bool {
        let hits = |p: &Path| batch.paths().any(|q| q == p);
        let mut any = setup.coverage_path.as_deref().is_some_and(hits);
        if let Some(path) = setup.config().baseline.as_deref().filter(|p| hits(p)) {
            match super::read_baseline(path) {
                Ok(b) => setup.baseline = Some(b),
                Err(e) => warn(format_args!("{e:#}; keeping the previous baseline")),
            }
            any = true;
        }
        let config = setup.config();
        if self.meta.config_evidence.is_some() {
            let configs = &self.meta.configs;
            let set: FxHashSet<&[u8]> = (0..configs.len()).map(|i| configs.get_bytes(i)).collect();
            if batch.paths().any(|p| set.contains(p.as_os_str().as_bytes())) {
                self.meta.config_evidence = pipeline::load_config_evidence(config, configs);
                any = true;
            }
        }
        any
    }

    /// Applies `changes`; returns the update counters and the number of changed paths.
    fn update(&mut self, setup: &Setup, root: &Path, changes: &watch::Changes<'_>) -> (UpdateStats, usize) {
        let config = setup.config();
        let m = &mut self.meta;
        let mut walked = None;
        let (_, st) = watch::update(&mut self.program, changes, || {
            let (inputs, configs, errors) = pipeline::rewalk(&m.roots, config, root);
            walked = Some((configs, errors));
            inputs
        });
        if let Some((configs, errors)) = walked {
            m.walk_errors = errors;
            if m.config_evidence.is_some() && configs != m.configs {
                m.config_evidence = pipeline::load_config_evidence(config, &configs);
            }
            m.configs = configs;
        }
        let n = if changes.all { st.reextracted as usize } else { changes.changed.len() + (st.added + st.removed) as usize };
        (st, n)
    }
}

/// `--stats` line of one incremental update.
fn print_update(st: &UpdateStats) {
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    let _ = writeln!(
        io::stderr().lock(),
        "  update: {} re-extracted, {} added, {} removed, imports re-resolved for {} file(s), calls for {}{}; plan {:.1} ms, extract {:.1} ms, patch {:.1} ms, resolve {:.1} + {:.1} ms, diff {:.1} ms",
        st.reextracted,
        st.added,
        st.removed,
        st.reresolved_imports,
        st.reresolved_calls,
        if st.full_resolve { " (full resolve)" } else { "" },
        ms(st.plan),
        ms(st.extract),
        ms(st.patch),
        ms(st.resolve_imports),
        ms(st.resolve_calls),
        ms(st.diff),
    );
}

fn status_line(msg: std::fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr().lock(), "pymapper: {msg}");
}

fn is_tty() -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

/// Clears the terminal and its scrollback, cursor home (as `ruff check --watch`).
fn clear_screen() {
    let mut out = io::stdout().lock();
    let _ = out.write_all(b"\x1b[2J\x1b[3J\x1b[H");
    let _ = out.flush();
}
