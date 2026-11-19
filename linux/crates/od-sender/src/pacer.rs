//! When the encoder may start the next frame under the stream's frame-rate
//! cap, with the time passed in so it can be tested without threads.
//!
//! A token bucket: each encode spends a token, and up to [`BURST`] are
//! kept, so content at the cap that arrives early or late starts the moment
//! it arrives. That is the normal case: the virtual output refreshes at the
//! stream's rate, so content arrives at the cap or slower. Tokens then
//! accrue [`HEADROOM`] faster than the cap: a compositor clock a hair fast
//! would otherwise drain a bucket refilled at exactly the cap for good, and
//! every frame would wait for its token. Content at the cap cannot be sent
//! faster than it arrives, so the headroom is not a higher rate.
//!
//! Content faster than the cap (a 120 Hz output under a 60 fps stream) is
//! told apart by the measured interval between arrivals, and paced
//! differently: tokens accrue at exactly the cap, starts are at least [`MIN_GAP`] of an interval apart, so
//! two never go one 120 Hz frame (8.3 ms) apart with the next 25 ms later;
//! and a frame whose start is more than [`SLACK`] away is held, so that a
//! newer frame replaces it, and the newest is encoded at the latest one
//! interval after it arrived. Content at the cap gets neither: there the
//! next frame is a whole interval away, so a held frame would be dropped or
//! sent an interval late.

use std::time::{Duration, Instant};

/// Tokens kept at most: a frame after an idle spell, and the next one,
/// start without waiting.
pub const BURST: f64 = 1.5;
/// Faster than the cap: a frame whose start is at most this fraction of an
/// interval away waits for it rather than for a fresher frame.
pub const SLACK: f64 = 0.12;
/// Faster than the cap: the shortest gap between encode starts, as a
/// fraction of an interval.
pub const MIN_GAP: f64 = 0.75;
/// At the cap: how much faster than the cap tokens accrue.
pub const HEADROOM: f64 = 1.05;
/// The source counts as faster than the cap once its mean arrival interval
/// drops under this fraction of the cap's interval, and as at the cap again
/// once it rises over [`FAST_LEAVE`]. Jitter does not move the mean of
/// content at the cap, and over [`MEAN_WINDOW`] intervals it barely moves
/// the running mean (±1 ms at 60 Hz: about 1.5%).
const FAST_ENTER: f64 = 0.93;
const FAST_LEAVE: f64 = 0.97;
/// Arrival intervals measured before the source can count as faster than
/// the cap; also the window of the running mean (a plain mean until then).
const MEAN_WINDOW: u32 = 32;

pub struct Pacer {
    fps: f64,
    interval: Duration,
    min_gap: Duration,
    slack: Duration,
    tokens: f64,
    refilled: Instant,
    last_start: Option<Instant>,
    last_arrival: Option<Instant>,
    /// Running mean of the arrival interval, in seconds, over `samples`
    /// intervals (at most [`MEAN_WINDOW`] count); reset by idle.
    source_interval: f64,
    samples: u32,
    fast: bool,
}

impl Pacer {
    pub fn new(max_fps: f32, now: Instant) -> Self {
        let fps = f64::from(max_fps.max(1.0));
        Self {
            fps,
            interval: Duration::from_secs_f64(1.0 / fps),
            min_gap: Duration::from_secs_f64(MIN_GAP / fps),
            slack: Duration::from_secs_f64(SLACK / fps),
            tokens: BURST,
            refilled: now,
            last_start: None,
            last_arrival: None,
            source_interval: 0.0,
            samples: 0,
            fast: false,
        }
    }

    /// A frame was captured at `at` (call once per new frame, not for a
    /// replay of an old one).
    pub fn arrived(&mut self, at: Instant) {
        // Tokens so far accrue at the rate of the mode they accrued in.
        self.refill(at);
        let Some(prev) = self.last_arrival.replace(at) else {
            return;
        };
        let gap = at.saturating_duration_since(prev).as_secs_f64();
        let iv = self.interval.as_secs_f64();
        if gap > 4.0 * iv {
            // After idle the source is unknown: start again at the cap.
            self.samples = 0;
            self.fast = false;
            return;
        }
        self.samples = (self.samples + 1).min(MEAN_WINDOW);
        let mean = self.source_interval + (gap - self.source_interval) / f64::from(self.samples);
        self.source_interval = mean;
        if self.fast && mean > FAST_LEAVE * iv {
            self.fast = false;
        } else if !self.fast && self.samples >= MEAN_WINDOW && mean < FAST_ENTER * iv {
            self.fast = true;
        }
    }

    /// Whether content is arriving faster than the cap.
    pub fn faster_than_cap(&self) -> bool {
        self.fast
    }

    /// Tokens per second: the cap for content faster than it, else a little
    /// more ([`HEADROOM`]).
    fn rate(&self) -> f64 {
        if self.fast {
            self.fps
        } else {
            self.fps * HEADROOM
        }
    }

    fn refill(&mut self, now: Instant) {
        let gained = now.saturating_duration_since(self.refilled).as_secs_f64() * self.rate();
        self.tokens = (self.tokens + gained).min(BURST);
        self.refilled = self.refilled.max(now);
    }

    /// Whether a frame may start now. Spend the token with [`Self::start`].
    pub fn ready(&mut self, now: Instant) -> bool {
        self.refill(now);
        // Float rounding must not leave a frame a hair short of its token.
        self.tokens >= 1.0 - 1e-9 && now >= self.gap_until(now)
    }

    /// The earliest the next start may be by [`MIN_GAP`], which applies
    /// only to content faster than the cap.
    fn gap_until(&self, now: Instant) -> Instant {
        match self.last_start {
            Some(t) if self.fast => (t + self.min_gap).max(now),
            _ => now,
        }
    }

    /// An encode starts now.
    pub fn start(&mut self, now: Instant) {
        self.refill(now);
        self.tokens -= 1.0;
        self.last_start = Some(now);
    }

    /// When to look again at a frame that arrived at `arrived` and is not
    /// [`Self::ready`]: when it may start; for content faster than the cap,
    /// only if that is within [`SLACK`], else one interval after it arrived
    /// (unless a newer frame replaces it first), by when it may.
    pub fn wake_at(&mut self, now: Instant, arrived: Instant) -> Instant {
        self.refill(now);
        // Rounded up to whole microseconds: a wake-up a few nanoseconds
        // short of the token would find it not yet there and look again at
        // once.
        let need_us = ((1.0 - self.tokens) / self.rate() * 1e6).max(0.0).ceil() as u64;
        let due = (now + Duration::from_micros(need_us)).max(self.gap_until(now));
        if !self.fast || due.saturating_duration_since(now) <= self.slack {
            due
        } else {
            due.max(arrived + self.interval)
        }
    }
}

/// The distribution of capture arrivals and encode starts, summarised
/// every [`PaceLog::EVERY`] starts (about 5 s at 60 fps) for the debug log. Gaps longer
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

    /// Frames arriving at `arrivals` (offsets from t0, sorted), run through
    /// the encoder loop's use of the pacer at `cap` with a latest-wins slot.
    /// Returns the encode starts as (arrival, start) offsets, and how many
    /// times the loop woke.
    fn run(cap: f32, arrivals: &[Duration]) -> (Vec<(Duration, Duration)>, usize) {
        let t0 = Instant::now();
        let mut p = Pacer::new(cap, t0);
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
                p.arrived(t0 + now);
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

    /// splitmix64: independent pseudo-random bits for each `x`.
    fn mix(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }

    /// Uniform jitter in [-amp, amp] µs, independent per frame, per `seed`.
    fn uniform(seed: u64, amp: u64) -> impl Fn(u64) -> i64 {
        move |i| (mix(seed << 32 ^ i) % (2 * amp + 1)) as i64 - amp as i64
    }

    /// Frames every `period_us` for `secs`, each moved by `jitter_us(i)`,
    /// in arrival order.
    fn every(period_us: u64, secs: u64, jitter_us: impl Fn(u64) -> i64) -> Vec<Duration> {
        let mut v: Vec<_> = (0..secs * 1_000_000 / period_us)
            .map(|i| {
                let t = (i * period_us) as i64 + 10_000 + jitter_us(i);
                Duration::from_micros(t.max(0) as u64)
            })
            .collect();
        v.sort();
        v
    }

    fn interval(cap: f32) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(cap))
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

    /// Starts in any one-second window never exceed the cap with its
    /// headroom plus the burst (content at the cap is sent as it comes).
    fn assert_capped(cap: f32, starts: &[(Duration, Duration)]) {
        let s: Vec<_> = starts.iter().map(|(_, s)| *s).collect();
        for (i, &t) in s.iter().enumerate() {
            let n = s[i..]
                .iter()
                .take_while(|&&u| u < t + Duration::from_secs(1))
                .count();
            assert!(
                n as f64 <= f64::from(cap) * HEADROOM + BURST,
                "{n} starts in 1 s from {t:?}"
            );
        }
    }

    #[test]
    fn content_at_the_cap_keeps_every_frame_and_does_not_wait() {
        // The normal case: the output refreshes at the stream's rate. With
        // capture jitter of up to ±4 ms (±8 at 120 fps: half an interval
        // either way), every frame is sent, at once.
        for cap in [60.0, 120.0] {
            let period = (1e6 / f64::from(cap)).round() as u64;
            for seed in 0..20 {
                let arrivals = every(period, 10, uniform(seed, period / 4));
                let (starts, _) = run(cap, &arrivals);
                assert_capped(cap, &starts);
                assert_eq!(
                    starts.len(),
                    arrivals.len(),
                    "{cap} fps seed {seed}: frames dropped"
                );
                let w = waits(&starts);
                let max = *w.last().unwrap();
                assert!(
                    max <= interval(cap) / 10,
                    "{cap} fps seed {seed}: a frame waited {max:?}"
                );
            }
        }
    }

    #[test]
    fn content_on_a_fast_clock_does_not_wait() {
        // A compositor clock a hair fast (60.06 and 60.24 Hz): a bucket
        // refilled at exactly the cap drains and every frame waits for its
        // token, up to an interval. Without jitter, the worst case.
        for period in [16_650, 16_600] {
            let arrivals = every(period, 10, |_| 0);
            let (starts, _) = run(60.0, &arrivals);
            assert_eq!(starts.len(), arrivals.len(), "{period} µs: frames dropped");
            let w = waits(&starts);
            assert_eq!(
                *w.last().unwrap(),
                Duration::ZERO,
                "{period} µs: a frame waited"
            );
        }
    }

    #[test]
    fn content_just_over_the_cap_is_not_sent_much_faster() {
        // 63 Hz content, still paced as at the cap: at most the headroom.
        let arrivals = every(15_873, 10, |_| 0);
        let (starts, _) = run(60.0, &arrivals);
        let fps = starts.len() as f64 / 10.0;
        assert!(fps <= 60.0 * HEADROOM + 0.5, "{fps} fps");
    }

    #[test]
    fn a_late_frame_does_not_cost_the_next_one() {
        // One frame in sixty 7 ms late, the rest on time: the late one and
        // the on-time one after it both go, at once.
        let arrivals = every(16_667, 10, |i| if i % 60 == 30 { 7_000 } else { 0 });
        let (starts, _) = run(60.0, &arrivals);
        assert_eq!(starts.len(), arrivals.len(), "frames dropped");
        assert_eq!(*waits(&starts).last().unwrap(), Duration::ZERO);
    }

    #[test]
    fn faster_content_is_capped_and_sends_fresh_frames() {
        for (cap, hz) in [(60.0, 90), (60.0, 120), (60.0, 144), (120.0, 240)] {
            let (starts, _) = run(cap, &every(1_000_000 / hz, 10, |_| 0));
            assert_capped(cap, &starts);
            // Once the source is measured (well within the first second),
            // exactly the cap: no headroom for content faster than it.
            let steady = starts
                .iter()
                .filter(|(_, s)| *s >= Duration::from_secs(1))
                .count();
            let fps = steady as f64 / (10.01 - 1.0);
            let c = f64::from(cap);
            assert!(
                (c * 0.97..=c + 0.5).contains(&fps),
                "{hz} Hz content at {cap}: {fps} fps"
            );
            let w = waits(&starts);
            let p95 = pct(&w, 0.95);
            assert!(
                p95 <= interval(cap).mul_f64(SLACK),
                "{hz} Hz content at {cap}: p95 wait {p95:?}"
            );
        }
    }

    #[test]
    fn faster_content_never_starts_closer_than_the_min_gap() {
        for (cap, hz, jitter) in [
            (60.0, 120, 0),
            (60.0, 120, 1_000),
            (60.0, 90, 0),
            (60.0, 144, 500),
            (120.0, 240, 500),
        ] {
            for seed in 0..10 {
                let (starts, _) = run(cap, &every(1_000_000 / hz, 10, uniform(seed, jitter)));
                // The first frames come before the source is measured.
                let g = gaps(&starts[40..]);
                // A start may land up to a microsecond early of the gap:
                // wake-ups are rounded to whole microseconds.
                let min_gap = interval(cap).mul_f64(MIN_GAP);
                assert!(
                    g[0] + Duration::from_micros(1) >= min_gap,
                    "{hz} Hz ±{jitter} µs at {cap}, seed {seed}: shortest gap {:?}",
                    g[0]
                );
            }
        }
    }

    #[test]
    fn content_at_twice_the_cap_is_sent_evenly() {
        // Every other frame: gaps stay one interval, never one source frame
        // then three, with up to ±1 ms of capture jitter (±0.5 ms at 120).
        for (cap, jitter) in [(60.0, 0), (60.0, 1_000), (120.0, 500)] {
            let period = (1e6 / f64::from(cap) / 2.0).round() as u64;
            for seed in 0..10 {
                let (starts, _) = run(cap, &every(period, 10, uniform(seed, jitter)));
                let g = gaps(&starts[40..]);
                let (p1, p99) = (pct(&g, 0.01), pct(&g, 0.99));
                let iv = interval(cap);
                let spread = Duration::from_micros(2 * jitter + 500);
                assert!(
                    p1 + spread >= iv && p99 <= iv + spread,
                    "{cap} fps ±{jitter} µs seed {seed}: gaps p1 {p1:?} p99 {p99:?}"
                );
            }
        }
    }

    #[test]
    fn the_source_rate_is_told_apart() {
        let t0 = Instant::now();
        let mut p = Pacer::new(60.0, t0);
        let ms = Duration::from_millis;
        let mut t = t0;
        for _ in 0..40 {
            t += Duration::from_micros(16_667);
            p.arrived(t);
        }
        assert!(!p.faster_than_cap(), "60 Hz content at a 60 fps cap");
        for _ in 0..40 {
            t += Duration::from_micros(8_333);
            p.arrived(t);
        }
        assert!(p.faster_than_cap(), "120 Hz content at a 60 fps cap");
        // An idle spell forgets it.
        t += ms(500);
        p.arrived(t);
        assert!(!p.faster_than_cap(), "after idle");
    }

    #[test]
    fn a_lone_frame_inside_the_cap_is_sent_within_an_interval() {
        // Three frames 1 ms apart, then nothing: the last must still go,
        // within one interval of its arrival.
        let ms = Duration::from_millis;
        let (starts, _) = run(60.0, &[ms(1), ms(2), ms(3)]);
        let (a, s) = *starts.last().unwrap();
        assert_eq!(a, ms(3));
        assert!(s - a <= interval(60.0) + Duration::from_micros(1));
    }

    #[test]
    fn after_idle_the_burst_lets_the_next_frame_go_at_once() {
        // After a long idle spell a frame starts, and the next one 9 ms
        // later (over half an interval) starts at once too: the half token
        // of BURST. With BURST 1 it would wait 7.7 ms. Then the steady cap.
        let ms = Duration::from_millis;
        let mut arrivals = vec![ms(5_000), ms(5_009)];
        arrivals.extend((1..60).map(|i| ms(5_009) + Duration::from_micros(i * 16_667)));
        let (starts, _) = run(60.0, &arrivals);
        assert_eq!(starts.len(), arrivals.len());
        assert_eq!(waits(&starts[..2]), vec![Duration::ZERO; 2], "the burst");
        assert_capped(60.0, &starts);
    }

    #[test]
    fn wakeups_are_bounded() {
        let arrivals = every(1_000_000 / 120, 5, |_| 0);
        let (_, wakes) = run(60.0, &arrivals);
        assert!(
            wakes <= arrivals.len() * 2,
            "{wakes} wake-ups for {} frames",
            arrivals.len()
        );
    }
}
