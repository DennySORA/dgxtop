//! Scalars, units, explicit unit conversions and the metric catalog.
//!
//! Values are either finite `f64` or exact `u64`. Large cumulative counters stay
//! `u64` until a delta is computed; conversions are explicit, identified and
//! never guessed for unknown units.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::quality::RawKind;

/// Errors for scalar construction and unit conversion.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValueError {
    #[error("value is not finite")]
    NonFinite,
    #[error("unknown native unit {native:?} for canonical unit {canonical}")]
    UnknownUnit {
        native: String,
        canonical: CanonicalUnit,
    },
    #[error("native unit text must be 1..=32 bytes")]
    NativeUnitLength,
    #[error("converted value overflows u64")]
    Overflow,
}

/// A finite `f64` or an exact `u64`.
#[derive(Clone, Copy, PartialEq)]
pub struct Scalar(ScalarRepr);

#[derive(Clone, Copy, PartialEq)]
enum ScalarRepr {
    F64(f64),
    U64(u64),
}

/// Borrowed view used for exhaustive matching on a [`Scalar`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScalarView {
    F64(f64),
    U64(u64),
}

impl Scalar {
    /// Accepts only finite values; NaN and infinities are rejected.
    pub fn from_f64(value: f64) -> Result<Self, ValueError> {
        if value.is_finite() {
            Ok(Self(ScalarRepr::F64(value)))
        } else {
            Err(ValueError::NonFinite)
        }
    }

    pub const fn from_u64(value: u64) -> Self {
        Self(ScalarRepr::U64(value))
    }

    pub const fn view(self) -> ScalarView {
        match self.0 {
            ScalarRepr::F64(value) => ScalarView::F64(value),
            ScalarRepr::U64(value) => ScalarView::U64(value),
        }
    }

    /// Exact `u64` if this scalar is one.
    pub const fn as_u64(self) -> Option<u64> {
        match self.0 {
            ScalarRepr::U64(value) => Some(value),
            ScalarRepr::F64(_) => None,
        }
    }

    /// Numeric value as `f64`; `u64` above 2^53 loses precision, so callers use
    /// this only for display or for gauges, never for counter deltas.
    #[allow(clippy::cast_precision_loss)]
    pub const fn to_f64_lossy(self) -> f64 {
        match self.0 {
            ScalarRepr::F64(value) => value,
            ScalarRepr::U64(value) => value as f64,
        }
    }

    /// Rough heap footprint used for byte accounting (scalars are inline).
    pub const fn heap_bytes(self) -> usize {
        0
    }
}

impl fmt::Debug for Scalar {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            ScalarRepr::F64(value) => write!(formatter, "f64({value})"),
            ScalarRepr::U64(value) => write!(formatter, "u64({value})"),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ScalarWire {
    #[serde(rename = "f64")]
    F64 { value: f64 },
    #[serde(rename = "u64")]
    U64 {
        #[serde(with = "crate::wire::u64_decimal")]
        value: u64,
    },
}

impl Serialize for Scalar {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = match self.0 {
            ScalarRepr::F64(value) => ScalarWire::F64 { value },
            ScalarRepr::U64(value) => ScalarWire::U64 { value },
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Every observation carries a scalar: decoded field by field, in
        // either key order, without buffering the object first (which an
        // internally tagged serde enum would do).
        deserializer.deserialize_map(scalar_wire::ScalarVisitor)
    }
}

mod scalar_wire {
    use std::fmt;

    use serde::de::{self, MapAccess, Visitor};
    use serde::{Deserialize, Deserializer};

    use super::Scalar;
    use crate::wire::{DecimalError, parse_decimal_u64};

    const FIELDS: &[&str] = &["type", "value"];
    const KINDS: &[&str] = &["f64", "u64"];

    enum Field {
        Type,
        Value,
    }

    enum Kind {
        F64,
        U64,
    }

    /// A `value` read before its `type` is known: f64 values are JSON
    /// numbers, u64 values decimal strings (parsed at once, nothing kept).
    enum Raw {
        Number(f64),
        Decimal(Result<u64, DecimalError>),
    }

    /// Visits one of a fixed set of names without allocating.
    struct NameVisitor<T>(fn(&str) -> Option<T>, &'static [&'static str], bool);

    impl<T> Visitor<'_> for NameVisitor<T> {
        type Value = T;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "one of {:?}", self.1)
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<T, E> {
            (self.0)(value).ok_or_else(|| {
                if self.2 {
                    E::unknown_field(value, self.1)
                } else {
                    E::unknown_variant(value, self.1)
                }
            })
        }
    }

    impl<'de> Deserialize<'de> for Field {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let parse = |name: &str| match name {
                "type" => Some(Self::Type),
                "value" => Some(Self::Value),
                _ => None,
            };
            deserializer.deserialize_identifier(NameVisitor(parse, FIELDS, true))
        }
    }

    impl<'de> Deserialize<'de> for Kind {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let parse = |name: &str| match name {
                "f64" => Some(Self::F64),
                "u64" => Some(Self::U64),
                _ => None,
            };
            deserializer.deserialize_str(NameVisitor(parse, KINDS, false))
        }
    }

    struct RawVisitor;

    impl Visitor<'_> for RawVisitor {
        type Value = Raw;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a JSON number or a decimal string")
        }

        fn visit_f64<E: de::Error>(self, value: f64) -> Result<Raw, E> {
            Ok(Raw::Number(value))
        }

        // JSON integers of an f64 value: the schema types them as numbers.
        #[allow(clippy::cast_precision_loss)]
        fn visit_u64<E: de::Error>(self, value: u64) -> Result<Raw, E> {
            Ok(Raw::Number(value as f64))
        }

        #[allow(clippy::cast_precision_loss)]
        fn visit_i64<E: de::Error>(self, value: i64) -> Result<Raw, E> {
            Ok(Raw::Number(value as f64))
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Raw, E> {
            Ok(Raw::Decimal(parse_decimal_u64(value)))
        }
    }

    impl<'de> Deserialize<'de> for Raw {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_any(RawVisitor)
        }
    }

    pub(super) struct ScalarVisitor;

    impl<'de> Visitor<'de> for ScalarVisitor {
        type Value = Scalar;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a scalar object with `type` and `value`")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Scalar, A::Error> {
            let mut kind = None;
            let mut raw = None;
            while let Some(field) = map.next_key::<Field>()? {
                match field {
                    Field::Type if kind.is_none() => kind = Some(map.next_value::<Kind>()?),
                    Field::Value if raw.is_none() => raw = Some(map.next_value::<Raw>()?),
                    Field::Type => return Err(de::Error::duplicate_field("type")),
                    Field::Value => return Err(de::Error::duplicate_field("value")),
                }
            }
            let kind = kind.ok_or_else(|| de::Error::missing_field("type"))?;
            let raw = raw.ok_or_else(|| de::Error::missing_field("value"))?;
            match (kind, raw) {
                (Kind::F64, Raw::Number(value)) => {
                    Scalar::from_f64(value).map_err(de::Error::custom)
                }
                (Kind::U64, Raw::Decimal(parsed)) => {
                    parsed.map(Scalar::from_u64).map_err(de::Error::custom)
                }
                (Kind::F64, Raw::Decimal(_)) => Err(de::Error::custom(
                    "an f64 scalar value must be a JSON number",
                )),
                (Kind::U64, Raw::Number(_)) => Err(de::Error::custom(
                    "a u64 scalar value must be a decimal string",
                )),
            }
        }
    }
}

/// Canonical units allowed in canonical samples (`canonical.schema.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CanonicalUnit {
    #[serde(rename = "degC")]
    DegC,
    #[serde(rename = "watts")]
    Watts,
    #[serde(rename = "bytes")]
    Bytes,
    #[serde(rename = "bytes_per_second")]
    BytesPerSecond,
    #[serde(rename = "percent")]
    Percent,
    #[serde(rename = "MHz")]
    MHz,
    #[serde(rename = "count")]
    Count,
    #[serde(rename = "count_per_second")]
    CountPerSecond,
    #[serde(rename = "milliseconds")]
    Milliseconds,
    #[serde(rename = "joules")]
    Joules,
}

impl CanonicalUnit {
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::DegC => "°C",
            Self::Watts => "W",
            Self::Bytes => "B",
            Self::BytesPerSecond => "B/s",
            Self::Percent => "%",
            Self::MHz => "MHz",
            Self::Count => "",
            Self::CountPerSecond => "/s",
            Self::Milliseconds => "ms",
            Self::Joules => "J",
        }
    }
}

impl fmt::Display for CanonicalUnit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::DegC => "degC",
            Self::Watts => "watts",
            Self::Bytes => "bytes",
            Self::BytesPerSecond => "bytes_per_second",
            Self::Percent => "percent",
            Self::MHz => "MHz",
            Self::Count => "count",
            Self::CountPerSecond => "count_per_second",
            Self::Milliseconds => "milliseconds",
            Self::Joules => "joules",
        };
        formatter.write_str(name)
    }
}

/// Source-declared native unit text (1..=32 bytes), e.g. `mdegC`, `uJ`, `KiB`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NativeUnit(Arc<str>);

impl Serialize for NativeUnit {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NativeUnit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(crate::identity::IdentityVisitor(
            Self::decoded,
            std::marker::PhantomData,
        ))
    }
}

fn validate_native_unit(value: &str) -> Result<(), ValueError> {
    if value.is_empty() || value.len() > 32 || value.chars().any(char::is_control) {
        return Err(ValueError::NativeUnitLength);
    }
    Ok(())
}

impl NativeUnit {
    pub fn new(value: impl Into<String>) -> Result<Self, ValueError> {
        let value = value.into();
        validate_native_unit(&value)?;
        Ok(Self(Arc::from(value)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Decoding path: validated once per distinct value, then shared.
    fn decoded(value: &str) -> Result<Self, ValueError> {
        thread_local! {
            static CACHE: crate::identity::InternCache =
                std::cell::RefCell::new(std::collections::HashSet::new());
        }
        crate::identity::intern(&CACHE, value, validate_native_unit).map(Self)
    }
}

impl TryFrom<String> for NativeUnit {
    type Error = ValueError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<NativeUnit> for String {
    fn from(value: NativeUnit) -> Self {
        value.0.to_string()
    }
}

impl fmt::Debug for NativeUnit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, formatter)
    }
}

impl fmt::Display for NativeUnit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Well-known native unit spellings used by the built-in adapters.
pub mod native {
    pub const DEG_C: &str = "degC";
    pub const MILLI_DEG_C: &str = "mdegC";
    pub const WATT: &str = "W";
    pub const MILLI_WATT: &str = "mW";
    pub const MICRO_WATT: &str = "uW";
    pub const JOULE: &str = "J";
    pub const MILLI_JOULE: &str = "mJ";
    pub const MICRO_JOULE: &str = "uJ";
    pub const MHZ: &str = "MHz";
    pub const KHZ: &str = "kHz";
    pub const HZ: &str = "Hz";
    pub const BYTES: &str = "bytes";
    /// `/proc/meminfo` "kB" fields are KiB (1024 bytes).
    pub const KIB: &str = "KiB";
    /// Linux diskstats sectors are always 512 bytes.
    pub const SECTORS_512: &str = "sectors_512";
    pub const BYTES_PER_SECOND: &str = "bytes_per_second";
    pub const KIB_PER_SECOND: &str = "KiB_per_second";
    /// SI kilobytes per second; only usable when a profile verified the definition.
    pub const KB_SI_PER_SECOND: &str = "KB_si_per_second";
    /// Unverified "KB/s" from a vendor API whose KB definition is not proven.
    pub const KB_UNVERIFIED_PER_SECOND: &str = "KB_unverified_per_second";
    pub const PERCENT: &str = "percent";
    pub const COUNT: &str = "count";
    /// Scheduler clock ticks (`USER_HZ`/`CLK_TCK`).
    pub const TICKS: &str = "ticks";
    pub const COUNT_PER_SECOND: &str = "count_per_second";
    pub const MILLISECONDS: &str = "milliseconds";
    pub const MICROSECONDS: &str = "microseconds";
    pub const NANOSECONDS: &str = "nanoseconds";
}

/// One explicit, identified conversion `value * multiply / divide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conversion {
    pub id: &'static str,
    pub native: &'static str,
    pub canonical: CanonicalUnit,
    pub multiply: u64,
    pub divide: u64,
}

const CONVERSIONS: &[Conversion] = &[
    Conversion {
        id: "identity.degC",
        native: native::DEG_C,
        canonical: CanonicalUnit::DegC,
        multiply: 1,
        divide: 1,
    },
    Conversion {
        id: "mdegC_to_degC",
        native: native::MILLI_DEG_C,
        canonical: CanonicalUnit::DegC,
        multiply: 1,
        divide: 1000,
    },
    Conversion {
        id: "identity.watts",
        native: native::WATT,
        canonical: CanonicalUnit::Watts,
        multiply: 1,
        divide: 1,
    },
    Conversion {
        id: "mW_to_W",
        native: native::MILLI_WATT,
        canonical: CanonicalUnit::Watts,
        multiply: 1,
        divide: 1000,
    },
    Conversion {
        id: "uW_to_W",
        native: native::MICRO_WATT,
        canonical: CanonicalUnit::Watts,
        multiply: 1,
        divide: 1_000_000,
    },
    Conversion {
        id: "identity.joules",
        native: native::JOULE,
        canonical: CanonicalUnit::Joules,
        multiply: 1,
        divide: 1,
    },
    Conversion {
        id: "mJ_to_J",
        native: native::MILLI_JOULE,
        canonical: CanonicalUnit::Joules,
        multiply: 1,
        divide: 1000,
    },
    Conversion {
        id: "uJ_to_J",
        native: native::MICRO_JOULE,
        canonical: CanonicalUnit::Joules,
        multiply: 1,
        divide: 1_000_000,
    },
    Conversion {
        id: "identity.MHz",
        native: native::MHZ,
        canonical: CanonicalUnit::MHz,
        multiply: 1,
        divide: 1,
    },
    Conversion {
        id: "kHz_to_MHz",
        native: native::KHZ,
        canonical: CanonicalUnit::MHz,
        multiply: 1,
        divide: 1000,
    },
    Conversion {
        id: "Hz_to_MHz",
        native: native::HZ,
        canonical: CanonicalUnit::MHz,
        multiply: 1,
        divide: 1_000_000,
    },
    Conversion {
        id: "identity.bytes",
        native: native::BYTES,
        canonical: CanonicalUnit::Bytes,
        multiply: 1,
        divide: 1,
    },
    Conversion {
        id: "KiB_to_bytes",
        native: native::KIB,
        canonical: CanonicalUnit::Bytes,
        multiply: 1024,
        divide: 1,
    },
    Conversion {
        id: "sectors512_to_bytes",
        native: native::SECTORS_512,
        canonical: CanonicalUnit::Bytes,
        multiply: 512,
        divide: 1,
    },
    Conversion {
        id: "identity.bytes_per_second",
        native: native::BYTES_PER_SECOND,
        canonical: CanonicalUnit::BytesPerSecond,
        multiply: 1,
        divide: 1,
    },
    Conversion {
        id: "KiBps_to_Bps",
        native: native::KIB_PER_SECOND,
        canonical: CanonicalUnit::BytesPerSecond,
        multiply: 1024,
        divide: 1,
    },
    Conversion {
        id: "KBsi_ps_to_Bps",
        native: native::KB_SI_PER_SECOND,
        canonical: CanonicalUnit::BytesPerSecond,
        multiply: 1000,
        divide: 1,
    },
    Conversion {
        id: "identity.percent",
        native: native::PERCENT,
        canonical: CanonicalUnit::Percent,
        multiply: 1,
        divide: 1,
    },
    Conversion {
        id: "identity.count",
        native: native::COUNT,
        canonical: CanonicalUnit::Count,
        multiply: 1,
        divide: 1,
    },
    Conversion {
        id: "identity.count_per_second",
        native: native::COUNT_PER_SECOND,
        canonical: CanonicalUnit::CountPerSecond,
        multiply: 1,
        divide: 1,
    },
    Conversion {
        id: "identity.milliseconds",
        native: native::MILLISECONDS,
        canonical: CanonicalUnit::Milliseconds,
        multiply: 1,
        divide: 1,
    },
    Conversion {
        id: "us_to_ms",
        native: native::MICROSECONDS,
        canonical: CanonicalUnit::Milliseconds,
        multiply: 1,
        divide: 1000,
    },
    Conversion {
        id: "ns_to_ms",
        native: native::NANOSECONDS,
        canonical: CanonicalUnit::Milliseconds,
        multiply: 1,
        divide: 1_000_000,
    },
];

/// Looks up the explicit conversion from `native` to `canonical`.
/// Unknown or unverified units (e.g. `KB_unverified_per_second`) return `None`.
pub fn conversion_for(native: &str, canonical: CanonicalUnit) -> Option<&'static Conversion> {
    CONVERSIONS
        .iter()
        .find(|conversion| conversion.native == native && conversion.canonical == canonical)
}

impl Conversion {
    /// Applies the conversion. `u64` input with an integral factor stays exact
    /// (checked); everything else becomes a finite `f64`.
    #[allow(clippy::cast_precision_loss)]
    pub fn apply(&self, value: Scalar) -> Result<Scalar, ValueError> {
        match value.view() {
            ScalarView::U64(raw) if self.divide == 1 => raw
                .checked_mul(self.multiply)
                .map(Scalar::from_u64)
                .ok_or(ValueError::Overflow),
            ScalarView::U64(raw) => {
                Scalar::from_f64(raw as f64 * self.multiply as f64 / self.divide as f64)
            }
            ScalarView::F64(raw) => {
                Scalar::from_f64(raw * self.multiply as f64 / self.divide as f64)
            }
        }
    }

    /// Converts an exact native delta (already computed in `u64`) to canonical.
    #[allow(clippy::cast_precision_loss)]
    pub fn apply_delta(&self, delta: u64) -> Result<f64, ValueError> {
        let value = delta as f64 * self.multiply as f64 / self.divide as f64;
        if value.is_finite() {
            Ok(value)
        } else {
            Err(ValueError::NonFinite)
        }
    }
}

/// Normalizes a native value; unknown units are rejected rather than guessed.
pub fn normalize(
    value: Scalar,
    native_unit: &str,
    canonical: CanonicalUnit,
) -> Result<(Scalar, &'static str), ValueError> {
    let conversion =
        conversion_for(native_unit, canonical).ok_or_else(|| ValueError::UnknownUnit {
            native: native_unit.to_owned(),
            canonical,
        })?;
    Ok((conversion.apply(value)?, conversion.id))
}

/// Which side of the pipeline a metric id lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricRole {
    /// Published canonical metric (may also be a pass-through raw value).
    Canonical,
    /// Adapter-level raw input that collectors consume (counters, meminfo fields).
    Raw,
}

/// Functional domain of a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    Cpu,
    Soc,
    Memory,
    Gpu,
    Disk,
    Filesystem,
    Network,
    Process,
    Sensor,
}

/// Catalog entry for one metric id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricDescriptor {
    pub id: &'static str,
    pub unit: CanonicalUnit,
    pub role: MetricRole,
    pub kind: RawKind,
    pub domain: Domain,
    /// Eligible for the 300 s raw / 24 h minute history (non-process series).
    pub history: bool,
    pub label: &'static str,
}

/// Metric id constants. Canonical ids follow `spec/dgxtop-v2/contracts/METRICS.md`.
pub mod ids {
    // CPU (canonical)
    pub const CPU_UTILIZATION_TOTAL: &str = "cpu.utilization.total";
    pub const CPU_UTILIZATION_USER: &str = "cpu.utilization.user";
    pub const CPU_UTILIZATION_SYSTEM: &str = "cpu.utilization.system";
    pub const CPU_UTILIZATION_IOWAIT: &str = "cpu.utilization.iowait";
    pub const CPU_UTILIZATION_STEAL: &str = "cpu.utilization.steal";
    pub const CPU_FREQUENCY_CURRENT: &str = "cpu.frequency.current";
    pub const CPU_FREQUENCY_MAXIMUM: &str = "cpu.frequency.maximum";
    pub const CPU_TEMPERATURE_PACKAGE: &str = "cpu.temperature.package";
    pub const CPU_TEMPERATURE_CORE: &str = "cpu.temperature.core";
    /// A sensor of a group of a package's cores, scope `cluster:<name>`
    /// (D-68).
    pub const CPU_TEMPERATURE_CLUSTER: &str = "cpu.temperature.cluster";
    pub const SOC_TEMPERATURE: &str = "soc.temperature";
    pub const CPU_POWER_PACKAGE: &str = "cpu.power.package";
    pub const CPU_POWER_PLATFORM: &str = "cpu.power.platform";
    pub const CPU_LOAD_1M: &str = "cpu.load.1m";
    pub const CPU_LOAD_5M: &str = "cpu.load.5m";
    pub const CPU_LOAD_15M: &str = "cpu.load.15m";
    pub const CPU_TASKS_RUNNING: &str = "cpu.tasks.running";
    pub const CPU_TASKS_TOTAL: &str = "cpu.tasks.total";
    // CPU (raw)
    pub const CPU_TIME_USER: &str = "cpu.time.user";
    pub const CPU_TIME_NICE: &str = "cpu.time.nice";
    pub const CPU_TIME_SYSTEM: &str = "cpu.time.system";
    pub const CPU_TIME_IDLE: &str = "cpu.time.idle";
    pub const CPU_TIME_IOWAIT: &str = "cpu.time.iowait";
    pub const CPU_TIME_IRQ: &str = "cpu.time.irq";
    pub const CPU_TIME_SOFTIRQ: &str = "cpu.time.softirq";
    pub const CPU_TIME_STEAL: &str = "cpu.time.steal";
    /// CPU cluster power component (e.g. GB10 `cpu_p`/`cpu_e`); summed only by a profile formula.
    pub const CPU_POWER_CLUSTER: &str = "cpu.power.cluster";
    pub const CPU_ENERGY_PACKAGE: &str = "cpu.energy.package";
    pub const CPU_ENERGY_PLATFORM: &str = "cpu.energy.platform";
    /// Unclassified temperature channel: diagnostics only, never CPU package.
    pub const SENSOR_TEMPERATURE: &str = "sensor.temperature";
    /// Unclassified power channel: diagnostics only.
    pub const SENSOR_POWER: &str = "sensor.power";
    /// Unclassified energy counter: diagnostics only.
    pub const SENSOR_ENERGY: &str = "sensor.energy";
    /// Fan tachometer (RPM is a count per minute; kept as diagnostics).
    pub const SENSOR_FAN: &str = "sensor.fan";

    // Memory
    pub const MEMORY_TOTAL: &str = "memory.total";
    pub const MEMORY_AVAILABLE: &str = "memory.available";
    pub const MEMORY_USED: &str = "memory.used";
    pub const MEMORY_BUFFERS: &str = "memory.buffers";
    pub const MEMORY_CACHED: &str = "memory.cached";
    pub const MEMORY_SWAP_TOTAL: &str = "memory.swap_total";
    pub const MEMORY_SWAP_USED: &str = "memory.swap_used";
    pub const MEMORY_SWAP_FREE_RAW: &str = "memory.swap_free";

    // GPU
    pub const GPU_UTILIZATION_COMPUTE: &str = "gpu.utilization.compute";
    pub const GPU_UTILIZATION_MEMORY_BUSY: &str = "gpu.utilization.memory_busy";
    pub const GPU_TEMPERATURE: &str = "gpu.temperature";
    pub const GPU_POWER_DRAW: &str = "gpu.power.draw";
    pub const GPU_POWER_LIMIT: &str = "gpu.power.limit";
    pub const GPU_CLOCK_GRAPHICS: &str = "gpu.clock.graphics";
    pub const GPU_CLOCK_MEMORY: &str = "gpu.clock.memory";
    pub const GPU_CLOCK_SM: &str = "gpu.clock.sm";
    pub const GPU_CLOCK_VIDEO: &str = "gpu.clock.video";
    /// Scope names the clock domain (`graphics`, `memory`, `sm`, `video`).
    pub const GPU_CLOCK_MAXIMUM: &str = "gpu.clock.maximum";
    pub const GPU_FAN_SPEED: &str = "gpu.fan.speed";
    pub const GPU_MEMORY_FB_USED: &str = "gpu.memory.framebuffer.used";
    pub const GPU_MEMORY_FB_FREE: &str = "gpu.memory.framebuffer.free";
    pub const GPU_MEMORY_FB_TOTAL: &str = "gpu.memory.framebuffer.total";
    pub const GPU_MEMORY_FB_RESERVED: &str = "gpu.memory.framebuffer.reserved";
    pub const GPU_MEMORY_PROCESS_ALLOCATED: &str = "gpu.memory.process_allocated";
    pub const GPU_PCIE_TX: &str = "gpu.pcie.tx";
    pub const GPU_PCIE_RX: &str = "gpu.pcie.rx";
    pub const GPU_ENERGY_TOTAL: &str = "gpu.energy.total";
    pub const GPU_ECC_CORRECTED: &str = "gpu.ecc.corrected";
    pub const GPU_ECC_UNCORRECTED: &str = "gpu.ecc.uncorrected";
    pub const GPU_RETIRED_PAGES_SINGLE: &str = "gpu.retired_pages.single";
    pub const GPU_RETIRED_PAGES_DOUBLE: &str = "gpu.retired_pages.double";
    pub const GPU_ENCODER_UTILIZATION: &str = "gpu.encoder.utilization";
    pub const GPU_DECODER_UTILIZATION: &str = "gpu.decoder.utilization";
    pub const GPU_NVLINK_ACTIVE: &str = "gpu.nvlink.active";
    /// Performance state, 0 (maximum performance) to 15 (minimum).
    pub const GPU_PERFORMANCE_STATE: &str = "gpu.performance_state";
    /// Why the clocks are held below their target, as the bits of
    /// [`crate::gpu_state::THROTTLE_REASONS`].
    pub const GPU_THROTTLE_REASONS: &str = "gpu.throttle_reasons";
    /// Scope names the threshold (`slowdown`, `shutdown`, `max_operating`,
    /// `target`).
    pub const GPU_TEMPERATURE_THRESHOLD: &str = "gpu.temperature.threshold";
    pub const GPU_PCIE_LINK_GENERATION: &str = "gpu.pcie.link.generation";
    pub const GPU_PCIE_LINK_WIDTH: &str = "gpu.pcie.link.width";
    /// Scope names the link attribute (`generation`, `width`).
    pub const GPU_PCIE_LINK_MAXIMUM: &str = "gpu.pcie.link.maximum";
    // GPU capabilities and slow health (D-48); none is kept as history.
    pub const GPU_CORES: &str = "gpu.cores";
    /// Width of the device-memory bus in bits.
    pub const GPU_MEMORY_BUS_WIDTH: &str = "gpu.memory.bus_width";
    pub const GPU_MEMORY_BAR1_TOTAL: &str = "gpu.memory.bar1.total";
    pub const GPU_MEMORY_BAR1_USED: &str = "gpu.memory.bar1.used";
    pub const GPU_POWER_LIMIT_DEFAULT: &str = "gpu.power.limit.default";
    pub const GPU_POWER_LIMIT_MINIMUM: &str = "gpu.power.limit.minimum";
    pub const GPU_POWER_LIMIT_MAXIMUM: &str = "gpu.power.limit.maximum";
    /// The limit set for the GPU, which other limiters may lower to the
    /// enforced one (D-54).
    pub const GPU_POWER_LIMIT_REQUESTED: &str = "gpu.power.limit.requested";
    /// The driver's instantaneous power reading; `gpu.power.draw` is a
    /// one-second average on Ampere and newer GPUs (D-54).
    pub const GPU_POWER_INSTANT: &str = "gpu.power.instant";
    pub const GPU_ENCODER_SESSIONS: &str = "gpu.encoder.sessions";
    pub const GPU_ENCODER_FPS: &str = "gpu.encoder.fps";
    pub const GPU_ENCODER_LATENCY: &str = "gpu.encoder.latency";
    /// PCIe link-level retransmissions since the GPU was reset.
    pub const GPU_PCIE_REPLAYS: &str = "gpu.pcie.replays";
    /// Time the clocks were held back since the driver loaded; scope names
    /// the cause (`power`, `thermal`, `sync_boost`, `board_limit`,
    /// `reliability`).
    pub const GPU_THROTTLE_TIME: &str = "gpu.throttle.time";

    // Disk (canonical)
    pub const DISK_READ_BPS: &str = "disk.read.bytes_per_second";
    pub const DISK_WRITE_BPS: &str = "disk.write.bytes_per_second";
    pub const DISK_READ_IOPS: &str = "disk.read.iops";
    pub const DISK_WRITE_IOPS: &str = "disk.write.iops";
    pub const DISK_AWAIT: &str = "disk.await";
    // Disk (raw counters from /proc/diskstats)
    pub const DISK_READS_COMPLETED: &str = "disk.reads.completed";
    pub const DISK_SECTORS_READ: &str = "disk.sectors.read";
    pub const DISK_READ_TIME: &str = "disk.read.time";
    pub const DISK_WRITES_COMPLETED: &str = "disk.writes.completed";
    pub const DISK_SECTORS_WRITTEN: &str = "disk.sectors.written";
    pub const DISK_WRITE_TIME: &str = "disk.write.time";

    // Filesystem
    pub const FS_TOTAL: &str = "filesystem.total";
    pub const FS_USED: &str = "filesystem.used";
    pub const FS_FREE: &str = "filesystem.free";
    pub const FS_AVAILABLE: &str = "filesystem.available";
    pub const FS_USE_PERCENT: &str = "filesystem.use_percent";

    // Network
    pub const NET_RX_BPS: &str = "network.rx.bytes_per_second";
    pub const NET_TX_BPS: &str = "network.tx.bytes_per_second";
    pub const NET_RX_PACKETS: &str = "network.rx.packets";
    pub const NET_TX_PACKETS: &str = "network.tx.packets";
    pub const NET_RX_ERRORS: &str = "network.rx.errors";
    pub const NET_TX_ERRORS: &str = "network.tx.errors";
    pub const NET_RX_DROPS: &str = "network.rx.drops";
    pub const NET_TX_DROPS: &str = "network.tx.drops";
    pub const NET_RX_BYTES: &str = "network.rx.bytes";
    pub const NET_TX_BYTES: &str = "network.tx.bytes";

    // Process
    pub const PROCESS_RSS: &str = "process.rss";
    pub const PROCESS_GPU_UTILIZATION: &str = "process.gpu.utilization";
    pub const PROCESS_GPU_MEMORY: &str = "process.gpu.memory_allocated";
    pub const PROCESS_CPU_TICKS: &str = "process.cpu.ticks";
    /// Numeric owner uid of a process (static metadata record; name lookups
    /// never happen on the hot path).
    pub const PROCESS_UID: &str = "process.uid";
}

macro_rules! descriptor {
    ($id:expr, $unit:ident, $role:ident, $kind:ident, $domain:ident, $history:expr, $label:expr) => {
        MetricDescriptor {
            id: $id,
            unit: CanonicalUnit::$unit,
            role: MetricRole::$role,
            kind: RawKind::$kind,
            domain: Domain::$domain,
            history: $history,
            label: $label,
        }
    };
}

/// Every metric id known to this build.
pub const METRIC_CATALOG: &[MetricDescriptor] = &[
    descriptor!(
        ids::CPU_UTILIZATION_TOTAL,
        Percent,
        Canonical,
        Gauge,
        Cpu,
        true,
        "CPU total"
    ),
    descriptor!(
        ids::CPU_UTILIZATION_USER,
        Percent,
        Canonical,
        Gauge,
        Cpu,
        true,
        "CPU user"
    ),
    descriptor!(
        ids::CPU_UTILIZATION_SYSTEM,
        Percent,
        Canonical,
        Gauge,
        Cpu,
        true,
        "CPU system"
    ),
    descriptor!(
        ids::CPU_UTILIZATION_IOWAIT,
        Percent,
        Canonical,
        Gauge,
        Cpu,
        true,
        "CPU iowait"
    ),
    descriptor!(
        ids::CPU_UTILIZATION_STEAL,
        Percent,
        Canonical,
        Gauge,
        Cpu,
        true,
        "CPU steal"
    ),
    descriptor!(
        ids::CPU_FREQUENCY_CURRENT,
        MHz,
        Canonical,
        Gauge,
        Cpu,
        false,
        "CPU frequency"
    ),
    descriptor!(
        ids::CPU_FREQUENCY_MAXIMUM,
        MHz,
        Canonical,
        Static,
        Cpu,
        false,
        "CPU max frequency"
    ),
    descriptor!(
        ids::CPU_TEMPERATURE_PACKAGE,
        DegC,
        Canonical,
        Gauge,
        Cpu,
        true,
        "CPU package temperature"
    ),
    descriptor!(
        ids::CPU_TEMPERATURE_CORE,
        DegC,
        Canonical,
        Gauge,
        Cpu,
        false,
        "CPU core temperature"
    ),
    descriptor!(
        ids::CPU_TEMPERATURE_CLUSTER,
        DegC,
        Canonical,
        Gauge,
        Cpu,
        false,
        "CPU cluster temperature"
    ),
    descriptor!(
        ids::SOC_TEMPERATURE,
        DegC,
        Canonical,
        Gauge,
        Soc,
        true,
        "SoC temperature"
    ),
    descriptor!(
        ids::CPU_POWER_PACKAGE,
        Watts,
        Canonical,
        Gauge,
        Cpu,
        true,
        "CPU package power"
    ),
    descriptor!(
        ids::CPU_POWER_PLATFORM,
        Watts,
        Canonical,
        Gauge,
        Cpu,
        true,
        "Platform power"
    ),
    descriptor!(
        ids::CPU_LOAD_1M,
        Count,
        Canonical,
        Gauge,
        Cpu,
        true,
        "Load 1m"
    ),
    descriptor!(
        ids::CPU_LOAD_5M,
        Count,
        Canonical,
        Gauge,
        Cpu,
        false,
        "Load 5m"
    ),
    descriptor!(
        ids::CPU_LOAD_15M,
        Count,
        Canonical,
        Gauge,
        Cpu,
        false,
        "Load 15m"
    ),
    descriptor!(
        ids::CPU_TASKS_RUNNING,
        Count,
        Canonical,
        Gauge,
        Cpu,
        false,
        "Tasks running"
    ),
    descriptor!(
        ids::CPU_TASKS_TOTAL,
        Count,
        Canonical,
        Gauge,
        Cpu,
        false,
        "Tasks total"
    ),
    descriptor!(
        ids::CPU_TIME_USER,
        Count,
        Raw,
        Counter,
        Cpu,
        false,
        "user ticks"
    ),
    descriptor!(
        ids::CPU_TIME_NICE,
        Count,
        Raw,
        Counter,
        Cpu,
        false,
        "nice ticks"
    ),
    descriptor!(
        ids::CPU_TIME_SYSTEM,
        Count,
        Raw,
        Counter,
        Cpu,
        false,
        "system ticks"
    ),
    descriptor!(
        ids::CPU_TIME_IDLE,
        Count,
        Raw,
        Counter,
        Cpu,
        false,
        "idle ticks"
    ),
    descriptor!(
        ids::CPU_TIME_IOWAIT,
        Count,
        Raw,
        Counter,
        Cpu,
        false,
        "iowait ticks"
    ),
    descriptor!(
        ids::CPU_TIME_IRQ,
        Count,
        Raw,
        Counter,
        Cpu,
        false,
        "irq ticks"
    ),
    descriptor!(
        ids::CPU_TIME_SOFTIRQ,
        Count,
        Raw,
        Counter,
        Cpu,
        false,
        "softirq ticks"
    ),
    descriptor!(
        ids::CPU_TIME_STEAL,
        Count,
        Raw,
        Counter,
        Cpu,
        false,
        "steal ticks"
    ),
    descriptor!(
        ids::CPU_POWER_CLUSTER,
        Watts,
        Raw,
        Gauge,
        Cpu,
        false,
        "CPU cluster power"
    ),
    descriptor!(
        ids::CPU_ENERGY_PACKAGE,
        Joules,
        Raw,
        Counter,
        Cpu,
        false,
        "package energy"
    ),
    descriptor!(
        ids::CPU_ENERGY_PLATFORM,
        Joules,
        Raw,
        Counter,
        Cpu,
        false,
        "platform energy"
    ),
    descriptor!(
        ids::SENSOR_TEMPERATURE,
        DegC,
        Raw,
        Gauge,
        Sensor,
        false,
        "sensor temperature"
    ),
    descriptor!(
        ids::SENSOR_POWER,
        Watts,
        Raw,
        Gauge,
        Sensor,
        false,
        "sensor power"
    ),
    descriptor!(
        ids::SENSOR_ENERGY,
        Joules,
        Raw,
        Counter,
        Sensor,
        false,
        "sensor energy"
    ),
    descriptor!(ids::SENSOR_FAN, Count, Raw, Gauge, Sensor, false, "fan RPM"),
    descriptor!(
        ids::MEMORY_TOTAL,
        Bytes,
        Canonical,
        Gauge,
        Memory,
        false,
        "RAM total"
    ),
    descriptor!(
        ids::MEMORY_AVAILABLE,
        Bytes,
        Canonical,
        Gauge,
        Memory,
        true,
        "RAM available"
    ),
    descriptor!(
        ids::MEMORY_USED,
        Bytes,
        Canonical,
        Gauge,
        Memory,
        true,
        "RAM used"
    ),
    descriptor!(
        ids::MEMORY_BUFFERS,
        Bytes,
        Canonical,
        Gauge,
        Memory,
        false,
        "Buffers"
    ),
    descriptor!(
        ids::MEMORY_CACHED,
        Bytes,
        Canonical,
        Gauge,
        Memory,
        false,
        "Cached"
    ),
    descriptor!(
        ids::MEMORY_SWAP_TOTAL,
        Bytes,
        Canonical,
        Gauge,
        Memory,
        false,
        "Swap total"
    ),
    descriptor!(
        ids::MEMORY_SWAP_USED,
        Bytes,
        Canonical,
        Gauge,
        Memory,
        true,
        "Swap used"
    ),
    descriptor!(
        ids::MEMORY_SWAP_FREE_RAW,
        Bytes,
        Raw,
        Gauge,
        Memory,
        false,
        "SwapFree"
    ),
    descriptor!(
        ids::GPU_UTILIZATION_COMPUTE,
        Percent,
        Canonical,
        Gauge,
        Gpu,
        true,
        "GPU compute"
    ),
    descriptor!(
        ids::GPU_UTILIZATION_MEMORY_BUSY,
        Percent,
        Canonical,
        Gauge,
        Gpu,
        true,
        "GPU memory busy"
    ),
    descriptor!(
        ids::GPU_TEMPERATURE,
        DegC,
        Canonical,
        Gauge,
        Gpu,
        true,
        "GPU temperature"
    ),
    descriptor!(
        ids::GPU_POWER_DRAW,
        Watts,
        Canonical,
        Gauge,
        Gpu,
        true,
        "GPU power"
    ),
    descriptor!(
        ids::GPU_POWER_LIMIT,
        Watts,
        Canonical,
        Static,
        Gpu,
        false,
        "GPU power limit"
    ),
    descriptor!(
        ids::GPU_CLOCK_GRAPHICS,
        MHz,
        Canonical,
        Gauge,
        Gpu,
        false,
        "Graphics clock"
    ),
    descriptor!(
        ids::GPU_CLOCK_MEMORY,
        MHz,
        Canonical,
        Gauge,
        Gpu,
        false,
        "Memory clock"
    ),
    descriptor!(
        ids::GPU_CLOCK_SM,
        MHz,
        Canonical,
        Gauge,
        Gpu,
        false,
        "SM clock"
    ),
    descriptor!(
        ids::GPU_CLOCK_VIDEO,
        MHz,
        Canonical,
        Gauge,
        Gpu,
        false,
        "Video clock"
    ),
    descriptor!(
        ids::GPU_CLOCK_MAXIMUM,
        MHz,
        Canonical,
        Static,
        Gpu,
        false,
        "Max clock"
    ),
    descriptor!(
        ids::GPU_FAN_SPEED,
        Percent,
        Canonical,
        Gauge,
        Gpu,
        false,
        "Fan"
    ),
    descriptor!(
        ids::GPU_MEMORY_FB_USED,
        Bytes,
        Canonical,
        Gauge,
        Gpu,
        true,
        "Framebuffer used"
    ),
    descriptor!(
        ids::GPU_MEMORY_FB_FREE,
        Bytes,
        Canonical,
        Gauge,
        Gpu,
        false,
        "Framebuffer free"
    ),
    descriptor!(
        ids::GPU_MEMORY_FB_TOTAL,
        Bytes,
        Canonical,
        Static,
        Gpu,
        false,
        "Framebuffer total"
    ),
    descriptor!(
        ids::GPU_MEMORY_FB_RESERVED,
        Bytes,
        Canonical,
        Static,
        Gpu,
        false,
        "Framebuffer reserved"
    ),
    descriptor!(
        ids::GPU_MEMORY_PROCESS_ALLOCATED,
        Bytes,
        Canonical,
        Gauge,
        Gpu,
        true,
        "Process allocations"
    ),
    descriptor!(
        ids::GPU_PCIE_TX,
        BytesPerSecond,
        Canonical,
        Gauge,
        Gpu,
        true,
        "PCIe TX"
    ),
    descriptor!(
        ids::GPU_PCIE_RX,
        BytesPerSecond,
        Canonical,
        Gauge,
        Gpu,
        true,
        "PCIe RX"
    ),
    descriptor!(
        ids::GPU_ENERGY_TOTAL,
        Joules,
        Canonical,
        Counter,
        Gpu,
        false,
        "GPU energy"
    ),
    descriptor!(
        ids::GPU_ECC_CORRECTED,
        Count,
        Canonical,
        Counter,
        Gpu,
        false,
        "ECC corrected"
    ),
    descriptor!(
        ids::GPU_ECC_UNCORRECTED,
        Count,
        Canonical,
        Counter,
        Gpu,
        false,
        "ECC uncorrected"
    ),
    descriptor!(
        ids::GPU_RETIRED_PAGES_SINGLE,
        Count,
        Canonical,
        Gauge,
        Gpu,
        false,
        "Retired pages (SBE)"
    ),
    descriptor!(
        ids::GPU_RETIRED_PAGES_DOUBLE,
        Count,
        Canonical,
        Gauge,
        Gpu,
        false,
        "Retired pages (DBE)"
    ),
    descriptor!(
        ids::GPU_ENCODER_UTILIZATION,
        Percent,
        Canonical,
        Gauge,
        Gpu,
        false,
        "Encoder"
    ),
    descriptor!(
        ids::GPU_DECODER_UTILIZATION,
        Percent,
        Canonical,
        Gauge,
        Gpu,
        false,
        "Decoder"
    ),
    descriptor!(
        ids::GPU_NVLINK_ACTIVE,
        Count,
        Canonical,
        Gauge,
        Gpu,
        false,
        "NVLink active links"
    ),
    descriptor!(
        ids::GPU_PERFORMANCE_STATE,
        Count,
        Canonical,
        Gauge,
        Gpu,
        false,
        "GPU performance state"
    ),
    descriptor!(
        ids::GPU_THROTTLE_REASONS,
        Count,
        Canonical,
        Gauge,
        Gpu,
        false,
        "GPU clock limits"
    ),
    descriptor!(
        ids::GPU_TEMPERATURE_THRESHOLD,
        DegC,
        Canonical,
        Static,
        Gpu,
        false,
        "GPU temperature threshold"
    ),
    descriptor!(
        ids::GPU_PCIE_LINK_GENERATION,
        Count,
        Canonical,
        Gauge,
        Gpu,
        false,
        "PCIe link generation"
    ),
    descriptor!(
        ids::GPU_PCIE_LINK_WIDTH,
        Count,
        Canonical,
        Gauge,
        Gpu,
        false,
        "PCIe link width"
    ),
    descriptor!(
        ids::GPU_PCIE_LINK_MAXIMUM,
        Count,
        Canonical,
        Static,
        Gpu,
        false,
        "PCIe link maximum"
    ),
    descriptor!(
        ids::GPU_CORES,
        Count,
        Canonical,
        Static,
        Gpu,
        false,
        "CUDA cores"
    ),
    descriptor!(
        ids::GPU_MEMORY_BUS_WIDTH,
        Count,
        Canonical,
        Static,
        Gpu,
        false,
        "Memory bus width"
    ),
    descriptor!(
        ids::GPU_MEMORY_BAR1_TOTAL,
        Bytes,
        Canonical,
        Static,
        Gpu,
        false,
        "BAR1 size"
    ),
    descriptor!(
        ids::GPU_MEMORY_BAR1_USED,
        Bytes,
        Canonical,
        Gauge,
        Gpu,
        false,
        "BAR1 used"
    ),
    descriptor!(
        ids::GPU_POWER_LIMIT_DEFAULT,
        Watts,
        Canonical,
        Static,
        Gpu,
        false,
        "Default power limit"
    ),
    descriptor!(
        ids::GPU_POWER_LIMIT_MINIMUM,
        Watts,
        Canonical,
        Static,
        Gpu,
        false,
        "Minimum power limit"
    ),
    descriptor!(
        ids::GPU_POWER_LIMIT_MAXIMUM,
        Watts,
        Canonical,
        Static,
        Gpu,
        false,
        "Maximum power limit"
    ),
    descriptor!(
        ids::GPU_POWER_LIMIT_REQUESTED,
        Watts,
        Canonical,
        Static,
        Gpu,
        false,
        "Requested power limit"
    ),
    descriptor!(
        ids::GPU_POWER_INSTANT,
        Watts,
        Canonical,
        Gauge,
        Gpu,
        false,
        "GPU instant power"
    ),
    descriptor!(
        ids::GPU_ENCODER_SESSIONS,
        Count,
        Canonical,
        Gauge,
        Gpu,
        false,
        "Encoder sessions"
    ),
    descriptor!(
        ids::GPU_ENCODER_FPS,
        CountPerSecond,
        Canonical,
        Gauge,
        Gpu,
        false,
        "Encoder frame rate"
    ),
    descriptor!(
        ids::GPU_ENCODER_LATENCY,
        Milliseconds,
        Canonical,
        Gauge,
        Gpu,
        false,
        "Encoder latency"
    ),
    descriptor!(
        ids::GPU_PCIE_REPLAYS,
        Count,
        Canonical,
        Gauge,
        Gpu,
        false,
        "PCIe replays"
    ),
    descriptor!(
        ids::GPU_THROTTLE_TIME,
        Milliseconds,
        Canonical,
        Gauge,
        Gpu,
        false,
        "Clock throttle time"
    ),
    descriptor!(
        ids::DISK_READ_BPS,
        BytesPerSecond,
        Canonical,
        Gauge,
        Disk,
        true,
        "Disk read"
    ),
    descriptor!(
        ids::DISK_WRITE_BPS,
        BytesPerSecond,
        Canonical,
        Gauge,
        Disk,
        true,
        "Disk write"
    ),
    descriptor!(
        ids::DISK_READ_IOPS,
        CountPerSecond,
        Canonical,
        Gauge,
        Disk,
        false,
        "Read IOPS"
    ),
    descriptor!(
        ids::DISK_WRITE_IOPS,
        CountPerSecond,
        Canonical,
        Gauge,
        Disk,
        false,
        "Write IOPS"
    ),
    descriptor!(
        ids::DISK_AWAIT,
        Milliseconds,
        Canonical,
        Gauge,
        Disk,
        false,
        "Await"
    ),
    descriptor!(
        ids::DISK_READS_COMPLETED,
        Count,
        Raw,
        Counter,
        Disk,
        false,
        "reads completed"
    ),
    descriptor!(
        ids::DISK_SECTORS_READ,
        Bytes,
        Raw,
        Counter,
        Disk,
        false,
        "sectors read"
    ),
    descriptor!(
        ids::DISK_READ_TIME,
        Milliseconds,
        Raw,
        Counter,
        Disk,
        false,
        "read time"
    ),
    descriptor!(
        ids::DISK_WRITES_COMPLETED,
        Count,
        Raw,
        Counter,
        Disk,
        false,
        "writes completed"
    ),
    descriptor!(
        ids::DISK_SECTORS_WRITTEN,
        Bytes,
        Raw,
        Counter,
        Disk,
        false,
        "sectors written"
    ),
    descriptor!(
        ids::DISK_WRITE_TIME,
        Milliseconds,
        Raw,
        Counter,
        Disk,
        false,
        "write time"
    ),
    descriptor!(
        ids::FS_TOTAL,
        Bytes,
        Canonical,
        Gauge,
        Filesystem,
        false,
        "FS total"
    ),
    descriptor!(
        ids::FS_USED,
        Bytes,
        Canonical,
        Gauge,
        Filesystem,
        true,
        "FS used"
    ),
    descriptor!(
        ids::FS_FREE,
        Bytes,
        Canonical,
        Gauge,
        Filesystem,
        false,
        "FS free"
    ),
    descriptor!(
        ids::FS_AVAILABLE,
        Bytes,
        Canonical,
        Gauge,
        Filesystem,
        false,
        "FS available"
    ),
    descriptor!(
        ids::FS_USE_PERCENT,
        Percent,
        Canonical,
        Gauge,
        Filesystem,
        false,
        "FS use%"
    ),
    descriptor!(
        ids::NET_RX_BPS,
        BytesPerSecond,
        Canonical,
        Gauge,
        Network,
        true,
        "RX"
    ),
    descriptor!(
        ids::NET_TX_BPS,
        BytesPerSecond,
        Canonical,
        Gauge,
        Network,
        true,
        "TX"
    ),
    descriptor!(
        ids::NET_RX_PACKETS,
        Count,
        Canonical,
        Counter,
        Network,
        false,
        "RX packets"
    ),
    descriptor!(
        ids::NET_TX_PACKETS,
        Count,
        Canonical,
        Counter,
        Network,
        false,
        "TX packets"
    ),
    descriptor!(
        ids::NET_RX_ERRORS,
        Count,
        Canonical,
        Counter,
        Network,
        false,
        "RX errors"
    ),
    descriptor!(
        ids::NET_TX_ERRORS,
        Count,
        Canonical,
        Counter,
        Network,
        false,
        "TX errors"
    ),
    descriptor!(
        ids::NET_RX_DROPS,
        Count,
        Canonical,
        Counter,
        Network,
        false,
        "RX drops"
    ),
    descriptor!(
        ids::NET_TX_DROPS,
        Count,
        Canonical,
        Counter,
        Network,
        false,
        "TX drops"
    ),
    descriptor!(
        ids::NET_RX_BYTES,
        Bytes,
        Raw,
        Counter,
        Network,
        false,
        "RX bytes"
    ),
    descriptor!(
        ids::NET_TX_BYTES,
        Bytes,
        Raw,
        Counter,
        Network,
        false,
        "TX bytes"
    ),
    descriptor!(
        ids::PROCESS_RSS,
        Bytes,
        Canonical,
        Gauge,
        Process,
        false,
        "Process RSS"
    ),
    descriptor!(
        ids::PROCESS_GPU_UTILIZATION,
        Percent,
        Canonical,
        Gauge,
        Process,
        false,
        "Process GPU"
    ),
    descriptor!(
        ids::PROCESS_GPU_MEMORY,
        Bytes,
        Canonical,
        Gauge,
        Process,
        false,
        "Process GPU memory"
    ),
    descriptor!(
        ids::PROCESS_UID,
        Count,
        Raw,
        Static,
        Process,
        false,
        "process uid"
    ),
    descriptor!(
        ids::PROCESS_CPU_TICKS,
        Count,
        Raw,
        Counter,
        Process,
        false,
        "process ticks"
    ),
];

/// Looks up a descriptor by metric id. Every collected record is looked up
/// several times on its way to the store, so the catalog is indexed once
/// instead of being scanned (process metrics sit at its end).
pub fn descriptor(metric_id: &str) -> Option<&'static MetricDescriptor> {
    static INDEX: LazyLock<HashMap<&'static str, &'static MetricDescriptor>> =
        LazyLock::new(|| {
            METRIC_CATALOG
                .iter()
                .map(|entry| (entry.id, entry))
                .collect()
        });
    INDEX.get(metric_id).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn millidegrees_convert_to_68_degrees() {
        let (value, id) = normalize(
            Scalar::from_f64(68000.0).unwrap(),
            native::MILLI_DEG_C,
            CanonicalUnit::DegC,
        )
        .unwrap();
        assert_eq!(value.to_f64_lossy(), 68.0);
        assert_eq!(id, "mdegC_to_degC");
    }

    #[test]
    fn unknown_and_unverified_units_are_rejected() {
        assert!(matches!(
            normalize(Scalar::from_u64(1), "furlongs", CanonicalUnit::Bytes),
            Err(ValueError::UnknownUnit { .. })
        ));
        assert!(
            conversion_for(
                native::KB_UNVERIFIED_PER_SECOND,
                CanonicalUnit::BytesPerSecond
            )
            .is_none()
        );
    }

    #[test]
    fn driver_microseconds_and_nanoseconds_become_milliseconds() {
        let (latency, id) = normalize(
            Scalar::from_u64(2_500),
            native::MICROSECONDS,
            CanonicalUnit::Milliseconds,
        )
        .unwrap();
        assert_eq!((latency.to_f64_lossy(), id), (2.5, "us_to_ms"));
        let (capped, id) = normalize(
            Scalar::from_u64(219_152_032),
            native::NANOSECONDS,
            CanonicalUnit::Milliseconds,
        )
        .unwrap();
        assert_eq!((capped.to_f64_lossy(), id), (219.152_032, "ns_to_ms"));
    }

    #[test]
    fn deep_gpu_metrics_are_catalogued_and_never_history() {
        for id in [
            ids::GPU_CORES,
            ids::GPU_MEMORY_BUS_WIDTH,
            ids::GPU_MEMORY_BAR1_TOTAL,
            ids::GPU_MEMORY_BAR1_USED,
            ids::GPU_POWER_LIMIT_DEFAULT,
            ids::GPU_POWER_LIMIT_MINIMUM,
            ids::GPU_POWER_LIMIT_MAXIMUM,
            ids::GPU_POWER_LIMIT_REQUESTED,
            ids::GPU_POWER_INSTANT,
            ids::GPU_ENCODER_SESSIONS,
            ids::GPU_ENCODER_FPS,
            ids::GPU_ENCODER_LATENCY,
            ids::GPU_PCIE_REPLAYS,
            ids::GPU_THROTTLE_TIME,
        ] {
            let known = descriptor(id).unwrap_or_else(|| panic!("{id} missing"));
            assert!(!known.history, "{id}");
        }
        assert_eq!(
            descriptor(ids::GPU_ENCODER_LATENCY).unwrap().unit,
            CanonicalUnit::Milliseconds
        );
        assert_eq!(
            descriptor(ids::GPU_POWER_LIMIT_MAXIMUM).unwrap().kind,
            RawKind::Static
        );
    }

    #[test]
    fn exact_u64_conversion_and_overflow() {
        let (value, _) = normalize(Scalar::from_u64(4), native::KIB, CanonicalUnit::Bytes).unwrap();
        assert_eq!(value.as_u64(), Some(4096));
        assert_eq!(
            normalize(
                Scalar::from_u64(u64::MAX),
                native::KIB,
                CanonicalUnit::Bytes
            ),
            Err(ValueError::Overflow)
        );
    }

    #[test]
    fn non_finite_scalars_are_rejected() {
        assert_eq!(Scalar::from_f64(f64::NAN), Err(ValueError::NonFinite));
        assert_eq!(Scalar::from_f64(f64::INFINITY), Err(ValueError::NonFinite));
        assert_eq!(
            Scalar::from_f64(f64::NEG_INFINITY),
            Err(ValueError::NonFinite)
        );
        assert!(Scalar::from_f64(0.0).is_ok());
    }

    #[test]
    fn scalar_decoding_accepts_either_key_order_and_rejects_mismatches() {
        let decode = |text: &str| serde_json::from_str::<Scalar>(text);
        assert_eq!(
            decode(r#"{"value":"42","type":"u64"}"#).unwrap(),
            Scalar::from_u64(42)
        );
        assert_eq!(
            decode(r#"{"type":"f64","value":1.5}"#)
                .unwrap()
                .to_f64_lossy(),
            1.5
        );
        assert_eq!(
            decode(r#"{"value":-2,"type":"f64"}"#)
                .unwrap()
                .to_f64_lossy(),
            -2.0
        );
        // u64 values are decimal strings; f64 values are numbers.
        assert!(decode(r#"{"type":"u64","value":42}"#).is_err());
        assert!(decode(r#"{"type":"f64","value":"1.5"}"#).is_err());
        assert!(decode(r#"{"type":"u64","value":"042"}"#).is_err());
        assert!(decode(r#"{"type":"i64","value":"1"}"#).is_err());
        assert!(decode(r#"{"type":"u64"}"#).is_err());
        assert!(decode(r#"{"value":"1"}"#).is_err());
        assert!(decode(r#"{"type":"u64","value":"1","extra":true}"#).is_err());
        assert!(decode(r#"{"type":"u64","type":"u64","value":"1"}"#).is_err());
    }

    #[test]
    fn scalar_json_shape_matches_schema() {
        let max = Scalar::from_u64(u64::MAX);
        let json = serde_json::to_value(max).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type":"u64","value":"18446744073709551615"})
        );
        assert_eq!(serde_json::from_value::<Scalar>(json).unwrap(), max);
        let gauge = serde_json::json!({"type":"f64","value":68.0});
        assert_eq!(
            serde_json::from_value::<Scalar>(gauge)
                .unwrap()
                .to_f64_lossy(),
            68.0
        );
        assert!(
            serde_json::from_value::<Scalar>(
                serde_json::json!({"type":"u64","value":"18446744073709551616"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<Scalar>(serde_json::json!({"type":"u64","value":5})).is_err()
        );
        assert!(
            serde_json::from_value::<Scalar>(serde_json::json!({"type":"f64","value":1.0,"x":1}))
                .is_err()
        );
    }

    #[test]
    fn catalog_ids_are_unique_and_valid() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in METRIC_CATALOG {
            assert!(seen.insert(entry.id), "duplicate metric id {}", entry.id);
            assert!(
                crate::identity::MetricId::new(entry.id).is_ok(),
                "{}",
                entry.id
            );
        }
        assert!(descriptor(ids::CPU_TEMPERATURE_PACKAGE).is_some());
    }

    proptest! {}
}
