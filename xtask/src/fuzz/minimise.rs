//! Test-case minimiser and a time-limited executor for running candidates.
//!
//! [`minimise`] is greedy chunk removal (ddmin-style): first whole lines, in chunks halving from
//! half the input down to one line, then bytes the same way, repeated until a round removes
//! nothing or the execution budget runs out. The predicate decides whether a candidate still
//! reproduces the problem; [`Executor`] runs candidates on a helper thread with the per-input
//! time budget so that a candidate that hangs is detected instead of hanging the minimiser.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use super::engine::{capture_panics_on_this_thread, last_panic, run_one};

/// What the helper thread sends back: the buffer (for reuse) and, on a panic, its signature,
/// location and message.
type Reply = (Vec<u8>, Option<(u64, String, String)>);

/// Result of running one input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// Panicked; the crash signature.
    Crash(u64),
    /// Did not finish within the budget (the helper thread was abandoned).
    Hang,
}

/// Runs a target on a helper thread with a time budget, reusing one buffer for all candidates.
pub struct Executor {
    target: fn(&[u8]),
    budget: Duration,
    stack_size: usize,
    chan: Option<(Sender<Vec<u8>>, Receiver<Reply>)>,
    spare: Vec<u8>,
    /// Location and message of the last crash.
    last_panic: (String, String),
    abandoned: usize,
    max_abandoned: usize,
}

impl Executor {
    /// At most `max_abandoned` helper threads may be abandoned on hangs; after that `run`
    /// returns `None`.
    pub fn new(target: fn(&[u8]), budget: Duration, stack_size: usize, max_abandoned: usize) -> Executor {
        Executor { target, budget, stack_size, chan: None, spare: Vec::new(), last_panic: Default::default(), abandoned: 0, max_abandoned }
    }

    #[cfg(test)]
    pub fn abandoned(&self) -> usize {
        self.abandoned
    }

    /// Location and message of the most recent `Outcome::Crash`.
    pub fn last_panic(&self) -> (&str, &str) {
        (&self.last_panic.0, &self.last_panic.1)
    }

    fn spawn(&mut self) -> std::io::Result<()> {
        let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>();
        let (out_tx, out_rx) = mpsc::channel::<Reply>();
        let target = self.target;
        thread::Builder::new().name("fuzz-minimise".into()).stack_size(self.stack_size).spawn(move || {
            capture_panics_on_this_thread();
            while let Ok(buf) = in_rx.recv() {
                let crash = run_one(target, &buf).map(|sig| {
                    let (location, message) = last_panic();
                    (sig, location, message)
                });
                if out_tx.send((buf, crash)).is_err() {
                    break;
                }
            }
        })?;
        self.chan = Some((in_tx, out_rx));
        Ok(())
    }

    /// Runs `input`; `None` once the abandoned-thread limit is reached (or a thread cannot be
    /// spawned).
    pub fn run(&mut self, input: &[u8]) -> Option<Outcome> {
        if self.abandoned >= self.max_abandoned {
            return None;
        }
        if self.chan.is_none() {
            self.spawn().ok()?;
        }
        let (tx, rx) = self.chan.as_ref()?;
        let mut buf = std::mem::take(&mut self.spare);
        buf.clear();
        buf.extend_from_slice(input);
        tx.send(buf).ok()?;
        match rx.recv_timeout(self.budget) {
            Ok((buf, crash)) => {
                self.spare = buf;
                Some(match crash {
                    None => Outcome::Ok,
                    Some((sig, location, message)) => {
                        self.last_panic = (location, message);
                        Outcome::Crash(sig)
                    }
                })
            }
            Err(RecvTimeoutError::Timeout) => {
                // Dropping the channels lets the thread exit if it ever finishes.
                self.chan = None;
                self.abandoned += 1;
                Some(Outcome::Hang)
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.chan = None;
                None
            }
        }
    }
}

/// Outcome of a minimisation.
#[derive(Debug)]
pub struct Minimised {
    pub data: Vec<u8>,
    pub execs: usize,
    /// The predicate gave up (returned `None`) before minimisation finished.
    pub aborted: bool,
}

/// Shrinks `input` while `still_fails` keeps returning `Some(true)`, running the predicate at
/// most `max_execs` times. `still_fails` returning `None` stops minimisation early. The input
/// itself is assumed to fail and is not re-checked.
pub fn minimise(input: &[u8], max_execs: usize, still_fails: &mut dyn FnMut(&[u8]) -> Option<bool>) -> Minimised {
    let mut m = Run { cur: input.to_vec(), cand: Vec::with_capacity(input.len()), execs: 0, max_execs, aborted: false };
    let mut starts: Vec<usize> = Vec::new();
    loop {
        let before = m.cur.len();
        m.remove_line_chunks(&mut starts, still_fails);
        m.remove_byte_chunks(still_fails);
        if m.cur.len() == before || m.out_of_budget() {
            break;
        }
    }
    Minimised { data: m.cur, execs: m.execs, aborted: m.aborted }
}

struct Run {
    cur: Vec<u8>,
    cand: Vec<u8>,
    execs: usize,
    max_execs: usize,
    aborted: bool,
}

impl Run {
    fn out_of_budget(&self) -> bool {
        self.aborted || self.execs >= self.max_execs
    }

    /// Tries `cur` without bytes `a..b`; keeps the removal if it still fails.
    fn try_remove(&mut self, a: usize, b: usize, pred: &mut dyn FnMut(&[u8]) -> Option<bool>) -> bool {
        self.cand.clear();
        self.cand.extend_from_slice(&self.cur[..a]);
        self.cand.extend_from_slice(&self.cur[b..]);
        self.execs += 1;
        match pred(&self.cand) {
            Some(true) => {
                std::mem::swap(&mut self.cur, &mut self.cand);
                true
            }
            Some(false) => false,
            None => {
                self.aborted = true;
                false
            }
        }
    }

    fn remove_line_chunks(&mut self, starts: &mut Vec<usize>, pred: &mut dyn FnMut(&[u8]) -> Option<bool>) {
        line_starts(&self.cur, starts);
        let mut chunk = (starts.len() - 1).div_ceil(2);
        while chunk >= 1 && !self.out_of_budget() {
            let mut i = 0;
            while i + 1 < starts.len() && !self.out_of_budget() {
                let j = (i + chunk).min(starts.len() - 1);
                if self.try_remove(starts[i], starts[j], pred) {
                    line_starts(&self.cur, starts);
                } else {
                    i += chunk;
                }
            }
            chunk = next_chunk(chunk);
        }
    }

    fn remove_byte_chunks(&mut self, pred: &mut dyn FnMut(&[u8]) -> Option<bool>) {
        let mut chunk = self.cur.len().div_ceil(2);
        while chunk >= 1 && !self.out_of_budget() {
            let mut i = 0;
            while i < self.cur.len() && !self.out_of_budget() {
                let j = (i + chunk).min(self.cur.len());
                if !self.try_remove(i, j, pred) {
                    i += chunk;
                }
            }
            chunk = next_chunk(chunk);
        }
    }
}

/// Next chunk size: halve (rounding up) down to 1, then 0 to stop.
fn next_chunk(chunk: usize) -> usize {
    if chunk == 1 { 0 } else { chunk.div_ceil(2) }
}

/// Byte offsets where lines start, plus `src.len()` as a final sentinel.
fn line_starts(src: &[u8], out: &mut Vec<usize>) {
    out.clear();
    out.push(0);
    for (i, &b) in src.iter().enumerate() {
        if b == b'\n' && i + 1 < src.len() {
            out.push(i + 1);
        }
    }
    out.push(src.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn line_starts_have_sentinel() {
        let mut v = Vec::new();
        line_starts(b"", &mut v);
        assert_eq!(v, [0, 0]);
        line_starts(b"a\nb\n", &mut v);
        assert_eq!(v, [0, 2, 4]);
        line_starts(b"a\n\nbc", &mut v);
        assert_eq!(v, [0, 2, 3, 5]);
    }

    #[test]
    fn shrinks_to_the_trigger() {
        let mut input = Vec::new();
        for i in 0..200 {
            input.extend_from_slice(format!("line {i} = foo(bar, baz)\n").as_bytes());
        }
        input.splice(1234..1234, b"XY".iter().copied());
        let m = minimise(&input, 10_000, &mut |c| Some(contains(c, b"XY")));
        assert_eq!(m.data, b"XY");
        assert!(!m.aborted);
        assert!(m.execs < 1500, "took {} execs", m.execs);
    }

    #[test]
    fn keeps_non_contiguous_requirements() {
        let input = b"aaaa\nbbbb\nNUL\0here\ncccc\n{{{{\ndddd\n";
        let m = minimise(input, 10_000, &mut |c| Some(c.contains(&0) && contains(c, b"{{")));
        assert!(m.data.contains(&0) && contains(&m.data, b"{{"));
        assert_eq!(m.data.len(), 3, "{:?}", String::from_utf8_lossy(&m.data));
    }

    #[test]
    fn respects_exec_budget_and_abort() {
        let input = vec![b'x'; 5000];
        let m = minimise(&input, 25, &mut |_| Some(false));
        assert_eq!(m.execs, 25);
        assert_eq!(m.data, input);
        let mut calls = 0;
        let m = minimise(&input, 1000, &mut |_| {
            calls += 1;
            if calls > 3 { None } else { Some(false) }
        });
        assert!(m.aborted);
        assert_eq!(m.execs, 4);
        assert_eq!(m.data, input);
        // Progress made before the predicate gives up is kept.
        let lines: Vec<u8> = b"a\n".repeat(64);
        let mut calls = 0;
        let m = minimise(&lines, 1000, &mut |c| {
            calls += 1;
            if calls > 2 { None } else { Some(c.len() >= 2) }
        });
        assert!(m.aborted);
        assert_eq!(m.execs, 3);
        assert_eq!(m.data.len(), 64, "the first half of the lines removed, removing the rest rejected");
    }

    #[test]
    fn chunk_sizes_reach_one() {
        let mut sizes = Vec::new();
        let mut c = 7usize.div_ceil(2);
        while c >= 1 {
            sizes.push(c);
            c = next_chunk(c);
        }
        assert_eq!(sizes, [4, 2, 1]);
        assert_eq!(next_chunk(1), 0);
    }

    #[test]
    fn empty_and_tiny_inputs() {
        let m = minimise(b"", 100, &mut |_| Some(true));
        assert_eq!(m.data, b"");
        let m = minimise(b"a", 100, &mut |_| Some(true));
        assert_eq!(m.data, b"");
        let m = minimise(b"a", 100, &mut |c| Some(c == b"a"));
        assert_eq!(m.data, b"a");
    }

    fn sleepy(src: &[u8]) {
        if src.contains(&b'S') {
            thread::sleep(Duration::from_millis(400));
        }
        if src.contains(&b'P') {
            panic!("planted");
        }
    }

    #[test]
    fn executor_reports_ok_crash_and_hang() {
        let mut ex = Executor::new(sleepy, Duration::from_millis(100), 1 << 20, 2);
        assert_eq!(ex.run(b"abc"), Some(Outcome::Ok));
        let Some(Outcome::Crash(sig)) = ex.run(b"aPc") else { panic!("expected crash") };
        assert_eq!(ex.last_panic().1, "planted");
        assert!(ex.last_panic().0.contains("minimise.rs:"), "{:?}", ex.last_panic());
        assert_eq!(ex.run(b"P"), Some(Outcome::Crash(sig)), "same location, same signature");
        assert_eq!(ex.run(b"S"), Some(Outcome::Hang));
        assert_eq!(ex.abandoned(), 1);
        // A fresh helper thread takes over.
        assert_eq!(ex.run(b"ok"), Some(Outcome::Ok));
        assert_eq!(ex.run(b"SS"), Some(Outcome::Hang));
        // Limit reached.
        assert_eq!(ex.run(b"ok"), None);
    }
}
