// SPDX-License-Identifier: LGPL-2.1-or-later
//! Borrowed stereo-audio types shared by UPSE-NG components.

use thiserror::Error;

/// The channel order used throughout the public playback surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelOrder {
    /// Interleaved left, right pairs.
    LeftRight,
}

/// Native output format for one player.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioFormat {
    sample_rate: u32,
    channels: u8,
    order: ChannelOrder,
}

impl AudioFormat {
    /// Constructs a stereo format.
    #[must_use]
    pub const fn stereo(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            channels: 2,
            order: ChannelOrder::LeftRight,
        }
    }

    /// Returns samples per second for each channel.
    #[must_use]
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    /// Returns the channel count, which is currently always two.
    #[must_use]
    pub const fn channels(self) -> u8 {
        self.channels
    }

    /// Returns the interleaved channel order.
    #[must_use]
    pub const fn order(self) -> ChannelOrder {
        self.order
    }
}

/// An invalid borrowed sample block.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AudioBlockError {
    /// The scalar count was not exactly twice the declared frame count.
    #[error("stereo scalar count does not match frame count")]
    FrameScalarMismatch,
}

/// A borrowed interleaved stereo floating-point block.
#[derive(Clone, Copy, Debug)]
pub struct AudioBlock<'a> {
    samples: &'a [f32],
    frames: usize,
}

impl<'a> AudioBlock<'a> {
    /// Validates and borrows a stereo sample slice.
    ///
    /// # Errors
    ///
    /// Returns [`AudioBlockError::FrameScalarMismatch`] unless `samples`
    /// contains exactly two scalars per declared frame.
    pub fn new(samples: &'a [f32], frames: usize) -> Result<Self, AudioBlockError> {
        if frames.checked_mul(2) != Some(samples.len()) {
            return Err(AudioBlockError::FrameScalarMismatch);
        }
        Ok(Self { samples, frames })
    }

    /// Returns interleaved left/right samples.
    #[must_use]
    pub const fn samples(self) -> &'a [f32] {
        self.samples
    }

    /// Returns the number of stereo frames.
    #[must_use]
    pub const fn frames(self) -> usize {
        self.frames
    }
}

/// Control flow requested by an audio consumer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioAction {
    /// Continue rendering.
    Continue,
    /// Stop gracefully after consuming the current block.
    Stop,
    /// Report a sink failure after consuming the current block.
    Error,
}

/// Result of one bounded render call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderOutcome {
    /// The requested frame count was delivered.
    Complete {
        /// Frames delivered by this call.
        frames: u64,
    },
    /// The module timeline ended after the given number of frames.
    End {
        /// Frames delivered before the end boundary.
        frames: u64,
    },
    /// The callback requested a graceful stop.
    Stopped {
        /// Frames consumed before the stop request took effect.
        frames: u64,
    },
}

impl RenderOutcome {
    /// Returns the frames delivered by the call.
    #[must_use]
    pub const fn frames(self) -> u64 {
        match self {
            Self::Complete { frames } | Self::End { frames } | Self::Stopped { frames } => frames,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioBlock, AudioBlockError, AudioFormat, ChannelOrder, RenderOutcome};

    #[test]
    fn stereo_block_validates_scalar_count_without_allocating() {
        let samples = [0.0, 1.0, 0.5, -0.5];
        let block = AudioBlock::new(&samples, 2).unwrap();
        assert_eq!(block.samples().as_ptr(), samples.as_ptr());
        assert_eq!(block.frames(), 2);
        assert_eq!(
            AudioBlock::new(&samples, 1).unwrap_err(),
            AudioBlockError::FrameScalarMismatch
        );
        assert_eq!(
            AudioBlock::new(&[], usize::MAX).unwrap_err(),
            AudioBlockError::FrameScalarMismatch
        );
    }

    #[test]
    fn public_format_and_outcome_are_exact() {
        let format = AudioFormat::stereo(44_100);
        assert_eq!(format.sample_rate(), 44_100);
        assert_eq!(format.channels(), 2);
        assert_eq!(format.order(), ChannelOrder::LeftRight);
        assert_eq!(RenderOutcome::End { frames: 17 }.frames(), 17);
    }
}
