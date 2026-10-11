//! Exact counter engine (STORAGE.md §4).
//!
//! Deltas are exact `u64` subtractions between two readings of the SAME
//! source incarnation (source id, generation and boot id). Rates divide the
//! exact delta by the actual elapsed boot time. A first reading, a changed
//! incarnation, a backwards step without declared modulus, a non-positive
//! interval or an interval longer than the stale window yields no value —
//! never a negative number and never a value saturated to 0.

use dgxtop_core::canonical::{CounterBase, CounterInterval};
use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::identity::{BootId, MetricKey, SourceId};
use dgxtop_core::metric::{Conversion, Scalar};

use crate::shared::reasons;

/// One reading of one counter source incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterReading<'a> {
    pub value: u64,
    pub boot_id: &'a BootId,
    pub generation: u32,
    /// Sample time on the boot clock (device time when known, else read time).
    pub sample_boot_ns: u64,
    /// Modulus declared by the driver (e.g. powercap `max_energy_range_uj`).
    /// Wrap arithmetic is applied only when it is declared.
    pub modulus: Option<u64>,
}

impl CounterReading<'_> {
    /// The base this reading leaves behind for the next delta.
    pub fn base(&self) -> CounterBase {
        CounterBase {
            value: self.value,
            boot_id: self.boot_id.clone(),
            generation: self.generation,
            sample_boot_ns: self.sample_boot_ns,
        }
    }
}

/// Exact counter movement over the measured interval `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterDelta {
    pub delta: u64,
    pub start_boot_ns: u64,
    pub end_boot_ns: u64,
    /// The declared modulus was crossed once.
    pub wrapped: bool,
}

impl CounterDelta {
    /// Elapsed boot time; always positive for a constructed delta.
    pub const fn elapsed_ns(&self) -> u64 {
        self.end_boot_ns.saturating_sub(self.start_boot_ns)
    }

    /// Rate in canonical units per second over the actual elapsed time.
    pub fn rate_per_second(&self, conversion: &Conversion) -> Option<f64> {
        let amount = conversion.apply_delta(self.delta).ok()?;
        rate_of(amount, self.elapsed_ns())
    }

    /// Exact delta in canonical units: `u64` whenever the conversion is
    /// integral (bytes, sectors x 512, counts), `f64` otherwise (uJ -> J).
    pub fn canonical_amount(&self, conversion: &Conversion) -> Option<Scalar> {
        if conversion.divide == 1 {
            self.delta
                .checked_mul(conversion.multiply)
                .map(Scalar::from_u64)
        } else {
            let amount = conversion.apply_delta(self.delta).ok()?;
            Scalar::from_f64(amount).ok()
        }
    }
}

/// `amount / seconds(elapsed_ns)`, `None` for an empty interval or a
/// non-finite result.
pub fn rate_of(amount: f64, elapsed_ns: u64) -> Option<f64> {
    if elapsed_ns == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let seconds = elapsed_ns as f64 / NANOS_PER_SECOND as f64;
    let rate = amount / seconds;
    rate.is_finite().then_some(rate)
}

/// Why a reading produced no delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterGap {
    Warmup,
    GenerationChanged,
    BootChanged,
    Reset,
    IntervalNotPositive,
    IntervalExceedsStaleWindow,
}

impl CounterGap {
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::Warmup => reasons::WARMUP,
            Self::GenerationChanged => reasons::GENERATION_CHANGED,
            Self::BootChanged => reasons::BOOT_CHANGED,
            Self::Reset => reasons::COUNTER_RESET,
            Self::IntervalNotPositive => reasons::INTERVAL_NOT_POSITIVE,
            Self::IntervalExceedsStaleWindow => reasons::INTERVAL_EXCEEDS_STALE_WINDOW,
        }
    }
}

/// Result of feeding one reading into the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CounterStep {
    pub outcome: Result<CounterDelta, CounterGap>,
    /// The reading always becomes the next base: after a gap the incarnation
    /// restarts from this reading instead of reusing a foreign base.
    pub next_base: CounterBase,
}

/// Computes the delta of `reading` against `previous`. `max_interval_ns` is
/// the stale window of the source; longer intervals are not trusted.
pub fn advance(
    previous: Option<&CounterBase>,
    reading: &CounterReading<'_>,
    max_interval_ns: u64,
) -> CounterStep {
    CounterStep {
        outcome: delta_between(previous, reading, max_interval_ns),
        next_base: reading.base(),
    }
}

/// Whether `reading` is exactly the committed base, i.e. an earlier job
/// already consumed it (retained records are never counted twice).
pub fn already_consumed(previous: Option<&CounterBase>, reading: &CounterReading<'_>) -> bool {
    previous.is_some_and(|base| {
        base.value == reading.value
            && base.generation == reading.generation
            && base.sample_boot_ns == reading.sample_boot_ns
            && base.boot_id == *reading.boot_id
    })
}

fn delta_between(
    previous: Option<&CounterBase>,
    reading: &CounterReading<'_>,
    max_interval_ns: u64,
) -> Result<CounterDelta, CounterGap> {
    let previous = previous.ok_or(CounterGap::Warmup)?;
    if previous.boot_id != *reading.boot_id {
        return Err(CounterGap::BootChanged);
    }
    if previous.generation != reading.generation {
        return Err(CounterGap::GenerationChanged);
    }
    let elapsed = reading
        .sample_boot_ns
        .checked_sub(previous.sample_boot_ns)
        .filter(|elapsed| *elapsed > 0)
        .ok_or(CounterGap::IntervalNotPositive)?;
    if elapsed > max_interval_ns {
        return Err(CounterGap::IntervalExceedsStaleWindow);
    }
    let (delta, wrapped) = movement(previous.value, reading.value, reading.modulus)?;
    Ok(CounterDelta {
        delta,
        start_boot_ns: previous.sample_boot_ns,
        end_boot_ns: reading.sample_boot_ns,
        wrapped,
    })
}

/// Exact movement; a backwards step is a single wrap only when the driver
/// declared the modulus and both readings lie inside it.
fn movement(previous: u64, current: u64, modulus: Option<u64>) -> Result<(u64, bool), CounterGap> {
    if current >= previous {
        return Ok((current - previous, false));
    }
    match modulus {
        Some(modulus) if previous <= modulus => (modulus - previous)
            .checked_add(current)
            .map(|delta| (delta, true))
            .ok_or(CounterGap::Reset),
        _ => Err(CounterGap::Reset),
    }
}

/// Exact interval for the history's window totals; `None` when the canonical
/// amount is not representable (reported by the caller).
pub fn interval(
    key: MetricKey,
    source_id: SourceId,
    source_generation: u32,
    delta: &CounterDelta,
    conversion: &Conversion,
) -> Option<CounterInterval> {
    Some(CounterInterval {
        key,
        source_id,
        source_generation,
        start_boot_ns: delta.start_boot_ns,
        end_boot_ns: delta.end_boot_ns,
        delta: delta.canonical_amount(conversion)?,
        unit: conversion.canonical,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::metric::{CanonicalUnit, conversion_for, native};
    use dgxtop_core::testing::{boot_id, metric_key, source_id};
    use proptest::prelude::*;

    const SECOND: u64 = 1_000_000_000;
    const WINDOW: u64 = 3 * SECOND;

    fn reading(boot: &BootId, value: u64, at: u64) -> CounterReading<'_> {
        CounterReading {
            value,
            boot_id: boot,
            generation: 1,
            sample_boot_ns: at,
            modulus: None,
        }
    }

    fn bytes() -> &'static Conversion {
        conversion_for(native::BYTES, CanonicalUnit::Bytes).unwrap()
    }

    #[test]
    fn bytes_1000_to_1300_over_3s_is_100_per_second_and_300_total() {
        let boot = boot_id("b");
        let first = advance(None, &reading(&boot, 1000, 10 * SECOND), WINDOW);
        assert_eq!(first.outcome, Err(CounterGap::Warmup));
        let second = advance(
            Some(&first.next_base),
            &reading(&boot, 1300, 13 * SECOND),
            WINDOW,
        );
        let delta = second.outcome.unwrap();
        assert_eq!(delta.delta, 300);
        assert_eq!(delta.rate_per_second(bytes()), Some(100.0));
        let interval = interval(
            metric_key("netif:ns:2", "network.rx.bytes_per_second", "interface"),
            source_id("network:eth0:rx_bytes"),
            1,
            &delta,
            bytes(),
        )
        .unwrap();
        assert_eq!(interval.delta.as_u64(), Some(300));
        assert_eq!(interval.unit, CanonicalUnit::Bytes);
        assert_eq!(
            (interval.start_boot_ns, interval.end_boot_ns),
            (10 * SECOND, 13 * SECOND)
        );
    }

    #[test]
    fn reset_generation_boot_and_interval_changes_are_gaps() {
        let boot = boot_id("b");
        let base = reading(&boot, 500, SECOND).base();
        let backwards = advance(Some(&base), &reading(&boot, 10, 2 * SECOND), WINDOW);
        assert_eq!(backwards.outcome, Err(CounterGap::Reset));
        assert_eq!(
            backwards.next_base.value, 10,
            "restarts from the new reading"
        );

        let mut regenerated = reading(&boot, 600, 2 * SECOND);
        regenerated.generation = 2;
        assert_eq!(
            advance(Some(&base), &regenerated, WINDOW).outcome,
            Err(CounterGap::GenerationChanged)
        );

        let other_boot = boot_id("other");
        assert_eq!(
            advance(Some(&base), &reading(&other_boot, 600, 2 * SECOND), WINDOW).outcome,
            Err(CounterGap::BootChanged)
        );
        assert_eq!(
            advance(Some(&base), &reading(&boot, 600, SECOND), WINDOW).outcome,
            Err(CounterGap::IntervalNotPositive)
        );
        assert_eq!(
            advance(Some(&base), &reading(&boot, 600, SECOND / 2), WINDOW).outcome,
            Err(CounterGap::IntervalNotPositive)
        );
        assert_eq!(
            advance(
                Some(&base),
                &reading(&boot, 600, SECOND + WINDOW + 1),
                WINDOW
            )
            .outcome,
            Err(CounterGap::IntervalExceedsStaleWindow)
        );
        assert!(
            advance(Some(&base), &reading(&boot, 600, SECOND + WINDOW), WINDOW)
                .outcome
                .is_ok(),
            "exactly the window is still trusted"
        );
    }

    #[test]
    fn near_u64_max_deltas_stay_exact() {
        let boot = boot_id("b");
        let base = reading(&boot, 0, SECOND).base();
        let step = advance(Some(&base), &reading(&boot, u64::MAX, 2 * SECOND), WINDOW);
        let delta = step.outcome.unwrap();
        assert_eq!(delta.delta, u64::MAX);
        assert_eq!(
            delta.canonical_amount(bytes()).unwrap().as_u64(),
            Some(u64::MAX)
        );

        let near = reading(&boot, u64::MAX - 7, SECOND).base();
        let step = advance(Some(&near), &reading(&boot, u64::MAX, 2 * SECOND), WINDOW);
        assert_eq!(step.outcome.unwrap().delta, 7);
    }

    #[test]
    fn sector_conversion_overflow_is_not_representable() {
        let sectors = conversion_for(native::SECTORS_512, CanonicalUnit::Bytes).unwrap();
        let delta = CounterDelta {
            delta: u64::MAX / 2,
            start_boot_ns: 0,
            end_boot_ns: SECOND,
            wrapped: false,
        };
        assert!(delta.canonical_amount(sectors).is_none());
        let small = CounterDelta { delta: 3, ..delta };
        assert_eq!(
            small.canonical_amount(sectors).unwrap().as_u64(),
            Some(1536)
        );
    }

    #[test]
    fn declared_modulus_allows_exactly_one_wrap() {
        let boot = boot_id("b");
        let mut previous = reading(&boot, 262_143_328_850, SECOND);
        previous.modulus = Some(262_143_328_850 + 1000);
        let base = previous.base();
        let mut current = reading(&boot, 400, 2 * SECOND);
        current.modulus = previous.modulus;
        let delta = advance(Some(&base), &current, WINDOW).outcome.unwrap();
        assert_eq!(delta.delta, 1400);
        assert!(delta.wrapped);

        // A previous value beyond the declared modulus is inconsistent: reset.
        let mut beyond = current;
        beyond.modulus = Some(100);
        assert_eq!(
            advance(Some(&base), &beyond, WINDOW).outcome,
            Err(CounterGap::Reset)
        );
        // Without a modulus a backwards step is never guessed as a wrap.
        let mut undeclared = current;
        undeclared.modulus = None;
        assert_eq!(
            advance(Some(&base), &undeclared, WINDOW).outcome,
            Err(CounterGap::Reset)
        );
    }

    #[test]
    fn micro_joules_become_fractional_joules() {
        let conversion = conversion_for(native::MICRO_JOULE, CanonicalUnit::Joules).unwrap();
        let delta = CounterDelta {
            delta: 2_500_000,
            start_boot_ns: 0,
            end_boot_ns: SECOND / 2,
            wrapped: false,
        };
        assert_eq!(delta.rate_per_second(conversion), Some(5.0));
        assert_eq!(
            delta.canonical_amount(conversion).unwrap().to_f64_lossy(),
            2.5
        );
    }

    #[test]
    fn already_consumed_detects_the_committed_reading() {
        let boot = boot_id("b");
        let current = reading(&boot, 42, SECOND);
        assert!(!already_consumed(None, &current));
        assert!(already_consumed(Some(&current.base()), &current));
        let later = reading(&boot, 42, 2 * SECOND);
        assert!(!already_consumed(Some(&current.base()), &later));
    }

    #[test]
    fn empty_interval_has_no_rate() {
        assert_eq!(rate_of(10.0, 0), None);
        assert_eq!(rate_of(10.0, SECOND), Some(10.0));
    }

    proptest! {
        #[test]
        fn delta_is_exact_and_never_negative(
            previous in any::<u64>(),
            current in any::<u64>(),
            elapsed in 1u64..=WINDOW,
        ) {
            let boot = boot_id("b");
            let base = reading(&boot, previous, SECOND).base();
            let step = advance(Some(&base), &reading(&boot, current, SECOND + elapsed), WINDOW);
            if current >= previous {
                let delta = step.outcome.unwrap();
                prop_assert_eq!(delta.delta, current - previous);
                prop_assert_eq!(delta.elapsed_ns(), elapsed);
                let rate = delta.rate_per_second(bytes()).unwrap();
                prop_assert!(rate >= 0.0 && rate.is_finite());
            } else {
                prop_assert_eq!(step.outcome, Err(CounterGap::Reset));
            }
            prop_assert_eq!(step.next_base.value, current);
        }

        #[test]
        fn wrapped_delta_matches_modular_arithmetic(
            modulus in 2u64..=u64::MAX,
            previous_fraction in 0.0f64..1.0,
            current_fraction in 0.0f64..1.0,
        ) {
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let previous = ((modulus as f64) * previous_fraction) as u64 % modulus;
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let current = ((modulus as f64) * current_fraction) as u64 % modulus;
            let boot = boot_id("b");
            let mut first = reading(&boot, previous, SECOND);
            first.modulus = Some(modulus);
            let mut second = reading(&boot, current, 2 * SECOND);
            second.modulus = Some(modulus);
            let delta = advance(Some(&first.base()), &second, WINDOW).outcome.unwrap();
            let expected = (u128::from(current) + u128::from(modulus) - u128::from(previous))
                % u128::from(modulus);
            prop_assert_eq!(u128::from(delta.delta), expected);
            prop_assert_eq!(delta.wrapped, current < previous);
        }
    }
}
