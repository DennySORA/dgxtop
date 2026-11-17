//! Integers as `f64` for statistics and display.
//!
//! An integer above 2^53 does not fit the mantissa of an `f64`. Every
//! conversion through [`ToF64Lossy`] is of a quantity that is charted,
//! averaged or written out (nanoseconds, bytes, counts), where the digits
//! an `f64` drops are below what is shown. Exact arithmetic, counter
//! deltas and identities among it, never goes through it.

/// An integer as the nearest `f64`.
pub trait ToF64Lossy {
    /// The value as the nearest `f64`; exact up to 2^53.
    #[must_use]
    fn to_f64_lossy(self) -> f64;
}

macro_rules! lossy {
    ($($integer:ty),+) => {
        $(impl ToF64Lossy for $integer {
            #[expect(
                clippy::cast_precision_loss,
                reason = "the one place an integer becomes an approximate f64 (module docs)"
            )]
            fn to_f64_lossy(self) -> f64 {
                self as f64
            }
        })+
    };
}

lossy!(u64, usize, u128, i64, isize, i128);

#[cfg(test)]
mod tests {
    use super::ToF64Lossy;

    #[test]
    fn integers_convert_to_the_nearest_f64() {
        assert_eq!(1_500_u64.to_f64_lossy(), 1_500.0);
        assert_eq!((-3_i64).to_f64_lossy(), -3.0);
        assert_eq!(usize::MAX.to_f64_lossy(), 18_446_744_073_709_551_615.0);
        // 2^53 + 1 is the first integer an f64 cannot hold: it rounds.
        assert_eq!(
            9_007_199_254_740_993_u64.to_f64_lossy(),
            9_007_199_254_740_992.0
        );
        assert_eq!(
            u128::MAX.to_f64_lossy(),
            340_282_366_920_938_463_463_374_607_431_768_211_455.0
        );
    }
}
