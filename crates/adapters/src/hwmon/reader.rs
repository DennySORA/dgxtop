//! Executes one source's [`ReadSpec`] against sysfs: the value plus optional
//! fault/alarm companions and an enable gate. Failures become a typed
//! [`ReadFailure`]; a number is never substituted for a missing value.
//!
//! Shared by the hwmon, thermal and powercap adapters.

use dgxtop_core::metric::Scalar;
use dgxtop_core::quality::ReadStatus;

use crate::common::{
    SystemFiles, exact_i64_to_f64, parse_i64, parse_u64, read_status_of, reason_of,
};

/// Reason codes attached by the sysfs read path.
pub(crate) mod reasons {
    /// The driver flags the sensor as faulty (`*_fault == 1`): value kept, quality invalid.
    pub const SENSOR_FAULT: &str = "SENSOR_FAULT";
    /// An alarm flag is raised; this is not a fault and the value stays valid.
    pub const ALARM: &str = "ALARM";
    /// The fault flag exists but could not be read or parsed.
    pub const FAULT_STATE_UNKNOWN: &str = "FAULT_STATE_UNKNOWN";
    /// The value file did not contain a decimal number.
    pub const PARSE_ERROR: &str = "PARSE_ERROR";
    /// The value cannot be represented exactly.
    pub const VALUE_OUT_OF_RANGE: &str = "VALUE_OUT_OF_RANGE";
    /// A thermal zone whose `mode` is `disabled`.
    pub const ZONE_DISABLED: &str = "ZONE_DISABLED";
    /// The read deadline passed before this source was read.
    pub const DEADLINE_EXCEEDED: &str = "DEADLINE_EXCEEDED";
}

/// How a value file is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueFormat {
    /// Signed decimal gauge (temperatures may be negative; 0 is valid).
    SignedGauge,
    /// Unsigned decimal counter kept exact as `u64`.
    UnsignedCounter,
}

/// One alarm flag file and the specific reason it raises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlarmPath {
    pub path: String,
    pub reason: &'static str,
}

/// Files read for one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadSpec {
    pub value_path: String,
    pub format: ValueFormat,
    pub fault_path: Option<String>,
    pub alarm_paths: Vec<AlarmPath>,
    /// File whose content `disabled` makes the source unsupported (thermal `mode`).
    pub gate_path: Option<String>,
}

impl ReadSpec {
    /// Spec reading only a value file.
    pub(crate) fn value(value_path: String, format: ValueFormat) -> Self {
        Self {
            value_path,
            format,
            fault_path: None,
            alarm_paths: Vec::new(),
            gate_path: None,
        }
    }
}

/// A successful reading.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reading {
    pub value: Scalar,
    /// The driver reported a fault for this channel.
    pub faulted: bool,
    pub reasons: Vec<String>,
}

/// A failed reading: status plus a bounded reason code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadFailure {
    pub status: ReadStatus,
    pub reason: String,
}

impl ReadFailure {
    pub(crate) fn new(status: ReadStatus, reason: &str) -> Self {
        Self {
            status,
            reason: reason.to_owned(),
        }
    }

    fn from_io(error: &std::io::Error) -> Self {
        Self {
            status: read_status_of(error),
            reason: reason_of(error),
        }
    }
}

/// Reads one source: gate, value, then fault and alarm companions.
pub(crate) fn read_source(
    files: &dyn SystemFiles,
    spec: &ReadSpec,
) -> Result<Reading, ReadFailure> {
    check_gate(files, spec.gate_path.as_deref())?;
    let text = files
        .read_to_string(&spec.value_path)
        .map_err(|error| ReadFailure::from_io(&error))?;
    let value = decode(&text, spec.format)?;
    let mut reasons = Vec::new();
    let faulted = read_fault(files, spec.fault_path.as_deref(), &mut reasons);
    read_alarms(files, &spec.alarm_paths, &mut reasons);
    Ok(Reading {
        value,
        faulted,
        reasons,
    })
}

/// A gate reading `disabled` makes the source unsupported; an absent or
/// unreadable gate does not block the value read.
fn check_gate(files: &dyn SystemFiles, gate_path: Option<&str>) -> Result<(), ReadFailure> {
    let Some(path) = gate_path else {
        return Ok(());
    };
    match files.read_to_string(path) {
        Ok(text) if text.trim() == "disabled" => Err(ReadFailure::new(
            ReadStatus::Unsupported,
            reasons::ZONE_DISABLED,
        )),
        _ => Ok(()),
    }
}

/// Decodes a value file without substituting anything on failure.
pub(crate) fn decode(text: &str, format: ValueFormat) -> Result<Scalar, ReadFailure> {
    match format {
        ValueFormat::SignedGauge => {
            let raw = parse_i64(text)
                .ok_or_else(|| ReadFailure::new(ReadStatus::Error, reasons::PARSE_ERROR))?;
            let exact = exact_i64_to_f64(raw)
                .ok_or_else(|| ReadFailure::new(ReadStatus::Error, reasons::VALUE_OUT_OF_RANGE))?;
            Scalar::from_f64(exact)
                .map_err(|_| ReadFailure::new(ReadStatus::Error, reasons::VALUE_OUT_OF_RANGE))
        }
        ValueFormat::UnsignedCounter => parse_u64(text)
            .map(Scalar::from_u64)
            .ok_or_else(|| ReadFailure::new(ReadStatus::Error, reasons::PARSE_ERROR)),
    }
}

/// Reads the fault flag; `true` only for an explicit non-zero flag.
fn read_fault(
    files: &dyn SystemFiles,
    fault_path: Option<&str>,
    reasons: &mut Vec<String>,
) -> bool {
    let Some(path) = fault_path else {
        return false;
    };
    match files
        .read_to_string(path)
        .ok()
        .and_then(|text| parse_u64(&text))
    {
        Some(0) => false,
        Some(_) => {
            reasons.push(reasons::SENSOR_FAULT.to_owned());
            true
        }
        None => {
            reasons.push(reasons::FAULT_STATE_UNKNOWN.to_owned());
            false
        }
    }
}

/// Collects raised alarm flags: the generic `ALARM` once plus each specific code.
fn read_alarms(files: &dyn SystemFiles, alarm_paths: &[AlarmPath], reasons: &mut Vec<String>) {
    for alarm in alarm_paths {
        let raised = files
            .read_to_string(&alarm.path)
            .ok()
            .and_then(|text| parse_u64(&text))
            .is_some_and(|flag| flag != 0);
        if !raised {
            continue;
        }
        if !reasons.iter().any(|reason| reason == reasons::ALARM) {
            reasons.push(reasons::ALARM.to_owned());
        }
        if alarm.reason != reasons::ALARM {
            reasons.push(alarm.reason.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_gauges_keep_zero_and_negative_values() {
        assert_eq!(
            decode("0\n", ValueFormat::SignedGauge)
                .unwrap()
                .to_f64_lossy(),
            0.0
        );
        assert_eq!(
            decode("-12500", ValueFormat::SignedGauge)
                .unwrap()
                .to_f64_lossy(),
            -12500.0
        );
    }

    #[test]
    fn garbage_and_huge_values_fail_instead_of_becoming_numbers() {
        assert_eq!(
            decode("n/a", ValueFormat::SignedGauge).unwrap_err().reason,
            reasons::PARSE_ERROR
        );
        assert_eq!(
            decode(&i64::MAX.to_string(), ValueFormat::SignedGauge)
                .unwrap_err()
                .reason,
            reasons::VALUE_OUT_OF_RANGE
        );
        assert_eq!(
            decode("-1", ValueFormat::UnsignedCounter)
                .unwrap_err()
                .status,
            ReadStatus::Error
        );
    }

    #[test]
    fn counters_stay_exact() {
        let value = decode("18446744073709551615", ValueFormat::UnsignedCounter).unwrap();
        assert_eq!(value.as_u64(), Some(u64::MAX));
    }
}
