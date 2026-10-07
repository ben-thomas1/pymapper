//! Render-on-demand scheduling (pure; no winit types so it is unit-testable).
//!
//! The event loop sleeps (`ControlFlow::Wait`) unless a redraw is pending. Redraws are
//! requested by input events (egui says whether an event needs a repaint), by egui itself
//! after a frame (`repaint_delay`: zero while animating, a finite delay for tooltips and
//! cursor blink, "never" when idle), and from other threads via egui's repaint callback
//! (the future layout thread). All of those funnel into [`Scheduler::request`]; the loop
//! asks [`Scheduler::poll`] once per iteration what to do.

use std::time::{Duration, Instant};

/// What the event loop should do before going back to sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// A redraw is due now: request one, then sleep until events arrive.
    RedrawNow,
    /// Sleep until this instant (or until events arrive).
    At(Instant),
    /// Nothing pending: sleep until events arrive.
    Never,
}

#[derive(Debug, Default, Clone)]
pub struct Scheduler {
    next: Option<Instant>,
}

impl Scheduler {
    pub const fn new() -> Self {
        Self { next: None }
    }

    /// Asks for a redraw `delay` after `now`, keeping the earliest pending deadline. Delays
    /// too large to represent (egui uses `Duration::MAX` for "no repaint") are ignored.
    pub fn request(&mut self, now: Instant, delay: Duration) {
        let Some(at) = now.checked_add(delay) else { return };
        match self.next {
            Some(cur) if cur <= at => {}
            _ => self.next = Some(at),
        }
    }

    /// A frame was just drawn: every request made before it is satisfied. `repaint_delay`
    /// is egui's request for the following frame.
    pub fn frame_done(&mut self, now: Instant, repaint_delay: Duration) {
        self.next = None;
        self.request(now, repaint_delay);
    }

    /// Consumes a due deadline. Call once per event-loop iteration (`about_to_wait`).
    pub fn poll(&mut self, now: Instant) -> Wake {
        match self.next {
            None => Wake::Never,
            Some(at) if at <= now => {
                self.next = None;
                Wake::RedrawNow
            }
            Some(at) => Wake::At(at),
        }
    }

    pub const fn pending(&self) -> Option<Instant> {
        self.next
    }
}

/// The earlier of two optional deadlines.
pub fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn idle_scheduler_never_wakes() {
        let mut s = Scheduler::new();
        let t = Instant::now();
        assert_eq!(s.poll(t), Wake::Never);
        s.frame_done(t, Duration::MAX);
        assert_eq!(s.poll(t + Duration::from_secs(3600)), Wake::Never);
        assert_eq!(s.pending(), None);
    }

    #[test]
    fn zero_delay_redraws_once() {
        let mut s = Scheduler::new();
        let t = Instant::now();
        s.request(t, Duration::ZERO);
        assert_eq!(s.poll(t), Wake::RedrawNow);
        // Consumed: the next iteration sleeps.
        assert_eq!(s.poll(t), Wake::Never);
    }

    #[test]
    fn keeps_earliest_deadline() {
        let mut s = Scheduler::new();
        let t = Instant::now();
        s.request(t, 500 * MS);
        s.request(t, 100 * MS);
        s.request(t, 300 * MS);
        s.request(t, Duration::MAX);
        assert_eq!(s.poll(t), Wake::At(t + 100 * MS));
        assert_eq!(s.poll(t + 99 * MS), Wake::At(t + 100 * MS));
        assert_eq!(s.poll(t + 100 * MS), Wake::RedrawNow);
        assert_eq!(s.poll(t + 100 * MS), Wake::Never);
    }

    #[test]
    fn frame_done_replaces_pending_requests() {
        let mut s = Scheduler::new();
        let t = Instant::now();
        s.request(t, 10 * MS);
        // The frame satisfied the request; egui now wants a tooltip repaint in 400 ms.
        s.frame_done(t + MS, 400 * MS);
        assert_eq!(s.poll(t + 20 * MS), Wake::At(t + 401 * MS));
        // Animation: egui asks for the next frame immediately.
        s.frame_done(t + 2 * MS, Duration::ZERO);
        assert_eq!(s.poll(t + 2 * MS), Wake::RedrawNow);
    }

    #[test]
    fn earliest_of_optionals() {
        let t = Instant::now();
        let u = t + MS;
        assert_eq!(earliest(None, None), None);
        assert_eq!(earliest(Some(t), None), Some(t));
        assert_eq!(earliest(None, Some(u)), Some(u));
        assert_eq!(earliest(Some(u), Some(t)), Some(t));
    }
}
