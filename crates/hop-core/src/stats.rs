//! Latency summaries.

/// Percentiles of a set of samples (in whatever unit they were given).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub p999: f64,
    pub max: f64,
    pub count: usize,
}

impl Summary {
    pub fn of(samples: impl IntoIterator<Item = f64>) -> Option<Summary> {
        let mut v: Vec<f64> = samples.into_iter().collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f64::total_cmp);
        let at = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
        Some(Summary { p50: at(0.5), p90: at(0.9), p99: at(0.99), p999: at(0.999), max: v[v.len() - 1], count: v.len() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let s = Summary::of((1..=1000).map(f64::from)).unwrap();
        assert_eq!((s.p50, s.p99, s.max, s.count), (501.0, 990.0, 1000.0, 1000));
        assert!(Summary::of(std::iter::empty()).is_none());
    }
}
