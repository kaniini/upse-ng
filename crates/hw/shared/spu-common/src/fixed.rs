// SPDX-License-Identifier: LGPL-2.1-or-later
//! Shared signed fixed-point operations.

/// Saturates a signed accumulator to a 16-bit sample.
#[must_use]
pub fn clamp_i16(value: i32) -> i16 {
    match i16::try_from(value) {
        Ok(value) => value,
        Err(_) if value < 0 => i16::MIN,
        Err(_) => i16::MAX,
    }
}

/// Multiplies two signed values as Q15 and truncates toward negative infinity.
#[must_use]
pub fn multiply_q15(left: i16, right: i16) -> i32 {
    (i32::from(left) * i32::from(right)) >> 15
}

/// Multiplies Q15 pairs, accumulates in 64 bits, and saturates to one sample.
#[must_use]
pub fn mac_q15(terms: &[(i16, i16)]) -> i16 {
    let sum = terms.iter().fold(0_i64, |sum, &(left, right)| {
        sum.saturating_add(i64::from(left) * i64::from(right))
    });
    let shifted = sum >> 15;
    if shifted < i64::from(i16::MIN) {
        i16::MIN
    } else if shifted > i64::from(i16::MAX) {
        i16::MAX
    } else {
        i16::try_from(shifted).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{clamp_i16, mac_q15, multiply_q15};

    #[test]
    fn saturation_and_q15_boundaries_are_golden() {
        assert_eq!(clamp_i16(-40_000), i16::MIN);
        assert_eq!(clamp_i16(40_000), i16::MAX);
        assert_eq!(clamp_i16(123), 123);
        assert_eq!(multiply_q15(16_384, 16_384), 8_192);
        assert_eq!(multiply_q15(-16_384, 16_384), -8_192);
        assert_eq!(mac_q15(&[(i16::MAX, i16::MAX); 4]), i16::MAX);
        assert_eq!(mac_q15(&[(i16::MIN, i16::MAX); 4]), i16::MIN);
    }
}
