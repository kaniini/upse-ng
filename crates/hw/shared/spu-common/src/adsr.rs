// SPDX-License-Identifier: LGPL-2.1-or-later
//! Integer ADSR phase and rate arithmetic.

/// One hardware-style envelope rate field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnvelopeRate {
    /// Five-bit shift controlling step interval and magnitude.
    pub shift: u8,
    /// Base signed step, conventionally from -8 through 7.
    pub step: i8,
}

impl EnvelopeRate {
    /// Constructs a bounded rate field.
    #[must_use]
    pub const fn new(shift: u8, step: i8) -> Self {
        Self {
            shift: if shift > 31 { 31 } else { shift },
            step,
        }
    }

    fn interval(self, level: u16, exponential_increase: bool) -> u64 {
        let shift = u32::from(self.shift);
        let mut interval = if shift > 11 { 1_u64 << (shift - 11) } else { 1 };
        if exponential_increase && level > 0x6000 {
            interval = interval.saturating_mul(4);
        }
        interval
    }

    fn delta(self, level: u16, exponential_decrease: bool) -> i32 {
        let shift = u32::from(self.shift);
        let mut delta = i32::from(self.step);
        if shift < 11 {
            delta = delta.saturating_mul(1_i32 << (11 - shift));
        }
        if exponential_decrease {
            delta = (delta * i32::from(level)) >> 15;
            if delta == 0 && level != 0 && self.step < 0 {
                delta = -1;
            }
        }
        delta
    }
}

/// Guest-visible ADSR phase.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EnvelopePhase {
    /// Voice is silent.
    #[default]
    Off,
    /// Rising toward full scale.
    Attack,
    /// Exponentially falling toward sustain level.
    Decay,
    /// Holding or changing according to sustain rate.
    Sustain,
    /// Falling toward silence after key-off.
    Release,
}

/// Decoded hardware envelope configuration.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnvelopeConfig {
    /// Attack rate.
    pub attack: EnvelopeRate,
    /// Exponential attack selection.
    pub attack_exponential: bool,
    /// Decay rate.
    pub decay: EnvelopeRate,
    /// Sustain threshold from zero through 0x7fff.
    pub sustain_level: u16,
    /// Sustain rate.
    pub sustain: EnvelopeRate,
    /// Sustain moves downward when set.
    pub sustain_decrease: bool,
    /// Exponential sustain selection.
    pub sustain_exponential: bool,
    /// Release rate.
    pub release: EnvelopeRate,
    /// Exponential release selection.
    pub release_exponential: bool,
}

impl EnvelopeConfig {
    /// Decodes the two 16-bit PS1 ADSR registers.
    #[must_use]
    pub fn from_registers(low: u16, high: u16) -> Self {
        let attack_index = i8::try_from(low.to_le_bytes()[1] & 3).unwrap_or(0);
        let attack_step = 7_i8 - attack_index;
        let sustain_decrease = high & (1 << 14) != 0;
        let sustain_index = i8::try_from((high.to_le_bytes()[0] >> 6) & 3).unwrap_or(0);
        let sustain_step = if sustain_decrease {
            -8 + sustain_index
        } else {
            7 - sustain_index
        };
        let sustain_level = ((low & 0x0f) + 1).saturating_mul(0x0800).min(0x7fff);
        Self {
            attack: EnvelopeRate::new((low.to_le_bytes()[1] >> 2) & 0x1f, attack_step),
            attack_exponential: low & (1 << 15) != 0,
            decay: EnvelopeRate::new((low.to_le_bytes()[0] >> 4) & 0x0f, -8),
            sustain_level,
            sustain: EnvelopeRate::new(high.to_le_bytes()[1] & 0x1f, sustain_step),
            sustain_decrease,
            sustain_exponential: high & (1 << 15) != 0,
            release: EnvelopeRate::new(high.to_le_bytes()[0] & 0x1f, -8),
            release_exponential: high & (1 << 5) != 0,
        }
    }
}

/// Stateful integer ADSR envelope.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Envelope {
    level: u16,
    phase: EnvelopePhase,
    clocks_until_step: u64,
}

impl Envelope {
    /// Constructs a silent envelope.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            level: 0,
            phase: EnvelopePhase::Off,
            clocks_until_step: 0,
        }
    }

    /// Returns the current unsigned 15-bit level.
    #[must_use]
    pub const fn level(self) -> u16 {
        self.level
    }

    /// Returns the current phase.
    #[must_use]
    pub const fn phase(self) -> EnvelopePhase {
        self.phase
    }

    /// Starts attack from silence.
    pub fn key_on(&mut self) {
        self.level = 0;
        self.phase = EnvelopePhase::Attack;
        self.clocks_until_step = 0;
    }

    /// Starts release from the current level.
    pub fn key_off(&mut self) {
        if self.phase != EnvelopePhase::Off {
            self.phase = EnvelopePhase::Release;
            self.clocks_until_step = 0;
        }
    }

    /// Advances envelope clocks without floating-point or wall-clock state.
    pub fn advance(&mut self, config: &EnvelopeConfig, mut clocks: u64) {
        while clocks != 0 && self.phase != EnvelopePhase::Off {
            if self.clocks_until_step == 0 {
                self.clocks_until_step = self.current_interval(config);
            }
            let elapsed = clocks.min(self.clocks_until_step);
            clocks -= elapsed;
            self.clocks_until_step -= elapsed;
            if self.clocks_until_step == 0 {
                self.apply_step(config);
            }
        }
    }

    fn current_interval(self, config: &EnvelopeConfig) -> u64 {
        match self.phase {
            EnvelopePhase::Attack => config
                .attack
                .interval(self.level, config.attack_exponential),
            EnvelopePhase::Decay => config.decay.interval(self.level, false),
            EnvelopePhase::Sustain => config.sustain.interval(
                self.level,
                config.sustain_exponential && !config.sustain_decrease,
            ),
            EnvelopePhase::Release => config.release.interval(self.level, false),
            EnvelopePhase::Off => 1,
        }
    }

    fn apply_step(&mut self, config: &EnvelopeConfig) {
        let delta = match self.phase {
            EnvelopePhase::Attack => config.attack.delta(self.level, false),
            EnvelopePhase::Decay => config.decay.delta(self.level, true),
            EnvelopePhase::Sustain => config.sustain.delta(
                self.level,
                config.sustain_exponential && config.sustain_decrease,
            ),
            EnvelopePhase::Release => config.release.delta(self.level, config.release_exponential),
            EnvelopePhase::Off => 0,
        };
        let next = i32::from(self.level).saturating_add(delta).clamp(0, 0x7fff);
        self.level = u16::try_from(next).unwrap_or(0);
        match self.phase {
            EnvelopePhase::Attack if self.level == 0x7fff => {
                self.phase = EnvelopePhase::Decay;
            }
            EnvelopePhase::Decay if self.level <= config.sustain_level => {
                self.phase = EnvelopePhase::Sustain;
            }
            EnvelopePhase::Release if self.level == 0 => {
                self.phase = EnvelopePhase::Off;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Envelope, EnvelopeConfig, EnvelopePhase, EnvelopeRate};

    fn config() -> EnvelopeConfig {
        EnvelopeConfig {
            attack: EnvelopeRate::new(11, 7),
            attack_exponential: false,
            decay: EnvelopeRate::new(11, -8),
            sustain_level: 0x5fff,
            sustain: EnvelopeRate::new(12, -5),
            sustain_decrease: true,
            sustain_exponential: false,
            release: EnvelopeRate::new(11, -8),
            release_exponential: false,
        }
    }

    #[test]
    fn phase_transitions_and_levels_match_golden_sequence() {
        let config = config();
        let mut envelope = Envelope::new();
        envelope.key_on();
        envelope.advance(&config, 4_681);
        assert_eq!(envelope.level(), 0x7fff);
        assert_eq!(envelope.phase(), EnvelopePhase::Decay);
        envelope.advance(&config, 1_200);
        assert_eq!(envelope.phase(), EnvelopePhase::Sustain);
        assert!(envelope.level() <= config.sustain_level);
        let sustain = envelope.level();
        envelope.advance(&config, 20);
        assert!(envelope.level() < sustain);
        envelope.key_off();
        envelope.advance(&config, 4_096);
        assert_eq!(envelope.level(), 0);
        assert_eq!(envelope.phase(), EnvelopePhase::Off);
    }

    #[test]
    fn register_decode_and_chunk_partitioning_are_stable() {
        let config = EnvelopeConfig::from_registers(0x8f7a, 0xc523);
        assert_eq!(config.sustain_level, 0x5800);
        assert!(config.attack_exponential);
        assert!(config.sustain_decrease);
        assert!(config.sustain_exponential);
        assert!(config.release_exponential);

        let mut whole = Envelope::new();
        whole.key_on();
        whole.advance(&config, 10_000);
        let mut chunks = Envelope::new();
        chunks.key_on();
        for _ in 0..100 {
            chunks.advance(&config, 100);
        }
        assert_eq!(whole, chunks);
    }

    #[test]
    fn exponential_release_never_stalls_above_zero() {
        let config = EnvelopeConfig {
            release_exponential: true,
            ..config()
        };
        let mut envelope = Envelope::new();
        envelope.key_on();
        envelope.advance(&config, 5_000);
        envelope.key_off();
        envelope.advance(&config, 100_000);
        assert_eq!(envelope.phase(), EnvelopePhase::Off);
        assert_eq!(envelope.level(), 0);
    }
}
