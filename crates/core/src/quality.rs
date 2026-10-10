//! The independent evidence axes of a sample.
//!
//! Read status, sample kind, quality, freshness and selection state are kept
//! separate on purpose: none of them is a probability and they are never
//! collapsed into a single "trust score".

use serde::{Deserialize, Serialize};

/// Whether the source could provide this value at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadStatus {
    Ok,
    Unsupported,
    PermissionDenied,
    Timeout,
    Error,
    Removed,
}

impl ReadStatus {
    pub const fn is_ok(self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// What kind of value this is. Simulated/replay values never enter live
/// canonical state or live history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleKind {
    Measured,
    Derived,
    Estimated,
    Simulated,
    Replay,
}

impl SampleKind {
    /// Namespace this kind belongs to.
    pub const fn namespace(self) -> Namespace {
        match self {
            Self::Measured | Self::Derived | Self::Estimated => Namespace::Live,
            Self::Simulated | Self::Replay => Namespace::Replay,
        }
    }
}

/// Data namespace. Live and replay data never mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Namespace {
    Live,
    Replay,
}

/// Versioned-profile quality grade. A grade, not a probability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Verified,
    Usable,
    Suspect,
    Invalid,
    Unknown,
}

impl Quality {
    /// Only verified/usable sources may become canonical
    /// (`allow_suspect_as_canonical` is a fixed `false`).
    pub const fn is_canonical_eligible(self) -> bool {
        matches!(self, Self::Verified | Self::Usable)
    }

    /// Ordering rank for arbitration (lower is better).
    pub const fn rank(self) -> u8 {
        match self {
            Self::Verified => 0,
            Self::Usable => 1,
            Self::Suspect => 2,
            Self::Unknown => 3,
            Self::Invalid => 4,
        }
    }
}

/// Freshness of either the last read or the hardware measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Fresh,
    Stale,
    Unknown,
}

/// Arbitration outcome for one `MetricKey`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionState {
    /// A healthy primary source was selected.
    Selected,
    /// A valid fallback was selected because the preferred source failed.
    Degraded,
    /// Independent comparable sources disagree beyond tolerance; value is null.
    Conflict,
    /// No eligible candidate; value is null.
    Unavailable,
}

impl SelectionState {
    pub const fn has_value(self) -> bool {
        matches!(self, Self::Selected | Self::Degraded)
    }
}

/// Whether a raw source value is a level or a monotonic accumulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawKind {
    Gauge,
    Counter,
    /// Static or slowly changing metadata value (limits, totals, maxima).
    Static,
}

/// Capability support state; "library loads" does not imply "metric supported".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Supported,
    Unsupported,
    Unknown,
}

/// Adapter discovery availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Available,
    Unavailable,
    PermissionDenied,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_kinds_are_never_live() {
        assert_eq!(SampleKind::Simulated.namespace(), Namespace::Replay);
        assert_eq!(SampleKind::Replay.namespace(), Namespace::Replay);
        assert_eq!(SampleKind::Measured.namespace(), Namespace::Live);
        assert_eq!(SampleKind::Estimated.namespace(), Namespace::Live);
    }

    #[test]
    fn suspect_is_not_canonical() {
        assert!(Quality::Verified.is_canonical_eligible());
        assert!(Quality::Usable.is_canonical_eligible());
        assert!(!Quality::Suspect.is_canonical_eligible());
        assert!(!Quality::Unknown.is_canonical_eligible());
        assert!(!Quality::Invalid.is_canonical_eligible());
        assert!(Quality::Verified.rank() < Quality::Usable.rank());
    }

    #[test]
    fn serde_names_match_schema() {
        assert_eq!(
            serde_json::to_string(&ReadStatus::PermissionDenied).unwrap(),
            "\"permission_denied\""
        );
        assert_eq!(
            serde_json::to_string(&SelectionState::Conflict).unwrap(),
            "\"conflict\""
        );
        assert_eq!(
            serde_json::to_string(&Freshness::Unknown).unwrap(),
            "\"unknown\""
        );
    }
}
