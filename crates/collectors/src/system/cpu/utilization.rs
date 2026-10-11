//! CPU utilization from ONE consistent `/proc/stat` sample set per scope
//! (`host`, and `core:N` for logical CPU N — identity from the entity, never
//! from record order).
//!
//! total  = (Δuser+Δnice+Δsystem+Δirq+Δsoftirq+Δsteal) / Δall
//! user   = (Δuser+Δnice) / Δall
//! system = (Δsystem+Δirq+Δsoftirq) / Δall
//! iowait = Δiowait / Δall,  steal = Δsteal / Δall
//! Δall   = the busy sum + Δidle + Δiowait
//!
//! Guest time is already inside user/nice and is never added. The eight
//! field bases advance together, so a paged read never mixes intervals; a
//! field moving backwards (e.g. iowait) yields warmup, never a negative.

use dgxtop_core::clock::ms_to_ns;
use dgxtop_core::identity::MetricKey;
use dgxtop_core::metric::{Scalar, ids, native};
use dgxtop_core::quality::Freshness;

use crate::shared::context::{Record, Session};
use crate::shared::counter::{self, CounterReading};
use crate::shared::counter_inputs;
use crate::shared::formulas;
use crate::shared::reasons;
use crate::shared::values::{self, Evidence};

/// `/proc/stat` fields in formula order.
const FIELDS: [&str; 8] = [
    ids::CPU_TIME_USER,
    ids::CPU_TIME_NICE,
    ids::CPU_TIME_SYSTEM,
    ids::CPU_TIME_IDLE,
    ids::CPU_TIME_IOWAIT,
    ids::CPU_TIME_IRQ,
    ids::CPU_TIME_SOFTIRQ,
    ids::CPU_TIME_STEAL,
];

/// Published breakdown metrics, in [`Breakdown::percentages`] order.
const OUTPUTS: [&str; 5] = [
    ids::CPU_UTILIZATION_TOTAL,
    ids::CPU_UTILIZATION_USER,
    ids::CPU_UTILIZATION_SYSTEM,
    ids::CPU_UTILIZATION_IOWAIT,
    ids::CPU_UTILIZATION_STEAL,
];

type Fields<'a> = [Option<Record<'a>>; 8];

pub(super) fn publish(session: &mut Session<'_>) {
    for scope_key in formulas::result_keys(session, &FIELDS, ids::CPU_UTILIZATION_TOTAL) {
        let fields = field_records(session, &scope_key);
        if !fields.iter().flatten().any(Record::is_new) {
            continue;
        }
        match evaluate(session, &fields) {
            Evaluation::Ready(breakdown) => {
                publish_breakdown(session, &scope_key, &fields, &breakdown)
            }
            Evaluation::Unavailable(code) => {
                publish_unavailable(session, &scope_key, &fields, code)
            }
            Evaluation::Wait | Evaluation::AlreadyConsumed => {}
        }
    }
}

/// Newest record of each field of one scope.
fn field_records<'a>(session: &Session<'a>, scope_key: &MetricKey) -> Fields<'a> {
    FIELDS.map(|field| {
        values::with_metric(scope_key, field).ok().and_then(|key| {
            session.records(&key).into_iter().max_by(|left, right| {
                (left.observation.read_finished_boot_ns, left.source_id())
                    .cmp(&(right.observation.read_finished_boot_ns, right.source_id()))
            })
        })
    })
}

#[derive(Debug, Clone, PartialEq)]
enum Evaluation {
    Ready(Breakdown),
    Unavailable(&'static str),
    /// Fields of a newer sample set are still arriving (paged read).
    Wait,
    AlreadyConsumed,
}

fn evaluate(session: &mut Session<'_>, fields: &Fields<'_>) -> Evaluation {
    let Some(records) = complete(fields) else {
        return Evaluation::Unavailable(reasons::INCOMPLETE_SAMPLE_SET);
    };
    let values = match validated_values(session, &records) {
        Ok(values) => values,
        Err(code) => return Evaluation::Unavailable(code),
    };
    if !one_sample_set(session, records.iter().map(Record::sample_time_boot_ns)) {
        return Evaluation::Wait;
    }
    let readings: Vec<CounterReading<'_>> = records
        .iter()
        .zip(values)
        .map(|(record, value)| counter_inputs::reading_of(session, *record, value))
        .collect();
    let bases: Vec<_> = records
        .iter()
        .map(|record| session.committed_counter(record.source_id()))
        .collect();
    if readings
        .iter()
        .zip(&bases)
        .all(|(reading, base)| counter::already_consumed(*base, reading))
    {
        return Evaluation::AlreadyConsumed;
    }
    advance_together(session, &records, &readings, &bases)
}

fn complete<'a>(fields: &Fields<'a>) -> Option<[Record<'a>; 8]> {
    let mut records = Vec::with_capacity(FIELDS.len());
    for field in fields {
        records.push((*field)?);
    }
    records.try_into().ok()
}

/// Exact values of all eight fields, sharing one tick-like native unit.
fn validated_values(
    session: &Session<'_>,
    records: &[Record<'_>; 8],
) -> Result<[u64; 8], &'static str> {
    let mut values = [0u64; 8];
    for (slot, record) in values.iter_mut().zip(records) {
        let (value, _) = counter_inputs::validate(session, *record, None)?;
        *slot = value;
    }
    let unit = records[0].observation.native_unit.as_str();
    if unit != native::TICKS && unit != native::COUNT {
        return Err(reasons::UNKNOWN_UNIT);
    }
    if records
        .iter()
        .any(|record| record.observation.native_unit.as_str() != unit)
    {
        return Err(reasons::UNIT_MISMATCH);
    }
    Ok(values)
}

/// All times within the alignment tolerance: one read of `/proc/stat`.
fn one_sample_set(session: &Session<'_>, times: impl Iterator<Item = u64> + Clone) -> bool {
    let earliest = times.clone().min().unwrap_or(0);
    let latest = times.max().unwrap_or(0);
    latest - earliest <= ms_to_ns(u64::from(session.policy().alignment_tolerance_ms))
}

/// Advances the eight bases together and computes the breakdown when every
/// field produced an exact delta over the same interval.
fn advance_together(
    session: &mut Session<'_>,
    records: &[Record<'_>; 8],
    readings: &[CounterReading<'_>],
    bases: &[Option<&dgxtop_core::canonical::CounterBase>],
) -> Evaluation {
    let mut deltas = [0u64; 8];
    let mut gap = None;
    let mut starts = Vec::with_capacity(FIELDS.len());
    for (index, (record, reading)) in records.iter().zip(readings).enumerate() {
        let step = counter::advance(bases[index], reading, session.stale_window_ns(*record));
        session.set_counter(record.source_id().clone(), step.next_base);
        match step.outcome {
            Ok(delta) => {
                deltas[index] = delta.delta;
                starts.push(delta.start_boot_ns);
            }
            Err(field_gap) => {
                gap.get_or_insert(field_gap);
            }
        }
    }
    if let Some(gap) = gap {
        return Evaluation::Unavailable(gap.reason_code());
    }
    if !one_sample_set(session, starts.iter().copied()) {
        return Evaluation::Unavailable(reasons::INCOMPLETE_SAMPLE_SET);
    }
    match Breakdown::from_deltas(deltas) {
        Some(breakdown) => Evaluation::Ready(breakdown),
        None => Evaluation::Unavailable(reasons::NO_ELAPSED_TICKS),
    }
}

/// Percentages of one interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Breakdown {
    pub total: f64,
    pub user: f64,
    pub system: f64,
    pub iowait: f64,
    pub steal: f64,
}

impl Breakdown {
    /// `deltas` in [`FIELDS`] order; `None` when no tick elapsed.
    pub fn from_deltas(deltas: [u64; 8]) -> Option<Self> {
        let [user, nice, system, idle, iowait, irq, softirq, steal] = deltas.map(u128::from);
        let busy = user + nice + system + irq + softirq + steal;
        let all = busy + idle + iowait;
        if all == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let percent = |ticks: u128| ticks as f64 / all as f64 * 100.0;
        Some(Self {
            total: percent(busy),
            user: percent(user + nice),
            system: percent(system + irq + softirq),
            iowait: percent(iowait),
            steal: percent(steal),
        })
    }

    fn percentages(&self) -> [f64; 5] {
        [self.total, self.user, self.system, self.iowait, self.steal]
    }
}

fn evidence(fields: &Fields<'_>) -> Vec<Evidence> {
    fields
        .iter()
        .flatten()
        .map(|record| Evidence::of(record.observation))
        .collect()
}

fn publish_breakdown(
    session: &mut Session<'_>,
    scope_key: &MetricKey,
    fields: &Fields<'_>,
    breakdown: &Breakdown,
) {
    let fresh = fields
        .iter()
        .flatten()
        .all(|record| counter_inputs::is_fresh(session, *record));
    if !fresh {
        publish_unavailable(session, scope_key, fields, reasons::STALE);
        return;
    }
    let evidence = evidence(fields);
    let context = session.context();
    for (metric, percent) in OUTPUTS.iter().zip(breakdown.percentages()) {
        let (Ok(key), Ok(value)) = (
            values::with_metric(scope_key, metric),
            Scalar::from_f64(percent),
        ) else {
            continue;
        };
        if let Some(value) = values::derived(context, key, value, &evidence, &[]) {
            session.publish_value(value);
        }
    }
}

fn publish_unavailable(
    session: &mut Session<'_>,
    scope_key: &MetricKey,
    fields: &Fields<'_>,
    code: &'static str,
) {
    let evidence = evidence(fields);
    let freshness = if fields
        .iter()
        .flatten()
        .any(|record| counter_inputs::is_fresh(session, *record))
    {
        Freshness::Fresh
    } else {
        Freshness::Stale
    };
    let context = session.context();
    for metric in OUTPUTS {
        let Ok(key) = values::with_metric(scope_key, metric) else {
            continue;
        };
        if let Some(value) = values::unavailable(context, key, &evidence, &[code], freshness) {
            session.publish_value(value);
        }
    }
}
