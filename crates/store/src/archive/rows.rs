//! Validation and conversion of an `ArchiveJournal` into `history.sql` rows.
//!
//! Boot-clock values become 20-digit TEXT, counter totals become u128
//! decimal TEXT, and REAL is used only for gauge sums/min/max. A journal
//! with any row the schema or the spec forbids (replay data, non-finite
//! values, inverted intervals, coverage above 60 s) is rejected as a whole
//! before anything is written.

use dgxtop_core::canonical::GapRecord;
use dgxtop_core::digest::sha256_hex;
use dgxtop_core::history::HistoryMethod;
use dgxtop_core::ports::{ArchiveJournal, ArchivedBridge, ArchivedBucket, ArchivedSegment};
use dgxtop_core::quality::SampleKind;
use dgxtop_core::wire::format_fixed_width;

use crate::sqlite::is_rfc3339_utc;

/// `covered_ns <= 60 s` (one minute bucket).
pub(crate) const MAX_COVERED_NS: u64 = 60_000_000_000;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_REASON_BYTES: usize = 128;
/// Fixed per-row overhead in the payload estimate.
const ROW_OVERHEAD_BYTES: u64 = 32;
/// Indexes roughly double or triple a row's on-disk cost.
const STORAGE_FACTOR: u64 = 3;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SegmentRow {
    pub segment_id: String,
    pub host_id: String,
    pub boot_id: String,
    pub entity_id: String,
    pub metric_id: String,
    pub scope: String,
    pub source_id: String,
    pub source_generation: i64,
    pub policy_revision: i64,
    pub unit: String,
    pub sample_kind: &'static str,
    pub started_utc: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BucketRow {
    pub segment_id: String,
    pub start_boot_ns: String,
    pub end_boot_ns: String,
    pub start_utc: String,
    pub end_utc: String,
    pub covered_ns: String,
    pub weighted_sum: Option<f64>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub counter_delta_decimal: Option<String>,
    pub sample_count: i64,
    pub method: &'static str,
    pub reason_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BridgeRow {
    pub segment_id: String,
    pub start_boot_ns: String,
    pub end_boot_ns: String,
    pub delta_decimal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GapRow {
    pub gap_id: String,
    pub host_id: String,
    pub boot_id: String,
    pub start_boot_ns: String,
    pub end_boot_ns: String,
    pub reason_code: String,
    pub affected_count: i64,
}

/// One row of any archive table, in foreign-key order within a batch.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ArchiveRow {
    Segment(SegmentRow),
    Bucket(BucketRow),
    Bridge(BridgeRow),
    Gap(GapRow),
}

fn text_bytes(texts: &[&str]) -> u64 {
    texts
        .iter()
        .map(|text| u64::try_from(text.len()).unwrap_or(u64::MAX))
        .fold(0, u64::saturating_add)
}

impl ArchiveRow {
    /// Approximate payload bytes (for `batch_bytes`).
    pub(crate) fn payload_bytes(&self) -> u64 {
        let (texts, numbers) = match self {
            Self::Segment(row) => (
                text_bytes(&[
                    &row.segment_id,
                    &row.host_id,
                    &row.boot_id,
                    &row.entity_id,
                    &row.metric_id,
                    &row.scope,
                    &row.source_id,
                    &row.unit,
                    row.sample_kind,
                    &row.started_utc,
                ]),
                2,
            ),
            Self::Bucket(row) => (
                text_bytes(&[
                    &row.segment_id,
                    &row.start_boot_ns,
                    &row.end_boot_ns,
                    &row.start_utc,
                    &row.end_utc,
                    &row.covered_ns,
                    row.counter_delta_decimal.as_deref().unwrap_or_default(),
                    row.method,
                    row.reason_code.as_deref().unwrap_or_default(),
                ]),
                4,
            ),
            Self::Bridge(row) => (
                text_bytes(&[
                    &row.segment_id,
                    &row.start_boot_ns,
                    &row.end_boot_ns,
                    &row.delta_decimal,
                ]),
                0,
            ),
            Self::Gap(row) => (
                text_bytes(&[
                    &row.gap_id,
                    &row.host_id,
                    &row.boot_id,
                    &row.start_boot_ns,
                    &row.end_boot_ns,
                    &row.reason_code,
                ]),
                1,
            ),
        };
        texts
            .saturating_add(numbers * 8)
            .saturating_add(ROW_OVERHEAD_BYTES)
    }
}

/// Every row of one journal, validated.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ArchiveBatch {
    pub rows: Vec<ArchiveRow>,
}

impl ArchiveBatch {
    pub(crate) fn from_journal(journal: &ArchiveJournal) -> Result<Self, String> {
        let host_id = identifier("host_id", &journal.host_id)?;
        let boot_id = identifier("boot_id", &journal.boot_id)?;
        let mut rows = Vec::with_capacity(
            journal.segments.len()
                + journal.buckets.len()
                + journal.bridges.len()
                + journal.gaps.len(),
        );
        for segment in &journal.segments {
            rows.push(ArchiveRow::Segment(segment_row(segment)?));
        }
        for bucket in &journal.buckets {
            rows.push(ArchiveRow::Bucket(bucket_row(bucket)?));
        }
        for bridge in &journal.bridges {
            rows.push(ArchiveRow::Bridge(bridge_row(bridge)?));
        }
        for gap in &journal.gaps {
            rows.push(ArchiveRow::Gap(gap_row(gap, &host_id, &boot_id)?));
        }
        Ok(Self { rows })
    }
}

/// Estimated on-disk growth of `rows` including their index entries.
pub(crate) fn storage_estimate(rows: &[ArchiveRow]) -> u64 {
    rows.iter()
        .map(ArchiveRow::payload_bytes)
        .fold(0, u64::saturating_add)
        .saturating_mul(STORAGE_FACTOR)
}

fn identifier(field: &str, text: &str) -> Result<String, String> {
    let valid = !text.is_empty()
        && text.len() <= MAX_IDENTIFIER_BYTES
        && !text.chars().any(char::is_control);
    if valid {
        Ok(text.to_owned())
    } else {
        Err(format!(
            "{field} must be 1..=256 bytes without control characters"
        ))
    }
}

fn utc(field: &str, text: &str) -> Result<String, String> {
    if is_rfc3339_utc(text) {
        Ok(text.to_owned())
    } else {
        Err(format!("{field} must be RFC 3339 UTC"))
    }
}

fn reason(field: &str, text: &str) -> Result<String, String> {
    let valid =
        !text.is_empty() && text.len() <= MAX_REASON_BYTES && !text.chars().any(char::is_control);
    if valid {
        Ok(text.to_owned())
    } else {
        Err(format!(
            "{field} must be 1..=128 bytes without control characters"
        ))
    }
}

fn finite(field: &str, value: Option<f64>) -> Result<Option<f64>, String> {
    match value {
        Some(number) if !number.is_finite() => Err(format!("{field} must be finite")),
        other => Ok(other),
    }
}

fn interval(start: u64, end: u64) -> Result<(String, String), String> {
    if start < end {
        Ok((format_fixed_width(start), format_fixed_width(end)))
    } else {
        Err(format!("interval [{start}, {end}) is empty or inverted"))
    }
}

/// Live sample kinds only: simulated/replay data never enters the archive.
fn sample_kind_name(kind: SampleKind) -> Result<&'static str, String> {
    match kind {
        SampleKind::Measured => Ok("measured"),
        SampleKind::Derived => Ok("derived"),
        SampleKind::Estimated => Ok("estimated"),
        SampleKind::Simulated | SampleKind::Replay => {
            Err("simulated/replay samples never enter the live archive".to_owned())
        }
    }
}

/// Methods a finished minute bucket may carry (`history.sql` CHECK list).
fn bucket_method_name(method: HistoryMethod) -> Result<&'static str, String> {
    match method {
        HistoryMethod::TimeWeightedStep => Ok("time_weighted_step"),
        HistoryMethod::CounterDelta => Ok("counter_delta"),
        HistoryMethod::EstimatedIntegral => Ok("estimated_integral"),
        HistoryMethod::Gap => Ok("gap"),
        HistoryMethod::Raw | HistoryMethod::BucketExact | HistoryMethod::BoundaryGap => {
            Err(format!("{method:?} is not an archived bucket method"))
        }
    }
}

fn segment_row(segment: &ArchivedSegment) -> Result<SegmentRow, String> {
    if segment.policy_revision == 0 {
        return Err("policy_revision must be at least 1".to_owned());
    }
    Ok(SegmentRow {
        segment_id: identifier("segment_id", &segment.segment_id)?,
        host_id: segment.key.host_id.as_str().to_owned(),
        boot_id: segment.key.boot_id.as_str().to_owned(),
        entity_id: segment.key.entity_id.as_str().to_owned(),
        metric_id: segment.key.metric_id.as_str().to_owned(),
        scope: segment.key.scope.as_str().to_owned(),
        source_id: segment.source_id.as_str().to_owned(),
        source_generation: i64::from(segment.source_generation),
        policy_revision: i64::from(segment.policy_revision),
        unit: segment.unit.to_string(),
        sample_kind: sample_kind_name(segment.sample_kind)?,
        started_utc: utc("started_utc", &segment.started_utc)?,
    })
}

fn bucket_row(bucket: &ArchivedBucket) -> Result<BucketRow, String> {
    let (start_boot_ns, end_boot_ns) = interval(bucket.start_boot_ns, bucket.end_boot_ns)?;
    let span = bucket.end_boot_ns - bucket.start_boot_ns;
    if bucket.covered_ns > MAX_COVERED_NS || bucket.covered_ns > span {
        return Err(format!(
            "covered_ns {} exceeds the bucket span or 60 s",
            bucket.covered_ns
        ));
    }
    Ok(BucketRow {
        segment_id: identifier("segment_id", &bucket.segment_id)?,
        start_boot_ns,
        end_boot_ns,
        start_utc: utc("start_utc", &bucket.start_utc)?,
        end_utc: utc("end_utc", &bucket.end_utc)?,
        covered_ns: format_fixed_width(bucket.covered_ns),
        weighted_sum: finite("weighted_sum", bucket.weighted_sum)?,
        minimum: finite("minimum", bucket.minimum)?,
        maximum: finite("maximum", bucket.maximum)?,
        counter_delta_decimal: bucket.counter_delta.map(|delta| delta.to_string()),
        sample_count: i64::from(bucket.sample_count),
        method: bucket_method_name(bucket.method)?,
        reason_code: bucket
            .reason_code
            .as_deref()
            .map(|code| reason("reason_code", code))
            .transpose()?,
    })
}

fn bridge_row(bridge: &ArchivedBridge) -> Result<BridgeRow, String> {
    let (start_boot_ns, end_boot_ns) = interval(bridge.start_boot_ns, bridge.end_boot_ns)?;
    Ok(BridgeRow {
        segment_id: identifier("segment_id", &bridge.segment_id)?,
        start_boot_ns,
        end_boot_ns,
        delta_decimal: bridge.delta.to_string(),
    })
}

/// Gap ids are content digests so re-appending a journal is idempotent and
/// distinct gaps never collide.
fn gap_id(gap: &GapRecord, host_id: &str, boot_id: &str) -> String {
    let key = gap.key.as_ref().map_or_else(String::new, |key| {
        format!(
            "{}|{}|{}|{}|{}",
            key.host_id, key.boot_id, key.entity_id, key.metric_id, key.scope
        )
    });
    let adapter = gap
        .adapter_id
        .as_ref()
        .map_or("", |adapter| adapter.as_str());
    let canonical = format!(
        "{host_id}\n{boot_id}\n{key}\n{adapter}\n{}\n{}\n{}",
        gap.start_boot_ns, gap.end_boot_ns, gap.reason
    );
    sha256_hex(canonical.as_bytes()).chars().take(32).collect()
}

fn gap_row(gap: &GapRecord, host_id: &str, boot_id: &str) -> Result<GapRow, String> {
    if gap.affected_count == 0 {
        return Err("gap affected_count must be at least 1".to_owned());
    }
    let (start_boot_ns, end_boot_ns) = interval(gap.start_boot_ns, gap.end_boot_ns)?;
    Ok(GapRow {
        gap_id: gap_id(gap, host_id, boot_id),
        host_id: host_id.to_owned(),
        boot_id: boot_id.to_owned(),
        start_boot_ns,
        end_boot_ns,
        reason_code: reason("gap reason", &gap.reason)?,
        affected_count: i64::from(gap.affected_count),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::metric::CanonicalUnit;
    use dgxtop_core::testing::{metric_key, source_id};

    pub(crate) fn segment() -> ArchivedSegment {
        ArchivedSegment {
            segment_id: "seg-1".to_owned(),
            key: metric_key("gpu:GPU-1", "gpu.temperature", "gpu"),
            source_id: source_id("nvml:GPU-1:temp"),
            source_generation: 1,
            policy_revision: 1,
            unit: CanonicalUnit::DegC,
            sample_kind: SampleKind::Measured,
            started_utc: "2026-10-04T12:00:00Z".to_owned(),
        }
    }

    pub(crate) fn bucket(start: u64) -> ArchivedBucket {
        ArchivedBucket {
            segment_id: "seg-1".to_owned(),
            start_boot_ns: start,
            end_boot_ns: start + MAX_COVERED_NS,
            start_utc: "2026-10-04T12:00:00Z".to_owned(),
            end_utc: "2026-10-04T12:01:00Z".to_owned(),
            covered_ns: MAX_COVERED_NS,
            weighted_sum: Some(68.0 * 60.0),
            minimum: Some(65.0),
            maximum: Some(71.0),
            counter_delta: None,
            sample_count: 60,
            method: HistoryMethod::TimeWeightedStep,
            reason_code: None,
        }
    }

    fn journal() -> ArchiveJournal {
        ArchiveJournal {
            revision: 1,
            host_id: "test-host".to_owned(),
            boot_id: "fake-boot".to_owned(),
            segments: vec![segment()],
            buckets: vec![bucket(0)],
            bridges: vec![ArchivedBridge {
                segment_id: "seg-1".to_owned(),
                start_boot_ns: 59_500_000_000,
                end_boot_ns: 60_500_000_000,
                delta: u128::MAX,
            }],
            gaps: vec![GapRecord {
                key: None,
                adapter_id: None,
                start_boot_ns: 1,
                end_boot_ns: 2,
                reason: "timeout".to_owned(),
                affected_count: 3,
            }],
            prunes: Vec::new(),
            expire: None,
            when_full: dgxtop_core::ports::ArchiveWhenFull::EvictOldest,
            permit: None,
        }
    }

    #[test]
    fn converts_every_table_in_foreign_key_order() {
        let batch = ArchiveBatch::from_journal(&journal()).unwrap();
        assert_eq!(batch.rows.len(), 4);
        let ArchiveRow::Segment(segment) = &batch.rows[0] else {
            panic!("segment first");
        };
        assert_eq!(segment.unit, "degC");
        assert_eq!(segment.sample_kind, "measured");
        let ArchiveRow::Bucket(bucket) = &batch.rows[1] else {
            panic!("bucket second");
        };
        assert_eq!(bucket.start_boot_ns, "00000000000000000000");
        assert_eq!(bucket.covered_ns, "00000000060000000000");
        assert_eq!(bucket.method, "time_weighted_step");
        let ArchiveRow::Bridge(bridge) = &batch.rows[2] else {
            panic!("bridge third");
        };
        assert_eq!(
            bridge.delta_decimal,
            "340282366920938463463374607431768211455"
        );
        let ArchiveRow::Gap(gap) = &batch.rows[3] else {
            panic!("gap last");
        };
        assert_eq!(gap.gap_id.len(), 32);
        assert!(storage_estimate(&batch.rows) > 0);
    }

    #[test]
    fn rejects_rows_the_schema_or_spec_forbid() {
        let mutations: Vec<fn(&mut ArchiveJournal)> = vec![
            |journal: &mut ArchiveJournal| journal.segments[0].sample_kind = SampleKind::Replay,
            |journal: &mut ArchiveJournal| journal.segments[0].sample_kind = SampleKind::Simulated,
            |journal: &mut ArchiveJournal| journal.segments[0].policy_revision = 0,
            |journal: &mut ArchiveJournal| journal.segments[0].started_utc = "now".to_owned(),
            |journal: &mut ArchiveJournal| journal.buckets[0].weighted_sum = Some(f64::NAN),
            |journal: &mut ArchiveJournal| journal.buckets[0].maximum = Some(f64::INFINITY),
            |journal: &mut ArchiveJournal| journal.buckets[0].method = HistoryMethod::Raw,
            |journal: &mut ArchiveJournal| journal.buckets[0].method = HistoryMethod::BoundaryGap,
            |journal: &mut ArchiveJournal| journal.buckets[0].covered_ns = MAX_COVERED_NS + 1,
            |journal: &mut ArchiveJournal| {
                journal.buckets[0].end_boot_ns = journal.buckets[0].start_boot_ns
            },
            |journal: &mut ArchiveJournal| {
                journal.bridges[0].end_boot_ns = journal.bridges[0].start_boot_ns
            },
            |journal: &mut ArchiveJournal| journal.gaps[0].affected_count = 0,
            |journal: &mut ArchiveJournal| journal.gaps[0].reason = String::new(),
            |journal: &mut ArchiveJournal| journal.host_id = String::new(),
            |journal: &mut ArchiveJournal| journal.segments[0].segment_id = "x\u{1b}".to_owned(),
        ];
        for (index, mutate) in mutations.iter().enumerate() {
            let mut candidate = journal();
            mutate(&mut candidate);
            assert!(
                ArchiveBatch::from_journal(&candidate).is_err(),
                "mutation {index} must be rejected"
            );
        }
    }

    #[test]
    fn gap_ids_are_stable_and_distinct() {
        let base = journal();
        let first = gap_id(&base.gaps[0], "h", "b");
        assert_eq!(first, gap_id(&base.gaps[0], "h", "b"));
        let mut other = base.gaps[0].clone();
        other.reason = "dropped".to_owned();
        assert_ne!(first, gap_id(&other, "h", "b"));
        assert_ne!(first, gap_id(&base.gaps[0], "h", "b2"));
    }
}
