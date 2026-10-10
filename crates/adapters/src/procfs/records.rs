//! Observation construction shared by the procfs, network and filesystem
//! adapters: every record goes through `RecordFactory`; a failure is a record
//! with `value = None` and a reason, never a substitute number.

use std::io;

use dgxtop_core::metric::Scalar;
use dgxtop_core::observation::{Observation, ReadPlan, SourceDescriptor};
use dgxtop_core::quality::{Quality, ReadStatus};

use super::mechanics::{StickyFailure, TimedRead, io_failure};
use super::parse::Field;
use crate::common::{RecordFactory, parse_u64};

/// Reason code of a column the kernel does not provide.
pub(crate) const FIELD_ABSENT: &str = "FIELD_ABSENT";
/// Reason code of a value that is present but not a number.
pub(crate) const PARSE_ERROR: &str = "PARSE_ERROR";
/// Added to a cached sticky failure while its re-probe is deferred.
pub(crate) const REPROBE_DEFERRED: &str = "REPROBE_DEFERRED";
/// Reason code of a catalogued device that is no longer present.
pub(crate) const DEVICE_NOT_PRESENT: &str = "DEVICE_NOT_PRESENT";

/// Boot-clock interval of the read that produced a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReadWindow {
    pub started_boot_ns: u64,
    pub finished_boot_ns: u64,
}

impl<T> From<&TimedRead<T>> for ReadWindow {
    fn from(read: &TimedRead<T>) -> Self {
        Self {
            started_boot_ns: read.started_boot_ns,
            finished_boot_ns: read.finished_boot_ns,
        }
    }
}

/// Successful reading (sources without a metric key yield nothing).
pub(crate) fn ok_record(
    plan: &ReadPlan,
    descriptor: &SourceDescriptor,
    value: Scalar,
    quality: Quality,
    window: ReadWindow,
    reasons: Vec<String>,
) -> Option<Observation> {
    let key = descriptor.metric.clone()?;
    Some(RecordFactory::ok(
        plan,
        descriptor,
        key,
        value,
        quality,
        window.started_boot_ns,
        window.finished_boot_ns,
        reasons,
    ))
}

/// Failed reading with an explicit status and reason.
pub(crate) fn failed_record(
    plan: &ReadPlan,
    descriptor: &SourceDescriptor,
    status: ReadStatus,
    reason: &str,
    window: ReadWindow,
) -> Option<Observation> {
    let key = descriptor.metric.clone()?;
    Some(RecordFactory::failed(
        plan,
        descriptor,
        key,
        status,
        window.started_boot_ns,
        window.finished_boot_ns,
        reason.to_owned(),
    ))
}

/// Failed reading caused by an I/O error.
pub(crate) fn io_failed_record(
    plan: &ReadPlan,
    descriptor: &SourceDescriptor,
    error: &io::Error,
    window: ReadWindow,
) -> Option<Observation> {
    let (status, reason) = io_failure(error);
    failed_record(plan, descriptor, status, &reason, window)
}

/// The parent still holds this source but the device is gone.
pub(crate) fn removed_record(
    plan: &ReadPlan,
    descriptor: &SourceDescriptor,
    now_boot_ns: u64,
) -> Option<Observation> {
    let window = ReadWindow {
        started_boot_ns: now_boot_ns,
        finished_boot_ns: now_boot_ns,
    };
    failed_record(
        plan,
        descriptor,
        ReadStatus::Removed,
        DEVICE_NOT_PRESENT,
        window,
    )
}

/// Cached sticky failure reported without touching the file again.
pub(crate) fn deferred_record(
    plan: &ReadPlan,
    descriptor: &SourceDescriptor,
    failure: &StickyFailure,
    now_boot_ns: u64,
) -> Option<Observation> {
    let mut record = failed_record(
        plan,
        descriptor,
        failure.status,
        &failure.reason,
        ReadWindow {
            started_boot_ns: now_boot_ns,
            finished_boot_ns: now_boot_ns,
        },
    )?;
    record.reason_codes.push(REPROBE_DEFERRED.to_owned());
    Some(record)
}

/// Record of one exact `u64` kernel field: absent columns are unsupported,
/// garbage is an error; both carry no value.
pub(crate) fn u64_field_record(
    plan: &ReadPlan,
    descriptor: &SourceDescriptor,
    field: Field<u64>,
    window: ReadWindow,
) -> Option<Observation> {
    match field {
        Field::Value(value) => ok_record(
            plan,
            descriptor,
            Scalar::from_u64(value),
            Quality::Verified,
            window,
            Vec::new(),
        ),
        Field::Absent => failed_record(
            plan,
            descriptor,
            ReadStatus::Unsupported,
            FIELD_ABSENT,
            window,
        ),
        Field::Invalid => failed_record(plan, descriptor, ReadStatus::Error, PARSE_ERROR, window),
    }
}

/// Failure of a value derivation: status plus bounded reason code.
pub(crate) type DerivationFailure = (ReadStatus, &'static str);

/// Record of a value derived from one read (e.g. pages × page size).
pub(crate) fn u64_outcome_record(
    plan: &ReadPlan,
    descriptor: &SourceDescriptor,
    outcome: Result<u64, DerivationFailure>,
    window: ReadWindow,
) -> Option<Observation> {
    match outcome {
        Ok(value) => ok_record(
            plan,
            descriptor,
            Scalar::from_u64(value),
            Quality::Verified,
            window,
            Vec::new(),
        ),
        Err((status, reason)) => failed_record(plan, descriptor, status, reason, window),
    }
}

/// Record of a single-number pseudo file (sysfs attribute) with the given
/// quality and notes; text that is not a number is an error.
pub(crate) fn u64_text_record(
    plan: &ReadPlan,
    descriptor: &SourceDescriptor,
    text: &str,
    quality: Quality,
    window: ReadWindow,
    notes: Vec<String>,
) -> Option<Observation> {
    match parse_u64(text) {
        Some(value) => ok_record(
            plan,
            descriptor,
            Scalar::from_u64(value),
            quality,
            window,
            notes,
        ),
        None => failed_record(plan, descriptor, ReadStatus::Error, PARSE_ERROR, window),
    }
}

/// Record of one finite `f64` kernel field.
pub(crate) fn f64_field_record(
    plan: &ReadPlan,
    descriptor: &SourceDescriptor,
    field: Field<f64>,
    window: ReadWindow,
) -> Option<Observation> {
    let value = match field {
        Field::Value(value) => Scalar::from_f64(value).ok(),
        Field::Absent => {
            return failed_record(
                plan,
                descriptor,
                ReadStatus::Unsupported,
                FIELD_ABSENT,
                window,
            );
        }
        Field::Invalid => None,
    };
    match value {
        Some(value) => ok_record(
            plan,
            descriptor,
            value,
            Quality::Verified,
            window,
            Vec::new(),
        ),
        None => failed_record(plan, descriptor, ReadStatus::Error, PARSE_ERROR, window),
    }
}
