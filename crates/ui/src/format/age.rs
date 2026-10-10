//! Age, countdown and interval text.

/// `<1s ago`, `12s ago`, `3m 05s ago`, `2h 03m ago`, `1d 4h ago`.
pub fn format_age(age_ms: u64) -> String {
    if age_ms < 1000 {
        return "<1s ago".to_owned();
    }
    format!("{} ago", format_span_seconds(age_ms / 1000))
}

/// Compact span: `12s`, `3m 05s`, `2h 03m`, `1d 4h`.
pub fn format_span_seconds(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else if seconds < 86_400 {
        format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
    } else {
        format!("{}d {}h", seconds / 86_400, (seconds % 86_400) / 3600)
    }
}

/// Countdown rounded up so a token never shows `0s` while still valid.
pub fn format_countdown_ms(remaining_ms: u64) -> String {
    format_span_seconds(remaining_ms.div_ceil(1000))
}

/// Sampling interval: `250 ms`, `1 s`, `1.5 s`.
pub fn format_interval_ms(interval_ms: u32) -> String {
    if interval_ms < 1000 {
        format!("{interval_ms} ms")
    } else if interval_ms.is_multiple_of(1000) {
        format!("{} s", interval_ms / 1000)
    } else {
        format!("{}.{} s", interval_ms / 1000, (interval_ms % 1000) / 100)
    }
}

/// Requested history span: whole units when exact (`5m`, `1h`, `24h`).
pub fn format_span_ns(span_ns: u64) -> String {
    let seconds = span_ns / 1_000_000_000;
    if seconds == 0 {
        return format!("{} ms", span_ns / 1_000_000);
    }
    if seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format_span_seconds(seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        assert_eq!(format_age(0), "<1s ago");
        assert_eq!(format_age(12_400), "12s ago");
        assert_eq!(format_age(185_000), "3m 05s ago");
        assert_eq!(format_age(7_380_000), "2h 03m ago");
        assert_eq!(format_age(100_800_000), "1d 4h ago");
        assert_eq!(format_age(u64::MAX), "213503982334d 14h ago");
    }

    #[test]
    fn countdown_rounds_up() {
        assert_eq!(format_countdown_ms(9_001), "10s");
        assert_eq!(format_countdown_ms(1), "1s");
        assert_eq!(format_countdown_ms(0), "0s");
    }
}
