// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS1 ADPCM block decoding.

use thiserror::Error;

use crate::clamp_i16;

const FILTERS: [(i32, i32); 5] = [(0, 0), (60, 0), (115, -52), (98, -55), (122, -60)];

/// Predictor history carried between ADPCM blocks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdpcmHistory {
    previous: i16,
    previous2: i16,
}

impl AdpcmHistory {
    /// Constructs explicit predictor history.
    #[must_use]
    pub const fn new(previous: i16, previous2: i16) -> Self {
        Self {
            previous,
            previous2,
        }
    }

    /// Returns the most recently decoded sample.
    #[must_use]
    pub const fn previous(self) -> i16 {
        self.previous
    }

    /// Returns the sample preceding [`AdpcmHistory::previous`].
    #[must_use]
    pub const fn previous2(self) -> i16 {
        self.previous2
    }
}

/// Loop and end markers carried by one ADPCM block.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdpcmFlags {
    /// Voice stops after this block unless repeat is also set.
    pub end: bool,
    /// Voice jumps to its captured repeat address after the block.
    pub repeat: bool,
    /// This block captures the voice repeat address.
    pub loop_start: bool,
}

/// Twenty-eight decoded samples and their source flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedBlock {
    /// Signed decoded PCM samples in playback order.
    pub samples: [i16; 28],
    /// Loop/end flags from the block header.
    pub flags: AdpcmFlags,
}

/// ADPCM decoder diagnostic.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AdpcmError {
    /// Shift is outside the decoder's supported hardware behavior.
    #[error("invalid PlayStation ADPCM shift {shift}")]
    InvalidShift {
        /// Four-bit header value.
        shift: u8,
    },
}

/// Decodes one 16-byte ADPCM block and updates predictor history.
///
/// # Errors
///
/// Returns [`AdpcmError`] for unsupported header encodings. Reserved shifts 13
/// through 15 decode as shift 9, while reserved predictor selectors 5 through
/// 15 use the hardware's zero-coefficient behavior.
pub fn decode_block(
    block: &[u8; 16],
    history: &mut AdpcmHistory,
) -> Result<DecodedBlock, AdpcmError> {
    let encoded_shift = block[0] & 0x0f;
    let shift = if encoded_shift > 12 { 9 } else { encoded_shift };
    let filter = block[0] >> 4;
    // Hardware treats the reserved predictor selectors as filter zero.
    let (positive, negative) = FILTERS.get(usize::from(filter)).copied().unwrap_or((0, 0));
    let mut samples = [0_i16; 28];
    for (index, output) in samples.iter_mut().enumerate() {
        let packed = block[2 + index / 2];
        let nibble = if index & 1 == 0 {
            packed & 0x0f
        } else {
            packed >> 4
        };
        let signed = if nibble & 8 == 0 {
            i32::from(nibble)
        } else {
            i32::from(nibble) - 16
        };
        let source = (signed << 12) >> shift;
        let predicted =
            i32::from(history.previous) * positive + i32::from(history.previous2) * negative;
        let sample = clamp_i16(source.saturating_add((predicted + 32) >> 6));
        history.previous2 = history.previous;
        history.previous = sample;
        *output = sample;
    }
    Ok(DecodedBlock {
        samples,
        flags: AdpcmFlags {
            end: block[1] & 1 != 0,
            repeat: block[1] & 2 != 0,
            loop_start: block[1] & 4 != 0,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::{AdpcmFlags, AdpcmHistory, decode_block};

    #[test]
    fn filter_zero_nibble_order_and_flags_match_golden_vector() {
        let mut block = [0_u8; 16];
        block[0] = 12;
        block[1] = 7;
        block[2] = 0xf1;
        block[3] = 0x87;
        let decoded = decode_block(&block, &mut AdpcmHistory::default()).unwrap();
        assert_eq!(&decoded.samples[..4], &[1, -1, 7, -8]);
        assert!(decoded.samples[4..].iter().all(|&sample| sample == 0));
        assert_eq!(
            decoded.flags,
            AdpcmFlags {
                end: true,
                repeat: true,
                loop_start: true
            }
        );
    }

    #[test]
    fn predictor_history_crosses_blocks_and_saturates() {
        let mut first = [0_u8; 16];
        first[0] = 0x1c;
        first[2] = 7;
        let mut history = AdpcmHistory::default();
        let decoded = decode_block(&first, &mut history).unwrap();
        assert!(decoded.samples.iter().all(|&sample| sample == 7));
        assert_eq!(history, AdpcmHistory::new(7, 7));

        let block = [
            0x40, 0, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77,
            0x77,
        ];
        let decoded = decode_block(&block, &mut history).unwrap();
        assert_eq!(decoded.samples.last(), Some(&i16::MAX));
        assert_eq!(history.previous(), i16::MAX);
    }

    #[test]
    fn reserved_shifts_decode_as_shift_nine() {
        for shift in 13..=15 {
            let mut block = [0_u8; 16];
            block[0] = shift;
            block[2] = 0xf1;
            block[3] = 0x87;
            let decoded = decode_block(&block, &mut AdpcmHistory::default()).unwrap();
            assert_eq!(&decoded.samples[..4], &[8, -8, 56, -64]);
        }
    }

    #[test]
    fn reserved_predictors_decode_with_zero_coefficients() {
        let mut block = [0_u8; 16];
        block[0] = 0xac;
        block[2] = 0xf1;
        let mut history = AdpcmHistory::new(12_000, -8_000);
        let decoded = decode_block(&block, &mut history).unwrap();
        assert_eq!(&decoded.samples[..4], &[1, -1, 0, 0]);
    }
}
