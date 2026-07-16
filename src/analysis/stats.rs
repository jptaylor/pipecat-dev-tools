use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct StatsSummary {
    pub count: usize,
    pub mean: f64,
    pub stdev: f64,
    pub min: f64,
    pub max: f64,
    pub p50: f64,
    pub p90: f64,
    pub p95: f64,
    pub p99: f64,
}

/// Nearest-rank percentile on a sorted slice.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

pub fn summarize(values: &[f64]) -> StatsSummary {
    if values.is_empty() {
        return StatsSummary::default();
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = sorted.len();
    let mean = sorted.iter().sum::<f64>() / n as f64;
    let var = sorted.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
    StatsSummary {
        count: n,
        mean,
        stdev: var.sqrt(),
        min: sorted[0],
        max: sorted[n - 1],
        p50: percentile(&sorted, 0.50),
        p90: percentile(&sorted, 0.90),
        p95: percentile(&sorted, 0.95),
        p99: percentile(&sorted, 0.99),
    }
}

/// Histogram over [min, max] with `bins` buckets. Returns (edges, counts):
/// edges has bins+1 entries.
pub fn histogram(values: &[f64], bins: usize) -> (Vec<f64>, Vec<usize>) {
    if values.is_empty() || bins == 0 {
        return (Vec::new(), Vec::new());
    }
    let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let span = (max - min).max(1e-9);
    let mut counts = vec![0usize; bins];
    for &v in values {
        let idx = (((v - min) / span) * bins as f64) as usize;
        counts[idx.min(bins - 1)] += 1;
    }
    let edges = (0..=bins)
        .map(|i| min + span * i as f64 / bins as f64)
        .collect();
    (edges, counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        let s = summarize(&v);
        assert_eq!(s.count, 100);
        assert_eq!(s.p50, 50.0);
        assert_eq!(s.p90, 90.0);
        assert_eq!(s.p95, 95.0);
        assert_eq!(s.p99, 99.0);
        assert_eq!(s.min, 1.0);
        assert_eq!(s.max, 100.0);
    }

    #[test]
    fn single_value() {
        let s = summarize(&[42.0]);
        assert_eq!(s.p50, 42.0);
        assert_eq!(s.p99, 42.0);
        assert_eq!(s.stdev, 0.0);
    }

    #[test]
    fn histogram_counts() {
        let v = vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0];
        let (edges, counts) = histogram(&v, 5);
        assert_eq!(edges.len(), 6);
        assert_eq!(counts.iter().sum::<usize>(), 10);
        assert_eq!(counts, vec![2, 2, 2, 2, 2]);
    }
}
