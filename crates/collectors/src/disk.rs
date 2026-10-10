//! Disk collector: block-device I/O from `/proc/diskstats` counters and
//! mounted filesystem capacity.
//!
//! Per block device scope (`device` and `partition` are separate scopes and
//! never summed): bytes/s = Δsectors × 512 / dt (intervals in bytes), IOPS =
//! Δcompleted / dt (intervals in counts), await = (Δread_ms + Δwrite_ms) /
//! (Δreads + Δwrites), unavailable when nothing completed. Filesystems:
//! used = total − free, use% = used / (used + available) × 100 (df).

use std::collections::BTreeMap;

use dgxtop_core::canonical::{CanonicalValue, CollectionBatch, CollectionPlan};
use dgxtop_core::identity::{AdapterId, CollectorId, IdentityError, MetricKey};
use dgxtop_core::metric::{CanonicalUnit, Domain, MetricDescriptor, MetricRole, Scalar, ids};
use dgxtop_core::naming;
use dgxtop_core::ports::Collector;
use dgxtop_core::quality::Freshness;

use crate::context::{self, CollectorSpec, Route, Session};
use crate::counter_inputs::{self, Step};
use crate::formulas::{self, Difference, Selections};
use crate::reasons;
use crate::values::{self, Evidence};

/// Raw diskstats counter, the canonical unit of its delta and the rate it feeds.
const COUNTERS: [(&str, CanonicalUnit, Option<&str>); 6] = [
    (
        ids::DISK_SECTORS_READ,
        CanonicalUnit::Bytes,
        Some(ids::DISK_READ_BPS),
    ),
    (
        ids::DISK_SECTORS_WRITTEN,
        CanonicalUnit::Bytes,
        Some(ids::DISK_WRITE_BPS),
    ),
    (
        ids::DISK_READS_COMPLETED,
        CanonicalUnit::Count,
        Some(ids::DISK_READ_IOPS),
    ),
    (
        ids::DISK_WRITES_COMPLETED,
        CanonicalUnit::Count,
        Some(ids::DISK_WRITE_IOPS),
    ),
    (ids::DISK_READ_TIME, CanonicalUnit::Milliseconds, None),
    (ids::DISK_WRITE_TIME, CanonicalUnit::Milliseconds, None),
];

const DERIVED: [&str; 7] = [
    ids::DISK_READ_BPS,
    ids::DISK_WRITE_BPS,
    ids::DISK_READ_IOPS,
    ids::DISK_WRITE_IOPS,
    ids::DISK_AWAIT,
    ids::FS_USED,
    ids::FS_USE_PERCENT,
];

const FS_PASS_THROUGH: [&str; 3] = [ids::FS_TOTAL, ids::FS_FREE, ids::FS_AVAILABLE];

const FS_USED: Difference = Difference {
    minuend: ids::FS_TOTAL,
    subtrahend: ids::FS_FREE,
    result: ids::FS_USED,
    negative: reasons::FREE_EXCEEDS_TOTAL,
};

fn accepts(metric: &MetricDescriptor) -> bool {
    matches!(metric.domain, Domain::Disk | Domain::Filesystem) && !DERIVED.contains(&metric.id)
}

/// Diskstats counters are consumed once per reading; filesystem capacity
/// values are kept for arbitration and formulas across batches.
fn retains(_route: &Route<'_>, metric: &MetricDescriptor) -> bool {
    metric.role == MetricRole::Canonical
}

static SPEC: CollectorSpec = CollectorSpec {
    id: naming::collectors::DISK,
    accepts,
    retains,
};

/// Block device and filesystem collector.
#[derive(Debug, Clone)]
pub struct DiskCollector {
    id: CollectorId,
}

impl DiskCollector {
    pub fn new() -> Result<Self, IdentityError> {
        Ok(Self {
            id: CollectorId::new(naming::collectors::DISK)?,
        })
    }
}

impl Collector for DiskCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn consumes(&self, adapter_id: &AdapterId, read_group: &str) -> bool {
        crate::routes_to(adapter_id, read_group, SPEC.id)
    }

    fn collect(&self, plan: &CollectionPlan) -> CollectionBatch {
        context::run(plan, &SPEC, collect_disk)
    }
}

fn collect_disk(session: &mut Session<'_>) {
    let steps = step_counters(session);
    publish_rates(session, &steps);
    publish_await(session, &steps);
    let mut selections = Selections::new();
    formulas::publish_pending(
        session,
        |metric| FS_PASS_THROUGH.contains(&metric),
        &mut selections,
    );
    formulas::publish_differences(session, FS_USED, &mut selections);
    publish_use_percent(session, &mut selections);
}

type Steps<'a> = BTreeMap<MetricKey, Step<'a>>;

/// One exact step per new diskstats counter reading (one source per field).
fn step_counters<'a>(session: &mut Session<'a>) -> Steps<'a> {
    let mut steps = Steps::new();
    for (metric, unit, _) in COUNTERS {
        for key in session.pending_keys(|key| key.metric_id.as_str() == metric) {
            for record in session.records(key) {
                let step = counter_inputs::step(session, record, Some(unit));
                if !matches!(step, Step::Unchanged) {
                    steps.insert(key.clone(), step);
                }
            }
        }
    }
    steps
}

fn publish_rates(session: &mut Session<'_>, steps: &Steps<'_>) {
    for (key, step) in steps {
        let Some((_, _, Some(rate_metric))) = COUNTERS
            .iter()
            .find(|(metric, _, _)| *metric == key.metric_id.as_str())
        else {
            continue;
        };
        counter_inputs::publish_rate(session, step, rate_metric);
    }
}

/// Await per block-device scope from four deltas over one interval.
fn publish_await(session: &mut Session<'_>, steps: &Steps<'_>) {
    let groups = formulas::result_keys(
        session,
        &[
            ids::DISK_READS_COMPLETED,
            ids::DISK_WRITES_COMPLETED,
            ids::DISK_READ_TIME,
            ids::DISK_WRITE_TIME,
        ],
        ids::DISK_AWAIT,
    );
    for await_key in groups {
        let inputs = [
            ids::DISK_READ_TIME,
            ids::DISK_WRITE_TIME,
            ids::DISK_READS_COMPLETED,
            ids::DISK_WRITES_COMPLETED,
        ]
        .map(|metric| {
            values::with_metric(&await_key, metric)
                .ok()
                .and_then(|key| steps.get(&key).copied())
        });
        if inputs.iter().all(Option::is_none) {
            continue;
        }
        if let Some(value) = await_value(session, await_key, &inputs) {
            session.publish_value(value);
        }
    }
}

fn await_value(
    session: &Session<'_>,
    key: MetricKey,
    inputs: &[Option<Step<'_>>; 4],
) -> Option<CanonicalValue> {
    let context = session.context();
    let evidence: Vec<Evidence> = inputs
        .iter()
        .flatten()
        .filter_map(Step::record)
        .map(|record| Evidence::of(record.observation))
        .collect();
    let deltas = match exact_deltas(inputs) {
        Ok(deltas) => deltas,
        Err(code) => {
            return values::unavailable(
                context,
                key,
                &evidence,
                &[code],
                input_freshness(session, inputs),
            );
        }
    };
    let [read_ms, write_ms, reads, writes] = deltas;
    let completed = u128::from(reads) + u128::from(writes);
    if completed == 0 {
        return values::unavailable(
            context,
            key,
            &evidence,
            &[reasons::NO_COMPLETED_IO],
            Freshness::Fresh,
        );
    }
    let fresh = inputs
        .iter()
        .flatten()
        .filter_map(Step::record)
        .all(|record| counter_inputs::is_fresh(session, record));
    if !fresh {
        return values::unavailable(context, key, &evidence, &[reasons::STALE], Freshness::Stale);
    }
    let busy_ms = u128::from(read_ms) + u128::from(write_ms);
    #[allow(clippy::cast_precision_loss)]
    let average = busy_ms as f64 / completed as f64;
    match Scalar::from_f64(average) {
        Ok(value) => values::derived(context, key, value, &evidence, &[]),
        Err(_) => values::unavailable(
            context,
            key,
            &evidence,
            &[reasons::VALUE_OVERFLOW],
            Freshness::Fresh,
        ),
    }
}

/// The four deltas when all are valid and span the same interval.
fn exact_deltas(inputs: &[Option<Step<'_>>; 4]) -> Result<[u64; 4], &'static str> {
    let mut deltas = [0u64; 4];
    let mut interval = None;
    for (slot, input) in deltas.iter_mut().zip(inputs) {
        match input {
            Some(Step::Delta { delta, .. }) => {
                let span = (delta.start_boot_ns, delta.end_boot_ns);
                if interval.is_some_and(|known| known != span) {
                    return Err(reasons::INPUTS_NOT_ALIGNED);
                }
                interval = Some(span);
                *slot = delta.delta;
            }
            Some(Step::Gap { gap, .. }) => return Err(gap.reason_code()),
            Some(Step::Unusable { code, .. }) => return Err(code),
            Some(Step::Unchanged) | None => return Err(reasons::MISSING_INPUT),
        }
    }
    Ok(deltas)
}

fn input_freshness(session: &Session<'_>, inputs: &[Option<Step<'_>>; 4]) -> Freshness {
    let records: Vec<_> = inputs.iter().flatten().filter_map(Step::record).collect();
    if records.is_empty() {
        Freshness::Unknown
    } else if records
        .iter()
        .any(|record| counter_inputs::is_fresh(session, *record))
    {
        Freshness::Fresh
    } else {
        Freshness::Stale
    }
}

/// df semantics: `used / (used + available) × 100`; `available` excludes the
/// reserved blocks, so use% can reach 100 before `free` reaches 0.
fn publish_use_percent(session: &mut Session<'_>, selections: &mut Selections) {
    let groups = formulas::result_keys(session, &FS_PASS_THROUGH, ids::FS_USE_PERCENT);
    for percent_key in groups {
        let (Ok(total), Ok(free), Ok(available)) = (
            values::with_metric(&percent_key, ids::FS_TOTAL),
            values::with_metric(&percent_key, ids::FS_FREE),
            values::with_metric(&percent_key, ids::FS_AVAILABLE),
        ) else {
            continue;
        };
        let input_keys = [&free, &total, &available];
        if !formulas::any_new(session, &input_keys) {
            continue;
        }
        let result =
            formulas::aligned_inputs(session, &input_keys, selections).and_then(|inputs| {
                let evidence: Vec<Evidence> = inputs.iter().map(Evidence::of_selection).collect();
                use_percent(inputs[1].value, inputs[0].value, inputs[2].value)
                    .map(|value| (value, evidence))
            });
        formulas::publish_formula(session, percent_key, &input_keys, result);
    }
}

fn use_percent(total: Scalar, free: Scalar, available: Scalar) -> Result<Scalar, &'static str> {
    let used = values::difference(total, free).ok_or(reasons::FREE_EXCEEDS_TOTAL)?;
    let capacity = values::sum(used, available).ok_or(reasons::VALUE_OVERFLOW)?;
    let capacity = capacity.to_f64_lossy();
    if capacity <= 0.0 {
        return Err(reasons::ZERO_CAPACITY);
    }
    Scalar::from_f64(used.to_f64_lossy() / capacity * 100.0).map_err(|_| reasons::VALUE_OVERFLOW)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{PlanBuilder, SECOND, assert_close, collect, number, reading, value_of};
    use dgxtop_core::metric::native;
    use dgxtop_core::observation::Observation;
    use dgxtop_core::testing::metric_key;

    fn diskstat(entity: &str, scope: &str, metric: &str, value: u64, at: u64) -> Observation {
        let unit = match metric {
            ids::DISK_SECTORS_READ | ids::DISK_SECTORS_WRITTEN => native::SECTORS_512,
            ids::DISK_READ_TIME | ids::DISK_WRITE_TIME => native::MILLISECONDS,
            _ => native::COUNT,
        };
        reading(
            &metric_key(entity, metric, scope),
            "procfs",
            &format!("procfs:{entity}:{scope}:{metric}"),
            at,
        )
        .unit(unit)
        .u64(value)
        .build()
    }

    /// `[sectors read, sectors written, reads, writes, read ms, write ms]`.
    fn device(entity: &str, scope: &str, values: [u64; 6], at: u64) -> Vec<Observation> {
        [
            ids::DISK_SECTORS_READ,
            ids::DISK_SECTORS_WRITTEN,
            ids::DISK_READS_COMPLETED,
            ids::DISK_WRITES_COMPLETED,
            ids::DISK_READ_TIME,
            ids::DISK_WRITE_TIME,
        ]
        .iter()
        .zip(values)
        .map(|(metric, value)| diskstat(entity, scope, metric, value, at))
        .collect()
    }

    fn run_pair(first: Vec<Observation>, second: Vec<Observation>) -> CollectionBatch {
        let collector = DiskCollector::new().unwrap();
        let warmup = collect(
            &collector,
            &PlanBuilder::new("disk")
                .at(SECOND + 1)
                .batch("procfs", "disk", first)
                .build(),
        );
        collect(
            &collector,
            &PlanBuilder::new("disk")
                .after(&warmup)
                .at(3 * SECOND + 1)
                .batch("procfs", "disk", second)
                .build(),
        )
    }

    #[test]
    fn sector_rates_iops_and_await_from_exact_deltas() {
        let entity = "blockdev:259:0";
        let batch = run_pair(
            device(entity, "device", [1000, 0, 10, 0, 50, 0], SECOND),
            // +2048 sectors read, +1024 written, +20 reads, +10 writes,
            // +120 ms reading, +30 ms writing over 2 s.
            device(entity, "device", [3048, 1024, 30, 10, 170, 30], 3 * SECOND),
        );
        let key = |metric| metric_key(entity, metric, "device");
        assert_eq!(
            number(&batch, &key(ids::DISK_READ_BPS)),
            Some(2048.0 * 512.0 / 2.0)
        );
        assert_eq!(
            number(&batch, &key(ids::DISK_WRITE_BPS)),
            Some(1024.0 * 512.0 / 2.0)
        );
        assert_eq!(number(&batch, &key(ids::DISK_READ_IOPS)), Some(10.0));
        assert_eq!(number(&batch, &key(ids::DISK_WRITE_IOPS)), Some(5.0));
        assert_eq!(number(&batch, &key(ids::DISK_AWAIT)), Some(150.0 / 30.0));
        let bytes = batch
            .counter_intervals
            .iter()
            .find(|interval| interval.key == key(ids::DISK_READ_BPS))
            .unwrap();
        assert_eq!(bytes.delta.as_u64(), Some(2048 * 512));
        assert_eq!(bytes.unit, CanonicalUnit::Bytes);
        let operations = batch
            .counter_intervals
            .iter()
            .find(|interval| interval.key == key(ids::DISK_READ_IOPS))
            .unwrap();
        assert_eq!(operations.delta.as_u64(), Some(20));
        assert_eq!(operations.unit, CanonicalUnit::Count);
    }

    #[test]
    fn await_without_completed_io_is_unavailable_not_a_division_by_zero() {
        let entity = "blockdev:8:0";
        let batch = run_pair(
            device(entity, "device", [1, 1, 5, 5, 9, 9], SECOND),
            device(entity, "device", [1, 1, 5, 5, 9, 9], 3 * SECOND),
        );
        let await_value = value_of(&batch, &metric_key(entity, ids::DISK_AWAIT, "device")).unwrap();
        assert_eq!(await_value.value, None);
        assert_eq!(
            await_value.reason_codes,
            vec![reasons::NO_COMPLETED_IO.to_owned()]
        );
        assert_eq!(
            number(&batch, &metric_key(entity, ids::DISK_READ_BPS, "device")),
            Some(0.0),
            "a true idle disk reads 0 B/s"
        );
    }

    #[test]
    fn partitions_and_devices_are_separate_scopes() {
        let device_entity = "blockdev:8:0";
        let partition_entity = "blockdev:8:1";
        let mut first = device(device_entity, "device", [0, 0, 0, 0, 0, 0], SECOND);
        first.extend(device(
            partition_entity,
            "partition",
            [0, 0, 0, 0, 0, 0],
            SECOND,
        ));
        let mut second = device(device_entity, "device", [200, 0, 2, 0, 4, 0], 3 * SECOND);
        second.extend(device(
            partition_entity,
            "partition",
            [100, 0, 1, 0, 2, 0],
            3 * SECOND,
        ));
        let batch = run_pair(first, second);
        assert_eq!(
            number(
                &batch,
                &metric_key(device_entity, ids::DISK_READ_BPS, "device")
            ),
            Some(100.0 * 512.0 / 2.0 * 2.0)
        );
        assert_eq!(
            number(
                &batch,
                &metric_key(partition_entity, ids::DISK_READ_BPS, "partition")
            ),
            Some(100.0 * 512.0 / 2.0)
        );
    }

    fn capacity(entity: &str, total: u64, free: u64, available: u64, at: u64) -> Vec<Observation> {
        [
            (ids::FS_TOTAL, total),
            (ids::FS_FREE, free),
            (ids::FS_AVAILABLE, available),
        ]
        .iter()
        .map(|(metric, value)| {
            reading(
                &metric_key(entity, metric, "filesystem"),
                "filesystem",
                &format!("filesystem:{entity}:{metric}"),
                at,
            )
            .unit(native::BYTES)
            .u64(*value)
            .build()
        })
        .collect()
    }

    #[test]
    fn filesystem_used_and_df_use_percent() {
        let collector = DiskCollector::new().unwrap();
        let entity = "fs:mnt:42";
        // 1000 total, 300 free, 250 available to users (50 reserved).
        let plan = PlanBuilder::new("disk")
            .at(SECOND + 1)
            .batch(
                "filesystem",
                "capacity",
                capacity(entity, 1000, 300, 250, SECOND),
            )
            .build();
        let batch = collect(&collector, &plan);
        let key = |metric| metric_key(entity, metric, "filesystem");
        assert_eq!(
            value_of(&batch, &key(ids::FS_USED))
                .and_then(|value| value.value)
                .and_then(|value| value.as_u64()),
            Some(700)
        );
        assert_close(
            number(&batch, &key(ids::FS_USE_PERCENT)).unwrap(),
            700.0 / 950.0 * 100.0,
        );
        assert_eq!(number(&batch, &key(ids::FS_AVAILABLE)), Some(250.0));
    }

    #[test]
    fn empty_filesystem_capacity_has_no_use_percent() {
        let collector = DiskCollector::new().unwrap();
        let entity = "fs:mnt:7";
        let plan = PlanBuilder::new("disk")
            .at(SECOND + 1)
            .batch("filesystem", "capacity", capacity(entity, 0, 0, 0, SECOND))
            .build();
        let batch = collect(&collector, &plan);
        let percent = value_of(
            &batch,
            &metric_key(entity, ids::FS_USE_PERCENT, "filesystem"),
        )
        .unwrap();
        assert_eq!(percent.value, None);
        assert_eq!(
            percent.reason_codes,
            vec![reasons::ZERO_CAPACITY.to_owned()]
        );
        let inconsistent = PlanBuilder::new("disk")
            .at(SECOND + 1)
            .batch(
                "filesystem",
                "capacity",
                capacity(entity, 10, 20, 5, SECOND),
            )
            .build();
        let batch = collect(&collector, &inconsistent);
        let used = value_of(&batch, &metric_key(entity, ids::FS_USED, "filesystem")).unwrap();
        assert_eq!(
            used.reason_codes,
            vec![reasons::FREE_EXCEEDS_TOTAL.to_owned()]
        );
    }
}
