// SPDX-License-Identifier: LGPL-2.1-or-later
use thiserror::Error;

/// An exact nonnegative duration represented as reduced rational seconds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Duration {
    numerator: u64,
    denominator: u64,
}

impl Duration {
    /// Zero seconds.
    pub const ZERO: Self = Self {
        numerator: 0,
        denominator: 1,
    };

    /// Parses `seconds`, `minutes:seconds`, or `hours:minutes:seconds`.
    ///
    /// A period or comma may introduce the decimal fractional part.
    ///
    /// # Errors
    ///
    /// Returns [`DurationError`] for invalid syntax, out-of-range colon
    /// components, or arithmetic overflow.
    pub fn parse(input: &str) -> Result<Self, DurationError> {
        let input = input.trim();
        if input.is_empty() || input.starts_with('-') || input.starts_with('+') {
            return Err(DurationError::InvalidSyntax);
        }
        let components: Vec<_> = input.split(':').collect();
        if !(1..=3).contains(&components.len()) {
            return Err(DurationError::InvalidSyntax);
        }
        let (hours, minutes, seconds_text) = match components.as_slice() {
            [seconds] => (0_u64, 0_u64, *seconds),
            [minutes, seconds] => (0, integer(minutes)?, *seconds),
            [hours, minutes, seconds] => (integer(hours)?, integer(minutes)?, *seconds),
            _ => unreachable!(),
        };
        if components.len() == 3 && minutes >= 60 {
            return Err(DurationError::OutOfRange);
        }
        let normalized = seconds_text.replace(',', ".");
        if normalized.matches('.').count() > 1 {
            return Err(DurationError::InvalidSyntax);
        }
        let (seconds_whole_text, fraction_text) = normalized
            .split_once('.')
            .map_or((normalized.as_str(), None), |(whole, fraction)| {
                (whole, Some(fraction))
            });
        let seconds = integer(seconds_whole_text)?;
        if components.len() > 1 && seconds >= 60 {
            return Err(DurationError::OutOfRange);
        }
        let (fraction, scale) = match fraction_text {
            None => (0_u64, 1_u64),
            Some("") => return Err(DurationError::InvalidSyntax),
            Some(text) if text.len() > 18 || !text.bytes().all(|byte| byte.is_ascii_digit()) => {
                return Err(DurationError::InvalidSyntax);
            }
            Some(text) => {
                let scale = 10_u64
                    .checked_pow(u32::try_from(text.len()).map_err(|_| DurationError::Overflow)?)
                    .ok_or(DurationError::Overflow)?;
                (integer(text)?, scale)
            }
        };
        let whole = hours
            .checked_mul(3600)
            .and_then(|value| minutes.checked_mul(60).and_then(|m| value.checked_add(m)))
            .and_then(|value| value.checked_add(seconds))
            .ok_or(DurationError::Overflow)?;
        let numerator = whole
            .checked_mul(scale)
            .and_then(|value| value.checked_add(fraction))
            .ok_or(DurationError::Overflow)?;
        let divisor = gcd(numerator, scale);
        Ok(Self {
            numerator: numerator / divisor,
            denominator: scale / divisor,
        })
    }

    /// Constructs a duration from an exact fraction of a second.
    ///
    /// # Errors
    ///
    /// Returns [`DurationError::ZeroDenominator`] for a zero denominator.
    pub fn from_ratio(numerator: u64, denominator: u64) -> Result<Self, DurationError> {
        if denominator == 0 {
            return Err(DurationError::ZeroDenominator);
        }
        let divisor = gcd(numerator, denominator);
        Ok(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    /// Returns the reduced numerator in seconds.
    #[must_use]
    pub const fn numerator(self) -> u64 {
        self.numerator
    }

    /// Returns the reduced denominator in seconds.
    #[must_use]
    pub const fn denominator(self) -> u64 {
        self.denominator
    }

    /// Maps the duration to native frames using deterministic floor rounding.
    ///
    /// # Errors
    ///
    /// Returns [`DurationError::Overflow`] if the frame count exceeds `u64`.
    pub fn to_frames_floor(self, sample_rate: u32) -> Result<u64, DurationError> {
        let frames = u128::from(self.numerator)
            .checked_mul(u128::from(sample_rate))
            .ok_or(DurationError::Overflow)?
            / u128::from(self.denominator);
        u64::try_from(frames).map_err(|_| DurationError::Overflow)
    }
}

/// Duration parse or conversion failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DurationError {
    /// Text did not match a supported duration grammar.
    #[error("invalid duration syntax")]
    InvalidSyntax,
    /// Minutes or seconds exceeded the range for a colon component.
    #[error("duration component out of range")]
    OutOfRange,
    /// Integer arithmetic overflowed.
    #[error("duration arithmetic overflow")]
    Overflow,
    /// A constructed ratio used a zero denominator.
    #[error("duration denominator is zero")]
    ZeroDenominator,
}

fn integer(text: &str) -> Result<u64, DurationError> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DurationError::InvalidSyntax);
    }
    text.parse().map_err(|_| DurationError::Overflow)
}

const fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    if left == 0 { 1 } else { left }
}

#[cfg(test)]
mod tests {
    use super::{Duration, DurationError};

    #[test]
    fn parses_every_specified_grammar_exactly() {
        let cases = [
            ("1.5", (3, 2)),
            ("2,25", (9, 4)),
            ("1:02.5", (125, 2)),
            ("1:02:03.125", (29_785, 8)),
        ];
        for (text, expected) in cases {
            let duration = Duration::parse(text).unwrap();
            assert_eq!((duration.numerator(), duration.denominator()), expected);
        }
    }

    #[test]
    fn maps_to_frames_once_with_floor_rounding() {
        assert_eq!(
            Duration::parse("0.0001")
                .unwrap()
                .to_frames_floor(44_100)
                .unwrap(),
            4
        );
    }

    #[test]
    fn rejects_invalid_ranges_and_overflow() {
        assert_eq!(Duration::parse("1:60"), Err(DurationError::OutOfRange));
        assert_eq!(Duration::parse("1::2"), Err(DurationError::InvalidSyntax));
        assert_eq!(Duration::parse("-1"), Err(DurationError::InvalidSyntax));
        assert_eq!(
            Duration::from_ratio(1, 0),
            Err(DurationError::ZeroDenominator)
        );
    }
}
