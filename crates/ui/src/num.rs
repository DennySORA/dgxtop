//! Audited numeric conversions for layout math.
//!
//! The workspace denies lossy `as` casts; terminal geometry is `u16` while
//! counts are `usize` and ratios are `f64`. Every narrowing conversion of the
//! crate goes through these helpers so saturation is explicit and tested.

/// Saturating `usize` → `u16` (terminal coordinates never exceed `u16::MAX`).
pub(crate) fn saturate_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// Saturating `usize` → `u32`.
pub(crate) fn saturate_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Floors a ratio-scaled value into `0..=max`. Non-finite and negative inputs
/// map to 0 so a corrupt ratio can never paint a full bar.
pub(crate) fn scaled_floor(value: f64, max: u16) -> u16 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    let ceiling = f64::from(max);
    if value >= ceiling {
        return max;
    }
    // The value is finite and inside 0..max here, so the cast is exact after floor().
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let floored = value.floor() as u16;
    floored
}

/// Rounds a ratio-scaled value into `0..=max` (nearest, ties away from zero).
pub(crate) fn scaled_round(value: f64, max: u16) -> u16 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    scaled_floor(value.round(), max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturating_conversions() {
        assert_eq!(saturate_u16(5), 5);
        assert_eq!(saturate_u16(usize::MAX), u16::MAX);
        assert_eq!(saturate_u32(usize::MAX), u32::MAX);
    }

    #[test]
    fn scaled_values_are_clamped_and_reject_non_finite() {
        assert_eq!(scaled_floor(3.9, 10), 3);
        assert_eq!(scaled_floor(-1.0, 10), 0);
        assert_eq!(scaled_floor(f64::NAN, 10), 0);
        assert_eq!(scaled_floor(f64::INFINITY, 10), 0);
        assert_eq!(scaled_floor(12.0, 10), 10);
        assert_eq!(scaled_round(3.5, 10), 4);
        assert_eq!(scaled_round(0.4, 10), 0);
    }
}
