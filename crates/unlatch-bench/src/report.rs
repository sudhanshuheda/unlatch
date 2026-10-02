//! Result files: JSON report, markdown scorecard, and run-to-run comparison.

use crate::measure::{Better, Measurement, Status, System};
use crate::netlab::{Calibration, Profile};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

pub const REPORT_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunReport {
    pub version: u32,
    /// Unix seconds.
    pub started: u64,
    pub finished: u64,
    pub host: String,
    pub kernel: String,
    pub quick: bool,
    pub profiles: Vec<Profile>,
    pub calibrations: Vec<Calibration>,
    pub measurements: Vec<Measurement>,
    /// Free-form notes (binaries used, skipped stages…).
    pub notes: Vec<String>,
    /// Host load average around each profile (the bench shares its host).
    #[serde(default)]
    pub load: Vec<LoadSample>,
    #[serde(default)]
    pub cpus: usize,
}

/// `/proc/loadavg` (1, 5, 15 min) when a profile started and finished.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LoadSample {
    pub profile: String,
    pub start: [f64; 3],
    pub end: [f64; 3],
}

impl RunReport {
    pub fn load(path: &Path) -> Result<RunReport> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Unlatch rows that were judged and failed.
    pub fn failed(&self) -> Vec<&Measurement> {
        self.measurements
            .iter()
            .filter(|m| m.system == System::Unlatch && m.pass == Some(false))
            .collect()
    }
}

fn fmt_value(m: &Measurement) -> String {
    match (m.status, m.value) {
        (Status::Ok, Some(v)) => {
            let s = if v == 0.0 {
                "0".to_string()
            } else if v.abs() >= 100.0 {
                format!("{v:.0}")
            } else if v.abs() >= 10.0 {
                format!("{v:.1}")
            } else if v.abs() >= 0.1 {
                format!("{v:.2}")
            } else {
                format!("{v:.4}")
            };
            if m.detail
                .as_deref()
                .is_some_and(|d| d.starts_with("extrapolated"))
            {
                format!("~{s}")
            } else {
                s
            }
        }
        (Status::Ok, None) => "—".into(),
        (Status::Unavailable, _) => "n/a".into(),
        (Status::Skipped, _) => "skip".into(),
        (Status::Timeout, _) => "timeout".into(),
        (Status::Error, _) => "error".into(),
    }
}

fn speedup(unlatch: Option<&Measurement>, sshfs: Option<&Measurement>) -> Option<f64> {
    let (h, s) = (unlatch?, sshfs?);
    if h.status != Status::Ok || s.status != Status::Ok {
        return None;
    }
    let (hv, sv) = (h.value?, s.value?);
    if hv <= 0.0 || sv <= 0.0 {
        return None;
    }
    Some(match h.better {
        Better::Lower => sv / hv,
        Better::Higher => hv / sv,
    })
}

fn escape(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

/// Markdown scorecard: one table per profile.
pub fn scorecard(r: &RunReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Unlatch bench scorecard\n");
    let _ = writeln!(
        out,
        "Host `{}` (kernel {}, {} CPUs, shared), {} mode. Shaping: unprivileged netns + `tc` on \
         `lo` (MTU 1500, offloads off), one bottleneck per direction (`netem` drop-tail, or \
         `netem` delay → `tbf` → `fq_codel` for `-fqcodel`); sshfs and Unlatch cross the same shaped \
         TCP path, through the raw bridge or, for `-ssh`, real `ssh` ⇄ `sshd`. Values are p50 \
         unless the metric says otherwise. `n/a` = system not available yet; `~` = extrapolated. \
         Speedup = sshfs ÷ unlatch for latencies, unlatch ÷ sshfs for throughput. The `raw` column is \
         `cat` of the T8 file through the same client path (`ssh bench cat` for `-ssh`), and for \
         T13 the link's own floor (plain-TCP bulk down+up, probe on its own connection).\n",
        r.host,
        r.kernel,
        r.cpus,
        if r.quick { "quick" } else { "full" }
    );
    let failed = r.failed();
    let judged = r
        .measurements
        .iter()
        .filter(|m| m.system == System::Unlatch && m.pass.is_some())
        .count();
    let unavailable = r
        .measurements
        .iter()
        .filter(|m| m.system == System::Unlatch && m.status == Status::Unavailable)
        .count();
    let _ = writeln!(
        out,
        "**Unlatch targets:** {} judged, {} failed, {} unlatch rows not available.\n",
        judged,
        failed.len(),
        unavailable
    );
    for p in &r.profiles {
        let _ = writeln!(out, "## {}\n", p.name);
        if let Some(l) = r.load.iter().find(|l| l.profile == p.name) {
            let _ = writeln!(
                out,
                "Host load average (1/5/15 min): {:.1}/{:.1}/{:.1} at start, {:.1}/{:.1}/{:.1} at end.\n",
                l.start[0], l.start[1], l.start[2], l.end[0], l.end[1], l.end[2]
            );
        }
        if let Some(c) = r.calibrations.iter().find(|c| c.profile == p.name) {
            let _ =
                writeln!(
                out,
                "Link: RTT p50 {:.2} ms (nominal {}), down {:.1} / up {:.1} Mbit/s (nominal {}), \
                 256 KiB fetch {:.1} ms warm vs {:.1} ms after 2 s idle.\n",
                c.rtt_ms_p50,
                p.rtt_ms,
                c.down_mbit,
                c.up_mbit,
                p.rate_mbit.map(|r| r.to_string()).unwrap_or_else(|| "unlimited".into()),
                c.fetch_256k_warm_ms,
                c.fetch_256k_idle_ms
            );
        }
        let rows: Vec<&Measurement> = r
            .measurements
            .iter()
            .filter(|m| m.profile == p.name)
            .collect();
        if rows.is_empty() {
            let _ = writeln!(out, "_no measurements_\n");
            continue;
        }
        // (id, metric) in first-seen order.
        let mut keys: Vec<(String, String)> = Vec::new();
        for m in &rows {
            let k = (m.id.clone(), m.metric.clone());
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        keys.sort_by(|a, b| {
            id_order(&a.0)
                .cmp(&id_order(&b.0))
                .then_with(|| a.1.cmp(&b.1))
        });
        let _ = writeln!(
            out,
            "| # | metric | unit | unlatch | sshfs | local | raw | target | pass | × vs sshfs |"
        );
        let _ = writeln!(out, "|---|---|---|---:|---:|---:|---:|---|:-:|---:|");
        for (id, metric) in keys {
            let get = |s: System| {
                rows.iter()
                    .copied()
                    .find(|m| m.id == id && m.metric == metric && m.system == s)
            };
            let any = rows.iter().find(|m| m.id == id && m.metric == metric);
            let unit = any.map(|m| m.unit.clone()).unwrap_or_default();
            let cell = |s: System| get(s).map(fmt_value).unwrap_or_default();
            let h = get(System::Unlatch);
            let target = h.and_then(|m| m.target.clone()).unwrap_or_default();
            let pass = match h.and_then(|m| m.pass) {
                Some(true) => "✅",
                Some(false) => "❌",
                None => "",
            };
            let sp = speedup(h, get(System::Sshfs))
                .or_else(|| {
                    // T3: unlatch warm vs sshfs cold is the headline comparison.
                    if metric == crate::targets::metric::LS_LA_MS {
                        let cold = rows.iter().copied().find(|m| {
                            m.id == id
                                && m.metric == crate::targets::metric::LS_LA_COLD_MS
                                && m.system == System::Sshfs
                        });
                        speedup(h, cold)
                    } else {
                        None
                    }
                })
                .map(|x| format!("{x:.1}×"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "| {id} | {metric} | {unit} | {} | {} | {} | {} | {} | {pass} | {sp} |",
                cell(System::Unlatch),
                cell(System::Sshfs),
                cell(System::Local),
                cell(System::Raw),
                escape(&target),
            );
        }
        // Why rows are missing (first line of each distinct reason).
        let mut reasons: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for m in &rows {
            if m.status != Status::Ok {
                let why = m.detail.clone().unwrap_or_default();
                let why = why
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(160)
                    .collect::<String>();
                reasons
                    .entry(format!("{:?} {}: {}", m.status, m.system.as_str(), why))
                    .or_default()
                    .push(m.id.clone());
            }
        }
        if !reasons.is_empty() {
            let _ = writeln!(
                out,
                "\n<details><summary>not measured ({})</summary>\n",
                reasons.len()
            );
            for (why, ids) in reasons {
                let mut ids = ids;
                ids.dedup();
                let _ = writeln!(out, "- {} — {}", ids.join(", "), escape(&why));
            }
            let _ = writeln!(out, "\n</details>");
        }
        let _ = writeln!(out);
    }
    if !r.notes.is_empty() {
        let _ = writeln!(out, "## Notes\n");
        for n in &r.notes {
            let _ = writeln!(out, "- {}", escape(n));
        }
    }
    out
}

fn id_order(id: &str) -> u32 {
    if id == "NET" {
        return 0;
    }
    id.trim_start_matches('T')
        .parse::<u32>()
        .map(|n| n + 1)
        .unwrap_or(1000)
}

/// One regression found by [`compare`].
#[derive(Clone, Debug, PartialEq)]
pub struct Regression {
    pub key: String,
    pub base: f64,
    pub new: f64,
    /// Relative change in the "worse" direction (0.25 = 25% worse).
    pub worse_by: f64,
    pub reason: String,
}

/// Absolute noise floor per unit: smaller absolute differences are never regressions
/// (microsecond-scale numbers routinely jitter by more than 10%).
fn noise_floor(unit: &str) -> f64 {
    match unit {
        "us" => 5.0,
        "ms" => 0.25,
        "s" => 0.05,
        "B/entry" => 10.0,
        "bytes" => 4096.0,
        "Mbit/s" => 1.0,
        "%" => 0.0,
        _ => 0.0,
    }
}

/// Compare Unlatch rows of `new` against `base`: a row regresses when it got worse by more than
/// `threshold` (and more than the unit's noise floor), or when it passed before and fails now,
/// or when it was measured before and is not now.
pub fn compare(base: &RunReport, new: &RunReport, threshold: f64) -> Vec<Regression> {
    let index: BTreeMap<_, _> = base.measurements.iter().map(|m| (m.key(), m)).collect();
    let mut out = Vec::new();
    for m in new
        .measurements
        .iter()
        .filter(|m| m.system == System::Unlatch)
    {
        let Some(b) = index.get(&m.key()) else {
            continue;
        };
        let key = format!(
            "{} {} {} ({})",
            m.profile,
            m.id,
            m.metric,
            m.system.as_str()
        );
        if b.status == Status::Ok && m.status != Status::Ok && m.status != Status::Skipped {
            out.push(Regression {
                key,
                base: b.value.unwrap_or(f64::NAN),
                new: f64::NAN,
                worse_by: f64::INFINITY,
                reason: format!("was measured, now {:?}", m.status),
            });
            continue;
        }
        if b.pass == Some(true) && m.pass == Some(false) {
            out.push(Regression {
                key: key.clone(),
                base: b.value.unwrap_or(f64::NAN),
                new: m.value.unwrap_or(f64::NAN),
                worse_by: f64::NAN,
                reason: "target passed before, fails now".into(),
            });
            continue;
        }
        let (Some(bv), Some(nv)) = (b.value, m.value) else {
            continue;
        };
        if b.status != Status::Ok || m.status != Status::Ok {
            continue;
        }
        let worse = match m.better {
            Better::Lower => nv - bv,
            Better::Higher => bv - nv,
        };
        if worse <= noise_floor(&m.unit) {
            continue;
        }
        let rel = if bv.abs() > 0.0 {
            worse / bv.abs()
        } else {
            f64::INFINITY
        };
        if rel > threshold {
            out.push(Regression {
                key,
                base: bv,
                new: nv,
                worse_by: rel,
                reason: format!("{:.1}% worse", rel * 100.0),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(metric: &str, v: f64, better: Better, unit: &str) -> Measurement {
        Measurement::new("T1", metric, System::Unlatch, "rtt40-bw50", unit, better).value(v, 5)
    }

    fn report(ms: Vec<Measurement>) -> RunReport {
        RunReport {
            version: REPORT_VERSION,
            profiles: vec![Profile::parse("rtt40-bw50").unwrap()],
            measurements: ms,
            ..Default::default()
        }
    }

    #[test]
    fn compare_detects_regressions_with_noise_floor() {
        let base = report(vec![
            m("a", 10.0, Better::Lower, "ms"),
            m("b", 100.0, Better::Higher, "Mbit/s"),
            m("c", 0.10, Better::Lower, "ms"),
            m("d", 10.0, Better::Lower, "ms"),
        ]);
        let new = report(vec![
            m("a", 11.5, Better::Lower, "ms"),      // 15% worse → regression
            m("b", 95.0, Better::Higher, "Mbit/s"), // 5% worse → fine
            m("c", 0.20, Better::Lower, "ms"),      // 100% worse but 0.1 ms < floor
            m("d", 5.0, Better::Lower, "ms"),       // better
        ]);
        let r = compare(&base, &new, 0.10);
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(r[0].key.contains(" a "));
    }

    #[test]
    fn compare_flags_lost_measurements_and_new_failures() {
        let mut ok = m("a", 1.0, Better::Lower, "ms");
        ok.pass = Some(true);
        let base = report(vec![ok.clone(), m("b", 1.0, Better::Lower, "ms")]);
        let mut failing = ok.clone();
        failing.pass = Some(false);
        let gone = m("b", 1.0, Better::Lower, "ms").status(Status::Error, "boom");
        let r = compare(&base, &report(vec![failing, gone]), 0.10);
        assert_eq!(r.len(), 2, "{r:?}");
    }

    #[test]
    fn scorecard_renders_all_profiles() {
        let mut r = report(vec![
            m(
                crate::targets::metric::LIST_1000_P50_MS,
                0.3,
                Better::Lower,
                "ms",
            ),
            Measurement::new(
                "T3",
                "ls_la_ms",
                System::Sshfs,
                "rtt40-bw50",
                "ms",
                Better::Lower,
            )
            .value(40.0, 3),
            Measurement::new(
                "T3",
                "ls_la_ms",
                System::Unlatch,
                "rtt40-bw50",
                "ms",
                Better::Lower,
            )
            .status(Status::Unavailable, "unlatch binary missing"),
        ]);
        r.calibrations.push(Calibration {
            profile: "rtt40-bw50".into(),
            rtt_ms_p50: 40.4,
            ..Default::default()
        });
        let md = scorecard(&r);
        assert!(md.contains("## rtt40-bw50"), "{md}");
        assert!(md.contains("RTT p50 40.40 ms"), "{md}");
        assert!(md.contains("| T3 | ls_la_ms | ms | n/a | 40.0 |"), "{md}");
        assert!(md.contains("unlatch binary missing"), "{md}");
    }
}
