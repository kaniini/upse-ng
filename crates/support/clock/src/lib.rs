// SPDX-License-Identifier: LGPL-2.1-or-later
//! Deterministic integer clocks and rate conversion.
//!
//! This crate deliberately has no wall-clock API. It converts counts between
//! clock domains with a remainder-preserving rational accumulator.

use thiserror::Error;

/// A count of elapsed ticks in a machine clock domain.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Ticks(u64);

impl Ticks {
    /// Zero elapsed ticks.
    pub const ZERO: Self = Self(0);

    /// Constructs a tick count.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the underlying count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Adds two tick counts, reporting overflow.
    ///
    /// # Errors
    ///
    /// Returns [`ClockError::Overflow`] if the sum does not fit in `u64`.
    pub fn checked_add(self, other: Self) -> Result<Self, ClockError> {
        self.0
            .checked_add(other.0)
            .map(Self)
            .ok_or(ClockError::Overflow)
    }
}

/// An absolute deadline in an emulated clock domain.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Deadline(u64);

impl Deadline {
    /// The initial deadline.
    pub const ZERO: Self = Self(0);

    /// Constructs a deadline.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the underlying timestamp.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advances this deadline by a tick count.
    ///
    /// # Errors
    ///
    /// Returns [`ClockError::Overflow`] if the deadline does not fit in `u64`.
    pub fn checked_advance(self, ticks: Ticks) -> Result<Self, ClockError> {
        self.0
            .checked_add(ticks.0)
            .map(Self)
            .ok_or(ClockError::Overflow)
    }

    /// Returns elapsed ticks since `earlier`, if it is not in the future.
    #[must_use]
    pub fn elapsed_since(self, earlier: Self) -> Option<Ticks> {
        self.0.checked_sub(earlier.0).map(Ticks)
    }
}

/// Failure from checked clock arithmetic.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ClockError {
    /// A clock rate was zero.
    #[error("clock rates must be nonzero")]
    ZeroRate,
    /// An integer result did not fit in the public 64-bit representation.
    #[error("clock arithmetic overflow")]
    Overflow,
}

/// Converts elapsed ticks from one integer-rate domain to another.
///
/// The converter preserves the division remainder between calls, so partitioning
/// an interval never changes the total result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateConverter {
    source_hz: u64,
    target_hz: u64,
    remainder: u64,
}

impl RateConverter {
    /// Constructs a conversion from `source_hz` to `target_hz`.
    ///
    /// # Errors
    ///
    /// Returns [`ClockError::ZeroRate`] if either rate is zero.
    pub fn new(source_hz: u64, target_hz: u64) -> Result<Self, ClockError> {
        if source_hz == 0 || target_hz == 0 {
            return Err(ClockError::ZeroRate);
        }
        Ok(Self {
            source_hz,
            target_hz,
            remainder: 0,
        })
    }

    /// Returns the source clock rate.
    #[must_use]
    pub const fn source_hz(&self) -> u64 {
        self.source_hz
    }

    /// Returns the target clock rate.
    #[must_use]
    pub const fn target_hz(&self) -> u64 {
        self.target_hz
    }

    /// Returns the current numerator remainder, which is less than `source_hz`.
    #[must_use]
    pub const fn remainder(&self) -> u64 {
        self.remainder
    }

    /// Converts an elapsed source interval and preserves its fractional result.
    ///
    /// # Errors
    ///
    /// Returns [`ClockError::Overflow`] if the result exceeds `u64`.
    pub fn advance(&mut self, source_ticks: Ticks) -> Result<Ticks, ClockError> {
        let total = u128::from(source_ticks.0)
            .checked_mul(u128::from(self.target_hz))
            .and_then(|value| value.checked_add(u128::from(self.remainder)))
            .ok_or(ClockError::Overflow)?;
        let source_hz = u128::from(self.source_hz);
        let whole = total / source_hz;
        self.remainder = u64::try_from(total % source_hz).map_err(|_| ClockError::Overflow)?;
        Ok(Ticks(
            u64::try_from(whole).map_err(|_| ClockError::Overflow)?,
        ))
    }

    /// Clears accumulated fractional time.
    pub fn reset(&mut self) {
        self.remainder = 0;
    }

    /// Converts one complete interval without mutating a converter.
    ///
    /// # Errors
    ///
    /// Returns [`ClockError::ZeroRate`] for a zero rate and
    /// [`ClockError::Overflow`] when the result exceeds `u64`.
    pub fn convert_floor(
        source_ticks: Ticks,
        source_hz: u64,
        target_hz: u64,
    ) -> Result<Ticks, ClockError> {
        if source_hz == 0 || target_hz == 0 {
            return Err(ClockError::ZeroRate);
        }
        let whole = u128::from(source_ticks.0)
            .checked_mul(u128::from(target_hz))
            .ok_or(ClockError::Overflow)?
            / u128::from(source_hz);
        Ok(Ticks(
            u64::try_from(whole).map_err(|_| ClockError::Overflow)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{ClockError, Deadline, RateConverter, Ticks};

    #[test]
    fn partitioning_preserves_exact_total() {
        let mut converter = RateConverter::new(33_868_800, 44_100).unwrap();
        let mut total = 0_u64;
        for _ in 0..1_000_000 {
            total += converter.advance(Ticks::new(37)).unwrap().get();
        }
        let exact = RateConverter::convert_floor(Ticks::new(37_000_000), 33_868_800, 44_100)
            .unwrap()
            .get();
        assert_eq!(total, exact);
    }

    #[test]
    fn sample_and_refresh_domains_do_not_drift() {
        for target in [44_100, 48_000, 50, 60] {
            let mut converter = RateConverter::new(33_868_800, target).unwrap();
            let mut total = 0_u64;
            for _ in 0..1_000_000 {
                total += converter.advance(Ticks::new(113)).unwrap().get();
            }
            assert_eq!(
                total,
                RateConverter::convert_floor(Ticks::new(113_000_000), 33_868_800, target)
                    .unwrap()
                    .get()
            );
        }
    }

    #[test]
    fn checked_deadlines_reject_overflow() {
        assert_eq!(
            Deadline::new(u64::MAX).checked_advance(Ticks::new(1)),
            Err(ClockError::Overflow)
        );
        assert_eq!(
            Deadline::new(9).elapsed_since(Deadline::new(4)),
            Some(Ticks::new(5))
        );
        assert_eq!(Deadline::new(4).elapsed_since(Deadline::new(9)), None);
    }

    #[test]
    fn rejects_zero_rates_and_large_results() {
        assert_eq!(RateConverter::new(0, 44_100), Err(ClockError::ZeroRate));
        let mut converter = RateConverter::new(1, u64::MAX).unwrap();
        assert_eq!(
            converter.advance(Ticks::new(u64::MAX)),
            Err(ClockError::Overflow)
        );
    }
}
