//! Rate and time-remaining estimates for the progress displays.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The rate is measured over (at least) this much recent history, so the
/// estimate follows the disk slowing down or speeding up without jumping
/// around on every sample.
const WINDOW: Duration = Duration::from_secs(12);
const MIN_GAP: Duration = Duration::from_millis(200);
/// No estimate until there is this much history to base it on.
const WARM_UP: Duration = Duration::from_millis(600);

pub struct Eta {
    started: Instant,
    samples: VecDeque<(Instant, u64)>,
}

impl Eta {
    pub fn new() -> Self {
        Self::starting_at(Instant::now())
    }

    pub fn starting_at(t: Instant) -> Self {
        Eta { started: t, samples: VecDeque::from([(t, 0)]) }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Record how much work is done now.
    pub fn update(&mut self, done: u64) {
        self.update_at(Instant::now(), done)
    }

    pub fn update_at(&mut self, now: Instant, done: u64) {
        if let Some(&(last, _)) = self.samples.back() {
            if self.samples.len() > 1 && now.saturating_duration_since(last) < MIN_GAP {
                return;
            }
        }
        self.samples.push_back((now, done));
        // Keep one sample that is at least WINDOW old as the baseline.
        while self.samples.len() > 2 && now.saturating_duration_since(self.samples[1].0) >= WINDOW {
            self.samples.pop_front();
        }
    }

    fn span(&self) -> (f64, u64, u64) {
        let (t0, d0) = *self.samples.front().expect("never empty");
        let (t1, d1) = *self.samples.back().expect("never empty");
        (t1.saturating_duration_since(t0).as_secs_f64(), d0, d1)
    }

    /// Units of work per second over the recent window.
    pub fn rate(&self) -> f64 {
        let (secs, d0, d1) = self.span();
        if secs <= 0.0 {
            0.0
        } else {
            d1.saturating_sub(d0) as f64 / secs
        }
    }

    /// Estimated time until `total` is reached; `None` while unknown.
    pub fn remaining(&self, total: u64) -> Option<Duration> {
        let (secs, _, done) = self.span();
        if done >= total {
            return Some(Duration::ZERO);
        }
        let rate = self.rate();
        if secs < WARM_UP.as_secs_f64() || rate <= 0.0 {
            return None;
        }
        Some(Duration::from_secs_f64((total - done) as f64 / rate))
    }

    /// "ETA 3m 05s", or "ETA --" while there is nothing to base it on.
    pub fn text(&self, total: u64) -> String {
        match self.remaining(total) {
            Some(d) => human_duration(d),
            None => "--".to_string(),
        }
    }
}

/// 42s / 3m 05s / 2h 07m
pub fn human_duration(d: Duration) -> String {
    // Round up, so the display never claims "0s" while work remains.
    let secs = d.as_secs() + u64::from(d.subsec_nanos() > 0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, secs % 3600 / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(human_duration(Duration::ZERO), "0s");
        assert_eq!(human_duration(Duration::from_millis(100)), "1s");
        assert_eq!(human_duration(Duration::from_secs(59)), "59s");
        assert_eq!(human_duration(Duration::from_secs(185)), "3m 05s");
        assert_eq!(human_duration(Duration::from_secs(7620)), "2h 07m");
    }

    #[test]
    fn steady_rate() {
        let t = Instant::now();
        let mut eta = Eta::starting_at(t);
        assert_eq!(eta.remaining(1000), None);
        for s in 1..=5u64 {
            eta.update_at(t + Duration::from_secs(s), s * 100);
        }
        assert!((eta.rate() - 100.0).abs() < 1e-6);
        assert_eq!(eta.remaining(1000), Some(Duration::from_secs(5)));
        assert_eq!(eta.text(1000), "5s");
        assert_eq!(eta.remaining(500), Some(Duration::ZERO));
    }

    #[test]
    fn follows_a_slowdown() {
        let t = Instant::now();
        let mut eta = Eta::starting_at(t);
        // 100/s for 20 s, then 10/s for 20 s.
        for s in 1..=20u64 {
            eta.update_at(t + Duration::from_secs(s), s * 100);
        }
        for s in 1..=20u64 {
            eta.update_at(t + Duration::from_secs(20 + s), 2000 + s * 10);
        }
        // The fast start is outside the window by now.
        assert!((eta.rate() - 10.0).abs() < 1e-6, "{}", eta.rate());
        assert_eq!(eta.remaining(2300), Some(Duration::from_secs(10)));
    }

    #[test]
    fn stalled_work_has_no_estimate() {
        let t = Instant::now();
        let mut eta = Eta::starting_at(t);
        eta.update_at(t + Duration::from_secs(1), 0);
        eta.update_at(t + Duration::from_secs(2), 0);
        assert_eq!(eta.remaining(10), None);
        assert_eq!(eta.text(10), "--");
    }

    #[test]
    fn rapid_updates_are_thinned() {
        let t = Instant::now();
        let mut eta = Eta::starting_at(t);
        for ms in 1..=1000u64 {
            eta.update_at(t + Duration::from_millis(ms), ms);
        }
        assert!(eta.samples.len() <= 7, "{}", eta.samples.len());
    }
}
