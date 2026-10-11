//! Typed identities: metric keys, sources, origins, entities and processes.
//!
//! Identity is never a row index or a display order. `MetricKey` names one
//! semantic series (host/boot/entity/metric/scope); `SourceId` names one source
//! incarnation; `ProcessKey` names one process instance (a PID plus its start
//! time inside one boot and PID namespace).

use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;
use std::thread::LocalKey;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Validation failures for identity values.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdentityError {
    #[error("{field} length {length} outside {min}..={max} bytes")]
    Length {
        field: &'static str,
        length: usize,
        min: usize,
        max: usize,
    },
    #[error("{field} contains a control character")]
    ControlCharacter { field: &'static str },
    #[error("metric_id must match ^[a-z][a-z0-9_.]{{1,127}}$")]
    MetricIdSyntax,
    #[error("pid must be within 1..=2147483647")]
    PidRange,
    #[error("confirmed origin requires origin_id")]
    ConfirmedOriginWithoutId,
}

/// Upper bound of each identity type's decode cache.
const INTERNED_PER_TYPE: usize = 8192;

/// Decode cache of one identity type: every entry passed that type's
/// validation once.
pub(crate) type InternCache = RefCell<HashSet<Arc<str>>>;

/// Shares repeated identity strings while decoding: thousands of records per
/// second repeat the same host, boot, metric and source ids. A cache hit is
/// a string that already passed this type's validation; a full cache starts
/// over, so memory stays bounded.
pub(crate) fn intern<E>(
    cache: &'static LocalKey<InternCache>,
    value: &str,
    validate: impl FnOnce(&str) -> Result<(), E>,
) -> Result<Arc<str>, E> {
    cache.with(|seen| {
        let mut seen = seen.borrow_mut();
        if let Some(shared) = seen.get(value) {
            return Ok(Arc::clone(shared));
        }
        validate(value)?;
        if seen.len() >= INTERNED_PER_TYPE {
            seen.clear();
        }
        let shared: Arc<str> = Arc::from(value);
        seen.insert(Arc::clone(&shared));
        Ok(shared)
    })
}

/// Deserializes a validated string type from a borrowed string (no
/// intermediate `String`).
pub(crate) struct IdentityVisitor<T, Invalid>(
    pub(crate) fn(&str) -> Result<T, Invalid>,
    pub(crate) PhantomData<T>,
);

impl<T, Invalid: fmt::Display> Visitor<'_> for IdentityVisitor<T, Invalid> {
    type Value = T;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an identity string")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<T, E> {
        (self.0)(value).map_err(E::custom)
    }
}

macro_rules! bounded_identity {
    ($(#[$meta:meta])* $name:ident, $field:literal, $min:expr, $max:expr) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Arc<str>);

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_str(IdentityVisitor(Self::decoded, PhantomData))
            }
        }

        impl $name {
            /// Validates length and rejects control characters.
            pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
                let value = value.into();
                validate_bounded($field, &value, $min, $max)?;
                Ok(Self(Arc::from(value)))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Decoding path: validated once per distinct value, then shared.
            fn decoded(value: &str) -> Result<Self, IdentityError> {
                thread_local! {
                    static CACHE: InternCache = RefCell::new(HashSet::new());
                }
                intern(&CACHE, value, |value| validate_bounded($field, value, $min, $max))
                    .map(Self)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdentityError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdentityError;
            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0.to_string()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&*self.0, formatter)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

fn validate_bounded(
    field: &'static str,
    value: &str,
    min: usize,
    max: usize,
) -> Result<(), IdentityError> {
    let length = value.len();
    if length < min || length > max {
        return Err(IdentityError::Length {
            field,
            length,
            min,
            max,
        });
    }
    if value.chars().any(char::is_control) {
        return Err(IdentityError::ControlCharacter { field });
    }
    Ok(())
}

bounded_identity!(
    /// Stable host identity (machine-id derived).
    HostId, "host_id", 1, 256
);
bounded_identity!(
    /// Kernel boot identity (`/proc/sys/kernel/random/boot_id`).
    BootId, "boot_id", 1, 256
);
bounded_identity!(
    /// Entity identity such as `gpu:GPU-<uuid>` or `cpu.package.0`.
    EntityId, "entity_id", 1, 256
);
bounded_identity!(
    /// Scope inside an entity, e.g. `package:0`, `core:3`, `soc`.
    Scope, "scope", 1, 128
);
bounded_identity!(
    /// Stable identity of one source incarnation.
    SourceId, "source_id", 1, 256
);
bounded_identity!(
    /// Registered adapter identity (`procfs`, `hwmon`, `nvml`, ...).
    AdapterId, "adapter_id", 1, 64
);
bounded_identity!(
    /// Physical origin identity used for alias de-duplication.
    OriginId, "origin_id", 1, 256
);
bounded_identity!(
    /// PID namespace identity, e.g. `pid:[4026531836]`.
    PidNamespace, "pid_namespace", 1, 256
);
bounded_identity!(
    /// Routing key of one view session; never an authority grant.
    SessionId, "session_id", 1, 128
);
bounded_identity!(
    /// Display module identity from the compile-time registry.
    ModuleId, "module_id", 1, 64
);
bounded_identity!(
    /// Read group of one adapter (`fast`, `capacity`, `process`, ...).
    ReadGroup, "read_group", 1, 128
);
bounded_identity!(
    /// Versioned source profile identity.
    ProfileId, "profile_id", 1, 256
);
bounded_identity!(
    /// Collector identity (`cpu`, `gpu`, `memory`, `disk`, `network`, `process`).
    CollectorId, "collector_id", 1, 64
);

/// Metric identity, e.g. `cpu.temperature.package`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MetricId(Arc<str>);

impl Serialize for MetricId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MetricId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(IdentityVisitor(Self::decoded, PhantomData))
    }
}

/// `^[a-z][a-z0-9_.]{1,127}$`.
fn validate_metric_id(value: &str) -> Result<(), IdentityError> {
    let bytes = value.as_bytes();
    let valid_length = (2..=128).contains(&bytes.len());
    let valid_head = bytes.first().is_some_and(u8::is_ascii_lowercase);
    let valid_tail = bytes.iter().skip(1).all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_' || *byte == b'.'
    });
    if valid_length && valid_head && valid_tail {
        Ok(())
    } else {
        Err(IdentityError::MetricIdSyntax)
    }
}

impl MetricId {
    /// Validates `^[a-z][a-z0-9_.]{1,127}$`.
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        validate_metric_id(&value)?;
        Ok(Self(Arc::from(value)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Decoding path: validated once per distinct value, then shared.
    fn decoded(value: &str) -> Result<Self, IdentityError> {
        thread_local! {
            static CACHE: InternCache = RefCell::new(HashSet::new());
        }
        intern(&CACHE, value, validate_metric_id).map(Self)
    }
}

impl TryFrom<String> for MetricId {
    type Error = IdentityError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for MetricId {
    type Error = IdentityError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<MetricId> for String {
    fn from(value: MetricId) -> Self {
        value.0.to_string()
    }
}

impl fmt::Debug for MetricId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, formatter)
    }
}

impl fmt::Display for MetricId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One semantic series. Only samples with an identical `MetricKey` may be
/// arbitrated against each other; differing scope means a different metric.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricKey {
    pub host_id: HostId,
    pub boot_id: BootId,
    pub entity_id: EntityId,
    pub metric_id: MetricId,
    pub scope: Scope,
}

impl MetricKey {
    pub fn new(
        host_id: HostId,
        boot_id: BootId,
        entity_id: EntityId,
        metric_id: MetricId,
        scope: Scope,
    ) -> Self {
        Self {
            host_id,
            boot_id,
            entity_id,
            metric_id,
            scope,
        }
    }

    /// Compact selector `metric_id|scope|entity_id` used inside view sessions
    /// (host and boot are implicit there). `metric_id` cannot contain `|`.
    pub fn series_selector(&self) -> String {
        format!("{}|{}|{}", self.metric_id, self.scope, self.entity_id)
    }

    /// Rebuilds a key from a selector produced by [`MetricKey::series_selector`].
    pub fn from_series_selector(
        host_id: &HostId,
        boot_id: &BootId,
        selector: &str,
    ) -> Result<Self, IdentityError> {
        let mut parts = selector.splitn(3, '|');
        let metric = parts.next().unwrap_or_default();
        let scope = parts.next().unwrap_or_default();
        let entity = parts.next().unwrap_or_default();
        Ok(Self {
            host_id: host_id.clone(),
            boot_id: boot_id.clone(),
            entity_id: EntityId::new(entity)?,
            metric_id: MetricId::new(metric)?,
            scope: Scope::new(scope)?,
        })
    }
}

/// Whether two sources are proven to read the same physical sensor/counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginRelation {
    /// A verified profile/topology proves `origin_id`.
    Confirmed,
    /// Independence is unknown; never counted as independent evidence.
    Unknown,
}

/// Physical origin of a source. Only confirmed origins de-duplicate aliases or
/// count as independent corroboration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OriginIdentity {
    pub origin_id: Option<OriginId>,
    pub relation: OriginRelation,
}

impl OriginIdentity {
    pub fn confirmed(origin_id: OriginId) -> Self {
        Self {
            origin_id: Some(origin_id),
            relation: OriginRelation::Confirmed,
        }
    }

    pub fn unknown() -> Self {
        Self {
            origin_id: None,
            relation: OriginRelation::Unknown,
        }
    }

    /// Rejects `confirmed` without an `origin_id` (schema `if/then`).
    pub fn validate(&self) -> Result<(), IdentityError> {
        if self.relation == OriginRelation::Confirmed && self.origin_id.is_none() {
            return Err(IdentityError::ConfirmedOriginWithoutId);
        }
        Ok(())
    }

    /// Confirmed origin id, if any.
    pub fn confirmed_id(&self) -> Option<&OriginId> {
        match self.relation {
            OriginRelation::Confirmed => self.origin_id.as_ref(),
            OriginRelation::Unknown => None,
        }
    }
}

/// Process id restricted to `1..=i32::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct Pid(i32);

impl Pid {
    pub fn new(value: i64) -> Result<Self, IdentityError> {
        i32::try_from(value)
            .ok()
            .filter(|pid| *pid > 0)
            .map(Self)
            .ok_or(IdentityError::PidRange)
    }

    pub const fn get(self) -> i32 {
        self.0
    }
}

impl TryFrom<i64> for Pid {
    type Error = IdentityError;
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Pid> for i64 {
    fn from(value: Pid) -> Self {
        i64::from(value.0)
    }
}

impl fmt::Display for Pid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// One process instance. The same PID with another start time is a different
/// process; a PID alone is never sufficient identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessKey {
    pub host_id: HostId,
    pub boot_id: BootId,
    pub pid_namespace: PidNamespace,
    pub pid: Pid,
    #[serde(with = "crate::wire::u64_decimal")]
    pub start_time_ticks: u64,
}

impl ProcessKey {
    /// Entity id used when a process appears in metric keys.
    pub fn entity_id(&self) -> Result<EntityId, IdentityError> {
        EntityId::new(format!(
            "process:{}:{}:{}",
            self.pid_namespace, self.pid, self.start_time_ticks
        ))
    }

    /// Rebuilds a key from `process:<pid_namespace>:<pid>:<start_time_ticks>`
    /// (the namespace itself may contain `:`, so parsing goes from the right).
    pub fn from_entity_id(host_id: &HostId, boot_id: &BootId, entity_id: &str) -> Option<Self> {
        let rest = entity_id.strip_prefix("process:")?;
        let (head, start) = rest.rsplit_once(':')?;
        let (namespace, pid) = head.rsplit_once(':')?;
        Some(Self {
            host_id: host_id.clone(),
            boot_id: boot_id.clone(),
            pid_namespace: PidNamespace::new(namespace).ok()?,
            pid: Pid::new(pid.parse::<i64>().ok()?).ok()?,
            start_time_ticks: crate::wire::parse_decimal_u64(start).ok()?,
        })
    }

    /// Short, non-secret digest of the identity for display/audit summaries.
    pub fn summary(&self) -> String {
        format!(
            "pid {} · start {} · ns {}",
            self.pid, self.start_time_ticks, self.pid_namespace
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(entity: &str, metric: &str, scope: &str) -> MetricKey {
        MetricKey::new(
            HostId::new("host").unwrap(),
            BootId::new("boot").unwrap(),
            EntityId::new(entity).unwrap(),
            MetricId::new(metric).unwrap(),
            Scope::new(scope).unwrap(),
        )
    }

    #[test]
    fn metric_id_syntax() {
        assert!(MetricId::new("cpu.temperature.package").is_ok());
        assert!(MetricId::new("a1").is_ok());
        assert!(MetricId::new("a").is_err());
        assert!(MetricId::new("Cpu.temp").is_err());
        assert!(MetricId::new("1cpu").is_err());
        assert!(MetricId::new("cpu-temp").is_err());
        assert!(MetricId::new("cpu|temp").is_err());
        assert!(MetricId::new(format!("a{}", "b".repeat(127))).is_ok());
        assert!(MetricId::new(format!("a{}", "b".repeat(128))).is_err());
    }

    #[test]
    fn different_scope_is_different_key() {
        let package = key("cpu.package.0", "cpu.temperature.package", "package:0");
        let soc = key("cpu.package.0", "cpu.temperature.package", "soc");
        assert_ne!(package, soc);
        assert_eq!(package, package.clone());
    }

    #[test]
    fn every_metric_key_field_participates_in_equality() {
        let base = key("e", "m.x", "s");
        let mut other = base.clone();
        other.host_id = HostId::new("other").unwrap();
        assert_ne!(base, other);
        let mut other = base.clone();
        other.boot_id = BootId::new("other").unwrap();
        assert_ne!(base, other);
        let mut other = base.clone();
        other.entity_id = EntityId::new("other").unwrap();
        assert_ne!(base, other);
        let mut other = base.clone();
        other.metric_id = MetricId::new("m.y").unwrap();
        assert_ne!(base, other);
        let mut other = base.clone();
        other.scope = Scope::new("other").unwrap();
        assert_ne!(base, other);
    }

    #[test]
    fn pid_range_is_enforced() {
        assert!(Pid::new(0).is_err());
        assert!(Pid::new(-1).is_err());
        assert!(Pid::new(i64::from(i32::MAX) + 1).is_err());
        assert_eq!(Pid::new(i64::from(i32::MAX)).unwrap().get(), i32::MAX);
        assert_eq!(Pid::new(1).unwrap().get(), 1);
    }

    #[test]
    fn process_key_fields_all_matter() {
        let base = ProcessKey {
            host_id: HostId::new("h").unwrap(),
            boot_id: BootId::new("b").unwrap(),
            pid_namespace: PidNamespace::new("pid:[1]").unwrap(),
            pid: Pid::new(42).unwrap(),
            start_time_ticks: 100,
        };
        let mut reused = base.clone();
        reused.start_time_ticks = 101;
        assert_ne!(base, reused);
        let mut other_ns = base.clone();
        other_ns.pid_namespace = PidNamespace::new("pid:[2]").unwrap();
        assert_ne!(base, other_ns);
        let mut other_host = base.clone();
        other_host.host_id = HostId::new("h2").unwrap();
        assert_ne!(base, other_host);
        let mut other_boot = base.clone();
        other_boot.boot_id = BootId::new("b2").unwrap();
        assert_ne!(base, other_boot);
        let mut other_pid = base.clone();
        other_pid.pid = Pid::new(43).unwrap();
        assert_ne!(base, other_pid);
        assert_eq!(base, base.clone());
    }

    #[test]
    fn process_key_entity_id_round_trips() {
        let process = ProcessKey {
            host_id: HostId::new("h").unwrap(),
            boot_id: BootId::new("b").unwrap(),
            pid_namespace: PidNamespace::new("pid:[4026531836]").unwrap(),
            pid: Pid::new(4242).unwrap(),
            start_time_ticks: 90_100,
        };
        let entity = process.entity_id().unwrap();
        assert_eq!(entity.as_str(), "process:pid:[4026531836]:4242:90100");
        let back = ProcessKey::from_entity_id(&process.host_id, &process.boot_id, entity.as_str())
            .unwrap();
        assert_eq!(back, process);
        assert!(
            ProcessKey::from_entity_id(&process.host_id, &process.boot_id, "process:ns:0:1")
                .is_none()
        );
        assert!(ProcessKey::from_entity_id(&process.host_id, &process.boot_id, "gpu:x").is_none());
    }

    #[test]
    fn process_key_json_uses_decimal_start_time() {
        let process = ProcessKey {
            host_id: HostId::new("h").unwrap(),
            boot_id: BootId::new("b").unwrap(),
            pid_namespace: PidNamespace::new("ns").unwrap(),
            pid: Pid::new(7).unwrap(),
            start_time_ticks: u64::MAX,
        };
        let json = serde_json::to_value(&process).unwrap();
        assert_eq!(json["start_time_ticks"], "18446744073709551615");
        assert_eq!(json["pid"], 7);
        let back: ProcessKey = serde_json::from_value(json).unwrap();
        assert_eq!(back, process);
        let bad = serde_json::json!({"host_id":"h","boot_id":"b","pid_namespace":"ns","pid":0,"start_time_ticks":"1"});
        assert!(serde_json::from_value::<ProcessKey>(bad).is_err());
    }

    #[test]
    fn bounded_identity_rejects_empty_long_and_control() {
        assert!(HostId::new("").is_err());
        assert!(HostId::new("x".repeat(257)).is_err());
        assert!(HostId::new("x".repeat(256)).is_ok());
        assert!(Scope::new("x".repeat(129)).is_err());
        assert!(EntityId::new("bad\u{1b}[0m").is_err());
    }

    #[test]
    fn confirmed_origin_requires_id() {
        let invalid = OriginIdentity {
            origin_id: None,
            relation: OriginRelation::Confirmed,
        };
        assert!(invalid.validate().is_err());
        assert!(OriginIdentity::unknown().validate().is_ok());
        assert!(OriginIdentity::unknown().confirmed_id().is_none());
    }

    #[test]
    fn series_selector_round_trips() {
        let original = key("gpu:GPU-1", "gpu.temperature", "gpu");
        let selector = original.series_selector();
        let back = MetricKey::from_series_selector(&original.host_id, &original.boot_id, &selector)
            .unwrap();
        assert_eq!(back, original);
        assert!(
            MetricKey::from_series_selector(&original.host_id, &original.boot_id, "bad").is_err()
        );
    }
}
