//! The compact per-process record of the procfs `process` group (D-79,
//! D-86): one `process.cpu.ticks` observation per process instead of one
//! per metric, its other `stat` facts riding along as `proc.<field>:<value>`
//! reason codes. The observation schema stays the spec's: a record already
//! carries free tags this way (NVML's `gpu_usage:*`), and a tag costs a few
//! bytes where a separate record repeats the whole key and provenance.
//!
//! A field the adapter could not read is left out, never sent as zero.

/// Prefix of every tag of the compact record.
pub const TAG_PREFIX: &str = "proc.";

const RSS: &str = "proc.rss:";
const UID: &str = "proc.uid:";
const PPID: &str = "proc.ppid:";
const STATE: &str = "proc.state:";
const REAPED_TICKS: &str = "proc.reaped_ticks:";

/// Reason codes the adapter attaches when it could not read the RSS; the
/// collector passes the one it finds on to the row.
pub const PAGE_SIZE_UNKNOWN: &str = "PAGE_SIZE_UNKNOWN";
pub const RSS_FIELD_ABSENT: &str = "RSS_FIELD_ABSENT";
pub const RSS_PARSE_ERROR: &str = "RSS_PARSE_ERROR";
pub const RSS_BYTES_OVERFLOW: &str = "RSS_BYTES_OVERFLOW";
const RSS_FAILURES: [&str; 4] = [
    PAGE_SIZE_UNKNOWN,
    RSS_FIELD_ABSENT,
    RSS_PARSE_ERROR,
    RSS_BYTES_OVERFLOW,
];

/// The RSS failure among a record's reason codes, if any.
pub fn rss_failure<'a>(codes: impl IntoIterator<Item = &'a String>) -> Option<&'static str> {
    codes
        .into_iter()
        .find_map(|code| RSS_FAILURES.into_iter().find(|failure| code == failure))
}

/// What one `stat` read (and the cached owner) says of a process besides
/// its own CPU ticks, which are the record's value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessStat {
    /// Resident memory, bytes (pages × page size).
    pub rss_bytes: Option<u64>,
    /// Owner of `/proc/<pid>`.
    pub uid: Option<u32>,
    pub ppid: Option<i32>,
    /// The state letter (`R`, `S`, `Z`, …).
    pub state: Option<char>,
    /// CPU time of its reaped children (`cutime + cstime`), ticks.
    pub reaped_ticks: Option<u64>,
}

impl ProcessStat {
    /// The tags this stat travels as, in a fixed order.
    pub fn reason_codes(&self) -> Vec<String> {
        let mut codes = Vec::with_capacity(5);
        let mut push = |prefix: &str, value: Option<String>| {
            if let Some(value) = value {
                codes.push(format!("{prefix}{value}"));
            }
        };
        push(RSS, self.rss_bytes.map(|value| value.to_string()));
        push(UID, self.uid.map(|value| value.to_string()));
        push(PPID, self.ppid.map(|value| value.to_string()));
        push(STATE, self.state.map(String::from));
        push(
            REAPED_TICKS,
            self.reaped_ticks.map(|value| value.to_string()),
        );
        codes
    }

    /// The stat in a record's reason codes; a field without its tag, or
    /// whose value does not parse, is absent.
    pub fn from_reason_codes<'a>(codes: impl IntoIterator<Item = &'a String>) -> Self {
        let mut stat = Self::default();
        for code in codes {
            if code.starts_with(TAG_PREFIX) {
                stat.take(code);
            }
        }
        stat
    }

    /// Whether `code` is one tag of this format exactly as the encoder
    /// writes it (a replay fixture may carry it as a sample reason).
    pub fn is_tag(code: &str) -> bool {
        let mut stat = Self::default();
        stat.take(code);
        stat.reason_codes() == [code]
    }

    fn take(&mut self, code: &str) {
        if let Some(value) = code.strip_prefix(RSS) {
            self.rss_bytes = value.parse().ok();
        } else if let Some(value) = code.strip_prefix(UID) {
            self.uid = value.parse().ok();
        } else if let Some(value) = code.strip_prefix(PPID) {
            self.ppid = value.parse().ok();
        } else if let Some(value) = code.strip_prefix(STATE) {
            let mut letters = value.chars();
            self.state = letters.next().filter(|_| letters.next().is_none());
        } else if let Some(value) = code.strip_prefix(REAPED_TICKS) {
            self.reaped_ticks = value.parse().ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::MAX_REASON_CODES;

    fn full() -> ProcessStat {
        ProcessStat {
            rss_bytes: Some(u64::MAX),
            uid: Some(u32::MAX),
            ppid: Some(i32::MAX),
            state: Some('S'),
            reaped_ticks: Some(u64::MAX),
        }
    }

    #[test]
    fn a_stat_round_trips_through_its_tags() {
        let stat = full();
        let codes = stat.reason_codes();
        assert_eq!(codes.len(), 5);
        assert_eq!(ProcessStat::from_reason_codes(&codes), stat);
    }

    #[test]
    fn every_tag_fits_a_reason_code() {
        let codes = full().reason_codes();
        assert!(codes.len() <= MAX_REASON_CODES);
        assert!(
            codes
                .iter()
                .all(|code| !code.is_empty() && code.len() <= 80),
            "{codes:?}"
        );
    }

    #[test]
    fn unread_fields_are_left_out_not_zero() {
        let stat = ProcessStat {
            uid: Some(1000),
            ..ProcessStat::default()
        };
        assert_eq!(stat.reason_codes(), vec!["proc.uid:1000".to_owned()]);
        let back = ProcessStat::from_reason_codes(&stat.reason_codes());
        assert_eq!(back.rss_bytes, None);
        assert_eq!(back.uid, Some(1000));
    }

    #[test]
    fn the_rss_failure_is_found_among_other_codes() {
        let codes: Vec<String> = ["proc.uid:1", RSS_BYTES_OVERFLOW, "OWNER_UNREADABLE"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(rss_failure(&codes), Some(RSS_BYTES_OVERFLOW));
        assert_eq!(rss_failure(&codes[..1]), None);
    }

    #[test]
    fn only_tags_as_the_encoder_writes_them_are_tags() {
        for code in full().reason_codes() {
            assert!(ProcessStat::is_tag(&code), "{code}");
        }
        for code in [
            "proc.rss:-1",
            "proc.rss:007",
            "proc.state:SS",
            "proc.unknown:5",
            "proc.uid:",
            "RSS_BYTES_OVERFLOW",
        ] {
            assert!(!ProcessStat::is_tag(code), "{code}");
        }
    }

    #[test]
    fn a_record_without_tags_has_no_stat() {
        let codes = vec![PAGE_SIZE_UNKNOWN.to_owned(), "gpu_usage:compute".to_owned()];
        assert_eq!(
            ProcessStat::from_reason_codes(&codes),
            ProcessStat::default()
        );
        assert_eq!(
            ProcessStat::from_reason_codes(&Vec::new()),
            ProcessStat::default()
        );
    }

    #[test]
    fn garbage_values_count_as_absent_and_other_codes_are_ignored() {
        let codes: Vec<String> = [
            "proc.rss:-1",
            "proc.state:SS",
            "proc.uid:",
            "proc.unknown:5",
            RSS_BYTES_OVERFLOW,
            "proc.ppid:1",
        ]
        .map(str::to_owned)
        .to_vec();
        let stat = ProcessStat::from_reason_codes(&codes);
        assert_eq!(
            stat,
            ProcessStat {
                ppid: Some(1),
                ..ProcessStat::default()
            }
        );
    }
}
