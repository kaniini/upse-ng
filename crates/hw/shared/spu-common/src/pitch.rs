// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS1 12-bit fractional pitch stepping.

/// Result of advancing one output sample at a programmed pitch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PitchStep {
    /// Number of decoded input samples crossed.
    pub whole_samples: u32,
    /// Remaining 12-bit fractional phase.
    pub phase: u16,
}

/// Remainder-preserving 12.12 pitch accumulator.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PitchCounter {
    phase: u16,
}

impl PitchCounter {
    /// Constructs a zero-phase pitch counter.
    #[must_use]
    pub const fn new() -> Self {
        Self { phase: 0 }
    }

    /// Returns the current 12-bit fractional phase.
    #[must_use]
    pub const fn phase(self) -> u16 {
        self.phase
    }

    /// Advances by one output sample, clamping pitch to the hardware maximum.
    pub fn advance(&mut self, pitch: u16) -> PitchStep {
        let pitch = u32::from(pitch.min(0x3fff));
        let total = u32::from(self.phase) + pitch;
        self.phase = u16::try_from(total & 0x0fff).unwrap_or(0);
        PitchStep {
            whole_samples: total >> 12,
            phase: self.phase,
        }
    }

    /// Resets the fractional phase.
    pub fn reset(&mut self) {
        self.phase = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{PitchCounter, PitchStep};

    #[test]
    fn unity_fractional_and_clamped_pitch_are_golden() {
        let mut pitch = PitchCounter::new();
        assert_eq!(
            pitch.advance(0x1000),
            PitchStep {
                whole_samples: 1,
                phase: 0
            }
        );
        assert_eq!(pitch.advance(0x0800).whole_samples, 0);
        assert_eq!(pitch.advance(0x0800).whole_samples, 1);
        assert_eq!(pitch.phase(), 0);
        assert_eq!(pitch.advance(u16::MAX).whole_samples, 3);
        assert_eq!(pitch.phase(), 0x0fff);
    }
}
