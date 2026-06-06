// SPDX-License-Identifier: LGPL-2.1-or-later
//! Deterministic 16-bit noise sequence primitive.

/// Nonzero 16-bit Fibonacci LFSR used as a reproducible noise source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NoiseGenerator {
    state: u16,
}

impl Default for NoiseGenerator {
    fn default() -> Self {
        Self::new(1)
    }
}

impl NoiseGenerator {
    /// Constructs the generator; a zero seed is replaced with one.
    #[must_use]
    pub const fn new(seed: u16) -> Self {
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    /// Returns the current nonzero state.
    #[must_use]
    pub const fn state(self) -> u16 {
        self.state
    }

    /// Advances the `x^16 + x^14 + x^13 + x^11 + 1` sequence one step.
    pub fn step(&mut self) -> i16 {
        let feedback = (self.state ^ (self.state >> 2) ^ (self.state >> 3) ^ (self.state >> 5)) & 1;
        self.state = (self.state >> 1) | (feedback << 15);
        i16::from_ne_bytes(self.state.to_ne_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::NoiseGenerator;

    #[test]
    fn zero_seed_and_initial_sequence_are_golden() {
        let mut noise = NoiseGenerator::new(0);
        assert_eq!(noise.state(), 1);
        let actual: Vec<_> = (0..8).map(|_| noise.step()).collect();
        assert_eq!(
            actual,
            [-32_768, 16_384, 8_192, 4_096, 2_048, 1_024, 512, 256]
        );
        assert_ne!(noise.state(), 0);
    }
}
