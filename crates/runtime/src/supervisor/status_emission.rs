//! When a host status is worth a `HostStatus` completion.
//!
//! Every field but the success timestamp changes rarely, so any change is
//! sent at once. The success timestamp moves on every read (each host at
//! about 1 Hz); sending each one would flood the Manager's control inbox, yet
//! the Settings tab shows it as an age ("LAST OK", D-91), so the Manager must
//! learn of the first success and then of later ones often enough for that
//! age to stay meaningful. The rate limit compares the success timestamps
//! themselves, so it needs no clock of its own.

use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::view::HostStatus;

/// Smallest advance of the success timestamp that is sent on its own.
pub(crate) const SUCCESS_REFRESH_NS: u64 = 5 * NANOS_PER_SECOND;

/// Whether `current` must be emitted given the last emitted status.
pub(crate) fn should_emit(previous: Option<&HostStatus>, current: &HostStatus) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    other_fields_changed(previous, current)
        || success_is_due(previous.last_success_boot_ns, current.last_success_boot_ns)
}

fn other_fields_changed(previous: &HostStatus, current: &HostStatus) -> bool {
    let comparable = HostStatus {
        last_success_boot_ns: current.last_success_boot_ns,
        ..previous.clone()
    };
    comparable != *current
}

fn success_is_due(previous: Option<u64>, current: Option<u64>) -> bool {
    match (previous, current) {
        (None, Some(_)) => true,
        (Some(sent), Some(latest)) => latest.saturating_sub(sent) >= SUCCESS_REFRESH_NS,
        (_, None) => false,
    }
}

#[cfg(test)]
mod tests {
    use dgxtop_core::view::HostState;

    use super::*;

    fn status(last_success_boot_ns: Option<u64>) -> HostStatus {
        HostStatus {
            adapter_id: "acme".into(),
            state: HostState::Healthy,
            generation: 1,
            pid: Some(42),
            last_success_boot_ns,
            timeouts_total: 0,
            consecutive_failures: 0,
            restarts_total: 0,
            backoff_until_boot_ns: None,
            detail: None,
        }
    }

    #[test]
    fn the_first_status_is_always_emitted() {
        assert!(should_emit(None, &status(None)));
    }

    #[test]
    fn the_first_success_is_emitted_at_once() {
        assert!(should_emit(Some(&status(None)), &status(Some(1))));
    }

    #[test]
    fn later_successes_are_emitted_at_most_every_refresh_interval() {
        let sent = status(Some(10 * NANOS_PER_SECOND));
        let just_before = 10 * NANOS_PER_SECOND + SUCCESS_REFRESH_NS - 1;
        let exactly_due = 10 * NANOS_PER_SECOND + SUCCESS_REFRESH_NS;
        assert!(!should_emit(
            Some(&sent),
            &status(Some(10 * NANOS_PER_SECOND))
        ));
        assert!(!should_emit(Some(&sent), &status(Some(just_before))));
        assert!(should_emit(Some(&sent), &status(Some(exactly_due))));
    }

    #[test]
    fn an_unchanged_status_without_successes_is_not_emitted() {
        assert!(!should_emit(Some(&status(None)), &status(None)));
    }

    #[test]
    fn any_other_change_is_emitted_even_within_the_interval() {
        let sent = status(Some(NANOS_PER_SECOND));
        let backoff = HostStatus {
            state: HostState::CircuitOpen,
            backoff_until_boot_ns: Some(9 * NANOS_PER_SECOND),
            ..status(Some(NANOS_PER_SECOND + 1))
        };
        let detail = HostStatus {
            detail: Some("restarted".into()),
            ..status(Some(NANOS_PER_SECOND))
        };
        let restarted = HostStatus {
            generation: 2,
            restarts_total: 1,
            ..status(Some(NANOS_PER_SECOND))
        };
        assert!(should_emit(Some(&sent), &backoff));
        assert!(should_emit(Some(&sent), &detail));
        assert!(should_emit(Some(&sent), &restarted));
    }

    #[test]
    fn a_success_timestamp_that_went_backwards_is_not_emitted_alone() {
        let sent = status(Some(20 * NANOS_PER_SECOND));
        assert!(!should_emit(Some(&sent), &status(Some(NANOS_PER_SECOND))));
    }
}
