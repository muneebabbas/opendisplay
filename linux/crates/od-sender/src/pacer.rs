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
//!
//! Starts are also at least [`MIN_GAP`] of an interval apart. The burst
//! alone would let two starts happen one 120 Hz frame (8.3 ms) apart and the
//! next one 25 ms later, with capture jitter deciding which frames go; with
//! the gap, content faster than the cap is decimated evenly (every other
//! frame of 120 Hz content), while content at the cap that arrives a little
//! early or late still starts at once.

use std::time::{Duration, Instant};

/// Tokens kept at most: a frame after an idle spell, and the next one,
/// start without waiting.
pub const BURST: f64 = 1.5;
/// A frame whose token is at most this far off waits for it rather than
/// for a fresher frame.
pub const SLACK: Duration = Duration::from_millis(2);
/// The shortest gap between encode starts, as a fraction of an interval.
pub const MIN_GAP: f64 = 0.75;

pub struct Pacer {
    fps: f64,
    interval: Duration,
    min_gap: Duration,
    tokens: f64,
    refilled: Instant,
    last_start: Option<Instant>,
}

impl Pacer {
    pub fn new(max_fps: f32, now: Instant) -> Self {
        let fps = f64::from(max_fps.max(1.0));
        Self {
            fps,
            interval: Duration::from_secs_f64(1.0 / fps),
            min_gap: Duration::from_secs_f64(MIN_GAP / fps),
            tokens: BURST,
            refilled: now,
            last_start: None,
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
        self.tokens >= 1.0 - 1e-9 && now >= self.gap_until(now)
    }

    /// The earliest the next start may be, by [`MIN_GAP`].
    fn gap_until(&self, now: Instant) -> Instant {
        self.last_start.map_or(now, |t| (t + self.min_gap).max(now))
    }

    /// An encode starts now.
    pub fn start(&mut self, now: Instant) {
        self.refill(now);
        self.tokens -= 1.0;
        self.last_start = Some(now);
    }

    /// When to look again at a frame that arrived at `arrived` and is not
    /// [`Self::ready`]: when its token and [`MIN_GAP`] allow it if that is
    /// within [`SLACK`], else one interval after it arrived (unless a newer
    /// frame replaces it first), by when both allow it.
    pub fn wake_at(&mut self, now: Instant, arrived: Instant) -> Instant {
        self.refill(now);
        // Rounded up to whole microseconds: a wake-up a few nanoseconds
        // short of the token would find it not yet there and look again at
        // once.
        let need_us = ((1.0 - self.tokens) / self.fps * 1e6).max(0.0).ceil() as u64;
        let need = Duration::from_micros(need_us);
        let due = (now + need).max(self.gap_until(now));
        if due.saturating_duration_since(now) <= SLACK {
            due
        } else {
            due.max(arrived + self.interval)
        }
    }
}

/// Development: the distribution of capture arrivals and encode starts,
/// summarised every [`PaceLog::EVERY`] starts for a debug log. Gaps longer
/// than four intervals (a screen that went static) are left out.
pub struct PaceLog {
    interval: Duration,
    last_arrival: Option<Instant>,
    last_start: Option<Instant>,
    arrivals: Vec<f64>,
    starts: Vec<f64>,
}

impl PaceLog {
    pub const EVERY: usize = 300;

    pub fn new(max_fps: f32) -> Self {
        Self {
            interval: Duration::from_secs_f64(1.0 / f64::from(max_fps.max(1.0))),
            last_arrival: None,
            last_start: None,
            arrivals: Vec::with_capacity(Self::EVERY * 3),
            starts: Vec::with_capacity(Self::EVERY),
        }
    }

    fn push(v: &mut Vec<f64>, last: &mut Option<Instant>, now: Instant, idle: Duration) {
        if let Some(prev) = last.replace(now) {
            let gap = now.saturating_duration_since(prev);
            if gap <= idle {
                v.push(gap.as_secs_f64() * 1e3);
            }
        }
    }

    pub fn arrived(&mut self, now: Instant) {
        Self::push(
            &mut self.arrivals,
            &mut self.last_arrival,
            now,
            self.interval * 4,
        );
    }

    /// An encode started; returns a summary every [`Self::EVERY`] starts.
    pub fn started(&mut self, now: Instant) -> Option<String> {
        Self::push(
            &mut self.starts,
            &mut self.last_start,
            now,
            self.interval * 4,
        );
        if self.starts.len() < Self::EVERY {
            return None;
        }
        let iv = self.interval.as_secs_f64() * 1e3;
        let summary = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            let q = |x: f64| {
                v.get(((v.len().max(1) - 1) as f64 * x).round() as usize)
                    .copied()
                    .unwrap_or(0.0)
            };
            format!(
                "n {} p1 {:.1} p50 {:.1} p99 {:.1} ms",
                v.len(),
                q(0.01),
                q(0.5),
                q(0.99)
            )
        };
        let short = self.starts.iter().filter(|g| **g < iv * 0.75).count();
        let long = self.starts.iter().filter(|g| **g > iv * 1.25).count();
        let line = format!(
            "pace: starts {} ({short} under 3/4, {long} over 5/4 interval) | arrivals {}",
            summary(&mut self.starts),
            summary(&mut self.arrivals),
        );
        self.starts.clear();
        self.arrivals.clear();
        Some(line)
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

    /// Start-to-start gaps, sorted.
    fn gaps(starts: &[(Duration, Duration)]) -> Vec<Duration> {
        let mut g: Vec<_> = starts.windows(2).map(|w| w[1].1 - w[0].1).collect();
        g.sort();
        g
    }

    fn pct(v: &[Duration], q: f64) -> Duration {
        v[((v.len() - 1) as f64 * q).round() as usize]
    }

    /// Deterministic jitter in [-amp, amp] µs (an LCG on the frame index).
    fn noise(amp: i64) -> impl Fn(u64) -> i64 {
        move |i| {
            let x = i
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407)
                >> 33;
            (x % (2 * amp as u64 + 1)) as i64 - amp
        }
    }

    fn min_gap() -> Duration {
        Duration::from_secs_f64(MIN_GAP / f64::from(CAP))
    }

    #[test]
    fn starts_are_never_closer_than_the_min_gap() {
        let cases: [(&str, Vec<Duration>); 5] = [
            ("120 Hz", every(8_333, 10, |_| 0)),
            ("120 Hz ±1 ms", every(8_333, 10, noise(1_000))),
            ("90 Hz", every(11_111, 10, |_| 0)),
            ("144 Hz", every(6_944, 10, noise(500))),
            ("60 Hz ±3 ms", every(16_667, 10, noise(3_000))),
        ];
        for (what, arrivals) in cases {
            let (starts, _) = run(&arrivals);
            assert_capped(&starts);
            let g = gaps(&starts);
            // A start may land up to a microsecond early of the gap: wake-ups
            // are rounded to whole microseconds.
            assert!(
                g[0] + Duration::from_micros(1) >= min_gap(),
                "{what}: shortest gap {:?}",
                g[0]
            );
            let fps = starts.len() as f64 / 10.0;
            assert!((57.0..=60.5).contains(&fps), "{what}: {fps} fps");
        }
    }

    #[test]
    fn content_at_twice_the_cap_is_sent_evenly() {
        // Every other frame: gaps stay one interval (16.7 ms), never 8.3
        // then 25, with up to ±1 ms of capture jitter.
        for (what, jitter) in [("steady", 0u64), ("±1 ms", 1_000)] {
            let (starts, _) = run(&every(8_333, 10, noise(jitter as i64)));
            let g = gaps(&starts);
            let (p1, p99) = (pct(&g, 0.01), pct(&g, 0.99));
            let lo = Duration::from_micros(16_667 - 2 * jitter - 500);
            let hi = Duration::from_micros(16_667 + 2 * jitter + 500);
            assert!(p1 >= lo && p99 <= hi, "{what}: gaps p1 {p1:?} p99 {p99:?}");
        }
        // More jitter than that shows through (which frame is nearest the
        // slot is a coin toss), but gaps still never drop to one 120 Hz
        // frame, and few run long. Without MIN_GAP: 87 of 600 outside
        // [12.5, 20.8] ms; with it, 48.
        let (starts, _) = run(&every(8_333, 10, noise(2_000)));
        let g = gaps(&starts);
        let long = g
            .iter()
            .filter(|x| **x > Duration::from_micros(20_800))
            .count();
        assert!(
            long * 10 <= g.len(),
            "±2 ms: {long} of {} gaps over 20.8 ms",
            g.len()
        );
    }

    #[test]
    fn jittered_content_at_the_cap_keeps_its_frames_and_does_not_wait() {
        let arrivals = every(16_667, 10, noise(3_000));
        let (starts, _) = run(&arrivals);
        assert!(
            starts.len() * 100 >= arrivals.len() * 97,
            "{} of {} sent",
            starts.len(),
            arrivals.len()
        );
        let w = waits(&starts);
        assert_eq!(w[w.len() / 2], Duration::ZERO, "p50 wait");
        assert!(pct(&w, 0.95) <= SLACK, "p95 wait {:?}", pct(&w, 0.95));
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
