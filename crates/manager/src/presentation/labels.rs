//! Display text helpers shared by the module builders.
//!
//! Strings from drivers, the catalog and processes are untrusted: every one
//! that reaches a projection is sanitized and bounded here so frames stay
//! terminal-safe and projections stay small.

use dgxtop_core::clock::NANOS_PER_MILLI;
use dgxtop_core::commands::MAX_FILTER_BYTES;
use dgxtop_core::naming::entity::SCOPE_SOC;
use dgxtop_core::text::sanitize_display;
use dgxtop_core::view::HostStatus;

/// Longest label kept for names coming from drivers, the catalog or processes.
pub(crate) const MAX_LABEL_BYTES: usize = 128;
/// Longest free-form note, detail or banner text kept in a projection.
pub(crate) const MAX_NOTE_BYTES: usize = 256;

pub(crate) fn label_text(raw: &str) -> String {
    sanitize_display(raw, MAX_LABEL_BYTES)
}

pub(crate) fn note_text(raw: &str) -> String {
    sanitize_display(raw, MAX_NOTE_BYTES)
}

/// `package:0` becomes `package 0`: scopes are spelled `kind:id` by the
/// naming contract and are shown as words.
pub(crate) fn humanize_scope(scope: &str) -> String {
    if scope == SCOPE_SOC {
        return "SoC".to_owned();
    }
    label_text(&scope.replace(':', " "))
}

/// Adapter part of an `adapter:device:channel` source id.
pub(crate) fn adapter_prefix(source_id: &str) -> &str {
    source_id.split(':').next().unwrap_or(source_id)
}

/// Milliseconds between a boot-clock instant and `now` (0 for future instants).
pub(crate) fn age_ms(now_boot_ns: u64, instant_boot_ns: u64) -> u64 {
    now_boot_ns.saturating_sub(instant_boot_ns) / NANOS_PER_MILLI
}

/// Joins at most `limit` items and says how many were left out.
pub(crate) fn bounded_list<S: AsRef<str>>(items: &[S], limit: usize) -> String {
    let shown: Vec<&str> = items.iter().take(limit).map(AsRef::as_ref).collect();
    let hidden = items.len().saturating_sub(limit);
    if hidden == 0 {
        shown.join(", ")
    } else {
        format!("{} (+{hidden} more)", shown.join(", "))
    }
}

/// "adapter nvml quarantined (requires intervention): <detail>".
pub(crate) fn host_state_text(subject: &str, status: &HostStatus) -> String {
    let text = match &status.detail {
        Some(detail) => format!("{subject} {}: {detail}", status.state.label()),
        None => format!("{subject} {}", status.state.label()),
    };
    note_text(&text)
}

/// The session filter as displayed (sanitized, at most the command limit).
pub(crate) fn filter_text(raw: &str) -> String {
    sanitize_display(raw, MAX_FILTER_BYTES)
}

/// Case-insensitive substring filter; an empty filter matches everything.
pub(crate) struct TextNeedle {
    needle: Option<String>,
}

impl TextNeedle {
    pub(crate) fn new(raw: &str) -> Self {
        let trimmed = raw.trim();
        Self {
            needle: (!trimmed.is_empty()).then(|| trimmed.to_lowercase()),
        }
    }

    pub(crate) fn matches(&self, haystack: &str) -> bool {
        self.needle
            .as_deref()
            .is_none_or(|needle| haystack.to_lowercase().contains(needle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_text_is_stripped_and_bounded() {
        assert_eq!(label_text("ev\u{1b}[31mil"), "evil");
        assert!(note_text(&"x".repeat(1000)).len() <= MAX_NOTE_BYTES);
    }

    #[test]
    fn age_saturates_for_future_instants() {
        assert_eq!(age_ms(5_000_000_000, 4_000_000_000), 1000);
        assert_eq!(age_ms(1, 2), 0);
    }

    #[test]
    fn bounded_list_reports_hidden_items() {
        assert_eq!(bounded_list(&["a", "b"], 5), "a, b");
        assert_eq!(bounded_list(&["a", "b", "c"], 2), "a, b (+1 more)");
        assert_eq!(bounded_list::<&str>(&[], 2), "");
    }
}
