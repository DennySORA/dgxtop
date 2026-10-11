//! Scan budget, deadline and cancellation of one query.

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::{ErrorCode, ErrorDetail};
use dgxtop_core::history::HistoryQuery;
use dgxtop_core::ports::CancelFlag;
use uuid::Uuid;

use super::QueryLimits;

/// Records scanned between two deadline/cancel checks.
const CHECK_INTERVAL: u64 = 512;

/// Enforces the per-query scan budget, deadline and cancel flag.
pub(crate) struct ScanGuard<'q> {
    request_id: Uuid,
    selector: String,
    budget: u64,
    scanned: u64,
    since_check: u64,
    deadline_ns: u64,
    cancel: &'q CancelFlag,
    clock: &'q dyn TimeProvider,
}

impl<'q> ScanGuard<'q> {
    pub(crate) fn new(
        query: &HistoryQuery,
        limits: &QueryLimits,
        deadline_ns: u64,
        cancel: &'q CancelFlag,
        clock: &'q dyn TimeProvider,
    ) -> Self {
        Self {
            request_id: query.request_id,
            selector: query.key.series_selector(),
            budget: limits.max_scan_records,
            scanned: 0,
            since_check: 0,
            deadline_ns,
            cancel,
            clock,
        }
    }

    fn error(&self, code: ErrorCode, message: impl AsRef<str>) -> ErrorDetail {
        ErrorDetail::new(code, message)
            .with_request(self.request_id)
            .with_affected(&self.selector)
    }

    /// Current boot time of the query clock.
    pub(crate) fn now(&self) -> u64 {
        self.clock.now_boot_ns()
    }

    /// Fails with `Cancelled` or `TimedOut` (cancellation wins).
    pub(crate) fn checkpoint(&mut self) -> Result<(), ErrorDetail> {
        self.since_check = 0;
        if self.cancel.is_cancelled() {
            return Err(self.error(ErrorCode::Cancelled, "history query cancelled"));
        }
        let now = self.clock.now_boot_ns();
        if now >= self.deadline_ns {
            return Err(self.error(
                ErrorCode::TimedOut,
                format!(
                    "history query passed its deadline after scanning {} records",
                    self.scanned
                ),
            ));
        }
        Ok(())
    }

    /// Charges scanned records; checks the clock every few hundred records.
    pub(crate) fn charge(&mut self, records: u64) -> Result<(), ErrorDetail> {
        self.scanned = self.scanned.saturating_add(records);
        if self.scanned > self.budget {
            return Err(self.error(
                ErrorCode::QueryTooLarge,
                format!(
                    "history query scanned more than {} records; retry with resolution_ms >= 60000 or a shorter window",
                    self.budget
                ),
            ));
        }
        self.since_check = self.since_check.saturating_add(records);
        if self.since_check >= CHECK_INTERVAL {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Rejects a query whose estimated scan exceeds the budget before any
    /// work is done, naming a coarser resolution or shorter window to retry.
    pub(crate) fn admit_estimate(
        &self,
        estimate: u64,
        suggestion: &str,
    ) -> Result<(), ErrorDetail> {
        if estimate <= self.budget {
            return Ok(());
        }
        Err(self.error(
            ErrorCode::QueryTooLarge,
            format!(
                "history query would scan {estimate} records (limit {}); {suggestion}",
                self.budget
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::history::{HISTORY_SCHEMA_VERSION, HistoryStat};
    use dgxtop_core::testing::{FakeClock, metric_key};

    fn query() -> HistoryQuery {
        HistoryQuery {
            schema_version: HISTORY_SCHEMA_VERSION,
            request_id: Uuid::new_v4(),
            key: metric_key("gpu:0", "gpu.temperature", "gpu"),
            start_boot_ns: 0,
            end_boot_ns: 10,
            resolution_ms: 0,
            stat: HistoryStat::Mean,
            max_points: 10,
            source_filter: None,
        }
    }

    #[test]
    fn cancellation_wins_over_deadline() {
        let clock = FakeClock::new(100);
        let cancel = CancelFlag::new();
        let limits = QueryLimits::new(100, 1 << 20, 1000);
        let query = query();
        let mut guard = ScanGuard::new(&query, &limits, 50, &cancel, clock.as_ref());
        assert_eq!(guard.checkpoint().unwrap_err().code, ErrorCode::TimedOut);
        cancel.cancel();
        let error = guard.checkpoint().unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert_eq!(error.request_id, Some(query.request_id));
    }

    #[test]
    fn budget_and_estimate_are_enforced() {
        let clock = FakeClock::new(0);
        let cancel = CancelFlag::new();
        let limits = QueryLimits::new(100, 1 << 20, 1000);
        let query = query();
        let mut guard = ScanGuard::new(&query, &limits, u64::MAX, &cancel, clock.as_ref());
        guard.charge(1000).unwrap();
        assert_eq!(guard.charge(1).unwrap_err().code, ErrorCode::QueryTooLarge);
        let error = guard
            .admit_estimate(5000, "retry with resolution_ms >= 60000")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::QueryTooLarge);
        assert!(error.message.contains("resolution_ms >= 60000"));
        assert!(guard.admit_estimate(1000, "").is_ok());
    }

    #[test]
    fn deadline_is_checked_while_scanning() {
        let clock = FakeClock::new(0);
        let cancel = CancelFlag::new();
        let limits = QueryLimits::new(100, 1 << 20, 100_000);
        let query = query();
        let mut guard = ScanGuard::new(&query, &limits, 10, &cancel, clock.as_ref());
        guard.charge(CHECK_INTERVAL - 1).unwrap();
        clock.set_boot_ns(10);
        assert_eq!(guard.charge(1).unwrap_err().code, ErrorCode::TimedOut);
    }
}
