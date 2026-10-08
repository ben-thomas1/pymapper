//! `--measure`: times frames with redraw forced every frame, then leaves the window alone and
//! measures process CPU time while idle. Prints a report and exits.
//!
//! Phases: warm-up (shader/pipeline compilation, font atlas upload; not recorded) -> timed
//! frames -> settle (lets egui finish any follow-up repaints) -> idle window -> done.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use crate::args::MeasureOpts;
use crate::map::MapState;
use crate::map::gpu::GpuMemory;
use crate::stats::{Samples, Summary, process_usage};

const WARMUP_FRAMES: u32 = 30;
const SETTLE: Duration = Duration::from_millis(500);
/// Frames are forced back to back during warm-up and timing, so a gap this long means none
/// can be presented (window hidden, occluded or on another Space): give up instead of
/// waiting forever.
pub const STALL: Duration = Duration::from_secs(10);

/// CPU-side timings of one frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameTimes {
    /// egui input + layout (`run_ui`) + tessellation.
    pub ui: Duration,
    /// Waiting for the swapchain image (includes vsync back-pressure).
    pub acquire: Duration,
    /// Texture/buffer upload, encoding, submit and present.
    pub submit: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Warmup { left: u32 },
    Frames { left: u32 },
    Settle { until: Instant },
    Idle { start: Instant, until: Instant, cpu: Duration, frames: u64, wakeups: u64 },
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IdleResult {
    pub wall: Duration,
    pub cpu: Duration,
    pub frames: u64,
    pub wakeups: u64,
}

impl IdleResult {
    /// CPU use as a percentage of one core.
    pub fn cpu_percent(&self) -> f64 {
        if self.wall.is_zero() { 0.0 } else { 100.0 * self.cpu.as_secs_f64() / self.wall.as_secs_f64() }
    }
}

pub struct Measure {
    opts: MeasureOpts,
    phase: Phase,
    /// When the last frame was rendered (or measurement started).
    last_frame: Instant,
    ui: Samples,
    acquire: Samples,
    submit: Samples,
    work: Samples,
    timed_start: Option<Instant>,
    timed_wall: Duration,
    first_frame: Option<Duration>,
    frames: u64,
    wakeups: u64,
    idle: Option<IdleResult>,
}

impl Measure {
    /// `now` starts the stall clock (see [`STALL`]).
    pub fn new(opts: MeasureOpts, now: Instant) -> Self {
        let cap = opts.frames as usize;
        Self {
            opts,
            phase: Phase::Warmup { left: WARMUP_FRAMES },
            last_frame: now,
            ui: Samples::with_capacity(cap),
            acquire: Samples::with_capacity(cap),
            submit: Samples::with_capacity(cap),
            work: Samples::with_capacity(cap),
            timed_start: None,
            timed_wall: Duration::ZERO,
            first_frame: None,
            frames: 0,
            wakeups: 0,
            idle: None,
        }
    }

    /// Records a rendered frame. `since_launch` is the time from process start to the end of
    /// this frame. Returns `true` if the next frame should be forced immediately.
    pub fn on_frame(&mut self, now: Instant, t: FrameTimes, since_launch: Duration) -> bool {
        self.frames += 1;
        self.last_frame = now;
        self.first_frame.get_or_insert(since_launch);
        match self.phase {
            Phase::Warmup { left } => {
                self.phase = if left <= 1 { Phase::Frames { left: self.opts.frames } } else { Phase::Warmup { left: left - 1 } };
                self.timed_start = Some(now);
                true
            }
            Phase::Frames { left } => {
                self.ui.push(t.ui);
                self.acquire.push(t.acquire);
                self.submit.push(t.submit);
                self.work.push(t.ui + t.submit);
                if left <= 1 {
                    self.timed_wall = self.timed_start.map_or(Duration::ZERO, |s| now - s);
                    self.phase = Phase::Settle { until: now + SETTLE };
                    false
                } else {
                    self.phase = Phase::Frames { left: left - 1 };
                    true
                }
            }
            Phase::Settle { .. } | Phase::Idle { .. } | Phase::Done => false,
        }
    }

    /// An input/OS event woke the event loop (not a timer we asked for).
    pub fn on_event_wakeup(&mut self) {
        self.wakeups += 1;
    }

    /// The next instant the loop must wake for the measurement, if any. While frames are
    /// being forced this is the stall deadline.
    pub fn deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Settle { until } | Phase::Idle { until, .. } => Some(until),
            Phase::Warmup { .. } | Phase::Frames { .. } => self.last_frame.checked_add(STALL),
            Phase::Done => None,
        }
    }

    /// True if frames are being forced but none has been rendered for [`STALL`].
    pub fn stalled(&self, now: Instant) -> bool {
        matches!(self.phase, Phase::Warmup { .. } | Phase::Frames { .. })
            && now.saturating_duration_since(self.last_frame) >= STALL
    }

    /// Advances timer-driven phases. Returns `true` once the measurement is complete.
    pub fn poll(&mut self, now: Instant) -> bool {
        match self.phase {
            Phase::Settle { until } if now >= until => {
                let secs = u64::from(self.opts.idle_secs);
                self.phase = Phase::Idle {
                    start: now,
                    until: now + Duration::from_secs(secs),
                    cpu: process_usage().cpu,
                    frames: self.frames,
                    wakeups: self.wakeups,
                };
                false
            }
            Phase::Idle { start, until, cpu, frames, wakeups } if now >= until => {
                self.idle = Some(IdleResult {
                    wall: now - start,
                    cpu: process_usage().cpu.saturating_sub(cpu),
                    frames: self.frames - frames,
                    wakeups: self.wakeups - wakeups,
                });
                self.phase = Phase::Done;
                true
            }
            Phase::Done => true,
            _ => false,
        }
    }

    /// Progress through the timed frames in [0, 1), or `None` outside that phase.
    pub fn progress(&self) -> Option<f32> {
        match self.phase {
            Phase::Frames { left } => Some(1.0 - left as f32 / self.opts.frames.max(1) as f32),
            _ => None,
        }
    }

    pub fn idle_result(&self) -> Option<IdleResult> {
        self.idle
    }

    /// Human-readable report.
    pub fn report(&self, renderer: &str, size: [u32; 2], pixels_per_point: f32) -> String {
        let mut scratch = Vec::with_capacity(self.ui.len());
        let mut out = String::new();
        let _ = writeln!(out, "pymapper-gui --measure");
        let _ = writeln!(out, "renderer: {}", renderer.replace('\n', "; "));
        let _ = writeln!(out, "surface: {}x{} px @ {pixels_per_point}x", size[0], size[1]);
        if let Some(d) = self.first_frame {
            let _ = writeln!(out, "launch to first frame presented: {:.1} ms", ms(d));
        }
        let n = self.ui.len();
        if n > 0 {
            let fps = n as f64 / self.timed_wall.as_secs_f64().max(f64::MIN_POSITIVE);
            let _ = writeln!(out, "timed frames: {n} (redraw forced every frame; {fps:.1} fps, vsync-bound)");
            let _ = writeln!(out, "  {:<28} {:>8} {:>8} {:>8} {:>8} {:>8}", "per frame (CPU, ms)", "mean", "p50", "p95", "p99", "max");
            for (label, s) in [
                ("ui: egui run + tessellate", &self.ui),
                ("upload+encode+submit+present", &self.submit),
                ("cpu work (ui + submit)", &self.work),
                ("swapchain acquire (vsync)", &self.acquire),
            ] {
                row(&mut out, label, s.summary(&mut scratch));
            }
        }
        if let Some(idle) = self.idle {
            let _ = writeln!(
                out,
                "idle: {:.1} s, cpu {:.1} ms ({:.3}% of one core), frames rendered {}, event wakeups {}",
                idle.wall.as_secs_f64(),
                ms(idle.cpu),
                idle.cpu_percent(),
                idle.frames,
                idle.wakeups
            );
        }
        let usage = process_usage();
        let _ = writeln!(out, "peak RSS: {:.1} MiB", usage.peak_rss_bytes as f64 / (1024.0 * 1024.0));
        out
    }
}

/// Appends map renderer statistics (scene size, LOD rebuilds, memory) to a report.
pub fn map_report(out: &mut String, map: &MapState, gpu: GpuMemory, scene_build: Option<Duration>) {
    let s = &map.scene;
    let st = &map.stats;
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let _ = writeln!(out, "map: {} rects, {} curves, {} lines", s.rects.len(), s.curves.len(), s.lines.len());
    if let Some(d) = scene_build {
        let _ = writeln!(out, "  scene build (generate + sort + grids): {:.1} ms", ms(d));
    }
    for (name, r) in [("band rebuilds", &st.band_rebuilds), ("label rebuilds", &st.label_rebuilds)] {
        let _ = writeln!(out, "  {name}: {} (mean {:.2} ms, max {:.2} ms)", r.count, ms(r.mean()), ms(r.max));
    }
    let _ = writeln!(
        out,
        "  last frame: {} instances in {} draw calls; {} labels, {} glyphs",
        st.drawn_instances, st.draw_calls, st.labels, st.glyphs
    );
    let _ = writeln!(
        out,
        "  memory: scene {:.1} MiB (CPU), GPU buffers {:.1} MiB, glyph atlas {:.1} MiB",
        mib(s.heap_bytes() as u64),
        mib(gpu.buffers),
        mib(gpu.atlas)
    );
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn row(out: &mut String, label: &str, s: Summary) {
    let _ = writeln!(
        out,
        "  {label:<28} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>8.3}",
        ms(s.mean),
        ms(s.p50),
        ms(s.p95),
        ms(s.p99),
        ms(s.max)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(ms: u64) -> FrameTimes {
        FrameTimes {
            ui: Duration::from_millis(ms),
            acquire: Duration::from_millis(1),
            submit: Duration::from_millis(2),
        }
    }

    #[test]
    fn phases_progress_to_done() {
        let t0 = Instant::now();
        let mut m = Measure::new(MeasureOpts { frames: 3, idle_secs: 1 }, t0);
        let mut t = t0;
        // Warm-up frames force redraws and are not recorded.
        for _ in 0..WARMUP_FRAMES {
            assert!(m.on_frame(t, times(100), Duration::from_millis(50)));
            t += Duration::from_millis(16);
        }
        assert_eq!(m.ui.len(), 0);
        // While frames are forced, the only deadline is the stall watchdog.
        assert_eq!(m.deadline(), Some(t - Duration::from_millis(16) + STALL));
        assert!(!m.stalled(t));
        // Timed frames: the last one stops forcing.
        assert!(m.on_frame(t, times(1), Duration::ZERO));
        assert!(m.on_frame(t, times(2), Duration::ZERO));
        assert!(!m.on_frame(t, times(3), Duration::ZERO));
        assert_eq!(m.ui.len(), 3);
        assert_eq!(m.first_frame, Some(Duration::from_millis(50)));
        // Settle, then idle.
        let settle = m.deadline().unwrap();
        assert_eq!(settle, t + SETTLE);
        assert!(!m.poll(t));
        assert!(!m.poll(settle));
        let idle_end = m.deadline().unwrap();
        assert_eq!(idle_end, settle + Duration::from_secs(1));
        // An event during idle and a frame it caused are counted.
        m.on_event_wakeup();
        assert!(!m.on_frame(settle, times(1), Duration::ZERO));
        assert!(!m.poll(idle_end - Duration::from_millis(1)));
        assert!(m.poll(idle_end));
        let idle = m.idle_result().unwrap();
        assert_eq!((idle.frames, idle.wakeups), (1, 1));
        assert_eq!(idle.wall, Duration::from_secs(1));
        assert!(m.poll(idle_end), "done stays done");
        assert!(!m.stalled(idle_end + 2 * STALL), "no stall once frames are done");
        assert_eq!(m.deadline(), None);

        let report = m.report("TestGPU\ndriver x", [100, 50], 2.0);
        assert!(report.contains("renderer: TestGPU; driver x"), "{report}");
        assert!(report.contains("timed frames: 3"), "{report}");
        assert!(report.contains("idle: 1.0 s"), "{report}");
        assert!(report.contains("peak RSS"), "{report}");
    }

    #[test]
    fn stall_while_forcing_frames() {
        let t0 = Instant::now();
        let mut m = Measure::new(MeasureOpts { frames: 2, idle_secs: 1 }, t0);
        // No frame at all (window never presentable).
        assert_eq!(m.deadline(), Some(t0 + STALL));
        assert!(!m.stalled(t0 + STALL - Duration::from_millis(1)));
        assert!(m.stalled(t0 + STALL));
        // A frame resets the clock; so does reaching the timed phase.
        let t1 = t0 + Duration::from_secs(9);
        assert!(m.on_frame(t1, times(1), Duration::ZERO));
        assert!(!m.stalled(t0 + STALL));
        assert!(m.stalled(t1 + STALL));
        for _ in 1..WARMUP_FRAMES {
            m.on_frame(t1, times(1), Duration::ZERO);
        }
        assert!(matches!(m.phase, Phase::Frames { .. }));
        assert!(m.stalled(t1 + STALL));
        // Settle and idle are timer-driven: never stalled.
        m.on_frame(t1, times(1), Duration::ZERO);
        assert!(!m.on_frame(t1, times(1), Duration::ZERO));
        assert!(matches!(m.phase, Phase::Settle { .. }));
        assert!(!m.stalled(t1 + 3 * STALL));
        // A clock earlier than the last frame never underflows.
        assert!(!m.stalled(t0));
    }

    #[test]
    fn cpu_percent() {
        let r = IdleResult { wall: Duration::from_secs(10), cpu: Duration::from_millis(5), frames: 0, wakeups: 0 };
        assert!((r.cpu_percent() - 0.05).abs() < 1e-9);
        let z = IdleResult { wall: Duration::ZERO, cpu: Duration::from_millis(5), frames: 0, wakeups: 0 };
        assert_eq!(z.cpu_percent(), 0.0);
    }
}
