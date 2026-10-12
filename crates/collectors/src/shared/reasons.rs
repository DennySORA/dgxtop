//! Reason codes attached by the collectors to canonical values, arbitration
//! explanations, process rows and collection failures.
//!
//! Codes are bounded (`1..=80` bytes) machine strings in `UPPER_SNAKE_CASE`.
//! [`PINNED_SOURCE_UNAVAILABLE`] keeps the spelling of the arbitration
//! contract so the pin outcome is searchable by
//! the same name the policy documents use.

use dgxtop_core::canonical::RejectReason;
use dgxtop_core::metric::native;
use dgxtop_core::quality::ReadStatus;

// Adapter-side markers read by the collectors.

/// The driver flagged the channel as faulty (e.g. hwmon `fault=1`).
pub const SENSOR_FAULT: &str = "SENSOR_FAULT";
/// An alarm is raised; the reading itself stays valid and is never dropped.
/// No collector reads it, which is the point: only tests name it.
#[cfg(test)]
pub const ALARM: &str = "ALARM";
/// NVML process association from the compute process list.
pub const GPU_USAGE_COMPUTE: &str = "gpu_usage:compute";
/// NVML process association from the graphics process list.
pub const GPU_USAGE_GRAPHICS: &str = "gpu_usage:graphics";
/// NVML process association present in both lists (already merged).
pub const GPU_USAGE_COMPUTE_AND_GRAPHICS: &str = "gpu_usage:compute_and_graphics";

// Counter engine.

/// First reading of a source incarnation: no previous base to subtract.
pub const WARMUP: &str = "WARMUP";
/// The counter moved backwards without a declared modulus.
pub const COUNTER_RESET: &str = "COUNTER_RESET";
/// A declared counter modulus was crossed once; the delta is still exact.
pub const COUNTER_WRAPPED: &str = "COUNTER_WRAPPED";
/// The source generation changed (driver/interface re-creation).
pub const GENERATION_CHANGED: &str = "GENERATION_CHANGED";
/// The kernel boot changed; deltas never cross boots.
pub const BOOT_CHANGED: &str = "BOOT_CHANGED";
/// The new sample is not later than the base sample.
pub const INTERVAL_NOT_POSITIVE: &str = "INTERVAL_NOT_POSITIVE";
/// The elapsed time exceeds the source stale window.
pub const INTERVAL_EXCEEDS_STALE_WINDOW: &str = "INTERVAL_EXCEEDS_STALE_WINDOW";
/// A counter reading is not an exact unsigned integer.
pub const COUNTER_NOT_INTEGER: &str = "COUNTER_NOT_INTEGER";
/// The exact canonical delta does not fit `u64`.
pub const COUNTER_DELTA_OVERFLOW: &str = "COUNTER_DELTA_OVERFLOW";

// Arbitration outcomes.

/// No candidate passed validation and freshness.
pub const NO_ELIGIBLE_CANDIDATE: &str = "NO_ELIGIBLE_CANDIDATE";
/// Independent confirmed origins disagree beyond tolerance.
pub const SOURCE_DISAGREEMENT: &str = "SOURCE_DISAGREEMENT";
/// The pinned source cannot serve the key; no other source is substituted.
pub const PINNED_SOURCE_UNAVAILABLE: &str = "pinned_source_unavailable";
/// A valid fallback is selected because the preferred source failed.
pub const PREFERRED_SOURCE_FAILED: &str = "PREFERRED_SOURCE_FAILED";
/// Alignment was judged on read (receipt) time only: weaker evidence.
pub const ALIGNED_BY_READ_TIME: &str = "ALIGNED_BY_READ_TIME";
/// Two entries of one confirmed physical origin disagree (diagnostic only).
pub const ALIAS_DISAGREEMENT: &str = "ALIAS_DISAGREEMENT";
/// A candidate of unknown origin disagrees (uncertainty, never a vote).
pub const UNCONFIRMED_ORIGIN_DISAGREEMENT: &str = "UNCONFIRMED_ORIGIN_DISAGREEMENT";
/// A better challenger is held back by switching hysteresis.
pub const HELD_BY_HYSTERESIS: &str = "HELD_BY_HYSTERESIS";
/// More candidates than `max_candidates_per_series`; lowest priority dropped.
pub const CANDIDATE_LIMIT_EXCEEDED: &str = "CANDIDATE_LIMIT_EXCEEDED";

// Candidate rejections.

pub const READ_UNSUPPORTED: &str = "READ_UNSUPPORTED";
pub const READ_PERMISSION_DENIED: &str = "READ_PERMISSION_DENIED";
pub const READ_TIMEOUT: &str = "READ_TIMEOUT";
pub const READ_ERROR: &str = "READ_ERROR";
pub const SOURCE_REMOVED: &str = "SOURCE_REMOVED";
pub const DRIVER_FAULT: &str = "DRIVER_FAULT";
pub const QUALITY_NOT_ELIGIBLE: &str = "QUALITY_NOT_ELIGIBLE";
pub const STALE: &str = "STALE";
pub const INVALID_VALUE: &str = "INVALID_VALUE";
pub const UNKNOWN_UNIT: &str = "UNKNOWN_UNIT";
/// The backend unit is declared but its definition is not verified (KB vs KiB).
pub const UNIT_UNVERIFIED: &str = "UNIT_UNVERIFIED";
pub const EXCLUDED_BY_POLICY: &str = "EXCLUDED_BY_POLICY";
pub const UNPROVEN_PURPOSE: &str = "UNPROVEN_PURPOSE";
pub const NAMESPACE_MISMATCH: &str = "NAMESPACE_MISMATCH";
pub const SOURCE_KEY_MISMATCH: &str = "SOURCE_KEY_MISMATCH";

// Derived values.

/// A formula input is missing or not eligible.
pub const MISSING_INPUT: &str = "MISSING_INPUT";
/// Formula inputs were sampled too far apart to combine.
pub const INPUTS_NOT_ALIGNED: &str = "INPUTS_NOT_ALIGNED";
pub const AVAILABLE_EXCEEDS_TOTAL: &str = "AVAILABLE_EXCEEDS_TOTAL";
pub const SWAP_FREE_EXCEEDS_TOTAL: &str = "SWAP_FREE_EXCEEDS_TOTAL";
pub const FREE_EXCEEDS_TOTAL: &str = "FREE_EXCEEDS_TOTAL";
/// No I/O completed in the interval: await has no denominator.
pub const NO_COMPLETED_IO: &str = "NO_COMPLETED_IO";
/// `used + available == 0`: use% has no denominator.
pub const ZERO_CAPACITY: &str = "ZERO_CAPACITY";
/// No scheduler ticks elapsed in the interval.
pub const NO_ELAPSED_TICKS: &str = "NO_ELAPSED_TICKS";
/// The fields of one `/proc/stat` line are not from one consistent sample set.
pub const INCOMPLETE_SAMPLE_SET: &str = "INCOMPLETE_SAMPLE_SET";
/// Some visible GPU processes report no memory figure; the sum covers the rest.
pub const PARTIAL_PROCESS_VISIBILITY: &str = "PARTIAL_PROCESS_VISIBILITY";
/// No visible GPU process reports a memory figure.
pub const NO_PROCESS_MEMORY_VISIBLE: &str = "NO_PROCESS_MEMORY_VISIBLE";
/// `clk_tck` is not published by the catalog; no default is assumed.
pub const CLK_TCK_UNKNOWN: &str = "CLK_TCK_UNKNOWN";
/// Inputs of one formula use different native units.
pub const UNIT_MISMATCH: &str = "UNIT_MISMATCH";
/// A derived value overflows its representation.
pub const VALUE_OVERFLOW: &str = "VALUE_OVERFLOW";
/// GB10 profile formula: CPU package power = `cluster:p` + `cluster:e`.
pub const FORMULA_GB10_CLUSTER_SUM: &str = "FORMULA_GB10_CLUSTER_SUM";
/// GB10 profile formula: CPU package temperature = the hottest of its four
/// cluster sensors (D-68).
pub const FORMULA_GB10_CLUSTER_MAX: &str = "FORMULA_GB10_CLUSTER_MAX";
/// Power derived from an energy counter delta over the actual interval.
pub const FORMULA_ENERGY_DELTA: &str = "FORMULA_ENERGY_DELTA";
/// The host's total power: every CPU package's plus every GPU's (D-106).
pub const FORMULA_HOST_POWER_TOTAL: &str = "FORMULA_HOST_POWER_TOTAL";

// Suspect heuristics (diagnostics only, never invalidating).

pub const VALUE_UNCHANGED_SINCE_LAST_READ: &str = "VALUE_UNCHANGED_SINCE_LAST_READ";
pub const DEVICE_TIME_AFTER_READ: &str = "DEVICE_TIME_AFTER_READ";

// Collection failures.

pub const CANONICAL_INVALID: &str = "CANONICAL_INVALID";
pub const UNKNOWN_METRIC: &str = "UNKNOWN_METRIC";
pub const METRIC_NOT_HANDLED: &str = "METRIC_NOT_HANDLED";
pub const BATCH_FAILED: &str = "BATCH_FAILED";
/// A batch reported an incomplete read (e.g. `NVML_PROCESS_LIST_PARTIAL`).
pub const BATCH_PARTIAL: &str = "BATCH_PARTIAL";
/// GPU process links were joined from an incomplete NVML process list.
pub const GPU_PROCESS_LIST_INCOMPLETE: &str = "GPU_PROCESS_LIST_INCOMPLETE";
pub const PLAN_COLLECTOR_MISMATCH: &str = "PLAN_COLLECTOR_MISMATCH";
pub const INVALID_ENTITY: &str = "INVALID_ENTITY";
/// The GB10 cluster entity does not name a CPU package.
pub const FORMULA_ENTITY_UNMAPPED: &str = "FORMULA_ENTITY_UNMAPPED";
pub const FAILURES_TRUNCATED: &str = "FAILURES_TRUNCATED";

/// Reason code for a failed read status.
pub const fn read_status_code(status: ReadStatus) -> &'static str {
    match status {
        ReadStatus::Unsupported => READ_UNSUPPORTED,
        ReadStatus::PermissionDenied => READ_PERMISSION_DENIED,
        ReadStatus::Timeout => READ_TIMEOUT,
        ReadStatus::Removed => SOURCE_REMOVED,
        ReadStatus::Ok | ReadStatus::Error => READ_ERROR,
    }
}

/// Canonical-level reason code for a candidate rejection. `native_unit`
/// distinguishes an unverified vendor unit from a plainly unknown one.
pub fn reject_code(reason: &RejectReason, native_unit: &str) -> &'static str {
    match reason {
        RejectReason::DifferentScope => SOURCE_KEY_MISMATCH,
        RejectReason::DifferentNamespace => NAMESPACE_MISMATCH,
        RejectReason::ReadFailed { status } => read_status_code(*status),
        RejectReason::DriverFault => DRIVER_FAULT,
        RejectReason::QualityNotEligible { .. } => QUALITY_NOT_ELIGIBLE,
        RejectReason::Stale => STALE,
        RejectReason::InvalidValue => INVALID_VALUE,
        RejectReason::UnknownUnit if native_unit == native::KB_UNVERIFIED_PER_SECOND => {
            UNIT_UNVERIFIED
        }
        RejectReason::UnknownUnit => UNKNOWN_UNIT,
        RejectReason::Excluded => EXCLUDED_BY_POLICY,
        RejectReason::UnprovenPurpose => UNPROVEN_PURPOSE,
        RejectReason::Hysteresis => HELD_BY_HYSTERESIS,
    }
}

/// Deduplicates codes, drops out-of-contract ones (empty or > 80 bytes) and
/// keeps at most 32, matching the canonical schema bounds.
pub fn bounded<I, S>(codes: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut kept: Vec<String> = Vec::new();
    for code in codes {
        let code = code.as_ref();
        if code.is_empty() || code.len() > 80 || kept.iter().any(|seen| seen == code) {
            continue;
        }
        if kept.len() == 32 {
            break;
        }
        kept.push(code.to_owned());
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::quality::Quality;

    #[test]
    fn every_code_respects_the_schema_bounds() {
        let codes = [
            SENSOR_FAULT,
            ALARM,
            WARMUP,
            COUNTER_RESET,
            INTERVAL_EXCEEDS_STALE_WINDOW,
            UNCONFIRMED_ORIGIN_DISAGREEMENT,
            VALUE_UNCHANGED_SINCE_LAST_READ,
            PINNED_SOURCE_UNAVAILABLE,
            FORMULA_GB10_CLUSTER_SUM,
            FORMULA_GB10_CLUSTER_MAX,
        ];
        for code in codes {
            assert!(!code.is_empty() && code.len() <= 80, "{code}");
        }
    }

    #[test]
    fn unverified_vendor_unit_is_named_separately() {
        assert_eq!(
            reject_code(&RejectReason::UnknownUnit, native::KB_UNVERIFIED_PER_SECOND),
            UNIT_UNVERIFIED
        );
        assert_eq!(
            reject_code(&RejectReason::UnknownUnit, "furlongs"),
            UNKNOWN_UNIT
        );
        assert_eq!(
            reject_code(
                &RejectReason::QualityNotEligible {
                    quality: Quality::Suspect
                },
                "degC"
            ),
            QUALITY_NOT_ELIGIBLE
        );
        assert_eq!(read_status_code(ReadStatus::Timeout), READ_TIMEOUT);
    }

    #[test]
    fn bounded_deduplicates_and_caps() {
        let long = "X".repeat(81);
        let many: Vec<String> = (0..40).map(|index| format!("CODE_{index}")).collect();
        let codes = bounded(
            ["A", "A", "", long.as_str()]
                .into_iter()
                .map(str::to_owned)
                .chain(many),
        );
        assert_eq!(codes.len(), 32);
        assert_eq!(codes[0], "A");
        assert!(
            codes
                .iter()
                .all(|code| !code.is_empty() && code.len() <= 80)
        );
    }
}
