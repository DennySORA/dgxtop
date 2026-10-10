//! JSON wire helpers shared by every contract type.
//!
//! The contracts in `spec/dgxtop-v2/contracts/*.schema.json` carry every `u64`
//! as a decimal string (`^(0|[1-9][0-9]{0,19})$`) so that counters and boot
//! clock nanoseconds never pass through an `f64`. SQLite stores the same values
//! as fixed 20-digit zero padded TEXT so that lexical order equals numeric order.

use thiserror::Error;

/// Width of the zero padded decimal used as a SQLite sort key.
pub const FIXED_DECIMAL_WIDTH: usize = 20;

/// Rejection reasons for decimal `u64` text.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DecimalError {
    #[error("decimal value is empty")]
    Empty,
    #[error("decimal value has a leading zero")]
    LeadingZero,
    #[error("decimal value contains a non-digit character")]
    NonDigit,
    #[error("decimal value exceeds u64::MAX")]
    Overflow,
    #[error("fixed-width decimal must be exactly {FIXED_DECIMAL_WIDTH} digits")]
    WrongWidth,
}

/// Parses canonical decimal text: no sign, no leading zero, value <= `u64::MAX`.
pub fn parse_decimal_u64(text: &str) -> Result<u64, DecimalError> {
    if text.is_empty() {
        return Err(DecimalError::Empty);
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DecimalError::NonDigit);
    }
    if text.len() > 1 && text.starts_with('0') {
        return Err(DecimalError::LeadingZero);
    }
    text.parse::<u64>().map_err(|_| DecimalError::Overflow)
}

/// Formats a value as the 20-digit zero padded SQLite sort key.
pub fn format_fixed_width(value: u64) -> String {
    format!("{value:0width$}", width = FIXED_DECIMAL_WIDTH)
}

/// Parses a 20-digit zero padded SQLite sort key back into a `u64`.
pub fn parse_fixed_width(text: &str) -> Result<u64, DecimalError> {
    if text.len() != FIXED_DECIMAL_WIDTH {
        return Err(DecimalError::WrongWidth);
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DecimalError::NonDigit);
    }
    text.parse::<u64>().map_err(|_| DecimalError::Overflow)
}

/// Serde adapter: `u64` <-> canonical decimal string.
/// Canonical decimal text of `value` in `buffer` (no allocation: several
/// timestamps of every record are written this way).
pub fn format_decimal_u64(value: u64, buffer: &mut [u8; 20]) -> &str {
    let mut start = buffer.len();
    let mut rest = value;
    loop {
        start -= 1;
        // `rest % 10` is a single digit.
        buffer[start] = b'0' + u8::try_from(rest % 10).unwrap_or(0);
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    // Only ASCII digits were written.
    std::str::from_utf8(&buffer[start..]).unwrap_or("0")
}

pub mod u64_decimal {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        let mut buffer = [0u8; 20];
        serializer.serialize_str(super::format_decimal_u64(*value, &mut buffer))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let text = std::borrow::Cow::<str>::deserialize(deserializer)?;
        super::parse_decimal_u64(&text).map_err(D::Error::custom)
    }
}

/// Serde adapter: `Option<u64>` <-> canonical decimal string or `null`.
pub mod opt_u64_decimal {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
        let mut buffer = [0u8; 20];
        match value {
            Some(inner) => serializer.serialize_str(super::format_decimal_u64(*inner, &mut buffer)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        let text = Option::<std::borrow::Cow<str>>::deserialize(deserializer)?;
        text.map(|inner| super::parse_decimal_u64(&inner).map_err(D::Error::custom))
            .transpose()
    }
}

/// Serde adapter: `Vec<u64>` <-> array of canonical decimal strings.
pub mod vec_u64_decimal {
    use serde::{Deserialize, Deserializer, Serializer, de::Error, ser::SerializeSeq};

    pub fn serialize<S: Serializer>(values: &[u64], serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(values.len()))?;
        for value in values {
            sequence.serialize_element(&value.to_string())?;
        }
        sequence.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u64>, D::Error> {
        let texts = Vec::<std::borrow::Cow<str>>::deserialize(deserializer)?;
        texts
            .iter()
            .map(|text| super::parse_decimal_u64(text).map_err(D::Error::custom))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_formatting_matches_to_string_at_the_edges() {
        let mut buffer = [0u8; 20];
        for value in [0, 7, 10, 42, 1_000_000_007, u64::MAX - 1, u64::MAX] {
            assert_eq!(format_decimal_u64(value, &mut buffer), value.to_string());
        }
    }
    use proptest::prelude::*;

    #[test]
    fn max_u64_round_trips_exactly() {
        let text = u64::MAX.to_string();
        assert_eq!(text, "18446744073709551615");
        assert_eq!(parse_decimal_u64(&text), Ok(u64::MAX));
    }

    #[test]
    fn rejects_overflow_sign_and_leading_zero() {
        assert_eq!(
            parse_decimal_u64("18446744073709551616"),
            Err(DecimalError::Overflow)
        );
        assert_eq!(parse_decimal_u64("-1"), Err(DecimalError::NonDigit));
        assert_eq!(parse_decimal_u64("+1"), Err(DecimalError::NonDigit));
        assert_eq!(parse_decimal_u64("01"), Err(DecimalError::LeadingZero));
        assert_eq!(parse_decimal_u64(""), Err(DecimalError::Empty));
        assert_eq!(parse_decimal_u64("1.0"), Err(DecimalError::NonDigit));
        assert_eq!(parse_decimal_u64("0"), Ok(0));
    }

    #[test]
    fn fixed_width_orders_lexically_like_numbers() {
        let mut keys = [
            format_fixed_width(100),
            format_fixed_width(2),
            format_fixed_width(10),
        ];
        keys.sort();
        assert_eq!(
            keys.iter()
                .map(|key| parse_fixed_width(key))
                .collect::<Result<Vec<_>, _>>(),
            Ok(vec![2, 10, 100])
        );
        assert_eq!(format_fixed_width(u64::MAX), "18446744073709551615");
        assert_eq!(parse_fixed_width("42"), Err(DecimalError::WrongWidth));
    }

    proptest! {
        #[test]
        fn decimal_round_trip(value in any::<u64>()) {
            prop_assert_eq!(parse_decimal_u64(&value.to_string()), Ok(value));
            prop_assert_eq!(parse_fixed_width(&format_fixed_width(value)), Ok(value));
        }

    }
}
