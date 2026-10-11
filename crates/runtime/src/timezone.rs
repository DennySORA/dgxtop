//! This host's offset from UTC (D-76), for showing wall-clock times in
//! local time: from `TZ` when set (a zone name or a POSIX rule), else from
//! `/etc/localtime`, the way the C library finds it. Read once at startup;
//! a time zone change while dgxtop runs is not followed.
//!
//! A zone file (TZif, RFC 8536) lists transitions; past the last one its
//! footer's POSIX rule (`CST-8`, `CET-1CEST,M3.5.0,M10.5.0/3`) applies.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LOCALTIME: &str = "/etc/localtime";
const ZONEINFO: &str = "/usr/share/zoneinfo";
/// Zone files are a few kilobytes; anything larger is not one.
const MAX_ZONE_FILE_BYTES: u64 = 256 * 1024;
const SECONDS_PER_DAY: i64 = 86_400;
const HOUR: i32 = 3600;

/// The offset east of UTC now, in seconds; 0 when it cannot be told.
pub fn local_utc_offset_s() -> i32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    offset_at(std::env::var("TZ").ok().as_deref(), now).unwrap_or(0)
}

/// The offset at `now` (Unix seconds) for a `TZ` value (`None`: unset).
fn offset_at(tz: Option<&str>, now: i64) -> Option<i32> {
    match tz.map(|value| value.trim_start_matches(':')) {
        None | Some("") => offset_from_tzif(&read_zone_file(Path::new(LOCALTIME))?, now),
        Some(value) => match zone_path(value) {
            Some(path) => offset_from_tzif(&read_zone_file(&path)?, now),
            None => posix_offset(value, now),
        },
    }
}

/// The file a zone name names (`Asia/Taipei`); `None` for a POSIX rule.
fn zone_path(value: &str) -> Option<PathBuf> {
    if value.starts_with('/') {
        return Some(PathBuf::from(value));
    }
    let named = value.contains('/') || value.chars().all(|c| c.is_ascii_alphabetic());
    let safe = !value.split('/').any(|part| part == ".." || part.is_empty());
    (named && safe && Path::new(ZONEINFO).join(value).is_file())
        .then(|| Path::new(ZONEINFO).join(value))
}

fn read_zone_file(path: &Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_ZONE_FILE_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

/// Big-endian reader over a zone file.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(count)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u32(&mut self) -> Option<usize> {
        let bytes: [u8; 4] = self.take(4)?.try_into().ok()?;
        usize::try_from(u32::from_be_bytes(bytes)).ok()
    }
}

/// The counts of a TZif header.
struct Counts {
    utc: usize,
    std: usize,
    leap: usize,
    times: usize,
    types: usize,
    chars: usize,
}

fn header(cursor: &mut Cursor<'_>) -> Option<(u8, Counts)> {
    if cursor.take(4)? != b"TZif" {
        return None;
    }
    let version = *cursor.take(1)?.first()?;
    cursor.take(15)?;
    Some((
        version,
        Counts {
            utc: cursor.u32()?,
            std: cursor.u32()?,
            leap: cursor.u32()?,
            times: cursor.u32()?,
            types: cursor.u32()?,
            chars: cursor.u32()?,
        },
    ))
}

/// The offset a zone file gives at `now`: the type of the last transition
/// at or before it, or the footer's rule past the last transition.
fn offset_from_tzif(bytes: &[u8], now: i64) -> Option<i32> {
    let mut cursor = Cursor { bytes, at: 0 };
    let (version, counts) = header(&mut cursor)?;
    let (time_bytes, counts) = if version >= b'2' {
        // Skip the 32-bit block for the 64-bit one that follows it.
        cursor.take(
            counts.times * 5
                + counts.types * 6
                + counts.chars
                + counts.leap * 8
                + counts.std
                + counts.utc,
        )?;
        (8, header(&mut cursor)?.1)
    } else {
        (4, counts)
    };
    let times: Vec<i64> = (0..counts.times)
        .map(|_| {
            let raw = cursor.take(time_bytes)?;
            Some(if time_bytes == 8 {
                i64::from_be_bytes(raw.try_into().ok()?)
            } else {
                i64::from(i32::from_be_bytes(raw.try_into().ok()?))
            })
        })
        .collect::<Option<_>>()?;
    let indices = cursor.take(counts.times)?.to_vec();
    let offsets: Vec<(i32, bool)> = (0..counts.types)
        .map(|_| {
            let info = cursor.take(6)?;
            let offset = i32::from_be_bytes(info[..4].try_into().ok()?);
            Some((offset, info[4] != 0))
        })
        .collect::<Option<_>>()?;
    cursor.take(counts.chars + counts.leap * (time_bytes + 4) + counts.std + counts.utc)?;
    let footer = (version >= b'2')
        .then(|| footer(&bytes[cursor.at..]))
        .flatten();
    let last = times.iter().rposition(|time| *time <= now);
    let past_the_last = last.map_or(times.is_empty(), |index| index + 1 == times.len());
    if past_the_last && let Some(offset) = footer.and_then(|rule| posix_offset(rule, now)) {
        return Some(offset);
    }
    match last {
        Some(index) => offsets
            .get(usize::from(*indices.get(index)?))
            .map(|(offset, _)| *offset),
        // Before the first transition: the first standard-time type.
        None => offsets
            .iter()
            .find(|(_, dst)| !dst)
            .or_else(|| offsets.first())
            .map(|(offset, _)| *offset),
    }
}

/// The POSIX rule between the newlines after the 64-bit block.
fn footer(rest: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(rest).ok()?;
    let rule = text.strip_prefix('\n')?.split('\n').next()?;
    (!rule.is_empty()).then_some(rule)
}

/// The offset a POSIX `TZ` rule gives at `now`. Its offsets count west,
/// so `CST-8` is 8 hours east.
fn posix_offset(rule: &str, now: i64) -> Option<i32> {
    let mut rest = rule;
    let _std_name = zone_name(&mut rest)?;
    let std_east = -signed_time(&mut rest)?;
    if rest.is_empty() {
        return Some(std_east);
    }
    let _dst_name = zone_name(&mut rest)?;
    let dst_east = if rest.starts_with(',') || rest.is_empty() {
        std_east + HOUR
    } else {
        -signed_time(&mut rest)?
    };
    let Some(rules) = rest.strip_prefix(',') else {
        // A DST name without rules: the C library's default rules are not
        // known here, so standard time.
        return Some(std_east);
    };
    let (start, end) = rules.split_once(',')?;
    let year = year_of(now + i64::from(std_east));
    let starts = transition(start, year)? - i64::from(std_east);
    let ends = transition(end, year)? - i64::from(dst_east);
    let in_dst = if starts < ends {
        (starts..ends).contains(&now)
    } else {
        !(ends..starts).contains(&now)
    };
    Some(if in_dst { dst_east } else { std_east })
}

/// `CST` or `<+08>`.
fn zone_name<'a>(rest: &mut &'a str) -> Option<&'a str> {
    if let Some(quoted) = rest.strip_prefix('<') {
        let (name, after) = quoted.split_once('>')?;
        *rest = after;
        return Some(name);
    }
    let length = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    if length < 3 {
        return None;
    }
    let (name, after) = rest.split_at(length);
    *rest = after;
    Some(name)
}

/// `[+-]hh[:mm[:ss]]` in seconds.
fn signed_time(rest: &mut &str) -> Option<i32> {
    let (sign, unsigned) = match rest.as_bytes().first()? {
        b'-' => (-1, &rest[1..]),
        b'+' => (1, &rest[1..]),
        _ => (1, *rest),
    };
    let length = unsigned
        .find(|c: char| !(c.is_ascii_digit() || c == ':'))
        .unwrap_or(unsigned.len());
    let (text, after) = unsigned.split_at(length);
    let mut parts = text.split(':');
    let hours: i32 = parts.next()?.parse().ok()?;
    let minutes: i32 = parts.next().map_or(Some(0), |part| part.parse().ok())?;
    let seconds: i32 = parts.next().map_or(Some(0), |part| part.parse().ok())?;
    *rest = after;
    Some(sign * (hours * HOUR + minutes * 60 + seconds))
}

/// The instant (local time, as Unix seconds) a `Mm.w.d[/time]`, `Jn` or
/// `n` rule names in `year`.
fn transition(rule: &str, year: i64) -> Option<i64> {
    let (date, time) = match rule.split_once('/') {
        Some((date, time)) => {
            let mut time = time;
            (date, signed_time(&mut time)?)
        }
        None => (rule, 2 * HOUR),
    };
    let day = if let Some(month_rule) = date.strip_prefix('M') {
        let mut fields = month_rule.split('.');
        let month: i64 = fields.next()?.parse().ok()?;
        let week: i64 = fields.next()?.parse().ok()?;
        let weekday: i64 = fields.next()?.parse().ok()?;
        nth_weekday(year, month, week, weekday)?
    } else if let Some(julian) = date.strip_prefix('J') {
        // 1..=365, February 29 never counted.
        let day: i64 = julian.parse().ok()?;
        let leap_shift = i64::from(is_leap(year) && day >= 60);
        days_from_civil(year, 1, 1) + day - 1 + leap_shift
    } else {
        let day: i64 = date.parse().ok()?;
        days_from_civil(year, 1, 1) + day
    };
    Some(day * SECONDS_PER_DAY + i64::from(time))
}

/// The day (since 1970) of the `week`th `weekday` (0 Sunday) of `month`;
/// week 5 is the last.
fn nth_weekday(year: i64, month: i64, week: i64, weekday: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=5).contains(&week) || !(0..=6).contains(&weekday) {
        return None;
    }
    let first = days_from_civil(year, month, 1);
    // 1970-01-01 was a Thursday (4).
    let first_weekday = (first + 4).rem_euclid(7);
    let mut day = first + (weekday - first_weekday).rem_euclid(7) + (week - 1) * 7;
    let next_month = if month == 12 {
        days_from_civil(year + 1, 1, 1)
    } else {
        days_from_civil(year, month + 1, 1)
    };
    while day >= next_month {
        day -= 7;
    }
    Some(day)
}

const fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days since 1970-01-01 of a Gregorian date (H. Hinnant's
/// `days_from_civil`).
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn year_of(unix_seconds: i64) -> i64 {
    dgxtop_core::clock::civil_from_days(unix_seconds.div_euclid(SECONDS_PER_DAY)).0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-01-15T00:00:00Z and 2026-07-15T00:00:00Z.
    const WINTER: i64 = 1_768_435_200;
    const SUMMER: i64 = 1_784_073_600;

    #[test]
    fn posix_rules_without_dst_count_west() {
        assert_eq!(posix_offset("CST-8", WINTER), Some(8 * HOUR));
        assert_eq!(posix_offset("<+0530>-5:30", WINTER), Some(5 * HOUR + 1800));
        assert_eq!(posix_offset("UTC0", SUMMER), Some(0));
        assert_eq!(posix_offset("EST5", SUMMER), Some(-5 * HOUR));
        assert_eq!(
            posix_offset("X", WINTER),
            None,
            "a name has 3 letters at least"
        );
    }

    #[test]
    fn posix_rules_switch_to_dst_in_both_hemispheres() {
        let europe = "CET-1CEST,M3.5.0,M10.5.0/3";
        assert_eq!(posix_offset(europe, WINTER), Some(HOUR));
        assert_eq!(posix_offset(europe, SUMMER), Some(2 * HOUR));
        let us = "EST5EDT,M3.2.0,M11.1.0";
        assert_eq!(posix_offset(us, WINTER), Some(-5 * HOUR));
        assert_eq!(posix_offset(us, SUMMER), Some(-4 * HOUR));
        let sydney = "AEST-10AEDT,M10.1.0,M4.1.0/3";
        assert_eq!(
            posix_offset(sydney, WINTER),
            Some(11 * HOUR),
            "summer down under"
        );
        assert_eq!(posix_offset(sydney, SUMMER), Some(10 * HOUR));
    }

    /// 2026-03-29 is the last Sunday of March; the switch is at 01:00 UTC.
    #[test]
    fn the_switch_happens_at_its_instant() {
        let europe = "CET-1CEST,M3.5.0,M10.5.0/3";
        let switch = days_from_civil(2026, 3, 29) * SECONDS_PER_DAY + 3600;
        assert_eq!(posix_offset(europe, switch - 1), Some(HOUR));
        assert_eq!(posix_offset(europe, switch), Some(2 * HOUR));
        assert_eq!(
            nth_weekday(2026, 3, 5, 0),
            Some(days_from_civil(2026, 3, 29))
        );
        assert_eq!(
            nth_weekday(2026, 2, 5, 0),
            Some(days_from_civil(2026, 2, 22))
        );
        assert_eq!(nth_weekday(2026, 13, 1, 0), None);
    }

    /// A version 2 zone file: an empty 32-bit block, then transitions to
    /// `types` at `times` and the footer.
    fn tzif(times: &[i64], indices: &[u8], types: &[(i32, bool)], footer: &str) -> Vec<u8> {
        let counts = |times: usize, types: usize| {
            let mut header = b"TZif2".to_vec();
            header.extend([0u8; 15]);
            for count in [0, 0, 0, times, types, 4] {
                header.extend(u32::try_from(count).unwrap().to_be_bytes());
            }
            header
        };
        let mut bytes = counts(0, 1);
        bytes.extend([0, 0, 0, 0, 0, 0]);
        bytes.extend(b"UTC\0");
        bytes.extend(counts(times.len(), types.len()));
        for time in times {
            bytes.extend(time.to_be_bytes());
        }
        bytes.extend(indices);
        for (offset, dst) in types {
            bytes.extend(offset.to_be_bytes());
            bytes.push(u8::from(*dst));
            bytes.push(0);
        }
        bytes.extend(b"ABC\0");
        bytes.push(b'\n');
        bytes.extend(footer.as_bytes());
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn zone_files_give_the_transition_in_force_or_the_footer_rule() {
        // UTC+9 until 2000, then UTC+8 with a footer saying so.
        let file = tzif(
            &[946_684_800],
            &[1],
            &[(9 * HOUR, false), (8 * HOUR, false)],
            "CST-8",
        );
        assert_eq!(
            offset_from_tzif(&file, 900_000_000),
            Some(9 * HOUR),
            "before"
        );
        assert_eq!(
            offset_from_tzif(&file, WINTER),
            Some(8 * HOUR),
            "after: the footer"
        );
        // Slim files: the last transition long ago, the footer's DST rule.
        let europe = tzif(&[0], &[0], &[(HOUR, false)], "CET-1CEST,M3.5.0,M10.5.0/3");
        assert_eq!(offset_from_tzif(&europe, SUMMER), Some(2 * HOUR));
        // No transition at all: the footer.
        let fixed = tzif(&[], &[], &[(0, false)], "<+0545>-5:45");
        assert_eq!(offset_from_tzif(&fixed, WINTER), Some(5 * HOUR + 45 * 60));
        assert_eq!(offset_from_tzif(b"not a zone", WINTER), None);
        assert_eq!(offset_from_tzif(&file[..30], WINTER), None, "cut short");
    }

    #[test]
    fn tz_values_are_rules_or_safe_zone_names() {
        assert_eq!(offset_at(Some("CST-8"), WINTER), Some(8 * HOUR));
        assert_eq!(offset_at(Some(":CST-8"), WINTER), Some(8 * HOUR));
        assert_eq!(zone_path("../../etc/passwd"), None);
        assert_eq!(zone_path("Asia//Taipei"), None);
    }

    #[test]
    fn civil_days_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        for day in [0, 11_017, 20_000, -1] {
            let (year, month, date) = dgxtop_core::clock::civil_from_days(day);
            assert_eq!(days_from_civil(year, month, date), day);
        }
        assert!(is_leap(2000) && !is_leap(1900) && is_leap(2028));
        assert_eq!(year_of(SUMMER), 2026);
    }
}
