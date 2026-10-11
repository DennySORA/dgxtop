//! Raw counter records turned into exact steps and published rates.
//!
//! A counter record is validated like any candidate (mapping, namespace,
//! policy, fault, read status, quality), must carry an exact `u64` and a
//! known unit, and is fed to the counter engine against the committed base
//! of its source exactly once.

use dgxtop_core::canonical::RejectReason;
use dgxtop_core::metric::{CanonicalUnit, Conversion, Scalar, conversion_for};
use dgxtop_core::quality::{Freshness, Namespace};

use crate::shared::arbitration::check_admission;
use crate::shared::context::{Record, Session};
use crate::shared::counter::{self, CounterDelta, CounterGap, CounterReading};
use crate::shared::output::Subject;
use crate::shared::reasons;
use crate::shared::values::{self, Evidence};

/// Outcome of feeding one counter record.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Step<'a> {
    /// Exact delta over a complete interval.
    Delta {
        record: Record<'a>,
        delta: CounterDelta,
        conversion: Option<&'static Conversion>,
    },
    /// Valid reading without a delta (warmup, reset, incarnation change ...).
    Gap { record: Record<'a>, gap: CounterGap },
    /// The record cannot be used at all.
    Unusable {
        record: Record<'a>,
        code: &'static str,
    },
    /// Not new in this plan, or already consumed by an earlier job.
    Unchanged,
}

impl<'a> Step<'a> {
    pub fn record(&self) -> Option<Record<'a>> {
        match self {
            Self::Delta { record, .. }
            | Self::Gap { record, .. }
            | Self::Unusable { record, .. } => Some(*record),
            Self::Unchanged => None,
        }
    }
}

/// Validates a counter record and returns its exact value, or the reason
/// code why it is unusable. `unit` is the canonical unit the native unit
/// must convert to; `None` for dimensionless tick ratios.
pub(crate) fn validate<'a>(
    session: &Session<'a>,
    record: Record<'a>,
    unit: Option<CanonicalUnit>,
) -> Result<(u64, Option<&'static Conversion>), &'static str> {
    let candidate = session.candidate(record);
    let native_unit = record.observation.native_unit.as_str();
    check_admission(&candidate, record.key(), session.policy())
        .map_err(|reason| reasons::reject_code(&reason, native_unit))?;
    let value = record
        .observation
        .value
        .and_then(Scalar::as_u64)
        .ok_or(reasons::COUNTER_NOT_INTEGER)?;
    let conversion = match unit {
        Some(unit) => Some(
            conversion_for(native_unit, unit)
                .ok_or_else(|| reasons::reject_code(&RejectReason::UnknownUnit, native_unit))?,
        ),
        None => None,
    };
    Ok((value, conversion))
}

/// The engine reading of a validated record.
pub(crate) fn reading_of<'a>(
    session: &Session<'a>,
    record: Record<'a>,
    value: u64,
) -> CounterReading<'a> {
    CounterReading {
        value,
        boot_id: &record.observation.key.boot_id,
        generation: record.observation.provenance.generation,
        sample_boot_ns: record.sample_time_boot_ns(),
        modulus: session
            .source_descriptor(record.source_id())
            .and_then(|descriptor| descriptor.counter_modulus),
    }
}

/// Feeds one record and commits the new base of its source.
pub(crate) fn step<'a>(
    session: &mut Session<'a>,
    record: Record<'a>,
    unit: Option<CanonicalUnit>,
) -> Step<'a> {
    if !record.is_new() {
        return Step::Unchanged;
    }
    let (value, conversion) = match validate(session, record, unit) {
        Ok(valid) => valid,
        Err(code) => return Step::Unusable { record, code },
    };
    let reading = reading_of(session, record, value);
    let base = session.committed_counter(record.source_id());
    if counter::already_consumed(base, &reading) {
        return Step::Unchanged;
    }
    let advanced = counter::advance(base, &reading, session.stale_window_ns(record));
    session.set_counter(record.source_id().clone(), advanced.next_base);
    match advanced.outcome {
        Ok(delta) => Step::Delta {
            record,
            delta,
            conversion,
        },
        Err(gap) => Step::Gap { record, gap },
    }
}

/// Whether the record's read is still fresh at `now`.
pub(crate) fn is_fresh(session: &Session<'_>, record: Record<'_>) -> bool {
    session.now() < session.deadlines(record).read_boot_ns
}

/// Freshness of the latest read for an unavailable derived value.
pub(crate) fn read_freshness(session: &Session<'_>, record: Record<'_>) -> Freshness {
    if is_fresh(session, record) {
        Freshness::Fresh
    } else {
        Freshness::Stale
    }
}

/// Publishes the `rate_metric` value of one step (value, unavailable with
/// reason, or nothing when unchanged) and its exact interval for live data.
pub(crate) fn publish_rate(session: &mut Session<'_>, step: &Step<'_>, rate_metric: &str) {
    let Some(record) = step.record() else {
        return;
    };
    let Ok(key) = values::with_metric(record.key(), rate_metric) else {
        return;
    };
    let evidence = [Evidence::of(record.observation)];
    let context = session.context();
    let freshness = read_freshness(session, record);
    let value = match step {
        Step::Delta {
            delta,
            conversion: Some(conversion),
            ..
        } => {
            emit_interval(session, &key, record, delta, conversion);
            rate_value(session, key, record, delta, conversion, &evidence)
        }
        Step::Delta { .. } => {
            values::unavailable(context, key, &evidence, &[reasons::UNKNOWN_UNIT], freshness)
        }
        Step::Gap { gap, .. } => {
            values::unavailable(context, key, &evidence, &[gap.reason_code()], freshness)
        }
        Step::Unusable { code, .. } => {
            values::unavailable(context, key, &evidence, &[code], freshness)
        }
        Step::Unchanged => None,
    };
    if let Some(value) = value {
        session.publish_value(value);
    }
}

fn rate_value(
    session: &Session<'_>,
    key: dgxtop_core::identity::MetricKey,
    record: Record<'_>,
    delta: &CounterDelta,
    conversion: &Conversion,
    evidence: &[Evidence],
) -> Option<dgxtop_core::canonical::CanonicalValue> {
    let context = session.context();
    if !is_fresh(session, record) {
        return values::unavailable(context, key, evidence, &[reasons::STALE], Freshness::Stale);
    }
    let codes: Vec<&str> = delta
        .wrapped
        .then_some(reasons::COUNTER_WRAPPED)
        .into_iter()
        .collect();
    match delta.rate_per_second(conversion).map(Scalar::from_f64) {
        Some(Ok(rate)) => values::derived(context, key, rate, evidence, &codes),
        _ => values::unavailable(
            context,
            key,
            evidence,
            &[reasons::VALUE_OVERFLOW],
            Freshness::Fresh,
        ),
    }
}

/// Exact interval for the store's window totals; live data only (replay
/// never feeds live history).
pub(crate) fn emit_interval(
    session: &mut Session<'_>,
    key: &dgxtop_core::identity::MetricKey,
    record: Record<'_>,
    delta: &CounterDelta,
    conversion: &Conversion,
) {
    let live = session.policy().namespace == Namespace::Live
        && record.observation.sample_kind.namespace() == Namespace::Live;
    if !live {
        return;
    }
    let source = record.source_id().clone();
    match counter::interval(
        key.clone(),
        source.clone(),
        record.observation.provenance.generation,
        delta,
        conversion,
    ) {
        Some(interval) => session.interval(interval),
        None => session.failure(
            reasons::COUNTER_DELTA_OVERFLOW,
            &format!("{}: exact delta exceeds u64", key.series_selector()),
            Subject::source(&source),
        ),
    }
}
