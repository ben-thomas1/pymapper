//! Cheap wall-clock phase timer, read by `cargo xtask bench` (docs/benchmarks.md).
//!
//! Always compiled in, disabled by default. While disabled, [`phase`] costs one relaxed atomic
//! load and returns an inert guard; nothing is allocated or locked. While enabled, each finished
//! phase takes one uncontended lock to accumulate into a small global table.
//!
//! Record phases on the coordinating thread (around a whole parallel section, not inside a
//! per-file closure) so the durations are wall time rather than summed CPU time. Phases with
//! the same name accumulate, keeping the order in which each name was first recorded.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);
/// A handful of entries at most, so a linear lookup by name beats any index.
static PHASES: Mutex<Vec<Phase>> = Mutex::new(Vec::new());

/// Accumulated wall time of one named phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Phase {
    pub name: &'static str,
    pub elapsed: Duration,
    /// Number of times the phase was recorded.
    pub count: u32,
}

/// Turns recording on or off for the whole process. Already-started guards keep their state.
pub fn enable(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether phases are currently being recorded.
#[inline]
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Starts timing `name`; the phase is recorded when the guard drops.
#[inline]
#[must_use = "the phase ends when the guard is dropped"]
pub fn phase(name: &'static str) -> PhaseGuard {
    PhaseGuard { name, start: if is_enabled() { Some(Instant::now()) } else { None } }
}

/// Runs `f` as phase `name` and returns its result.
#[inline]
pub fn time<R>(name: &'static str, f: impl FnOnce() -> R) -> R {
    let _guard = phase(name);
    f()
}

/// Adds `elapsed` to phase `name` if recording is enabled.
pub fn record(name: &'static str, elapsed: Duration) {
    if is_enabled() {
        add(name, elapsed);
    }
}

/// Returns the recorded phases in first-recorded order and clears the table.
pub fn take() -> Vec<Phase> {
    std::mem::take(&mut *PHASES.lock().unwrap_or_else(PoisonError::into_inner))
}

fn add(name: &'static str, elapsed: Duration) {
    let mut phases = PHASES.lock().unwrap_or_else(PoisonError::into_inner);
    for p in phases.iter_mut() {
        if p.name == name {
            p.elapsed += elapsed;
            p.count += 1;
            return;
        }
    }
    phases.push(Phase { name, elapsed, count: 1 });
}

/// Guard returned by [`phase`]. Records on drop if recording was enabled when it was created.
pub struct PhaseGuard {
    name: &'static str,
    start: Option<Instant>,
}

impl Drop for PhaseGuard {
    #[inline]
    fn drop(&mut self) {
        if let Some(start) = self.start {
            add(self.name, start.elapsed());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recorder is process-global; serialise the tests that use it.
    static LOCK: Mutex<()> = Mutex::new(());

    fn locked() -> std::sync::MutexGuard<'static, ()> {
        let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        enable(false);
        let _ = take();
        guard
    }

    /// The recorded phases named in `names`: tests that load an index run in parallel and
    /// record their own phases while a test here has recording enabled.
    fn own(names: &[&str]) -> Vec<Phase> {
        let mut phases = take();
        phases.retain(|p| names.contains(&p.name));
        phases
    }

    #[test]
    fn disabled_records_nothing() {
        let _l = locked();
        drop(phase("walk"));
        record("read", Duration::from_millis(5));
        assert_eq!(time("x", || 7), 7);
        assert!(take().is_empty());
    }

    #[test]
    fn enabled_records_in_first_seen_order_and_accumulates() {
        let _l = locked();
        enable(true);
        record("walk", Duration::from_millis(2));
        record("read", Duration::from_millis(3));
        record("walk", Duration::from_millis(4));
        enable(false);
        let phases = own(&["walk", "read"]);
        assert_eq!(
            phases,
            vec![
                Phase { name: "walk", elapsed: Duration::from_millis(6), count: 2 },
                Phase { name: "read", elapsed: Duration::from_millis(3), count: 1 },
            ]
        );
        assert!(take().is_empty(), "take clears the table");
    }

    #[test]
    fn guard_measures_wall_time_and_nests() {
        let _l = locked();
        enable(true);
        {
            let _outer = phase("outer");
            time("inner", || std::thread::sleep(Duration::from_millis(10)));
        }
        enable(false);
        let phases = own(&["inner", "outer"]);
        assert_eq!(phases.len(), 2);
        // Inner finishes first, so it is recorded first.
        assert_eq!(phases[0].name, "inner");
        assert_eq!(phases[1].name, "outer");
        assert!(phases[0].elapsed >= Duration::from_millis(10));
        assert!(phases[1].elapsed >= phases[0].elapsed);
    }

    #[test]
    fn guard_keeps_state_from_creation() {
        let _l = locked();
        let inert = phase("inert");
        enable(true);
        let live = phase("live");
        drop(inert);
        enable(false);
        drop(live);
        let phases = take();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].name, "live");
    }

    #[test]
    fn same_name_from_distinct_literals_merges() {
        let _l = locked();
        enable(true);
        let a: &'static str = "phase";
        let owned: &'static str = String::from("phase").leak();
        record(a, Duration::from_millis(1));
        record(owned, Duration::from_millis(1));
        enable(false);
        let phases = take();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].count, 2);
    }

    #[test]
    fn concurrent_recording_is_complete() {
        let _l = locked();
        enable(true);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..100 {
                        record("p", Duration::from_nanos(1));
                    }
                });
            }
        });
        enable(false);
        let phases = take();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].count, 800);
        assert_eq!(phases[0].elapsed, Duration::from_nanos(800));
    }
}
