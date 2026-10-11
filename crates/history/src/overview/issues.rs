//! What went wrong with the Overview's store (D-95): the latest issue is
//! shown on the bars (`OverviewBars::store_issue`) and each new one is
//! logged once under target `history.overview` with its `HISTORY_TIERS_*`
//! code and the cause's error code, so a failing store is traceable in the
//! log, not only on screen.

use std::fmt;
use std::sync::Arc;

use dgxtop_core::errors::{ErrorCode, InfraError};
use dgxtop_core::ports::{DispatchError, LogLevel, Logger};

pub(crate) const LOG_TARGET: &str = "history.overview";

pub(crate) const LOAD_FAILED: &str = "HISTORY_TIERS_LOAD_FAILED";
pub(crate) const WRITE_FAILED: &str = "HISTORY_TIERS_WRITE_FAILED";
pub(crate) const ARCHIVE_LIST_FAILED: &str = "HISTORY_TIERS_ARCHIVE_LIST_FAILED";
pub(crate) const ARCHIVE_READ_FAILED: &str = "HISTORY_TIERS_ARCHIVE_READ_FAILED";
pub(crate) const STORE_STOPPED: &str = "HISTORY_TIERS_STORE_STOPPED";
pub(crate) const ROWS_DROPPED: &str = "HISTORY_TIERS_ROWS_DROPPED";
pub(crate) const SERIES_GAVE_WAY: &str = "HISTORY_TIERS_SERIES_GAVE_WAY";

/// The store's latest issue and where issues are logged.
pub(crate) struct StoreIssues {
    logger: Arc<dyn Logger>,
    latest: Option<String>,
}

impl fmt::Debug for StoreIssues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoreIssues")
            .field("latest", &self.latest)
            .finish_non_exhaustive()
    }
}

impl StoreIssues {
    pub(crate) fn new(logger: Arc<dyn Logger>) -> Self {
        Self {
            logger,
            latest: None,
        }
    }

    /// The issue shown on the bars.
    pub(crate) fn latest(&self) -> Option<&str> {
        self.latest.as_deref()
    }

    /// A failed store or archive call.
    pub(crate) fn infra(&mut self, code: &'static str, prefix: &str, error: &InfraError) {
        self.report(
            LogLevel::Warn,
            code,
            error.code(),
            format!("{prefix}: {error}"),
        );
    }

    /// The store actor refused a job for good.
    pub(crate) fn stopped(&mut self, error: &DispatchError) {
        let cause = match error {
            DispatchError::Capacity(_) => ErrorCode::CapacityExceeded,
            DispatchError::Busy | DispatchError::Closed => ErrorCode::Unavailable,
        };
        let text = format!("history tiers store stopped: {error}");
        self.report(LogLevel::Error, STORE_STOPPED, cause, text);
    }

    /// Closed buckets dropped while the store fell behind.
    pub(crate) fn rows_dropped(&mut self) {
        let text = "history tiers store fell behind; oldest buckets dropped".to_owned();
        self.report(
            LogLevel::Warn,
            ROWS_DROPPED,
            ErrorCode::CapacityExceeded,
            text,
        );
    }

    /// Series let go for the store's budget: logged, not an issue.
    pub(crate) fn gave_way(&self, series: usize, used_bytes: u64, budget_bytes: u64) {
        self.logger.log(
            LogLevel::Info,
            LOG_TARGET,
            &format!(
                "{SERIES_GAVE_WAY}: {series} series let go, the store holding \
                 {used_bytes} bytes past its budget of {budget_bytes}"
            ),
        );
    }

    /// Shows `text` and logs it once, until another issue replaces it.
    fn report(&mut self, level: LogLevel, code: &'static str, cause: ErrorCode, text: String) {
        if self.latest.as_deref() != Some(text.as_str()) {
            let line = format!("{code} [{}]: {text}", cause.as_str());
            self.logger.log(level, LOG_TARGET, &line);
        }
        self.latest = Some(text);
    }
}
