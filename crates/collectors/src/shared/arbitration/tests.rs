//! Arbitration engine tests (AT03, AT04).

use std::collections::BTreeMap;

use dgxtop_core::canonical::{
    CandidateDisposition, CollectorEvent, PolicySnapshot, RejectReason, SelectionMemo,
};
use dgxtop_core::identity::MetricKey;
use dgxtop_core::metric::{descriptor, ids, native};
use dgxtop_core::observation::Observation;
use dgxtop_core::quality::{Freshness, Quality, RawKind, ReadStatus, SampleKind, SelectionState};
use dgxtop_core::testing::{ObservationBuilder, metric_key, source_id};
use proptest::prelude::*;

use super::*;
use crate::shared::reasons;
use crate::testkit::{MILLISECOND, SECOND, policy, replay_policy};

const NOW: u64 = 42 * SECOND;
const READ: u64 = NOW - 10 * MILLISECOND;

fn package() -> MetricKey {
    metric_key("cpu.package.0", ids::CPU_TEMPERATURE_PACKAGE, "package:0")
}

fn soc() -> MetricKey {
    metric_key("soc", ids::SOC_TEMPERATURE, "soc")
}

fn millidegrees(key: &MetricKey, adapter: &str, source: &str, value: f64) -> ObservationBuilder {
    ObservationBuilder::new(key.clone(), adapter, source)
        .unit(native::MILLI_DEG_C)
        .f64(value)
        .read_at(READ)
}

fn candidate(observation: &Observation, rank: u16) -> Candidate<'_> {
    Candidate {
        observation,
        profile_rank: rank,
        raw_kind: RawKind::Gauge,
        mapping: CatalogMapping::Matches,
        deadlines: Deadlines {
            read_boot_ns: observation.read_finished_boot_ns + 3 * SECOND,
            measurement_boot_ns: observation
                .device_sample_boot_ns
                .map(|device| device + 3 * SECOND),
        },
        input_source_ids: Vec::new(),
        previous_value: None,
    }
}

fn run<'a>(
    key: &'a MetricKey,
    candidates: Vec<Candidate<'a>>,
    policy: &'a PolicySnapshot,
    memo: Option<&'a SelectionMemo>,
    now: u64,
) -> Outcome {
    arbitrate(Request {
        key,
        descriptor: descriptor(key.metric_id.as_str()).unwrap(),
        cycle_id: 42,
        now_boot_ns: now,
        policy,
        memo,
        candidates,
        max_candidates: 4,
        notes: Vec::new(),
    })
}

fn disposition_of(outcome: &Outcome, source: &str) -> CandidateDisposition {
    outcome
        .explanation
        .candidates
        .iter()
        .find(|candidate| candidate.source_id.as_str() == source)
        .map(|candidate| candidate.disposition.clone())
        .unwrap_or_else(|| panic!("{source} missing from the explanation"))
}

fn value(outcome: &Outcome) -> Option<f64> {
    outcome.canonical.value.map(|value| value.to_f64_lossy())
}

fn assert_valid(outcome: &Outcome) {
    outcome.canonical.validate().unwrap();
}

// ----- ARCHITECTURE §5.5 synthetic table --------------------------------

#[test]
fn soc_scope_is_a_different_key_and_never_competes() {
    let a = millidegrees(&package(), "hwmon", "hwmon:package0:temp1", 68000.0).build();
    let b = millidegrees(&soc(), "thermal", "thermal:zone0", 42000.0).build();
    let policy = policy();
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&a, 0), candidate(&b, 0)],
        &policy,
        None,
        NOW,
    );
    assert_valid(&outcome);
    assert_eq!(value(&outcome), Some(68.0));
    assert_eq!(
        disposition_of(&outcome, "thermal:zone0"),
        CandidateDisposition::Rejected(RejectReason::DifferentScope)
    );
    let soc_key = soc();
    let soc_outcome = run(&soc_key, vec![candidate(&b, 0)], &policy, None, NOW);
    assert_eq!(value(&soc_outcome), Some(42.0));
}

#[test]
fn confirmed_alias_is_counted_once_and_keeps_an_inconsistency_hint() {
    let a = millidegrees(&package(), "hwmon", "hwmon:package0:temp1", 68000.0)
        .origin("sensor:package0")
        .build();
    let b = millidegrees(&package(), "thermal", "thermal:package0", 42000.0)
        .origin("sensor:package0")
        .build();
    let policy = policy();
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&a, 0), candidate(&b, 1)],
        &policy,
        None,
        NOW,
    );
    assert_valid(&outcome);
    assert_eq!(outcome.canonical.selection_state, SelectionState::Selected);
    assert_eq!(value(&outcome), Some(68.0));
    assert_eq!(
        disposition_of(&outcome, "thermal:package0"),
        CandidateDisposition::Alias {
            of: source_id("hwmon:package0:temp1")
        }
    );
    assert!(
        outcome
            .explanation
            .reason_codes
            .contains(&reasons::ALIAS_DISAGREEMENT.to_owned())
    );
    assert!(
        !outcome
            .events
            .iter()
            .any(|event| matches!(event, CollectorEvent::SourceConflict { .. }))
    );
}

#[test]
fn independent_disagreement_is_a_conflict_never_an_average() {
    let a = millidegrees(&package(), "hwmon", "hwmon:package0:temp1", 68000.0)
        .origin("sensor:package0")
        .device_time(READ - 10 * MILLISECOND)
        .build();
    let b = millidegrees(&package(), "thermal", "thermal:package0", 42000.0)
        .origin("sensor:independent-package0")
        .device_time(READ - 20 * MILLISECOND)
        .build();
    let policy = policy();
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&a, 0), candidate(&b, 0)],
        &policy,
        None,
        NOW,
    );
    assert_valid(&outcome);
    assert_eq!(outcome.canonical.selection_state, SelectionState::Conflict);
    assert_eq!(
        outcome.canonical.value, None,
        "never 55 °C, never the lower one"
    );
    assert_eq!(outcome.canonical.source_id, None);
    assert!(!outcome.canonical.eligible_for_history);
    assert_eq!(
        outcome.canonical.reason_codes,
        vec![reasons::SOURCE_DISAGREEMENT.to_owned()]
    );
    assert_eq!(outcome.canonical.candidate_source_ids.len(), 2);
    assert_eq!(outcome.explanation.conflict_spread, Some(26.0));
    assert_eq!(outcome.explanation.tolerance, Some(8.0));
    assert_eq!(
        disposition_of(&outcome, "hwmon:package0:temp1"),
        CandidateDisposition::Conflicting
    );
    assert!(outcome.events.contains(&CollectorEvent::SourceConflict {
        key: package(),
        source_ids: vec![
            source_id("hwmon:package0:temp1"),
            source_id("thermal:package0")
        ],
    }));
}

#[test]
fn faulted_fallback_is_excluded_and_the_valid_source_selected() {
    let a = millidegrees(&package(), "hwmon", "hwmon:package0:temp1", 68000.0)
        .origin("sensor:package0")
        .build();
    let b = millidegrees(&package(), "thermal", "thermal:package0", 42000.0)
        .origin("sensor:independent-package0")
        .quality(Quality::Invalid)
        .reason(reasons::SENSOR_FAULT)
        .build();
    let policy = policy();
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&a, 0), candidate(&b, 1)],
        &policy,
        None,
        NOW,
    );
    assert_valid(&outcome);
    assert_eq!(value(&outcome), Some(68.0));
    assert_eq!(outcome.canonical.selection_state, SelectionState::Selected);
    assert_eq!(
        disposition_of(&outcome, "thermal:package0"),
        CandidateDisposition::Rejected(RejectReason::DriverFault)
    );
}

#[test]
fn faulted_preferred_source_degrades_to_the_same_key_fallback() {
    let a = millidegrees(&package(), "hwmon", "hwmon:package0:temp1", 68000.0)
        .reason(reasons::SENSOR_FAULT)
        .build();
    let b = millidegrees(&package(), "thermal", "thermal:package0", 67000.0).build();
    let policy = policy();
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&a, 0), candidate(&b, 1)],
        &policy,
        None,
        NOW,
    );
    assert_valid(&outcome);
    assert_eq!(outcome.canonical.selection_state, SelectionState::Degraded);
    assert_eq!(value(&outcome), Some(67.0));
    assert!(
        outcome
            .canonical
            .reason_codes
            .contains(&reasons::PREFERRED_SOURCE_FAILED.to_owned())
    );
}

#[test]
fn package_never_falls_back_to_a_soc_reading() {
    let a = millidegrees(&package(), "hwmon", "hwmon:package0:temp1", 68000.0)
        .failed(ReadStatus::Error)
        .build();
    let b = millidegrees(&soc(), "thermal", "thermal:zone0", 42000.0).build();
    let policy = policy();
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&a, 0), candidate(&b, 0)],
        &policy,
        None,
        NOW,
    );
    assert_valid(&outcome);
    assert_eq!(
        outcome.canonical.selection_state,
        SelectionState::Unavailable
    );
    assert_eq!(outcome.canonical.value, None);
}

// ----- ordering, tolerance and alignment -----------------------------------

#[test]
fn agreeing_independent_sources_select_by_profile_rank() {
    let a = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
        .origin("o1")
        .build();
    let b = millidegrees(&package(), "thermal", "thermal:b", 67000.0)
        .origin("o2")
        .build();
    let policy = policy();
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&a, 2), candidate(&b, 1)],
        &policy,
        None,
        NOW,
    );
    assert_eq!(outcome.canonical.source_id, Some(source_id("thermal:b")));
    assert_eq!(
        disposition_of(&outcome, "hwmon:a"),
        CandidateDisposition::Standby
    );
    assert_eq!(outcome.explanation.conflict_spread, Some(1.0));
}

#[test]
fn quality_grade_orders_before_profile_rank() {
    let usable = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
        .quality(Quality::Usable)
        .build();
    let verified = millidegrees(&package(), "thermal", "thermal:b", 68000.0).build();
    let policy = policy();
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&usable, 0), candidate(&verified, 5)],
        &policy,
        None,
        NOW,
    );
    assert_eq!(outcome.canonical.source_id, Some(source_id("thermal:b")));
}

#[test]
fn unknown_origins_never_conflict_but_leave_uncertainty() {
    let a = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0).build();
    let b = millidegrees(&package(), "thermal", "thermal:b", 42000.0).build();
    let policy = policy();
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&a, 0), candidate(&b, 1)],
        &policy,
        None,
        NOW,
    );
    assert_eq!(outcome.canonical.selection_state, SelectionState::Selected);
    assert_eq!(value(&outcome), Some(68.0));
    assert!(
        outcome
            .explanation
            .reason_codes
            .contains(&reasons::UNCONFIRMED_ORIGIN_DISAGREEMENT.to_owned())
    );
}

#[test]
fn unaligned_independent_sources_are_not_compared() {
    let a = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
        .origin("o1")
        .read_at(READ)
        .build();
    let b = millidegrees(&package(), "thermal", "thermal:b", 42000.0)
        .origin("o2")
        .read_at(READ - 900 * MILLISECOND)
        .build();
    let policy = policy();
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&a, 0), candidate(&b, 0)],
        &policy,
        None,
        NOW,
    );
    assert_eq!(outcome.canonical.selection_state, SelectionState::Selected);
    assert_eq!(outcome.explanation.conflict_spread, None);
}

#[test]
fn device_time_decides_alignment_when_both_report_it() {
    // Read together, but measured 2 s apart: not comparable.
    let a = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
        .origin("o1")
        .device_time(READ - 100 * MILLISECOND)
        .build();
    let b = millidegrees(&package(), "thermal", "thermal:b", 42000.0)
        .origin("o2")
        .device_time(READ - 2100 * MILLISECOND)
        .build();
    let policy = policy();
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&a, 0), candidate(&b, 0)],
        &policy,
        None,
        NOW,
    );
    assert_ne!(outcome.canonical.selection_state, SelectionState::Conflict);
}

#[test]
fn non_temperature_gauges_use_relative_tolerance() {
    let key = metric_key("cpu.package.0", ids::CPU_POWER_PACKAGE, "package:0");
    let watts = |source: &str, origin: &str, value: f64| {
        ObservationBuilder::new(key.clone(), "hwmon", source)
            .unit(native::WATT)
            .f64(value)
            .origin(origin)
            .read_at(READ)
            .build()
    };
    let policy = policy();
    let close = [watts("a", "o1", 100.0), watts("b", "o2", 105.0)];
    let outcome = run(
        &key,
        close.iter().map(|o| candidate(o, 0)).collect(),
        &policy,
        None,
        NOW,
    );
    assert_eq!(outcome.canonical.selection_state, SelectionState::Selected);
    let far = [watts("a", "o1", 100.0), watts("b", "o2", 120.0)];
    let outcome = run(
        &key,
        far.iter().map(|o| candidate(o, 0)).collect(),
        &policy,
        None,
        NOW,
    );
    assert_eq!(outcome.canonical.selection_state, SelectionState::Conflict);
    assert_close(outcome.explanation.tolerance.unwrap(), 12.0);
}

fn assert_close(left: f64, right: f64) {
    assert!((left - right).abs() < 1e-9, "{left} != {right}");
}

#[test]
fn counters_are_never_compared() {
    let key = metric_key("gpu:GPU-1", ids::GPU_ECC_CORRECTED, "gpu");
    let count = |source: &str, origin: &str, value: u64| {
        ObservationBuilder::new(key.clone(), "nvml", source)
            .unit(native::COUNT)
            .u64(value)
            .origin(origin)
            .read_at(READ)
            .build()
    };
    let readings = [count("a", "o1", 10), count("b", "o2", 9000)];
    let policy = policy();
    let candidates = readings
        .iter()
        .map(|observation| Candidate {
            raw_kind: RawKind::Counter,
            ..candidate(observation, 0)
        })
        .collect();
    let outcome = run(&key, candidates, &policy, None, NOW);
    assert_eq!(outcome.canonical.selection_state, SelectionState::Selected);
}

// ----- missing values, quality, freshness, namespace (AT04) ---------------

#[test]
fn an_unchanged_reading_is_only_a_suspect_diagnostic_never_fake_data() {
    let reading = millidegrees(&package(), "hwmon", "hwmon:package0:temp1", 68000.0).build();
    let policy = policy();
    let key = package();
    let suspicions = |previous: Option<Scalar>| {
        let mut repeated = candidate(&reading, 0);
        repeated.previous_value = previous;
        let outcome = run(&key, vec![repeated], &policy, None, NOW);
        assert_valid(&outcome);
        // AT16: a fixed value stays the selected reading; it is not replaced
        // or invalidated, only explained.
        assert_eq!(value(&outcome), Some(68.0));
        assert_eq!(
            disposition_of(&outcome, "hwmon:package0:temp1"),
            CandidateDisposition::Selected
        );
        outcome.explanation.candidates[0].suspect_reasons.clone()
    };
    assert!(
        suspicions(reading.value).contains(&reasons::VALUE_UNCHANGED_SINCE_LAST_READ.to_owned())
    );
    assert!(suspicions(Some(Scalar::from_f64(67_000.0).unwrap())).is_empty());
    assert!(suspicions(None).is_empty());
}

#[test]
fn true_zero_is_a_value_not_a_missing_sentinel() {
    let zero_degrees = ObservationBuilder::new(package(), "hwmon", "hwmon:a")
        .unit(native::DEG_C)
        .f64(0.0)
        .read_at(READ)
        .build();
    let policy = policy();
    let key = package();
    let outcome = run(&key, vec![candidate(&zero_degrees, 0)], &policy, None, NOW);
    assert_valid(&outcome);
    assert_eq!(value(&outcome), Some(0.0));

    let utilization = metric_key("gpu:GPU-1", ids::GPU_UTILIZATION_COMPUTE, "gpu");
    let zero_percent = ObservationBuilder::new(utilization.clone(), "nvml", "nvml:gpu1:util")
        .unit(native::PERCENT)
        .u64(0)
        .read_at(READ)
        .build();
    let outcome = run(
        &utilization,
        vec![candidate(&zero_percent, 0)],
        &policy,
        None,
        NOW,
    );
    assert_valid(&outcome);
    assert_eq!(
        outcome.canonical.value.and_then(|value| value.as_u64()),
        Some(0)
    );
}

#[test]
fn failed_reads_and_stale_values_are_never_zero() {
    let policy = policy();
    let key = package();
    for status in [
        ReadStatus::Timeout,
        ReadStatus::Unsupported,
        ReadStatus::PermissionDenied,
    ] {
        let failed = ObservationBuilder::new(package(), "hwmon", "hwmon:a")
            .failed(status)
            .read_at(READ)
            .build();
        let outcome = run(&key, vec![candidate(&failed, 0)], &policy, None, NOW);
        assert_valid(&outcome);
        assert_eq!(
            outcome.canonical.selection_state,
            SelectionState::Unavailable
        );
        assert_eq!(outcome.canonical.value, None);
        assert!(
            outcome
                .canonical
                .reason_codes
                .contains(&reasons::read_status_code(status).to_owned())
        );
    }
    let stale = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
        .read_at(NOW - 3 * SECOND)
        .build();
    let outcome = run(&key, vec![candidate(&stale, 0)], &policy, None, NOW);
    assert_valid(&outcome);
    assert_eq!(
        outcome.canonical.value, None,
        "deadline boundary is already stale"
    );
    assert_eq!(outcome.canonical.freshness, Freshness::Stale);
    assert!(
        outcome
            .canonical
            .reason_codes
            .contains(&reasons::STALE.to_owned())
    );
    // The stale reading remains visible as evidence only.
    assert_eq!(
        outcome.explanation.candidates[0]
            .value
            .map(|value| value.to_f64_lossy()),
        Some(68.0)
    );
}

#[test]
fn alarm_keeps_the_hot_reading() {
    let hot = millidegrees(&package(), "hwmon", "hwmon:a", 99000.0)
        .reason(reasons::ALARM)
        .build();
    let policy = policy();
    let key = package();
    let outcome = run(&key, vec![candidate(&hot, 0)], &policy, None, NOW);
    assert_valid(&outcome);
    assert_eq!(value(&outcome), Some(99.0));
    assert!(
        outcome
            .canonical
            .reason_codes
            .contains(&reasons::ALARM.to_owned())
    );
}

#[test]
fn missing_device_timestamp_means_read_fresh_measurement_unknown() {
    let reading = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0).build();
    let policy = policy();
    let key = package();
    let outcome = run(&key, vec![candidate(&reading, 0)], &policy, None, NOW);
    assert_eq!(outcome.canonical.freshness, Freshness::Fresh);
    assert_eq!(outcome.canonical.measurement_freshness, Freshness::Unknown);
    assert_eq!(outcome.canonical.observed_boot_ns, None);

    let timed = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
        .device_time(READ - MILLISECOND)
        .build();
    let outcome = run(&key, vec![candidate(&timed, 0)], &policy, None, NOW);
    assert_eq!(outcome.canonical.measurement_freshness, Freshness::Fresh);
    assert_eq!(outcome.canonical.observed_boot_ns, Some(READ - MILLISECOND));

    let old_measurement = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
        .device_time(NOW - 5 * SECOND)
        .build();
    let outcome = run(
        &key,
        vec![candidate(&old_measurement, 0)],
        &policy,
        None,
        NOW,
    );
    assert_eq!(
        outcome.canonical.value, None,
        "a confirmed stale measurement is never authoritative"
    );
}

#[test]
fn suspect_and_unknown_quality_are_not_canonical() {
    let policy = policy();
    let key = package();
    for quality in [Quality::Suspect, Quality::Unknown, Quality::Invalid] {
        let reading = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
            .quality(quality)
            .build();
        let outcome = run(&key, vec![candidate(&reading, 0)], &policy, None, NOW);
        assert_eq!(outcome.canonical.value, None, "{quality:?}");
        assert_eq!(
            disposition_of(&outcome, "hwmon:a"),
            CandidateDisposition::Rejected(RejectReason::QualityNotEligible { quality })
        );
    }
    let usable = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
        .quality(Quality::Usable)
        .build();
    let outcome = run(&key, vec![candidate(&usable, 0)], &policy, None, NOW);
    assert_eq!(value(&outcome), Some(68.0));
}

#[test]
fn unknown_and_unverified_units_are_rejected_not_guessed() {
    let policy = policy();
    let key = package();
    let volts = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
        .unit("mV")
        .build();
    let outcome = run(&key, vec![candidate(&volts, 0)], &policy, None, NOW);
    assert_eq!(outcome.canonical.value, None);
    assert_eq!(
        disposition_of(&outcome, "hwmon:a"),
        CandidateDisposition::Rejected(RejectReason::UnknownUnit)
    );
    assert!(
        outcome
            .canonical
            .reason_codes
            .contains(&reasons::UNKNOWN_UNIT.to_owned())
    );

    let pcie = metric_key("gpu:GPU-1", ids::GPU_PCIE_TX, "gpu");
    let unverified = ObservationBuilder::new(pcie.clone(), "nvml", "nvml:gpu1:pcie_tx")
        .unit(native::KB_UNVERIFIED_PER_SECOND)
        .u64(1000)
        .read_at(READ)
        .build();
    let outcome = run(&pcie, vec![candidate(&unverified, 0)], &policy, None, NOW);
    assert_valid(&outcome);
    assert_eq!(outcome.canonical.value, None);
    assert!(
        outcome
            .canonical
            .reason_codes
            .contains(&reasons::UNIT_UNVERIFIED.to_owned())
    );
    assert_eq!(
        outcome.explanation.candidates[0].native_value, unverified.value,
        "the native reading is kept as evidence"
    );
}

#[test]
fn replay_is_rejected_by_a_live_policy_and_accepted_by_a_replay_policy() {
    let key = package();
    let replayed = millidegrees(&package(), "replay", "replay:hwmon:package0", 68000.0)
        .kind(SampleKind::Replay)
        .build();
    let live = policy();
    let outcome = run(&key, vec![candidate(&replayed, 0)], &live, None, NOW);
    assert_eq!(outcome.canonical.value, None);
    assert!(!outcome.has_namespace_evidence);
    assert_eq!(
        disposition_of(&outcome, "replay:hwmon:package0"),
        CandidateDisposition::Rejected(RejectReason::DifferentNamespace)
    );

    let replay = replay_policy();
    let outcome = run(&key, vec![candidate(&replayed, 0)], &replay, None, NOW);
    assert_valid(&outcome);
    assert_eq!(value(&outcome), Some(68.0));
    assert_eq!(outcome.canonical.sample_kind, SampleKind::Replay);
    assert!(!outcome.canonical.eligible_for_history);

    let mislabeled = millidegrees(&package(), "replay", "replay:hwmon:package0", 68000.0)
        .kind(SampleKind::Measured)
        .build();
    for policy in [&live, &replay] {
        let outcome = run(&key, vec![candidate(&mislabeled, 0)], policy, None, NOW);
        assert_eq!(
            outcome.canonical.value, None,
            "replay adapters never feed live kinds"
        );
    }
}

#[test]
fn live_measured_values_of_history_metrics_are_history_eligible() {
    let reading = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0).build();
    let policy = policy();
    let key = package();
    let outcome = run(&key, vec![candidate(&reading, 0)], &policy, None, NOW);
    assert!(outcome.canonical.eligible_for_history);

    let core = metric_key("cpu.package.0", ids::CPU_TEMPERATURE_CORE, "core:0");
    let core_reading = millidegrees(&core, "hwmon", "hwmon:core0", 60000.0).build();
    let outcome = run(&core, vec![candidate(&core_reading, 0)], &policy, None, NOW);
    assert!(
        !outcome.canonical.eligible_for_history,
        "catalog marks core temperatures as non-history"
    );
}

#[test]
fn catalog_mapping_mismatch_and_unproven_purpose_are_rejected() {
    let reading = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0).build();
    let policy = policy();
    let key = package();
    let mismatched = Candidate {
        mapping: CatalogMapping::KeyMismatch,
        ..candidate(&reading, 0)
    };
    let outcome = run(&key, vec![mismatched], &policy, None, NOW);
    assert_eq!(
        disposition_of(&outcome, "hwmon:a"),
        CandidateDisposition::Rejected(RejectReason::DifferentScope)
    );
    let unproven = Candidate {
        mapping: CatalogMapping::UnprovenPurpose,
        ..candidate(&reading, 0)
    };
    let outcome = run(&key, vec![unproven], &policy, None, NOW);
    assert_eq!(
        disposition_of(&outcome, "hwmon:a"),
        CandidateDisposition::Rejected(RejectReason::UnprovenPurpose)
    );
}

#[test]
fn excluded_source_keeps_its_evidence_and_reason() {
    let a = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0).build();
    let b = millidegrees(&package(), "thermal", "thermal:b", 67000.0).build();
    let mut policy = policy();
    policy.excluded_sources.insert(source_id("hwmon:a"));
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&a, 0), candidate(&b, 1)],
        &policy,
        None,
        NOW,
    );
    assert_eq!(outcome.canonical.source_id, Some(source_id("thermal:b")));
    assert_eq!(
        outcome.canonical.selection_state,
        SelectionState::Selected,
        "a user exclusion is not a failure"
    );
    assert_eq!(
        disposition_of(&outcome, "hwmon:a"),
        CandidateDisposition::Rejected(RejectReason::Excluded)
    );
}

// ----- pins ---------------------------------------------------------------

fn pinned_policy(source: &str) -> PolicySnapshot {
    let mut policy = policy();
    policy.pinned_sources = BTreeMap::from([(package(), source_id(source))]);
    policy
}

#[test]
fn valid_pin_overrides_the_ranking() {
    let a = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0).build();
    let b = millidegrees(&package(), "thermal", "thermal:b", 67000.0).build();
    let policy = pinned_policy("thermal:b");
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&a, 0), candidate(&b, 9)],
        &policy,
        None,
        NOW,
    );
    assert_valid(&outcome);
    assert_eq!(outcome.canonical.source_id, Some(source_id("thermal:b")));
    assert_eq!(
        outcome.explanation.pinned_source,
        Some(source_id("thermal:b"))
    );
}

#[test]
fn unusable_pin_is_unavailable_and_never_silently_replaced() {
    let key = package();
    let good = millidegrees(&package(), "thermal", "thermal:b", 67000.0).build();
    let unusable = [
        millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
            .reason(reasons::SENSOR_FAULT)
            .build(),
        millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
            .read_at(NOW - 10 * SECOND)
            .build(),
        millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
            .unit("mV")
            .build(),
        millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
            .quality(Quality::Suspect)
            .build(),
        millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
            .failed(ReadStatus::Timeout)
            .build(),
    ];
    let policy = pinned_policy("hwmon:a");
    for pinned in &unusable {
        let outcome = run(
            &key,
            vec![candidate(pinned, 0), candidate(&good, 1)],
            &policy,
            None,
            NOW,
        );
        assert_valid(&outcome);
        assert_eq!(
            outcome.canonical.selection_state,
            SelectionState::Unavailable
        );
        assert_eq!(outcome.canonical.value, None);
        assert_eq!(
            outcome.canonical.reason_codes[0],
            reasons::PINNED_SOURCE_UNAVAILABLE
        );
        assert_eq!(
            disposition_of(&outcome, "thermal:b"),
            CandidateDisposition::Standby,
            "the alternative is suggested, not substituted"
        );
    }
    let mismatched = Candidate {
        mapping: CatalogMapping::KeyMismatch,
        ..candidate(&unusable[0], 0)
    };
    let outcome = run(
        &key,
        vec![mismatched, candidate(&good, 1)],
        &policy,
        None,
        NOW,
    );
    assert_eq!(
        outcome.canonical.selection_state,
        SelectionState::Unavailable
    );
    let outcome = run(&key, vec![candidate(&good, 1)], &policy, None, NOW);
    assert_eq!(
        outcome.canonical.reason_codes,
        vec![reasons::PINNED_SOURCE_UNAVAILABLE.to_owned()],
        "a missing pinned source is unavailable too"
    );
}

#[test]
fn pin_never_hides_a_conflict() {
    let a = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
        .origin("o1")
        .build();
    let b = millidegrees(&package(), "thermal", "thermal:b", 42000.0)
        .origin("o2")
        .build();
    let policy = pinned_policy("hwmon:a");
    let key = package();
    let outcome = run(
        &key,
        vec![candidate(&a, 0), candidate(&b, 0)],
        &policy,
        None,
        NOW,
    );
    assert_eq!(outcome.canonical.selection_state, SelectionState::Conflict);
    assert_eq!(outcome.canonical.value, None);
}

// ----- hysteresis through the full engine ---------------------------------

#[test]
fn healthy_source_switches_only_after_three_wins_and_five_seconds() {
    let key = package();
    let policy = policy();
    let current = millidegrees(&package(), "thermal", "thermal:b", 67000.0).build();
    let memo = SelectionMemo {
        selected: Some(source_id("thermal:b")),
        selected_since_boot_ns: 0,
        challenger: None,
        challenger_streak: 0,
        challenger_since_boot_ns: 0,
    };
    let mut memo = Some(memo);
    let mut switched_after = None;
    for round in 0..8u64 {
        let now = NOW + round * SECOND;
        let better = millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
            .read_at(now - MILLISECOND)
            .build();
        let fallback = Observation {
            read_finished_boot_ns: now - MILLISECOND,
            ..current.clone()
        };
        let outcome = run(
            &key,
            vec![candidate(&better, 0), candidate(&fallback, 1)],
            &policy,
            memo.as_ref(),
            now,
        );
        assert_valid(&outcome);
        if outcome.canonical.source_id == Some(source_id("hwmon:a")) {
            switched_after = Some(round);
            assert!(outcome.events.iter().any(|event| matches!(
                event,
                CollectorEvent::SourceChanged { previous_source: Some(previous), .. }
                    if previous.as_str() == "thermal:b"
            )));
            break;
        }
        assert_eq!(
            disposition_of(&outcome, "hwmon:a"),
            CandidateDisposition::Rejected(RejectReason::Hysteresis)
        );
        assert!(outcome.explanation.switch.is_some());
        memo = outcome.memo;
    }
    // Wins in rounds 0..=5: 5 s after the first win, with >= 3 wins.
    assert_eq!(switched_after, Some(5));
}

// ----- bounds and determinism ---------------------------------------------

#[test]
fn candidates_beyond_the_limit_are_dropped_by_priority() {
    let key = package();
    let policy = policy();
    let readings: Vec<Observation> = (0..6u16)
        .map(|index| millidegrees(&package(), "hwmon", &format!("hwmon:s{index}"), 60000.0).build())
        .collect();
    let candidates = readings
        .iter()
        .enumerate()
        .map(|(index, observation)| candidate(observation, u16::try_from(index).unwrap()))
        .collect();
    let outcome = run(&key, candidates, &policy, None, NOW);
    assert_eq!(
        outcome.explanation.candidates.len(),
        MAX_CANDIDATES_PER_SERIES
    );
    assert_eq!(
        outcome.dropped_candidates,
        vec![source_id("hwmon:s4"), source_id("hwmon:s5")]
    );
    assert!(
        outcome
            .explanation
            .reason_codes
            .contains(&reasons::CANDIDATE_LIMIT_EXCEEDED.to_owned())
    );
}

fn permutation_fixture() -> Vec<Observation> {
    vec![
        millidegrees(&package(), "hwmon", "hwmon:a", 68000.0)
            .origin("o1")
            .build(),
        millidegrees(&package(), "thermal", "thermal:b", 66000.0)
            .origin("o1")
            .build(),
        millidegrees(&package(), "thermal", "thermal:c", 64000.0)
            .origin("o2")
            .build(),
        millidegrees(&package(), "hwmon", "hwmon:d", 30000.0)
            .reason(reasons::SENSOR_FAULT)
            .build(),
    ]
}

proptest! {
    #[test]
    fn outcome_is_independent_of_candidate_order(seed in any::<u64>(), memo_choice in 0usize..3) {
        let readings = permutation_fixture();
        let policy = policy();
        let key = package();
        let memo = match memo_choice {
            0 => None,
            1 => Some(SelectionMemo {
                selected: Some(source_id("thermal:c")),
                selected_since_boot_ns: 0,
                challenger: Some(source_id("hwmon:a")),
                challenger_streak: 1,
                challenger_since_boot_ns: NOW - SECOND,
            }),
            _ => Some(SelectionMemo {
                selected: Some(source_id("hwmon:d")),
                selected_since_boot_ns: 0,
                challenger: None,
                challenger_streak: 0,
                challenger_since_boot_ns: 0,
            }),
        };
        let ranks = [0u16, 1, 1, 0];
        let baseline: Vec<Candidate<'_>> = readings
            .iter()
            .zip(ranks)
            .map(|(observation, rank)| candidate(observation, rank))
            .collect();
        let mut shuffled = baseline.clone();
        let mut state = seed;
        for index in (1..shuffled.len()).rev() {
            state = dgxtop_core::clock::splitmix64(state);
            let other = usize::try_from(state % (index as u64 + 1)).unwrap();
            shuffled.swap(index, other);
        }
        let expected = run(&key, baseline, &policy, memo.as_ref(), NOW);
        let actual = run(&key, shuffled, &policy, memo.as_ref(), NOW);
        prop_assert_eq!(expected, actual);
    }
}
