use std::collections::VecDeque;
use std::f64::consts::LN_2;

const RESOLUTION_BUF_CAP: usize = 1024;
const RESOLUTION_MIN_SAMPLES: usize = 10;
const RESOLUTION_WINDOW_US: i64 = 60_000_000;

struct ResolutionEstimator {
    buf: VecDeque<i64>,
    scratch: Vec<i64>,
}

impl ResolutionEstimator {
    fn new() -> Self {
        Self {
            buf: VecDeque::with_capacity(RESOLUTION_BUF_CAP),
            scratch: Vec::with_capacity(RESOLUTION_BUF_CAP),
        }
    }

    fn push_events(&mut self, ts: &[i64]) {
        debug_assert!(!ts.is_empty());
        debug_assert!(
            ts.windows(2).all(|w| w[1] > w[0]),
            "must be strictly increasing"
        );
        debug_assert!(
            self.buf.back().is_none_or(|&prev| ts[0] > prev),
            "first timestamp must be strictly after buf.back()"
        );

        let cutoff = ts.last().unwrap() - RESOLUTION_WINDOW_US;
        while self.buf.front().is_some_and(|&t| t < cutoff) {
            self.buf.pop_front();
        }

        let ts = &ts[ts.partition_point(|&t| t < cutoff)..];
        if ts.is_empty() {
            return;
        }

        let total = self.buf.len() + ts.len();
        if total > RESOLUTION_BUF_CAP {
            let excess = total - RESOLUTION_BUF_CAP;
            let drain_existing = excess.min(self.buf.len());
            self.buf.drain(..drain_existing);
            let ts = &ts[excess - drain_existing..];
            self.buf.extend(ts);
        } else {
            self.buf.extend(ts);
        }
    }

    fn num_deltas(&self) -> usize {
        self.buf.len().saturating_sub(1)
    }

    pub fn p10(&mut self) -> Option<f64> {
        if self.num_deltas() < RESOLUTION_MIN_SAMPLES {
            return None;
        }
        self.scratch.clear();
        let mut prev = self.buf[0];
        self.scratch.extend(self.buf.iter().skip(1).map(|&t| {
            let d = t - prev;
            prev = t;
            d
        }));
        let k = self.scratch.len() / 10;
        let (_, val, _) = self.scratch.select_nth_unstable(k);
        Some(*val as f64)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FpsDetail {
    pub fps_limit: f64,
    pub y: f64,
    pub expected: f64,
    pub p10: Option<f64>,
    pub events: u32,
}

pub struct DeviceEwm {
    attack_hl_us: f64,
    release_hl_us: f64,
    y: f64,
    pending_events: u32,
    last_tick_us: i64,
    last_event_us: i64,
    resolution: ResolutionEstimator,
}

impl DeviceEwm {
    pub fn new(attack_hl_us: f64, release_hl_us: f64) -> Self {
        Self {
            attack_hl_us,
            release_hl_us,
            y: 0.0,
            pending_events: 0,
            last_tick_us: i64::MIN,
            last_event_us: i64::MIN,
            resolution: ResolutionEstimator::new(),
        }
    }

    pub fn observe_batch(&mut self, timestamps_us: &[i64]) {
        let from = timestamps_us.iter().position(|&t| t > self.last_event_us);
        let skipped = from.map(|i| &timestamps_us[i..]).unwrap_or_default();
        self.last_event_us = skipped.last().copied().unwrap_or(self.last_event_us);
        self.pending_events += skipped.len() as u32;
        if !skipped.is_empty() {
            self.resolution.push_events(skipped);
        }
    }

    pub fn compute_fps(&mut self, now_us: i64, min_fps: f64, max_fps: f64) -> f64 {
        self.compute_fps_detailed(now_us, min_fps, max_fps)
            .fps_limit
    }

    pub fn compute_fps_detailed(&mut self, now_us: i64, min_fps: f64, max_fps: f64) -> FpsDetail {
        let events = self.pending_events;
        self.pending_events = 0;

        let p10 = self.resolution.p10();

        if self.last_event_us == i64::MIN {
            self.last_tick_us = now_us;
            return FpsDetail {
                fps_limit: min_fps,
                y: 0.0,
                expected: 0.0,
                p10,
                events,
            };
        }

        let prev_us = self.last_tick_us;
        let dt = if prev_us != i64::MIN {
            (now_us - prev_us).max(1) as f64
        } else {
            0.0
        };
        self.last_tick_us = now_us;

        let input_res = p10.unwrap_or(dt.max(1.0));
        let expected = dt / input_res;

        if dt > 0.0 {
            let transition = self.last_event_us + input_res as i64 * 2;

            if now_us <= transition {
                let alpha = 1.0 - (-LN_2 * dt / self.attack_hl_us).exp();
                self.y += alpha * (1.0 - self.y);
            } else if prev_us >= transition {
                let alpha = 1.0 - (-LN_2 * dt / self.release_hl_us).exp();
                self.y += alpha * (0.0 - self.y);
            } else {
                // Split the tick interval at the hold->idle transition so that
                // the we are independent of tick frequency.
                let attack_dt = transition - prev_us;
                let release_dt = now_us - transition;

                let a = 1.0 - (-LN_2 * attack_dt as f64 / self.attack_hl_us).exp();
                self.y += a * (1.0 - self.y);

                let r = 1.0 - (-LN_2 * release_dt as f64 / self.release_hl_us).exp();
                self.y += r * (0.0 - self.y);
            }
        }

        debug_assert!((0.0..=1.0).contains(&self.y), "y={} escaped [0,1]", self.y);
        let fps_limit = self.y * (max_fps - min_fps) + min_fps;

        FpsDetail {
            fps_limit,
            y: self.y,
            expected,
            p10,
            events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: f64 = 30.0;
    const MAX: f64 = 144.0;
    #[allow(unused)]
    const TICK: i64 = 1000; // 1ms ticks (1kHz)

    /// Simulate a tick loop: feed events that fall within each tick interval,
    /// then call compute_fps. Ticks are at TICK, 2*TICK, ...
    #[allow(unused)]
    fn simulate(
        ewm: &mut DeviceEwm,
        tick_us: i64,
        num_ticks: usize,
        events: &[i64],
        min_fps: f64,
        max_fps: f64,
    ) -> f64 {
        let mut last = min_fps;
        let mut ev_idx = 0;
        for i in 0..num_ticks {
            let t = (i as i64 + 1) * tick_us;
            let start = ev_idx;
            while ev_idx < events.len() && events[ev_idx] <= t {
                ev_idx += 1;
            }
            if start < ev_idx {
                ewm.observe_batch(&events[start..ev_idx]);
            }
            last = ewm.compute_fps(t, min_fps, max_fps);
        }
        last
    }

    #[test]
    #[cfg(not(miri))]
    fn steady_state_convergence_125hz() {
        // Key property: with p10-based normalization, a low polling-rate device
        // (125Hz = 8000µs deltas) ramps to the same limit as a 1kHz device.
        let mut ewm = DeviceEwm::new(10_000.0, 100_000.0);
        let events: Vec<i64> = (1..=10_000).map(|i| i * 8000).collect();
        let last = simulate(&mut ewm, TICK, 80_000, &events, MIN, MAX);
        assert!(
            (last - MAX).abs() < 0.5,
            "125Hz steady state: expected ~{MAX}, got {last}"
        );
    }

    #[test]
    #[cfg(not(miri))]
    fn half_life_accuracy() {
        // Use 100ms HL so the 2ms in-rhythm grace period is negligible.
        let hl = 100_000.0;
        let mut ewm = DeviceEwm::new(hl, hl);
        // 5s warm-up at 1kHz, then 100 idle ticks = 100ms = 1 half-life
        let events: Vec<i64> = (1..=5_000).map(|i| i * 1000).collect();
        let last = simulate(&mut ewm, TICK, 5_100, &events, MIN, MAX);
        let expected = 0.5 * (MAX - MIN) + MIN;
        assert!(
            (last - expected).abs() < 2.0,
            "after 1 half-life: expected ~{expected}, got {last}"
        );
    }

    #[test]
    fn leading_stale_timestamps_skipped() {
        let mut ewm = DeviceEwm::new(10_000.0, 100_000.0);
        ewm.observe_batch(&[1000, 2000, 3000]);
        ewm.observe_batch(&[1000, 2000, 4000, 5000]);
        let detail = ewm.compute_fps_detailed(6000, MIN, MAX);
        assert_eq!(
            detail.events, 5,
            "stale leading timestamps should be skipped"
        );
    }

    #[test]
    fn split_tick_matches_separate_ticks() {
        // A single tick spanning the hold->idle transition (split path) must
        // produce the same y as two ticks landing exactly on each side.
        let hl_a = 50_000.0;
        let hl_r = 100_000.0;

        // 20 events at 1kHz — enough for p10, short enough that y stays near 0.
        // p10 = 1000µs
        let events: Vec<i64> = (1..=20).map(|i| i * 1000).collect();

        let mut a = DeviceEwm::new(hl_a, hl_r);
        let mut b = DeviceEwm::new(hl_a, hl_r);

        a.observe_batch(&events);
        b.observe_batch(&events);
        a.compute_fps(20_000, MIN, MAX);
        b.compute_fps(20_000, MIN, MAX);

        // one coarse tick spanning the transition (split path)
        let da = a.compute_fps_detailed(30_000, MIN, MAX);

        // EWM b: pure attack up to transition, then pure release to same endpoint
        b.compute_fps(22_000, MIN, MAX);
        let db = b.compute_fps_detailed(30_000, MIN, MAX);

        assert!(
            (da.y - db.y).abs() < 1e-10,
            "split tick y={} != separate ticks y={}",
            da.y, db.y
        );
    }
}
