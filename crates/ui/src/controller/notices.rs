//! Bounded, expiring notices (busy queue, command results, refusals).

use std::collections::VecDeque;
use std::time::Duration;

use dgxtop_core::view::BannerLevel;

use crate::viewer::Notice;

/// At most this many notices are shown at once.
pub const MAX_NOTICES: usize = 4;

const INFO_TTL: Duration = Duration::from_secs(5);
const WARNING_TTL: Duration = Duration::from_secs(8);
const ERROR_TTL: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, PartialEq, Eq)]
struct TimedNotice {
    notice: Notice,
    expires_at: Duration,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Notices {
    entries: VecDeque<TimedNotice>,
}

impl Notices {
    /// Adds a notice; an identical text refreshes its expiry instead of
    /// duplicating, and the oldest notice is evicted beyond the bound.
    pub fn push(&mut self, level: BannerLevel, text: impl Into<String>, now: Duration) {
        let text = text.into();
        let ttl = match level {
            BannerLevel::Info => INFO_TTL,
            BannerLevel::Warning => WARNING_TTL,
            BannerLevel::Error | BannerLevel::Confirmation => ERROR_TTL,
        };
        let expires_at = now.saturating_add(ttl);
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|entry| entry.notice.text == text && entry.notice.level == level)
        {
            existing.expires_at = expires_at;
            return;
        }
        if self.entries.len() == MAX_NOTICES {
            self.entries.pop_front();
        }
        self.entries.push_back(TimedNotice {
            notice: Notice { level, text },
            expires_at,
        });
    }

    /// Drops expired notices; returns whether anything changed.
    pub fn expire(&mut self, now: Duration) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.expires_at > now);
        before != self.entries.len()
    }

    /// Removes everything; returns whether anything was shown.
    pub fn clear(&mut self) -> bool {
        let had = !self.entries.is_empty();
        self.entries.clear();
        had
    }

    pub fn visible(&self) -> Vec<Notice> {
        self.entries
            .iter()
            .map(|entry| entry.notice.clone())
            .collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_are_bounded_deduplicated_and_expire() {
        let mut notices = Notices::default();
        let start = Duration::from_secs(1);
        for index in 0..10 {
            notices.push(BannerLevel::Info, format!("n{index}"), start);
        }
        assert_eq!(notices.len(), MAX_NOTICES);
        assert_eq!(notices.visible()[0].text, "n6");
        notices.push(BannerLevel::Info, "n9", start + Duration::from_secs(3));
        assert_eq!(notices.len(), MAX_NOTICES);
        assert!(notices.expire(start + Duration::from_secs(6)));
        assert_eq!(notices.visible().len(), 1, "refreshed notice survives");
        notices.push(BannerLevel::Error, "bad", start);
        assert!(notices.clear());
        assert!(notices.is_empty());
        assert!(!notices.expire(start));
    }
}
