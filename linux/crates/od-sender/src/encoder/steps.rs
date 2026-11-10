//! Where an encode call's time goes: wall time per named step, averaged over
//! a few seconds and logged at debug level (`RUST_LOG=debug`). Costs a clock
//! read per step, so it is always on.

use std::time::{Duration, Instant};

use tracing::debug;

/// How often the averages are logged.
const WINDOW: Duration = Duration::from_secs(2);

pub(super) struct Steps {
    /// What is being timed, for the log line.
    label: &'static str,
    /// Summed wall time per step, in first-seen order.
    sums: Vec<(&'static str, Duration)>,
    frames: u32,
    window_start: Instant,
    /// The end of the last step of the current frame.
    mark: Instant,
}

impl Steps {
    pub(super) fn new(label: &'static str) -> Self {
        let now = Instant::now();
        Self {
            label,
            sums: Vec::new(),
            frames: 0,
            window_start: now,
            mark: now,
        }
    }

    /// Start timing a frame.
    pub(super) fn start(&mut self) {
        self.mark = Instant::now();
    }

    /// End the step `name`, which began at the previous mark.
    pub(super) fn lap(&mut self, name: &'static str) {
        let now = Instant::now();
        let took = now - self.mark;
        self.mark = now;
        match self.sums.iter_mut().find(|(n, _)| *n == name) {
            Some((_, sum)) => *sum += took,
            None => self.sums.push((name, took)),
        }
    }

    /// The frame is done; logs the averages once a window is full.
    pub(super) fn finish(&mut self) {
        self.frames += 1;
        if self.window_start.elapsed() < WINDOW {
            return;
        }
        let n = self.frames;
        let line = self
            .sums
            .iter()
            .map(|(name, sum)| format!("{name} {:.2}", sum.as_secs_f64() * 1000.0 / n as f64))
            .collect::<Vec<_>>()
            .join(", ");
        debug!("{}: avg ms over {n} frames: {line}", self.label);
        self.sums.iter_mut().for_each(|(_, s)| *s = Duration::ZERO);
        self.frames = 0;
        self.window_start = Instant::now();
    }
}
