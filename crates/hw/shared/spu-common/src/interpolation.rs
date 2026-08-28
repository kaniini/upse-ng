// SPDX-License-Identifier: LGPL-2.1-or-later
//! Hardware four-tap Gaussian interpolation.

#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use crate::clamp_i16;
use std::sync::LazyLock;

const PHASES: usize = 256;

/// Four-tap coefficients for every eight-bit hardware phase.
pub static GAUSSIAN_WEIGHTS: LazyLock<[[i16; 4]; PHASES]> = LazyLock::new(make_weights);

fn make_weights() -> [[i16; 4]; PHASES] {
    let mut source = [0.0_f64; 512];
    for sample in 0_u16..512 {
        let index = usize::from(sample);
        let position = 0.5 + f64::from(sample);
        let fundamental = (std::f64::consts::PI * position * 2.048 / 1024.0).sin();
        let first = ((std::f64::consts::PI * position * 2.0 / 1023.0).cos() - 1.0) * 0.5;
        let second = ((std::f64::consts::PI * position * 4.0 / 1023.0).cos() - 1.0) * 0.08;
        source[511 - index] = fundamental * (first + second + 1.0) / position;
    }
    let scale = f64::from(0x7f80 * 128) / source.iter().sum::<f64>();
    for sample in &mut source {
        *sample *= scale;
    }

    let mut table = [[0_i16; 4]; PHASES];
    for phase in 0_u16..256 {
        let phase = usize::from(phase);
        let sum = source[phase] + source[phase + 256] + source[511 - phase] + source[255 - phase];
        let correction = (sum - f64::from(0x7f80)) / 4.0;
        table[255 - phase] = [
            (source[phase] - correction).round() as i16,
            (source[phase + 256] - correction).round() as i16,
            (source[511 - phase] - correction).round() as i16,
            (source[255 - phase] - correction).round() as i16,
        ];
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
            .fold(0_i32, |sum, (sample, weight)| {
                sum + ((i32::from(sample) * i32::from(weight)) >> 15)
            });
        clamp_i16(sum)
    }
}

#[cfg(test)]
mod tests {
    use super::{GAUSSIAN_WEIGHTS, GaussianInterpolator};

    #[test]
    fn hardware_table_is_normalized_and_symmetric() {
        for (phase, weights) in GAUSSIAN_WEIGHTS.iter().enumerate() {
            let sum = weights.iter().copied().map(i32::from).sum::<i32>();
            assert!((32_639..=32_641).contains(&sum), "phase {phase}: {sum}");
            let mut mirrored = GAUSSIAN_WEIGHTS[255 - phase];
            mirrored.reverse();
            assert_eq!(*weights, mirrored);
        }
        assert_eq!(GAUSSIAN_WEIGHTS[0], [0x12c7, 0x59b3, 0x1307, -1]);
        assert_eq!(GAUSSIAN_WEIGHTS[255], [-1, 0x1307, 0x59b3, 0x12c7]);
    }

    #[test]
    fn interpolation_phases_and_saturation_match_goldens() {
        let samples = [0, 3_000, 6_000, 9_000];
        assert_eq!(GaussianInterpolator::interpolate(samples, 0), 2_992);
        assert_eq!(GaussianInterpolator::interpolate(samples, 128), 4_487);
        assert_eq!(GaussianInterpolator::interpolate(samples, u8::MAX), 5_969);
        assert_eq!(GaussianInterpolator::interpolate([i16::MAX; 4], 77), 32_635);
    }
}
