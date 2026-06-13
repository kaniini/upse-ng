// SPDX-License-Identifier: LGPL-2.1-or-later
//! Final signed-integer conversion, tag volume, and linear fade timeline.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use thiserror::Error;

/// Invalid timeline buffer or range.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PostMixError {
    /// Integer input was not interleaved stereo.
    #[error("integer input has an odd scalar count")]
    OddInput,
    /// Floating output did not match the integer scalar count.
    #[error("floating output length does not match integer input")]
    OutputSize,
    /// Processing would cross the declared end boundary.
    #[error("post-mix block crosses the declared end frame")]
    CrossesEnd,
    /// Timeline position overflowed.
    #[error("post-mix timeline overflow")]
    PositionOverflow,
}

/// Chunk-independent PSF volume, length, and fade state.
#[derive(Clone, Debug, PartialEq)]
pub struct PostMixer {
    volume: f64,
    length_frames: Option<u64>,
    fade_frames: u64,
    position: u64,
}

impl PostMixer {
    /// Constructs a timeline at frame zero.
    #[must_use]
    pub const fn new(volume: f64, length_frames: Option<u64>, fade_frames: u64) -> Self {
        Self {
            volume,
            length_frames,
            fade_frames,
            position: 0,
        }
    }

    /// Returns the number of frames already converted.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.position
    }

    /// Returns the exclusive end frame, or no automatic ending.
    #[must_use]
    pub fn end_frame(&self) -> Option<u64> {
        self.length_frames
            .map(|length| length.saturating_add(self.fade_frames))
    }

    /// Reports whether the declared timeline has ended.
    #[must_use]
    pub fn ended(&self) -> bool {
        self.end_frame().is_some_and(|end| self.position >= end)
    }

    /// Limits a requested block to the declared timeline boundary.
    #[must_use]
    pub fn available_frames(&self, requested: u64) -> u64 {
        match self.end_frame() {
            Some(end) => requested.min(end.saturating_sub(self.position)),
            None => requested,
        }
    }

    /// Restores frame zero without changing volume or duration policy.
    pub fn reset(&mut self) {
        self.position = 0;
    }

    /// Converts interleaved signed 16-bit samples to floating point in place.
    ///
    /// Volume is deliberately not clipped; negative and over-unity tag values
    /// remain observable at the public boundary. Fade gain is linear in
    /// amplitude from the declared length through the exclusive end frame.
    ///
    /// # Errors
    ///
    /// Returns [`PostMixError`] for invalid buffer sizes, a block crossing the
    /// end boundary, or position overflow.
    pub fn process(&mut self, input: &[i16], output: &mut [f32]) -> Result<u64, PostMixError> {
        if input.len() & 1 != 0 {
            return Err(PostMixError::OddInput);
        }
        if input.len() != output.len() {
            return Err(PostMixError::OutputSize);
        }
        let frames = u64::try_from(input.len() / 2).map_err(|_| PostMixError::PositionOverflow)?;
        if self.available_frames(frames) != frames {
            return Err(PostMixError::CrossesEnd);
        }
        for (frame_index, (source, destination)) in input
            .chunks_exact(2)
            .zip(output.chunks_exact_mut(2))
            .enumerate()
        {
            let frame_index =
                u64::try_from(frame_index).map_err(|_| PostMixError::PositionOverflow)?;
            let position = self
                .position
                .checked_add(frame_index)
                .ok_or(PostMixError::PositionOverflow)?;
            let gain = self.volume * self.fade_gain(position);
            destination[0] = ((f64::from(source[0]) / 32_768.0) * gain) as f32;
            destination[1] = ((f64::from(source[1]) / 32_768.0) * gain) as f32;
        }
        self.position = self
            .position
            .checked_add(frames)
            .ok_or(PostMixError::PositionOverflow)?;
        Ok(frames)
    }

    fn fade_gain(&self, position: u64) -> f64 {
        let Some(length) = self.length_frames else {
            return 1.0;
        };
        if position < length {
            return 1.0;
        }
        if self.fade_frames == 0 {
            return 0.0;
        }
        let elapsed = position - length;
        let remaining = self.fade_frames.saturating_sub(elapsed);
        remaining as f64 / self.fade_frames as f64
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::{PostMixError, PostMixer};

    #[test]
    fn integer_conversion_volume_and_fade_match_exact_frame_goldens() {
        let input = [
            16_384_i16, -16_384, 16_384, -16_384, 16_384, -16_384, 16_384, -16_384,
        ];
        let mut output = [0.0_f32; 8];
        let mut mixer = PostMixer::new(2.0, Some(2), 2);
        assert_eq!(mixer.process(&input, &mut output), Ok(4));
        assert_eq!(output, [1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 0.5, -0.5]);
        assert!(mixer.ended());
        assert_eq!(mixer.end_frame(), Some(4));
    }

    #[test]
    fn partitioning_negative_volume_and_absent_length_are_stable() {
        let input = [8_192_i16, -8_192, 16_384, -16_384, 24_576, -24_576];
        let mut whole = PostMixer::new(-1.5, None, 99);
        let mut expected = [0.0; 6];
        whole.process(&input, &mut expected).unwrap();
        assert!(!whole.ended());
        assert_eq!(whole.end_frame(), None);

        let mut chunks = PostMixer::new(-1.5, None, 99);
        let mut actual = [0.0; 6];
        chunks.process(&input[..2], &mut actual[..2]).unwrap();
        chunks.process(&input[2..], &mut actual[2..]).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(chunks.position(), 3);
        chunks.reset();
        assert_eq!(chunks.position(), 0);
    }

    #[test]
    fn invalid_blocks_and_end_crossing_are_rejected_before_mutation() {
        let mut mixer = PostMixer::new(1.0, Some(1), 0);
        assert_eq!(mixer.process(&[1], &mut [0.0]), Err(PostMixError::OddInput));
        assert_eq!(
            mixer.process(&[1, 2], &mut []),
            Err(PostMixError::OutputSize)
        );
        assert_eq!(
            mixer.process(&[1, 2, 3, 4], &mut [0.0; 4]),
            Err(PostMixError::CrossesEnd)
        );
        assert_eq!(mixer.position(), 0);
    }
}
