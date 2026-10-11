//! Rendering of one [`MetricCell`] with its evidence: value + unit, source,
//! quality word, sample-kind tag, conflict values and last-good annotation.
//!
//! The rules are fixed by `ARCHITECTURE.md` §9 and `UI.md` §2:
//! `68°C · package 0 · hwmon · usable`, `≠ source conflict · hwmon 68°C /
//! thermal 42°C`, `stale · last 72°C 12s ago`; a cell without a value shows
//! its state label and never `0` or blank.

use dgxtop_core::quality::{Quality, SampleKind};
use dgxtop_core::view::{CellState, ConflictView, LastGoodView, MetricCell};

use super::age::format_age;
use super::text::{clean, display_width};
use super::units::format_scalar;

/// Symbol that accompanies every source-conflict text (never color alone).
pub const CONFLICT_SYMBOL: &str = "≠";

/// Visual tone of a formatted cell; the text always carries the same meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellTone {
    Normal,
    Fallback,
    Missing,
    Conflict,
    Stale,
}

/// How much evidence to append after the primary text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellDetail {
    pub source: bool,
    pub quality: bool,
    pub age: bool,
}

impl CellDetail {
    /// Value, kind tag and non-verified quality word.
    pub const COMPACT: Self = Self {
        source: false,
        quality: true,
        age: false,
    };
    /// Everything the projection carries.
    #[cfg(test)]
    pub const FULL: Self = Self {
        source: true,
        quality: true,
        age: true,
    };
    /// Value plus source label (no age).
    pub const SOURCED: Self = Self {
        source: true,
        quality: true,
        age: false,
    };
    /// Value with its kind tag and fallback / last-good notes but no quality
    /// word, for at-a-glance panels. Canonical values come only from
    /// verified or usable sources, so nothing suspect is hidden; the detail
    /// views keep the quality.
    pub const GLANCE: Self = Self {
        source: false,
        quality: false,
        age: false,
    };
}

/// Formatted cell: primary text plus ordered annotations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellText {
    pub primary: String,
    pub annotations: Vec<String>,
    pub tone: CellTone,
}

impl CellText {
    /// `primary · annotation · annotation`.
    pub fn joined(&self) -> String {
        let mut text = self.primary.clone();
        for annotation in &self.annotations {
            text.push_str(" · ");
            text.push_str(annotation);
        }
        text
    }
}

/// Primary text only (value or state label), for narrow table cells.
pub fn value_text(cell: &MetricCell) -> String {
    primary(cell).0
}

/// Full description of a cell. `age_offset_ms` is the time elapsed since
/// the projection was received; it advances ages between projections.
pub fn describe_cell(cell: &MetricCell, detail: CellDetail, age_offset_ms: u64) -> CellText {
    let (primary_text, tone) = primary(cell);
    let mut annotations = Vec::new();
    let has_value = cell.state.has_value() && cell.value.is_some();
    if cell.state == CellState::Degraded {
        annotations.push(CellState::Degraded.label().to_owned());
    }
    if has_value && let Some(tag) = kind_tag(cell.kind) {
        annotations.push(tag.to_owned());
    }
    if detail.source
        && let Some(label) = cell
            .source_label
            .as_deref()
            .filter(|label| !label.is_empty())
    {
        annotations.push(clean(label));
    }
    if detail.quality
        && has_value
        && let Some(word) = quality_word(cell.quality)
    {
        annotations.push(word.to_owned());
    }
    if detail.age
        && has_value
        && let Some(age) = cell.age_ms
    {
        annotations.push(format_age(age.saturating_add(age_offset_ms)));
    }
    if let Some(last) = last_good_annotation(cell, age_offset_ms) {
        annotations.push(last);
    }
    CellText {
        primary: primary_text,
        annotations,
        tone,
    }
}

/// Tag for non-measured sample kinds (`derived` is a normal computed value).
pub const fn kind_tag(kind: SampleKind) -> Option<&'static str> {
    match kind {
        SampleKind::Measured | SampleKind::Derived => None,
        SampleKind::Estimated => Some("estimated"),
        SampleKind::Simulated => Some("simulated"),
        SampleKind::Replay => Some("replay"),
    }
}

/// Quality grade word shown whenever the grade is not `verified`.
pub const fn quality_word(quality: Quality) -> Option<&'static str> {
    match quality {
        Quality::Verified => None,
        Quality::Usable => Some("usable"),
        Quality::Suspect => Some("suspect"),
        Quality::Invalid => Some("invalid"),
        Quality::Unknown => Some("unknown quality"),
    }
}

/// State label that fits `width` columns, falling back to a short form.
pub fn state_label_for_width(state: CellState, width: usize) -> &'static str {
    let full = state.label();
    if display_width(full) <= width {
        return full;
    }
    match state {
        CellState::Ok => "ok",
        CellState::Degraded => "fallback",
        CellState::Unsupported => "unsupp.",
        CellState::ReadFailed => "read fail",
        CellState::PermissionDenied => "no perm",
        CellState::Conflict => "≠ conflict",
        CellState::Stale => "stale",
        CellState::Warmup => "warmup",
        CellState::Unavailable => "unavail.",
    }
}

/// Tone of a cell without formatting its text.
pub fn cell_tone(cell: &MetricCell) -> CellTone {
    match (cell.state, cell.value.is_some()) {
        (CellState::Ok, true) => CellTone::Normal,
        (CellState::Degraded, true) => CellTone::Fallback,
        (CellState::Conflict, _) => CellTone::Conflict,
        (CellState::Stale, _) => CellTone::Stale,
        _ => CellTone::Missing,
    }
}

fn primary(cell: &MetricCell) -> (String, CellTone) {
    let tone = cell_tone(cell);
    let text = match (cell.state, cell.value) {
        (CellState::Ok | CellState::Degraded, Some(value)) => format_scalar(value, cell.unit),
        // A "healthy" state without a value is a projection defect; it must
        // still never be drawn as zero.
        (CellState::Ok | CellState::Degraded, None) => CellState::Unavailable.label().to_owned(),
        (CellState::Conflict, _) => conflict_text(cell),
        (other, _) => other.label().to_owned(),
    };
    (text, tone)
}

fn conflict_text(cell: &MetricCell) -> String {
    let mut text = format!("{CONFLICT_SYMBOL} {}", CellState::Conflict.label());
    if let Some(ConflictView { entries }) = &cell.conflict {
        let values: Vec<String> = entries
            .iter()
            .map(|(label, value)| format!("{} {}", clean(label), format_scalar(*value, cell.unit)))
            .collect();
        if !values.is_empty() {
            text.push_str(" · ");
            text.push_str(&values.join(" / "));
        }
    }
    text
}

/// `last 72°C 12s ago` next to (never instead of) a missing current value.
fn last_good_annotation(cell: &MetricCell, age_offset_ms: u64) -> Option<String> {
    if cell.state.has_value() {
        return None;
    }
    let (value, age_ms) = match (&cell.last_good, cell.state) {
        (Some(LastGoodView { value, age_ms }), _) => (*value, *age_ms),
        (None, CellState::Stale) => (cell.value?, cell.age_ms?),
        (None, _) => return None,
    };
    Some(format!(
        "last {} {}",
        format_scalar(value, cell.unit),
        format_age(age_ms.saturating_add(age_offset_ms))
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::metric::{CanonicalUnit, Scalar};

    fn scalar(value: f64) -> Scalar {
        Scalar::from_f64(value).unwrap()
    }

    fn temperature(value: f64) -> MetricCell {
        MetricCell {
            value: Some(scalar(value)),
            unit: CanonicalUnit::DegC,
            state: CellState::Ok,
            kind: SampleKind::Measured,
            quality: Quality::Usable,
            age_ms: Some(400),
            source_label: Some("package 0 · hwmon".into()),
            conflict: None,
            last_good: None,
        }
    }

    #[test]
    fn verified_quality_has_no_word_and_zero_is_shown() {
        let mut cell = temperature(0.0);
        cell.quality = Quality::Verified;
        assert_eq!(describe_cell(&cell, CellDetail::COMPACT, 0).joined(), "0°C");
    }

    #[test]
    fn missing_states_render_their_labels_never_zero() {
        for (state, label) in [
            (CellState::Unsupported, "unsupported"),
            (CellState::ReadFailed, "read failed"),
            (CellState::PermissionDenied, "permission denied"),
            (CellState::Warmup, "warming up"),
            (CellState::Unavailable, "unavailable"),
        ] {
            let cell = MetricCell::missing(CanonicalUnit::Watts, state);
            let text = describe_cell(&cell, CellDetail::FULL, 0);
            assert_eq!(text.primary, label);
            assert_eq!(text.tone, CellTone::Missing);
            assert!(!text.joined().contains('0'));
            assert_eq!(value_text(&cell), label);
        }
    }

    #[test]
    fn ok_state_without_value_is_unavailable_not_zero() {
        let mut cell = temperature(1.0);
        cell.value = None;
        assert_eq!(value_text(&cell), "unavailable");
    }

    #[test]
    fn conflict_shows_both_values_and_symbol() {
        let mut cell = MetricCell::missing(CanonicalUnit::DegC, CellState::Conflict);
        cell.conflict = Some(ConflictView {
            entries: vec![
                ("hwmon".into(), scalar(68.0)),
                ("thermal".into(), scalar(42.0)),
            ],
        });
        let text = describe_cell(&cell, CellDetail::COMPACT, 0);
        assert_eq!(
            text.primary,
            "≠ source conflict · hwmon 68°C / thermal 42°C"
        );
        assert_eq!(text.tone, CellTone::Conflict);
    }

    #[test]
    fn stale_shows_last_value_with_advancing_age() {
        let mut cell = MetricCell::missing(CanonicalUnit::DegC, CellState::Stale);
        cell.last_good = Some(LastGoodView {
            value: scalar(72.0),
            age_ms: 10_000,
        });
        assert_eq!(
            describe_cell(&cell, CellDetail::COMPACT, 2_000).joined(),
            "stale · last 72°C 12s ago"
        );
        let mut legacy = MetricCell::missing(CanonicalUnit::DegC, CellState::Stale);
        legacy.value = Some(scalar(71.0));
        legacy.age_ms = Some(5_000);
        assert_eq!(value_text(&legacy), "stale");
        assert_eq!(
            describe_cell(&legacy, CellDetail::COMPACT, 0).joined(),
            "stale · last 71°C 5s ago"
        );
    }

    #[test]
    fn last_good_is_annotation_for_read_failures() {
        let mut cell = MetricCell::missing(CanonicalUnit::Watts, CellState::ReadFailed);
        cell.last_good = Some(LastGoodView {
            value: scalar(45.0),
            age_ms: 3_000,
        });
        assert_eq!(
            describe_cell(&cell, CellDetail::COMPACT, 0).joined(),
            "read failed · last 45.0 W 3s ago"
        );
    }

    #[test]
    fn sample_kind_tags_and_fallback() {
        let mut cell = temperature(50.0);
        cell.kind = SampleKind::Estimated;
        cell.state = CellState::Degraded;
        cell.quality = Quality::Verified;
        let text = describe_cell(&cell, CellDetail::COMPACT, 0);
        assert_eq!(text.joined(), "50°C · fallback · estimated");
        assert_eq!(text.tone, CellTone::Fallback);
        cell.kind = SampleKind::Simulated;
        assert!(
            describe_cell(&cell, CellDetail::COMPACT, 0)
                .joined()
                .contains("simulated")
        );
        cell.kind = SampleKind::Replay;
        assert!(
            describe_cell(&cell, CellDetail::COMPACT, 0)
                .joined()
                .contains("replay")
        );
    }

    #[test]
    fn untrusted_labels_are_cleaned() {
        let mut cell = temperature(68.0);
        cell.source_label = Some("evil\u{1b}]0;title\u{7}label".into());
        assert_eq!(
            describe_cell(&cell, CellDetail::SOURCED, 0).joined(),
            "68°C · evillabel · usable"
        );
    }
}
