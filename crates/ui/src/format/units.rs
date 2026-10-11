//! Unit-aware number formatting with explicit symbols (°C, W, binary byte
//! prefixes, %, MHz, ms, J, counts). A valid zero is printed as zero; absence
//! is never formatted here (callers render the state text instead).

use dgxtop_core::metric::{CanonicalUnit, Scalar, ScalarView};

/// Text for a value the projection does not carry (never `0`, never blank).
pub const NOT_AVAILABLE: &str = "n/a";

const BINARY_PREFIXES: [&str; 7] = ["", "Ki", "Mi", "Gi", "Ti", "Pi", "Ei"];
const SI_PREFIXES: [&str; 7] = ["", "k", "M", "G", "T", "P", "E"];

/// Formats a scalar in its canonical unit. Exact `u64` counts stay exact.
pub fn format_scalar(value: Scalar, unit: CanonicalUnit) -> String {
    match value.view() {
        ScalarView::U64(exact) if unit == CanonicalUnit::Count => exact.to_string(),
        _ => format_value(value.to_f64_lossy(), unit),
    }
}

/// Formats a finite value with the unit symbol.
pub fn format_value(value: f64, unit: CanonicalUnit) -> String {
    if !value.is_finite() {
        return NOT_AVAILABLE.to_owned();
    }
    match unit {
        CanonicalUnit::DegC => format!("{}°C", fixed(value, 0)),
        CanonicalUnit::Watts => format!("{} W", magnitude_fixed(value)),
        CanonicalUnit::Bytes => binary_scaled(value, "B"),
        CanonicalUnit::BytesPerSecond => binary_scaled(value, "B/s"),
        CanonicalUnit::Percent => format_percent(value),
        CanonicalUnit::MHz => format!("{} MHz", fixed(value, 0)),
        CanonicalUnit::Count => format_count(value),
        CanonicalUnit::CountPerSecond => format!("{}/s", format_count(value)),
        CanonicalUnit::Milliseconds => format!("{} ms", magnitude_fixed(value)),
        CanonicalUnit::Joules => si_scaled(value, "J"),
    }
}

/// Percent with one decimal below 100 (`45.3%`), none at or above (`100%`).
pub fn format_percent(value: f64) -> String {
    if !value.is_finite() {
        return NOT_AVAILABLE.to_owned();
    }
    if value.abs() >= 99.95 {
        format!("{}%", fixed(value, 0))
    } else {
        format!("{}%", fixed(value, 1))
    }
}

/// Process-table percent where the projection may omit the value.
pub fn format_optional_percent(value: Option<f64>) -> String {
    value.map_or_else(|| NOT_AVAILABLE.to_owned(), format_percent)
}

/// Exact byte count with binary prefixes (`1.5 GiB`).
pub fn format_bytes(bytes: u64) -> String {
    format_scalar(Scalar::from_u64(bytes), CanonicalUnit::Bytes)
}

/// Process-table byte count where the projection may omit the value.
pub fn format_optional_bytes(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| NOT_AVAILABLE.to_owned(), format_bytes)
}

/// Joules in the units of an electricity bill: watt-hours below 1 kWh
/// (`0.05 Wh`, `80.0 Wh`, `990 Wh`), then kilowatt-hours (`9.25 kWh`).
pub fn format_energy(joules: f64) -> String {
    const JOULES_PER_WH: f64 = 3_600.0;
    if !joules.is_finite() {
        return NOT_AVAILABLE.to_owned();
    }
    let wh = joules / JOULES_PER_WH;
    if wh.abs() < 1000.0 {
        let decimals = match wh.abs() {
            magnitude if magnitude < 10.0 => 2,
            magnitude if magnitude < 100.0 => 1,
            _ => 0,
        };
        return format!("{} Wh", fixed(wh, decimals));
    }
    let kwh = wh / 1000.0;
    let decimals = if kwh.abs() < 100.0 { 2 } else { 1 };
    format!("{} kWh", fixed(kwh, decimals))
}

/// Bytes per second in decimal units (`960 GB/s`, `63.0 GB/s`), the way
/// GPU and PCIe data sheets state peak bandwidths.
pub fn format_si_rate(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() {
        return NOT_AVAILABLE.to_owned();
    }
    si_scaled(bytes_per_second, "B/s")
}

/// A duration in the largest unit that reads well: `219 ms`, `12.3 s`,
/// then minutes, hours and days (`3m 39s`, `1h 35m`).
pub fn format_duration_ms(milliseconds: f64) -> String {
    if !milliseconds.is_finite() || milliseconds < 0.0 {
        return NOT_AVAILABLE.to_owned();
    }
    if milliseconds < 1000.0 {
        return format!("{} ms", fixed(milliseconds, 0));
    }
    if milliseconds < 60_000.0 {
        return format!("{} s", fixed(milliseconds / 1000.0, 1));
    }
    let whole_seconds = fixed(milliseconds / 1000.0, 0)
        .parse::<u64>()
        .unwrap_or(u64::MAX);
    super::age::format_span_seconds(whole_seconds)
}

/// A count in a few columns: exact below 1000, then SI prefixes without a
/// space (`412`, `12.3k`, `654k`, `1.2M`), for totals beside each other.
pub fn format_compact_count(value: f64) -> String {
    if !value.is_finite() {
        return NOT_AVAILABLE.to_owned();
    }
    if value.abs() < 1000.0 {
        return fixed(value, 0);
    }
    scaled(value, 1000.0, &SI_PREFIXES, "").replace(' ', "")
}

/// Fixed decimals without a `-0` artifact for values that round to zero.
pub fn fixed(value: f64, decimals: usize) -> String {
    let text = format!("{value:.decimals$}");
    match text.strip_prefix('-') {
        Some(unsigned) if unsigned.chars().all(|c| c == '0' || c == '.') => unsigned.to_owned(),
        _ => text,
    }
}

/// One decimal below 100, none above (`7.5 W`, `245 W`).
fn magnitude_fixed(value: f64) -> String {
    if value.abs() < 100.0 {
        fixed(value, 1)
    } else {
        fixed(value, 0)
    }
}

fn format_count(value: f64) -> String {
    let magnitude = value.abs();
    if value.fract() == 0.0 && magnitude < 1e15 {
        fixed(value, 0)
    } else if magnitude < 10.0 {
        fixed(value, 2)
    } else if magnitude < 1000.0 {
        fixed(value, 1)
    } else {
        fixed(value, 0)
    }
}

fn binary_scaled(value: f64, suffix: &str) -> String {
    scaled(value, 1024.0, &BINARY_PREFIXES, suffix)
}

fn si_scaled(value: f64, suffix: &str) -> String {
    scaled(value, 1000.0, &SI_PREFIXES, suffix)
}

fn scaled(value: f64, base: f64, prefixes: &[&str], suffix: &str) -> String {
    let mut magnitude = value.abs();
    let mut index = 0usize;
    while magnitude >= base && index + 1 < prefixes.len() {
        magnitude /= base;
        index += 1;
    }
    let number = if index == 0 {
        if suffix.starts_with('B') {
            fixed(magnitude, 0)
        } else {
            magnitude_fixed(magnitude)
        }
    } else {
        magnitude_fixed(magnitude)
    };
    let sign = if value < 0.0 && number.chars().any(|c| c.is_ascii_digit() && c != '0') {
        "-"
    } else {
        ""
    };
    let prefix = prefixes.get(index).copied().unwrap_or_default();
    format!("{sign}{number} {prefix}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(number: f64, unit: CanonicalUnit) -> String {
        format_value(number, unit)
    }

    #[test]
    fn temperatures_power_and_clocks() {
        assert_eq!(value(68.4, CanonicalUnit::DegC), "68°C");
        assert_eq!(value(-0.2, CanonicalUnit::DegC), "0°C");
        assert_eq!(value(7.46, CanonicalUnit::Watts), "7.5 W");
        assert_eq!(value(245.4, CanonicalUnit::Watts), "245 W");
        assert_eq!(value(1980.0, CanonicalUnit::MHz), "1980 MHz");
        assert_eq!(value(4.25, CanonicalUnit::Milliseconds), "4.2 ms");
    }

    #[test]
    fn bytes_use_binary_prefixes() {
        assert_eq!(value(0.0, CanonicalUnit::Bytes), "0 B");
        assert_eq!(value(512.0, CanonicalUnit::Bytes), "512 B");
        assert_eq!(value(1536.0, CanonicalUnit::Bytes), "1.5 KiB");
        assert_eq!(
            value(12.3 * 1024.0 * 1024.0 * 1024.0, CanonicalUnit::Bytes),
            "12.3 GiB"
        );
        assert_eq!(
            value(2.0 * 1024f64.powi(4), CanonicalUnit::Bytes),
            "2.0 TiB"
        );
        assert_eq!(
            value(1.25 * 1024.0 * 1024.0, CanonicalUnit::BytesPerSecond),
            "1.2 MiB/s"
        );
        assert_eq!(value(0.0, CanonicalUnit::BytesPerSecond), "0 B/s");
        assert_eq!(format_bytes(u64::MAX), "16.0 EiB");
    }

    #[test]
    fn percent_and_counts() {
        assert_eq!(value(45.26, CanonicalUnit::Percent), "45.3%");
        assert_eq!(value(0.0, CanonicalUnit::Percent), "0.0%");
        assert_eq!(value(100.0, CanonicalUnit::Percent), "100%");
        assert_eq!(value(250.0, CanonicalUnit::Percent), "250%");
        assert_eq!(value(0.52, CanonicalUnit::Count), "0.52");
        assert_eq!(value(412.0, CanonicalUnit::Count), "412");
        assert_eq!(value(4.5, CanonicalUnit::CountPerSecond), "4.50/s");
        assert_eq!(
            format_scalar(
                Scalar::from_u64(18_446_744_073_709_551_615),
                CanonicalUnit::Count
            ),
            "18446744073709551615"
        );
    }

    #[test]
    fn compact_counts_fit_a_few_columns() {
        assert_eq!(format_compact_count(0.0), "0");
        assert_eq!(format_compact_count(999.0), "999");
        assert_eq!(format_compact_count(1000.0), "1.0k");
        assert_eq!(format_compact_count(12_345.0), "12.3k");
        assert_eq!(format_compact_count(654_321.0), "654k");
        assert_eq!(format_compact_count(1.25e9), "1.2G");
        assert_eq!(format_compact_count(f64::NAN), NOT_AVAILABLE);
    }

    #[test]
    fn energy_uses_si_prefixes() {
        assert_eq!(value(123.4, CanonicalUnit::Joules), "123 J");
        assert_eq!(value(12.34, CanonicalUnit::Joules), "12.3 J");
        assert_eq!(value(4_560_000.0, CanonicalUnit::Joules), "4.6 MJ");
    }

    #[test]
    fn non_finite_and_missing_values_are_not_zero() {
        assert_eq!(value(f64::NAN, CanonicalUnit::Watts), NOT_AVAILABLE);
        assert_eq!(format_percent(f64::INFINITY), NOT_AVAILABLE);
        assert_eq!(format_optional_percent(None), NOT_AVAILABLE);
        assert_eq!(format_optional_bytes(None), NOT_AVAILABLE);
        assert_eq!(format_optional_bytes(Some(0)), "0 B");
    }

    #[test]
    fn negative_values_keep_their_sign() {
        assert_eq!(value(-5.0, CanonicalUnit::DegC), "-5°C");
        assert_eq!(value(-2048.0, CanonicalUnit::Bytes), "-2.0 KiB");
        assert_eq!(fixed(-0.04, 1), "0.0");
    }
}
