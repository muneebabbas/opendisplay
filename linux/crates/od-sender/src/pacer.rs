//! When the encoder may start the next frame under the stream's frame-rate
//! cap, with the time passed in so it can be tested without threads.
//!
//! A token bucket refilled at exactly the cap: each encode spends a token,
//! and up to [`BURST`] are kept, so content at the cap that arrives with a
//! little jitter starts the moment it arrives. A frame that would have to
//! wait long for its token is held instead: a newer frame replaces it, and
//! the newest is encoded once its token comes, at the latest one interval
//! after it arrived. So content faster than the cap (a 120 Hz desktop under
//! a 60 fps HEVC cap) sends the freshest frame rather than an old one that
//! waited, and the last frame of a screen that goes static is still sent.
//! The long-run rate never exceeds the cap.

use std::time::{Duration, Instant};

/// Tokens kept at most: a frame after an idle spell, and the next one,
/// start without waiting.
pub const BURST: f64 = 1.5;
/// A frame whose token is at most this far off waits for it rather than
/// for a fresher frame.
pub const SLACK: Duration = Duration::from_millis(2);

pub struct Pacer {
    fps: f64,
    interval: Duration,
    tokens: f64,
    refilled: Instant,
}

impl Pacer {
    pub fn new(max_fps: f32, now: Instant) -> Self {
        let fps = f64::from(max_fps.max(1.0));
        Self {
            fps,
            interval: Duration::from_secs_f64(1.0 / fps),
            tokens: BURST,
            refilled: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let gained = now.saturating_duration_since(self.refilled).as_secs_f64() * self.fps;
        self.tokens = (self.tokens + gained).min(BURST);
        self.refilled = self.refilled.max(now);
    }

    /// Whether a frame may start now. Spend the token with [`Self::start`].
    pub fn ready(&mut self, now: Instant) -> bool {
        self.refill(now);
        // Float rounding must not leave a frame a hair short of its token.
        self.tokens >= 1.0 - 1e-9
    }

    /// An encode starts now.
    pub fn start(&mut self, now: Instant) {
        self.refill(now);
        self.tokens -= 1.0;
    }

    /// When to look again at a frame that arrived at `arrived` and is not
    /// [`Self::ready`]: when its token comes if that is within [`SLACK`],
    /// else one interval after it arrived (unless a newer frame replaces it
    /// first), by when its token has come.
    pub fn wake_at(&mut self, now: Instant, arrived: Instant) -> Instant {
        self.refill(now);
        // Rounded up to whole microseconds: a wake-up a few nanoseconds
        // short of the token would find it not yet there and look again at
        // once.
        let need_us = ((1.0 - self.tokens) / self.fps * 1e6).max(0.0).ceil() as u64;
        let need = Duration::from_micros(need_us);
        let token_at = now + need;
        if need <= SLACK {
            token_at
        } else {
            token_at.max(arrived + self.interval)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: f32 = 60.0;

    /// Frames arriving at `arrivals` (offsets from t0), run through the
    /// encoder loop's use of the pacer with a latest-wins slot. Returns the
    /// encode starts as (arrival, start) offsets, and how many times the
    /// loop woke.
    fn run(arrivals: &[Duration]) -> (Vec<(Duration, Duration)>, usize) {
        let t0 = Instant::now();
        let mut p = Pacer::new(CAP, t0);
        let mut starts = Vec::new();
        let mut pending: Option<Duration> = None;
        let mut next = 0;
        let mut now = Duration::ZERO;
        let mut wakes = 0;
        let end = *arrivals.last().unwrap() + Duration::from_secs(1);
        while now < end {
            wakes += 1;
            assert!(wakes < arrivals.len() * 4 + 10, "the loop spins");
            // Sleep until the pacer's wake-up or the next arrival, as
            // recv_timeout does.
            let wake = pending.map(|a| p.wake_at(t0 + now, t0 + a) - t0);
            let arrival = arrivals.get(next).copied();
            now = match (wake, arrival) {
                (Some(w), Some(a)) => w.min(a),
                (Some(w), None) => w,
                (None, Some(a)) => a,
                (None, None) => break,
            }
            .max(now);
            if arrival == Some(now) {
                pending = Some(now);
                next += 1;
            }
            if let Some(a) = pending {
                if p.ready(t0 + now) {
                    p.start(t0 + now);
                    starts.push((a, now));
                    pending = None;
                }
            }
        }
        assert!(pending.is_none(), "a frame was held forever");
        (starts, wakes)
    }

    fn every(period_us: u64, secs: u64, jitter_us: impl Fn(u64) -> i64) -> Vec<Duration> {
        (0..secs * 1_000_000 / period_us)
            .map(|i| {
                let t = (i * period_us) as i64 + 1_000 + jitter_us(i);
                Duration::from_micros(t.max(0) as u64)
            })
            .collect()
    }

    fn waits(starts: &[(Duration, Duration)]) -> Vec<Duration> {
        let mut w: Vec<_> = starts.iter().map(|(a, s)| *s - *a).collect();
        w.sort();
        w
    }

    /// Starts in any one-second window never exceed the cap plus the burst.
    fn assert_capped(starts: &[(Duration, Duration)]) {
        let s: Vec<_> = starts.iter().map(|(_, s)| *s).collect();
        for (i, &t) in s.iter().enumerate() {
            let n = s[i..]
                .iter()
                .take_while(|&&u| u < t + Duration::from_secs(1))
                .count();
            assert!(
                n as f64 <= f64::from(CAP) + BURST,
                "{n} starts in 1 s from {t:?}"
            );
        }
    }

    #[test]
    fn faster_content_is_capped_and_sends_fresh_frames() {
        for hz in [90, 120, 144] {
            let (starts, _) = run(&every(1_000_000 / hz, 10, |_| 0));
            assert_capped(&starts);
            let fps = starts.len() as f64 / 10.0;
            assert!((58.0..=60.5).contains(&fps), "{hz} Hz content: {fps} fps");
            let w = waits(&starts);
            let p95 = w[w.len() * 95 / 100];
            assert!(p95 <= SLACK, "{hz} Hz content: p95 wait {p95:?}");
        }
    }

    #[test]
    fn content_at_the_cap_does_not_wait() {
        // Exactly 60 Hz with ±1 ms of jitter, and a clock a little fast.
        let jittered = every(16_667, 10, |i| if i % 2 == 0 { 1_000 } else { -1_000 });
        let fast = every(16_650, 10, |_| 0);
        for (what, arrivals) in [("jittered", jittered), ("fast clock", fast)] {
            let (starts, _) = run(&arrivals);
            assert_capped(&starts);
            let w = waits(&starts);
            assert!(
                w[w.len() / 2] == Duration::ZERO,
                "{what}: p50 wait {:?}",
                w[w.len() / 2]
            );
            assert!(
                *w.last().unwrap() <= SLACK * 2,
                "{what}: max wait {:?}",
                w.last()
            );
            assert!(
                starts.len() + 2 >= arrivals.len(),
                "{what}: {} of {} sent",
                starts.len(),
                arrivals.len()
            );
        }
    }

    #[test]
    fn a_lone_frame_inside_the_cap_is_sent_within_an_interval() {
        // Two frames 1 ms apart, then nothing: the second must still go,
        // within one interval of its arrival.
        let (starts, _) = run(&[
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(3),
        ]);
        let (a, s) = *starts.last().unwrap();
        assert_eq!(a, Duration::from_millis(3));
        assert!(s - a <= Duration::from_secs_f64(1.0 / f64::from(CAP)) + Duration::from_micros(1));
    }

    #[test]
    fn after_idle_only_the_burst_starts_back_to_back() {
        // Ten frames 100 µs apart after a long idle spell.
        let arrivals: Vec<_> = (0..10)
            .map(|i| Duration::from_secs(5) + Duration::from_micros(i * 100))
            .collect();
        let (starts, _) = run(&arrivals);
        let quick = starts
            .windows(2)
            .take_while(|w| w[1].1 - w[0].1 < Duration::from_millis(5))
            .count()
            + 1;
        assert!(quick as f64 <= BURST.ceil(), "{quick} starts back to back");
    }

    #[test]
    fn wakeups_are_bounded() {
        let arrivals = every(1_000_000 / 120, 5, |_| 0);
        let (_, wakes) = run(&arrivals);
        assert!(
            wakes <= arrivals.len() * 2,
            "{wakes} wake-ups for {} frames",
            arrivals.len()
        );
    }
}
