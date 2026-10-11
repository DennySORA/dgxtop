//! Accumulates the products of one collection job. Every canonical value is
//! validated against the canonical schema rules before it is accepted; a
//! violation drops the value and records a traceable failure instead.

use std::collections::BTreeSet;

use dgxtop_core::canonical::{
    CanonicalValue, CollectionFailure, CollectorEvent, CounterInterval, Explanation, ProcessTable,
};
use dgxtop_core::identity::{MetricKey, SourceId};

use crate::shared::reasons;

/// Upper bound on failures reported by one batch.
pub(crate) const MAX_FAILURES: usize = 256;

#[derive(Debug, Default)]
pub(crate) struct Output {
    pub canonical: Vec<CanonicalValue>,
    pub explanations: Vec<Explanation>,
    pub intervals: Vec<CounterInterval>,
    pub failures: Vec<CollectionFailure>,
    pub events: Vec<CollectorEvent>,
    pub process_table: Option<ProcessTable>,
    reported: BTreeSet<(String, String)>,
    truncated: bool,
}

/// Subject of a failure: the source and/or key it concerns.
#[derive(Debug, Clone, Default)]
pub(crate) struct Subject {
    pub source_id: Option<SourceId>,
    pub key: Option<MetricKey>,
}

impl Subject {
    pub fn source(source_id: &SourceId) -> Self {
        Self {
            source_id: Some(source_id.clone()),
            key: None,
        }
    }

    pub fn key(key: &MetricKey) -> Self {
        Self {
            source_id: None,
            key: Some(key.clone()),
        }
    }

    pub fn none() -> Self {
        Self::default()
    }
}

impl Output {
    /// Accepts `value` when it satisfies the canonical schema; returns
    /// whether it was accepted.
    pub fn push_canonical(&mut self, value: CanonicalValue) -> bool {
        match value.validate() {
            Ok(()) => {
                self.canonical.push(value);
                true
            }
            Err(error) => {
                let message = format!("{}: {error}", value.key.series_selector());
                self.failure(
                    reasons::CANONICAL_INVALID,
                    &message,
                    Subject::key(&value.key),
                );
                false
            }
        }
    }

    /// Records a failure once per `(code, message)`; bounded by [`MAX_FAILURES`].
    pub fn failure(&mut self, code: &str, message: &str, subject: Subject) {
        if self.truncated || !self.reported.insert((code.to_owned(), message.to_owned())) {
            return;
        }
        if self.failures.len() + 1 >= MAX_FAILURES {
            self.truncated = true;
            self.failures.push(CollectionFailure {
                source_id: None,
                key: None,
                code: reasons::FAILURES_TRUNCATED.to_owned(),
                message: format!("more than {MAX_FAILURES} failures in one batch"),
            });
            return;
        }
        self.failures.push(CollectionFailure {
            source_id: subject.source_id,
            key: subject.key,
            code: code.to_owned(),
            message: dgxtop_core::text::sanitize_message(message),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::metric::{CanonicalUnit, Scalar};
    use dgxtop_core::quality::{Freshness, Quality, SampleKind, SelectionState};
    use dgxtop_core::testing::metric_key;

    fn unavailable_with_value() -> CanonicalValue {
        CanonicalValue {
            key: metric_key("memory", "memory.used", "system"),
            cycle_id: 1,
            source_id: None,
            source_generation: None,
            unit: CanonicalUnit::Bytes,
            value: Some(Scalar::from_u64(1)),
            sample_kind: SampleKind::Derived,
            selection_state: SelectionState::Unavailable,
            quality: Quality::Unknown,
            freshness: Freshness::Unknown,
            measurement_freshness: Freshness::Unknown,
            observed_boot_ns: None,
            read_finished_boot_ns: 0,
            policy_revision: 1,
            candidate_source_ids: vec![],
            reason_codes: vec![],
            eligible_for_history: false,
        }
    }

    #[test]
    fn invalid_canonical_value_is_dropped_with_a_failure() {
        let mut output = Output::default();
        assert!(!output.push_canonical(unavailable_with_value()));
        assert!(output.canonical.is_empty());
        assert_eq!(output.failures.len(), 1);
        assert_eq!(output.failures[0].code, reasons::CANONICAL_INVALID);
    }

    #[test]
    fn failures_are_deduplicated_and_bounded() {
        let mut output = Output::default();
        output.failure("X", "same", Subject::none());
        output.failure("X", "same", Subject::none());
        assert_eq!(output.failures.len(), 1);
        for index in 0..1000 {
            output.failure("Y", &format!("message {index}"), Subject::none());
        }
        assert_eq!(output.failures.len(), MAX_FAILURES);
        assert_eq!(
            output.failures.last().map(|failure| failure.code.as_str()),
            Some(reasons::FAILURES_TRUNCATED)
        );
    }
}
