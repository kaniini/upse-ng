// SPDX-License-Identifier: LGPL-2.1-or-later
//! Deterministic four-tap Gaussian-shaped interpolation.

#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use crate::clamp_i16;

const PHASES: usize = 256;
const Q15_ONE: i64 = 32_768;
const GRID: i64 = 256;
const DENOMINATOR: i64 = 6 * GRID * GRID * GRID;

/// Compile-time-generated four-tap cubic B-spline Gaussian approximation.
///
/// Each row is generated from the cubic B-spline basis and sums to exactly
/// 32768.
pub const GAUSSIAN_WEIGHTS: [[i16; 4]; PHASES] = make_weights();

const fn make_weights() -> [[i16; 4]; PHASES] {
    let mut table = [[0_i16; 4]; PHASES];
    let mut phase = 0_usize;
    while phase < PHASES {
        let t = phase as i64;
        let u = GRID - t;
        let t2 = t * t;
        let t3 = t2 * t;
        let u2 = u * u;
        let u3 = u2 * u;
        let w0 = (u3 * Q15_ONE + DENOMINATOR / 2) / DENOMINATOR;
        let w2 = ((-3 * t3 + 3 * GRID * t2 + 3 * GRID * GRID * t + GRID * GRID * GRID) * Q15_ONE
            + DENOMINATOR / 2)
            / DENOMINATOR;
        let w3 = (t3 * Q15_ONE + DENOMINATOR / 2) / DENOMINATOR;
        let w1 = Q15_ONE - w0 - w2 - w3;
        table[phase] = [w0 as i16, w1 as i16, w2 as i16, w3 as i16];
        phase += 1;
    }
    table
}

/// Stateless four-sample interpolator using [`GAUSSIAN_WEIGHTS`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GaussianInterpolator;

impl GaussianInterpolator {
    /// Interpolates samples at an eight-bit phase and saturates the result.
    #[must_use]
    pub fn interpolate(samples: [i16; 4], phase: u8) -> i16 {
        let weights = GAUSSIAN_WEIGHTS[usize::from(phase)];
        let sum = samples
            .into_iter()
            .zip(weights)
            .fold(0_i64, |sum, (sample, weight)| {
                sum + i64::from(sample) * i64::from(weight)
            });
        let rounded = (sum + (1 << 14)) >> 15;
        let bounded = if rounded < i64::from(i32::MIN) {
            i32::MIN
        } else if rounded > i64::from(i32::MAX) {
            i32::MAX
        } else {
            i32::try_from(rounded).unwrap_or_default()
        };
        clamp_i16(bounded)
    }
}

#[cfg(test)]
mod tests {
    use super::{GAUSSIAN_WEIGHTS, GaussianInterpolator};

    #[test]
    fn every_phase_is_normalized_and_symmetric() {
        for weights in GAUSSIAN_WEIGHTS {
            assert_eq!(weights.into_iter().map(i32::from).sum::<i32>(), 32_768);
            assert!(weights.into_iter().all(|weight| weight >= 0));
        }
        assert_eq!(GAUSSIAN_WEIGHTS[0][0], GAUSSIAN_WEIGHTS[0][2]);
        assert_eq!(GAUSSIAN_WEIGHTS[128][0], GAUSSIAN_WEIGHTS[128][3]);
        assert_eq!(GAUSSIAN_WEIGHTS[128][1], GAUSSIAN_WEIGHTS[128][2]);
    }

    #[test]
    fn interpolation_phases_and_saturation_match_goldens() {
        let samples = [0, 3_000, 6_000, 9_000];
        assert_eq!(GaussianInterpolator::interpolate(samples, 0), 3_000);
        assert_eq!(GaussianInterpolator::interpolate(samples, 128), 4_500);
        assert_eq!(GaussianInterpolator::interpolate(samples, u8::MAX), 5_988);
        assert_eq!(
            GaussianInterpolator::interpolate([i16::MAX; 4], 77),
            i16::MAX
        );
    }
}
