//! Per-budget verdicts for human-readable bench summaries.
//!
//! A verdict is `pass` only when the metric was measured from at least one
//! sample and is finite; a missing measurement is `not_measured`, never a
//! pass. Thresholds come from `bench/budgets.json` unchanged.

use serde::Serialize;

use super::{Bound, Budgets};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictStatus {
    Pass,
    Fail,
    NotMeasured,
}

/// One budget compared with one measurement.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BudgetVerdict {
    pub metric: String,
    pub max: Option<f64>,
    pub min: Option<f64>,
    pub unit: String,
    pub measured: Option<f64>,
    pub samples: u64,
    pub status: VerdictStatus,
}

impl Budgets {
    /// Verdict of `metric` (a key of `limits`); `None` if the budgets do not
    /// define that metric.
    pub fn verdict(
        &self,
        metric: &str,
        measured: Option<f64>,
        samples: u64,
    ) -> Option<BudgetVerdict> {
        let bound = self.limits.get(metric)?;
        Some(BudgetVerdict {
            metric: metric.to_owned(),
            max: bound.max,
            min: bound.min,
            unit: bound.unit.clone(),
            measured,
            samples,
            status: judge(bound, measured, samples),
        })
    }
}

fn judge(bound: &Bound, measured: Option<f64>, samples: u64) -> VerdictStatus {
    let Some(value) = measured.filter(|value| value.is_finite()) else {
        return VerdictStatus::NotMeasured;
    };
    if samples == 0 {
        return VerdictStatus::NotMeasured;
    }
    let above = bound.max.is_some_and(|max| value > max);
    let below = bound.min.is_some_and(|min| value < min);
    if above || below {
        VerdictStatus::Fail
    } else {
        VerdictStatus::Pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn budgets() -> Budgets {
        Budgets::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benches/budgets.json"))
            .unwrap()
    }

    #[test]
    fn measured_values_pass_or_fail_on_the_spec_bounds() {
        let budgets = budgets();
        let pass = budgets.verdict("history_p95_ms", Some(99.9), 1000).unwrap();
        assert_eq!(pass.status, VerdictStatus::Pass);
        assert_eq!(pass.max, Some(100.0));
        let fail = budgets
            .verdict("history_p95_ms", Some(100.1), 1000)
            .unwrap();
        assert_eq!(fail.status, VerdictStatus::Fail);
        let coverage = budgets.verdict("coverage_percent", Some(99.8), 10).unwrap();
        assert_eq!(coverage.status, VerdictStatus::Fail);
        assert_eq!(coverage.min, Some(99.9));
    }

    #[test]
    fn nothing_passes_without_samples() {
        let budgets = budgets();
        for (value, samples) in [(None, 10), (Some(1.0), 0), (Some(f64::NAN), 5)] {
            let verdict = budgets.verdict("frame_p95_ms", value, samples).unwrap();
            assert_eq!(
                verdict.status,
                VerdictStatus::NotMeasured,
                "{value:?}/{samples}"
            );
        }
        assert!(budgets.verdict("not_a_budget", Some(1.0), 1).is_none());
    }
}
