//! Fixed-memory duration summaries. Quantiles are intervals, never exact samples.
use serde_json::{Value, json};
use std::time::Duration;

const BUCKETS: usize = 1 + 64 * 8;
#[derive(Clone, Debug)]
pub struct StageStats {
    counts: [u64; BUCKETS],
    count: u64,
    total: u128,
    minimum: Option<u64>,
    maximum: Option<u64>,
    saturated_observations: u64,
    duration_clipped_observations: u64,
}
impl Default for StageStats {
    fn default() -> Self {
        Self {
            counts: [0; BUCKETS],
            count: 0,
            total: 0,
            minimum: None,
            maximum: None,
            saturated_observations: 0,
            duration_clipped_observations: 0,
        }
    }
}
fn bucket(value: u64) -> usize {
    if value == 0 {
        return 0;
    }
    let exponent = 63 - value.leading_zeros() as usize;
    let base = 1_u64 << exponent;
    let step = (base / 8).max(1);
    1 + exponent * 8 + ((value - base) / step) as usize
}
fn bounds(index: usize) -> (u64, u64) {
    if index == 0 {
        return (0, 0);
    }
    let exponent = (index - 1) / 8;
    let base = 1_u64 << exponent;
    let step = (base / 8).max(1);
    let low = base + ((index - 1) % 8) as u64 * step;
    (low, low.saturating_add(step - 1))
}
impl StageStats {
    pub fn observe(&mut self, duration: Duration) {
        if duration.as_nanos() > u128::from(u64::MAX) {
            self.duration_clipped_observations =
                self.duration_clipped_observations.saturating_add(1);
        }
        self.observe_ns(duration.as_nanos().min(u128::from(u64::MAX)) as u64);
    }
    pub fn observe_ns(&mut self, value: u64) {
        if self.count == u64::MAX {
            self.saturated_observations = self.saturated_observations.saturating_add(1);
            return;
        }
        self.count += 1;
        self.total += u128::from(value);
        self.counts[bucket(value)] += 1;
        self.minimum = Some(self.minimum.map_or(value, |old| old.min(value)));
        self.maximum = Some(self.maximum.map_or(value, |old| old.max(value)));
    }
    pub fn samples(&self) -> u64 {
        self.count
    }
    pub fn quantile_bounds(&self, percentile: u32) -> Option<(u64, u64)> {
        if self.count == 0 || !(1..=100).contains(&percentile) {
            return None;
        }
        let rank = (u128::from(self.count) * u128::from(percentile)).div_ceil(100);
        let mut cumulative = 0_u128;
        for (index, count) in self.counts.iter().enumerate() {
            cumulative += u128::from(*count);
            if cumulative >= rank {
                let (low, high) = bounds(index);
                return Some((
                    low.max(self.minimum.unwrap_or(0)),
                    high.min(self.maximum.unwrap_or(0)),
                ));
            }
        }
        None
    }
    pub fn report(&self) -> Value {
        let quantile = |p| {
            self.quantile_bounds(p)
                .map(|(low, high)| json!({"lower_bound_ns":low,"upper_bound_ns":high}))
        };
        json!({"samples":self.count,"minimum_ns":self.minimum,"maximum_ns":self.maximum,
            "total_ns":self.total.to_string(),"mean_ns":(self.count>0).then(||self.total as f64 / self.count as f64),
            "p50_ns":quantile(50),"p95_ns":quantile(95),"p99_ns":quantile(99),
            "quantile_method":"fixed-log2-eight-sub-buckets-interval","histogram_buckets":BUCKETS,
            "saturated_observations":self.saturated_observations,"duration_clipped_observations":self.duration_clipped_observations})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_zero_exact_samples_and_extremes_are_not_conflated() {
        let mut s = StageStats::default();
        assert_eq!(s.report()["maximum_ns"], Value::Null);
        assert_eq!(s.quantile_bounds(0), None);
        assert_eq!(s.quantile_bounds(101), None);
        for n in [0, 1, 2, 3, 4, 7, 8, 15, 16, u64::MAX] {
            let (low, high) = bounds(bucket(n));
            assert!(low <= n && n <= high);
            s.observe_ns(n);
        }
        assert_eq!(s.samples(), 10);
        assert_eq!(s.report()["minimum_ns"], 0);
        assert_eq!(s.report()["maximum_ns"], u64::MAX);
        assert_eq!(s.quantile_bounds(100).unwrap().1, u64::MAX);
    }
    #[test]
    fn long_tail_quantiles_bound_exact_rank_and_storage_stays_fixed() {
        let mut s = StageStats::default();
        for _ in 0..990 {
            s.observe(Duration::from_millis(10));
        }
        for _ in 0..10 {
            s.observe(Duration::from_millis(355));
        }
        let p99 = s.quantile_bounds(99).unwrap();
        assert!(p99.0 <= 10_000_000 && p99.1 >= 10_000_000);
        let p100 = s.quantile_bounds(100).unwrap();
        assert!(p100.0 <= 355_000_000 && p100.1 >= 355_000_000);
        assert_eq!(s.report()["total_ns"], "13450000000");
        assert!(std::mem::size_of_val(&s) < 5000);
        s.observe(Duration::MAX);
        assert_eq!(s.report()["maximum_ns"], u64::MAX);
        assert_eq!(s.quantile_bounds(101), None);
        assert_eq!(s.report()["duration_clipped_observations"], 1);
        s.count = u64::MAX;
        s.observe_ns(1);
        assert_eq!(s.report()["saturated_observations"], 1);
    }
}
