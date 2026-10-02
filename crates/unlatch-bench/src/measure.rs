//! Measurement records and statistics.

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum System {
    /// Unlatch (engine API, FUSE frontend, or unlatchd directly).
    Unlatch,
    Sshfs,
    /// The same operation on the local disk (the native target).
    Local,
    /// Raw bytes over the same shaped bridge (`cat file`, echo): the network's own ceiling.
    Raw,
    /// Link calibration.
    Net,
}

impl System {
    pub fn parse(s: &str) -> Option<System> {
        match s {
            "unlatch" => Some(System::Unlatch),
            "sshfs" => Some(System::Sshfs),
            "local" => Some(System::Local),
            "raw" => Some(System::Raw),
            "net" => Some(System::Net),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            System::Unlatch => "unlatch",
            System::Sshfs => "sshfs",
            System::Local => "local",
            System::Raw => "raw",
            System::Net => "net",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    /// The system under test is not built/available yet (e.g. `unlatchd` still `todo!()`).
    Unavailable,
    /// Not applicable for this profile / mode (e.g. T13 only runs at ≤ 50 Mbit/s).
    Skipped,
    Timeout,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Better {
    Lower,
    Higher,
}

/// One number (or the reason there is none).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    /// Target id (`T1`..`T17`) or `NET`.
    pub id: String,
    pub metric: String,
    pub value: Option<f64>,
    pub unit: String,
    pub better: Better,
    /// Human-readable target (filled by [`crate::targets::evaluate`]).
    pub target: Option<String>,
    /// Pass/fail vs. the target (Unlatch rows only; baselines are `None`).
    pub pass: Option<bool>,
    pub system: System,
    pub profile: String,
    pub status: Status,
    /// Why there is no value, or how it was obtained (e.g. "extrapolated").
    pub detail: Option<String>,
    pub samples: u32,
}

impl Measurement {
    pub fn new(
        id: &str,
        metric: &str,
        system: System,
        profile: &str,
        unit: &str,
        better: Better,
    ) -> Measurement {
        Measurement {
            id: id.into(),
            metric: metric.into(),
            value: None,
            unit: unit.into(),
            better,
            target: None,
            pass: None,
            system,
            profile: profile.into(),
            status: Status::Ok,
            detail: None,
            samples: 0,
        }
    }

    pub fn value(mut self, v: f64, samples: usize) -> Measurement {
        self.value = Some(v);
        self.samples = samples as u32;
        self.status = Status::Ok;
        self
    }

    pub fn status(mut self, s: Status, detail: impl Into<String>) -> Measurement {
        self.status = s;
        self.detail = Some(detail.into());
        self
    }

    pub fn detail(mut self, d: impl Into<String>) -> Measurement {
        self.detail = Some(d.into());
        self
    }

    /// Scenario-judged target (kept as-is by [`crate::targets::evaluate`]).
    pub fn judged(mut self, target: String, pass: bool) -> Measurement {
        self.target = Some(target);
        self.pass = Some(pass);
        self
    }

    /// Key used to match rows across runs.
    pub fn key(&self) -> (String, String, String, String) {
        (
            self.profile.clone(),
            self.id.clone(),
            self.metric.clone(),
            self.system.as_str().to_string(),
        )
    }
}

/// Latency samples in milliseconds.
#[derive(Clone, Debug, Default)]
pub struct Samples {
    ms: Vec<f64>,
}

impl Samples {
    pub fn new() -> Samples {
        Samples::default()
    }

    pub fn push(&mut self, d: Duration) {
        self.ms.push(d.as_secs_f64() * 1e3);
    }

    pub fn push_ms(&mut self, ms: f64) {
        self.ms.push(ms);
    }

    pub fn len(&self) -> usize {
        self.ms.len()
    }

    /// The most recent sample (ms).
    pub fn last_ms(&self) -> Option<f64> {
        self.ms.last().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.ms.is_empty()
    }

    /// Nearest-rank percentile (`p` in 0..=100). `None` when empty.
    pub fn percentile(&self, p: f64) -> Option<f64> {
        percentile(&self.ms, p)
    }

    pub fn p50(&self) -> Option<f64> {
        self.percentile(50.0)
    }

    pub fn p99(&self) -> Option<f64> {
        self.percentile(99.0)
    }

    pub fn max(&self) -> Option<f64> {
        self.ms.iter().copied().max_by(f64::total_cmp)
    }

    pub fn mean(&self) -> Option<f64> {
        (!self.ms.is_empty()).then(|| self.ms.iter().sum::<f64>() / self.ms.len() as f64)
    }
}

/// Nearest-rank percentile of unsorted `v`.
pub fn percentile(v: &[f64], p: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * s.len() as f64).ceil() as usize;
    Some(s[rank.clamp(1, s.len()) - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&[3.0], 99.0), Some(3.0));
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 50.0), Some(50.0));
        assert_eq!(percentile(&v, 99.0), Some(99.0));
        assert_eq!(percentile(&v, 100.0), Some(100.0));
        assert_eq!(percentile(&v, 0.0), Some(1.0));
        let mut s = Samples::new();
        s.push(Duration::from_millis(2));
        s.push_ms(4.0);
        assert_eq!(s.mean(), Some(3.0));
        assert_eq!(s.max(), Some(4.0));
    }

    #[test]
    fn serde_shape() {
        let m = Measurement::new(
            "T3",
            "ls_la_warm_ms",
            System::Sshfs,
            "rtt40-bw50",
            "ms",
            Better::Lower,
        )
        .value(12.5, 10);
        let js = serde_json::to_string(&m).unwrap();
        assert!(js.contains("\"system\":\"sshfs\""), "{js}");
        assert!(js.contains("\"status\":\"ok\""), "{js}");
        let back: Measurement = serde_json::from_str(&js).unwrap();
        assert_eq!(back, m);
    }
}
