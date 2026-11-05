//! Sliding-window message rate measurement.

use std::{collections::VecDeque, time::Instant};

const WINDOW_SECONDS: u64 = 3;

#[derive(Debug, Clone)]
pub struct RateMeter {
    origin: Instant,
    buckets: VecDeque<(u64, u64)>,
}

impl RateMeter {
    pub fn new(origin: Instant) -> Self {
        Self {
            origin,
            buckets: VecDeque::new(),
        }
    }

    fn second(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.origin).as_secs()
    }

    pub fn record(&mut self, now: Instant) {
        let second = self.second(now);
        match self.buckets.back_mut() {
            Some((last, count)) if *last == second => *count += 1,
            _ => self.buckets.push_back((second, 1)),
        }
        let oldest = second.saturating_sub(WINDOW_SECONDS);
        while self
            .buckets
            .front()
            .is_some_and(|(bucket, _)| *bucket < oldest)
        {
            self.buckets.pop_front();
        }
    }

    /// Average messages per second over the last few seconds.
    pub fn per_second(&self, now: Instant) -> f64 {
        let second = self.second(now);
        let first = second.saturating_sub(WINDOW_SECONDS - 1);
        let total: u64 = self
            .buckets
            .iter()
            .filter(|(bucket, _)| *bucket >= first)
            .map(|(_, count)| count)
            .sum();
        total as f64 / (second - first + 1) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn rate_averages_over_the_window_and_decays_to_zero() {
        let origin = Instant::now();
        let mut meter = RateMeter::new(origin);
        for second in 0..3 {
            for _ in 0..30 {
                meter.record(origin + Duration::from_secs(second));
            }
        }
        let rate = meter.per_second(origin + Duration::from_secs(2));
        assert!((rate - 30.0).abs() < f64::EPSILON, "rate was {rate}");
        assert_eq!(meter.per_second(origin + Duration::from_secs(20)), 0.0);
    }
}
