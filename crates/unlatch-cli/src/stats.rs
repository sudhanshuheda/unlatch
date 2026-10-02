//! Latency statistics for the probe and doctor.

use serde::Serialize;

/// Nearest-rank percentile (`p` in 0..=100) of `samples`. `None` when empty.
pub fn percentile(samples: &[f64], p: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut v: Vec<f64> = samples.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let p = p.clamp(0.0, 100.0);
    // Nearest rank: ceil(p/100 · n), 1-based; p = 0 → the minimum.
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    Some(v[rank.clamp(1, v.len()) - 1])
}

pub fn median(samples: &[f64]) -> Option<f64> {
    percentile(samples, 50.0)
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Summary {
    pub n: usize,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
    pub mean: f64,
}

pub fn summarize(samples: &[f64]) -> Option<Summary> {
    let finite: Vec<f64> = samples.iter().copied().filter(|x| x.is_finite()).collect();
    if finite.is_empty() {
        return None;
    }
    let min = finite.iter().copied().fold(f64::INFINITY, f64::min);
    let max = finite.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Some(Summary {
        n: finite.len(),
        min,
        p50: percentile(&finite, 50.0)?,
        p95: percentile(&finite, 95.0)?,
        max,
        mean: finite.iter().sum::<f64>() / finite.len() as f64,
    })
}

/// Round to 3 decimals for JSON output (µs resolution when values are milliseconds).
pub fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 50.0), Some(50.0));
        assert_eq!(percentile(&v, 95.0), Some(95.0));
        assert_eq!(percentile(&v, 100.0), Some(100.0));
        assert_eq!(percentile(&v, 0.0), Some(1.0));
        assert_eq!(percentile(&[3.0, 1.0, 2.0], 50.0), Some(2.0));
        assert_eq!(percentile(&[5.0], 95.0), Some(5.0));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn ignores_non_finite() {
        assert_eq!(median(&[f64::NAN, 1.0, 3.0, f64::INFINITY, 2.0]), Some(2.0));
        assert_eq!(median(&[f64::NAN]), None);
    }

    #[test]
    fn summary_fields() {
        let s = summarize(&[4.0, 1.0, 3.0, 2.0]).unwrap();
        assert_eq!(s.n, 4);
        assert_eq!(s.min, 1.0);
        assert_eq!(s.max, 4.0);
        assert_eq!(s.p50, 2.0);
        assert_eq!(s.p95, 4.0);
        assert!((s.mean - 2.5).abs() < 1e-9);
        assert!(summarize(&[]).is_none());
    }

    #[test]
    fn rounding() {
        assert_eq!(round3(1.23456), 1.235);
    }
}
