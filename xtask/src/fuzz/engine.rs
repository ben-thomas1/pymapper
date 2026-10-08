//! The fuzzing loop: worker threads run the target under `catch_unwind`, a watchdog (the calling
//! thread) detects inputs that exceed the time budget.
//!
//! Inputs are numbered; input `k` is `Mutator::generate(k)`. Workers claim iterations in small
//! batches from a shared counter, so a run with `--iterations N` executes exactly the inputs
//! `start..start + N` whatever the thread count. A worker publishes the iteration it is running
//! in its slot word; the watchdog declares an input hung by swapping that word to `HUNG` (a CAS,
//! so exactly one of "worker finished" and "watchdog fired" wins), regenerates the input from
//! its number, abandons the stuck thread and starts a replacement. Nothing is copied per input.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use super::inflight::{HUNG, IDLE, Inflight, MAX_ITER};
use super::mutate::{Mutator, Scratch, Seeds};

/// Iterations claimed per counter access.
const BATCH: u64 = 16;

/// FNV-1a 64: stable across runs and Rust versions (artifact names depend on it).
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------------------------
// Panic capture

/// Where and why the last captured panic on this thread happened.
#[derive(Default)]
struct PanicRecord {
    sig: u64,
    location: String,
    message: String,
}

thread_local! {
    static CAPTURE: Cell<bool> = const { Cell::new(false) };
    static LAST: RefCell<PanicRecord> = RefCell::new(PanicRecord::default());
}

/// Installs (once per process) a panic hook that silently records panics on capturing threads
/// and defers to the previous hook everywhere else.
fn install_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !CAPTURE.get() {
                prev(info);
                return;
            }
            LAST.with_borrow_mut(|r| {
                r.location.clear();
                if let Some(l) = info.location() {
                    let _ = write!(r.location, "{}:{}:{}", l.file(), l.line(), l.column());
                }
                r.message.clear();
                r.message.push_str(info.payload_as_str().unwrap_or("<non-string panic payload>"));
                // Deduplicate by location; fall back to the message when there is none.
                r.sig = if r.location.is_empty() { fnv1a(r.message.as_bytes()) } else { fnv1a(r.location.as_bytes()) };
            });
        }));
    });
}

/// Marks the current thread as capturing: its panics are recorded, not printed.
pub fn capture_panics_on_this_thread() {
    install_hook();
    CAPTURE.set(true);
}

/// Runs `target(input)`; returns the crash signature if it panicked. The calling thread must
/// have called [`capture_panics_on_this_thread`].
#[inline]
pub fn run_one(target: fn(&[u8]), input: &[u8]) -> Option<u64> {
    match panic::catch_unwind(AssertUnwindSafe(|| target(input))) {
        Ok(()) => None,
        Err(_payload) => Some(LAST.with_borrow(|r| r.sig)),
    }
}

/// Location and message of the last captured panic on this thread.
pub fn last_panic() -> (String, String) {
    LAST.with_borrow(|r| (r.location.clone(), r.message.clone()))
}

// ---------------------------------------------------------------------------------------------
// Engine

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Crash,
    Hang,
    /// Killed the whole process (stack overflow, abort, OOM); found by the supervisor.
    Abort,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Crash => "crash",
            Kind::Hang => "hang",
            Kind::Abort => "abort",
        }
    }
}

/// A new, distinct problem (first input seen for its signature).
#[derive(Debug)]
pub struct Finding {
    pub kind: Kind,
    /// Crashes: hash of the panic location. Hangs: hash of the input.
    pub sig: u64,
    pub iter: u64,
    pub input: Vec<u8>,
    pub location: String,
    pub message: String,
}

pub struct Config {
    pub seed: u64,
    pub start_iter: u64,
    /// Total inputs to run (None: until `duration` or forever).
    pub iterations: Option<u64>,
    pub duration: Option<Duration>,
    pub jobs: usize,
    pub budget: Duration,
    pub max_len: usize,
    /// Stop once this many threads have been abandoned on hung inputs.
    pub max_hangs: usize,
    pub stack_size: usize,
    /// Print a status line to stderr every this often (None: quiet).
    pub progress: Option<Duration>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub execs: u64,
    pub elapsed: Duration,
    /// Every crashing input (not only distinct ones).
    pub crashes: u64,
    pub hangs: u64,
    pub findings: Vec<Finding>,
    /// The run stopped because `max_hangs` threads were abandoned.
    pub hang_limit_hit: bool,
    /// First iteration not claimed by any worker (resume point).
    pub next_iter: u64,
}

struct Shared {
    target: fn(&[u8]),
    seeds: Arc<Seeds>,
    seed: u64,
    max_len: usize,
    end_iter: u64,
    stop: AtomicBool,
    inflight: Arc<Inflight>,
    base: Instant,
    /// Per slot: start time (ns since `base`) of the running input.
    started: Box<[AtomicU64]>,
    /// Per slot: inputs completed.
    execs: Box<[AtomicU64]>,
    /// Per slot: the worker returned normally.
    done: Box<[AtomicBool]>,
    crashes: AtomicU64,
    /// Crash signatures already reported (by any worker).
    seen: Mutex<HashSet<u64>>,
    /// New findings waiting for the watchdog to hand them to the caller.
    pending: Mutex<Vec<Finding>>,
}

impl Shared {
    #[inline]
    fn now_ns(&self) -> u64 {
        self.base.elapsed().as_nanos() as u64
    }
}

fn worker(shared: Arc<Shared>, slot: usize) {
    capture_panics_on_this_thread();
    let mutator = Mutator::new(&shared.seeds, shared.seed, shared.max_len);
    let word = shared.inflight.slot(slot);
    let next = shared.inflight.next_iter();
    let mut buf = Vec::with_capacity(shared.max_len.min(1 << 16));
    let mut scratch = Scratch::default();
    let mut known: HashSet<u64> = HashSet::new();
    let mut execs = 0u64;
    'claim: loop {
        let start = next.fetch_add(BATCH, Ordering::Relaxed);
        if start >= shared.end_iter {
            break;
        }
        for iter in start..start.saturating_add(BATCH).min(shared.end_iter) {
            if shared.stop.load(Ordering::Relaxed) {
                break 'claim;
            }
            mutator.generate(iter, &mut buf, &mut scratch);
            shared.started[slot].store(shared.now_ns(), Ordering::Relaxed);
            word.store(iter, Ordering::Release);
            let crash = run_one(shared.target, &buf);
            if word.compare_exchange(iter, IDLE, Ordering::AcqRel, Ordering::Relaxed).is_err() {
                // The watchdog declared this input hung and has started a replacement worker.
                return;
            }
            execs += 1;
            shared.execs[slot].store(execs, Ordering::Relaxed);
            if let Some(sig) = crash {
                shared.crashes.fetch_add(1, Ordering::Relaxed);
                if known.insert(sig) && shared.seen.lock().expect("seen lock").insert(sig) {
                    let (location, message) = last_panic();
                    let f = Finding { kind: Kind::Crash, sig, iter, input: buf.clone(), location, message };
                    shared.pending.lock().expect("pending lock").push(f);
                }
            }
        }
    }
    shared.done[slot].store(true, Ordering::Release);
}

/// Starts a worker on `slot`. On failure, tells the running workers to stop.
fn spawn_worker(shared: &Arc<Shared>, slot: usize, stack_size: usize) -> anyhow::Result<thread::JoinHandle<()>> {
    let s = Arc::clone(shared);
    let spawned = thread::Builder::new().name(format!("fuzz-{slot}")).stack_size(stack_size).spawn(move || worker(s, slot));
    spawned.map_err(|e| {
        shared.stop.store(true, Ordering::Relaxed);
        anyhow::Error::new(e).context("spawning a fuzz worker thread")
    })
}

/// Fuzzes `target` until the iteration/time limit, the hang limit, or forever. `on_finding` is
/// called on the calling thread as soon as each new finding is known (e.g. to save it).
/// `inflight` must have at least `jobs + max_hangs` slots.
pub fn fuzz(
    target: fn(&[u8]),
    seeds: Arc<Seeds>,
    inflight: Arc<Inflight>,
    cfg: &Config,
    on_finding: &mut dyn FnMut(&Finding),
) -> anyhow::Result<Report> {
    let jobs = cfg.jobs.max(1);
    let slots = jobs + cfg.max_hangs;
    anyhow::ensure!(inflight.slots() >= slots, "in-flight table too small");
    anyhow::ensure!(seeds.len() > 0, "no seed inputs");
    let end_iter = match cfg.iterations {
        Some(n) => cfg.start_iter.saturating_add(n).min(MAX_ITER),
        None => MAX_ITER,
    };
    inflight.next_iter().store(cfg.start_iter, Ordering::Relaxed);
    let shared = Arc::new(Shared {
        target,
        seeds,
        seed: cfg.seed,
        max_len: cfg.max_len,
        end_iter,
        stop: AtomicBool::new(false),
        inflight,
        base: Instant::now(),
        started: (0..slots).map(|_| AtomicU64::new(0)).collect(),
        execs: (0..slots).map(|_| AtomicU64::new(0)).collect(),
        done: (0..slots).map(|_| AtomicBool::new(false)).collect(),
        crashes: AtomicU64::new(0),
        seen: Mutex::new(HashSet::new()),
        pending: Mutex::new(Vec::new()),
    });
    // Mutator::new panics on an empty seed set; checked above. Build one here for regenerating
    // hung inputs on this thread.
    let mutator = Mutator::new(&shared.seeds, cfg.seed, cfg.max_len);
    let mut scratch = Scratch::default();

    let mut active: Vec<(usize, thread::JoinHandle<()>)> = Vec::with_capacity(jobs);
    for slot in 0..jobs {
        active.push((slot, spawn_worker(&shared, slot, cfg.stack_size)?));
    }
    let mut next_slot = jobs;
    let mut report = Report::default();
    let budget_ns = cfg.budget.as_nanos() as u64;
    let tick = (cfg.budget / 4).clamp(Duration::from_millis(1), Duration::from_millis(50));
    let deadline = cfg.duration.map(|d| shared.base + d);
    let mut last_progress = Instant::now();
    let mut last_execs = 0u64;

    loop {
        thread::sleep(tick);
        let now = shared.now_ns();
        let mut i = 0;
        while i < active.len() {
            let slot = active[i].0;
            if shared.done[slot].load(Ordering::Acquire) {
                let (_, handle) = active.swap_remove(i);
                let _ = handle.join();
                continue;
            }
            i += 1;
            let word = shared.inflight.slot(slot);
            let cur = word.load(Ordering::Acquire);
            if cur >= HUNG {
                continue;
            }
            let started = shared.started[slot].load(Ordering::Relaxed);
            if now.saturating_sub(started) <= budget_ns
                || word.compare_exchange(cur, HUNG, Ordering::AcqRel, Ordering::Relaxed).is_err()
            {
                continue;
            }
            // Hung: abandon the thread (it may finish later and will then just exit).
            i -= 1;
            drop(active.swap_remove(i));
            report.hangs += 1;
            let mut input = Vec::new();
            mutator.generate(cur, &mut input, &mut scratch);
            let f = Finding {
                kind: Kind::Hang,
                sig: fnv1a(&input),
                iter: cur,
                input,
                location: String::new(),
                message: format!("exceeded the {} ms budget", cfg.budget.as_millis()),
            };
            on_finding(&f);
            report.findings.push(f);
            if report.hangs as usize >= cfg.max_hangs {
                report.hang_limit_hit = true;
                shared.stop.store(true, Ordering::Relaxed);
            } else {
                active.push((next_slot, spawn_worker(&shared, next_slot, cfg.stack_size)?));
                next_slot += 1;
            }
        }
        let fresh = std::mem::take(&mut *shared.pending.lock().expect("pending lock"));
        for f in fresh {
            on_finding(&f);
            report.findings.push(f);
        }
        if active.is_empty() {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            shared.stop.store(true, Ordering::Relaxed);
        }
        if let Some(every) = cfg.progress
            && last_progress.elapsed() >= every
        {
            let execs: u64 = shared.execs.iter().map(|e| e.load(Ordering::Relaxed)).sum();
            let rate = (execs - last_execs) as f64 / last_progress.elapsed().as_secs_f64();
            eprintln!(
                "fuzz: {:>6.1}s  execs {execs}  ({rate:.0}/s)  crashes {}  hangs {}",
                shared.base.elapsed().as_secs_f64(),
                shared.crashes.load(Ordering::Relaxed),
                report.hangs
            );
            last_progress = Instant::now();
            last_execs = execs;
        }
    }
    report.execs = shared.execs.iter().map(|e| e.load(Ordering::Relaxed)).sum();
    report.crashes = shared.crashes.load(Ordering::Relaxed);
    report.elapsed = shared.base.elapsed();
    report.next_iter = shared.inflight.next_iter().load(Ordering::Relaxed).min(end_iter);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn seeds(items: &[&[u8]]) -> Arc<Seeds> {
        let mut s = Seeds::default();
        for it in items {
            s.push(it, 1 << 16).unwrap();
        }
        Arc::new(s)
    }

    fn config(jobs: usize, iterations: Option<u64>) -> Config {
        Config {
            seed: 11,
            start_iter: 0,
            iterations,
            duration: None,
            jobs,
            budget: Duration::from_secs(5),
            max_len: 4096,
            max_hangs: 2,
            stack_size: 1 << 21,
            progress: None,
        }
    }

    fn run(target: fn(&[u8]), seeds: Arc<Seeds>, cfg: &Config) -> (Report, usize) {
        let inflight = Arc::new(Inflight::heap(cfg.jobs + cfg.max_hangs, cfg.seed, cfg.start_iter));
        let mut callbacks = 0;
        let report = fuzz(target, seeds, inflight, cfg, &mut |_| callbacks += 1).unwrap();
        (report, callbacks)
    }

    static COUNT: AtomicUsize = AtomicUsize::new(0);

    fn counting(_: &[u8]) {
        COUNT.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn runs_exactly_the_requested_iterations_on_any_thread_count() {
        let s = seeds(&[b"x = 1\n", b"def f(): pass\n"]);
        for jobs in [1, 3, 8] {
            COUNT.store(0, Ordering::Relaxed);
            let mut cfg = config(jobs, Some(1001));
            cfg.start_iter = 500;
            let (report, callbacks) = run(counting, Arc::clone(&s), &cfg);
            assert_eq!(report.execs, 1001, "jobs {jobs}");
            assert_eq!(COUNT.load(Ordering::Relaxed), 1001, "jobs {jobs}");
            assert_eq!(report.next_iter, 1501);
            assert_eq!((report.crashes, report.hangs, callbacks), (0, 0, 0));
            assert!(!report.hang_limit_hit);
        }
    }

    fn panics_on_nul(src: &[u8]) {
        if src.contains(&0) {
            panic!("nul at {}", src.iter().position(|&b| b == 0).unwrap_or(0));
        }
    }

    #[test]
    fn crashes_are_caught_deduplicated_and_regenerable() {
        let s = seeds(&[b"a\0b\n", b"plain\n"]);
        let cfg = config(4, Some(2000));
        let (report, callbacks) = run(panics_on_nul, Arc::clone(&s), &cfg);
        assert_eq!(report.execs, 2000);
        assert!(report.crashes > 100, "only {} crashes", report.crashes);
        // One panic location, so one finding, however many inputs crashed and threads saw it.
        assert_eq!(report.findings.len(), 1);
        assert_eq!(callbacks, 1);
        let f = &report.findings[0];
        assert_eq!(f.kind, Kind::Crash);
        assert!(f.location.contains("engine.rs:"), "{}", f.location);
        assert!(f.message.starts_with("nul at "), "{}", f.message);
        assert_eq!(f.sig, fnv1a(f.location.as_bytes()));
        let mut again = Vec::new();
        Mutator::new(&s, cfg.seed, cfg.max_len).generate(f.iter, &mut again, &mut Scratch::default());
        assert_eq!(again, f.input, "a finding is regenerable from its iteration");
    }

    fn stalls_on_s(src: &[u8]) {
        if src.contains(&b'S') {
            thread::sleep(Duration::from_millis(400));
        }
    }

    #[test]
    fn hangs_are_detected_and_cap_the_run() {
        // Every input starts from a seed full of 'S', so nearly all of them stall.
        let s = seeds(&[b"SSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSS"]);
        let mut cfg = config(2, None);
        cfg.budget = Duration::from_millis(40);
        cfg.duration = Some(Duration::from_secs(20));
        let t = Instant::now();
        let (report, callbacks) = run(stalls_on_s, Arc::clone(&s), &cfg);
        assert!(t.elapsed() < Duration::from_secs(5), "the hang cap must end the run early");
        assert!(report.hang_limit_hit);
        assert!(report.hangs >= 2);
        assert_eq!(report.findings.len() as u64, report.hangs);
        assert_eq!(callbacks as u64, report.hangs);
        let mut again = Vec::new();
        for f in &report.findings {
            assert_eq!(f.kind, Kind::Hang);
            assert!(f.input.contains(&b'S'));
            assert_eq!(f.sig, fnv1a(&f.input));
            Mutator::new(&s, cfg.seed, cfg.max_len).generate(f.iter, &mut again, &mut Scratch::default());
            assert_eq!(again, f.input);
        }
    }

    #[test]
    fn duration_limit_stops_an_unbounded_run() {
        let s = seeds(&[b"x\n"]);
        let mut cfg = config(2, None);
        cfg.duration = Some(Duration::from_millis(150));
        let t = Instant::now();
        let (report, _) = run(|_| {}, s, &cfg);
        assert!(t.elapsed() < Duration::from_secs(3));
        assert!(report.execs > 0);
        assert!(report.next_iter >= report.execs);
    }

    #[test]
    fn rejects_bad_setups() {
        let cfg = config(2, Some(10));
        let small = Arc::new(Inflight::heap(1, cfg.seed, 0));
        assert!(fuzz(|_| {}, seeds(&[b""]), small, &cfg, &mut |_| {}).is_err());
        let ok = Arc::new(Inflight::heap(4, cfg.seed, 0));
        assert!(fuzz(|_| {}, Arc::new(Seeds::default()), ok, &cfg, &mut |_| {}).is_err());
    }
}
