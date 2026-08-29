// SPDX-License-Identifier: LGPL-2.1-or-later
//! Final signed-integer conversion, tag volume, fade timeline, and silence detection.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::collections::VecDeque;

use thiserror::Error;

/// Invalid silence-detector configuration, input, or output.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SilenceError {
    /// The normalized amplitude threshold was negative or non-finite.
    #[error("silence threshold must be finite and nonnegative")]
    InvalidThreshold,
    /// At least one trailing frame is required.
    #[error("silence duration must contain at least one frame")]
    InvalidDuration,
    /// Input or output was not interleaved stereo.
    #[error("silence detector requires interleaved stereo samples")]
    OddSamples,
    /// Host buffering for render-ahead audio could not be reserved.
    #[error("cannot allocate silence render-ahead buffer")]
    Allocation,
}

/// Render-ahead trailing-silence detector.
///
/// Leading silence is delivered normally. Once an audible frame has been
/// observed, quiet frames are withheld until audio resumes or the configured
/// duration is reached. A short quiet passage is delivered byte-for-byte;
/// confirmed trailing silence is discarded.
#[derive(Clone, Debug, PartialEq)]
pub struct SilenceDetector {
    threshold: f32,
    required_frames: u64,
    heard_audio: bool,
    ended: bool,
    candidate: VecDeque<f32>,
    ready: VecDeque<f32>,
}

impl SilenceDetector {
    /// Constructs an armed detector with empty render-ahead buffers.
    ///
    /// # Errors
    ///
    /// Returns [`SilenceError`] for a negative/non-finite threshold or a zero
    /// frame duration.
    pub fn new(threshold: f32, required_frames: u64) -> Result<Self, SilenceError> {
        if !threshold.is_finite() || threshold < 0.0 {
            return Err(SilenceError::InvalidThreshold);
        }
        if required_frames == 0 {
            return Err(SilenceError::InvalidDuration);
        }
        Ok(Self {
            threshold,
            required_frames,
            heard_audio: false,
            ended: false,
            candidate: VecDeque::new(),
            ready: VecDeque::new(),
        })
    }

    /// Accepts post-mixed stereo samples for render-ahead classification.
    ///
    /// Samples after a confirmed silent ending are ignored.
    ///
    /// # Errors
    ///
    /// Returns [`SilenceError`] for odd input or failed host allocation.
    pub fn push(&mut self, samples: &[f32]) -> Result<(), SilenceError> {
        if samples.len() & 1 != 0 {
            return Err(SilenceError::OddSamples);
        }
        if self.ended || samples.is_empty() {
            return Ok(());
        }
        self.ready
            .try_reserve(samples.len())
            .map_err(|_| SilenceError::Allocation)?;
        self.candidate
            .try_reserve(samples.len())
            .map_err(|_| SilenceError::Allocation)?;

        for frame in samples.chunks_exact(2) {
            let quiet = frame[0].abs() <= self.threshold && frame[1].abs() <= self.threshold;
            if !self.heard_audio {
                self.ready.extend(frame.iter().copied());
                if !quiet {
                    self.heard_audio = true;
                }
                continue;
            }
            if quiet {
                self.candidate.extend(frame.iter().copied());
                let frames = u64::try_from(self.candidate.len() / 2).unwrap_or(u64::MAX);
                if frames >= self.required_frames {
                    self.candidate.clear();
                    self.ended = true;
                    break;
                }
                continue;
            }
            self.commit_candidate()?;
            self.ready.extend(frame.iter().copied());
        }
        Ok(())
    }

    /// Commits an unconfirmed quiet tail when another timeline boundary ends.
    ///
    /// # Errors
    ///
    /// Returns [`SilenceError::Allocation`] if the ready buffer cannot grow.
    pub fn finish(&mut self) -> Result<(), SilenceError> {
        if !self.ended {
            self.commit_candidate()?;
        }
        Ok(())
    }

    /// Drains at most the supplied stereo output capacity.
    ///
    /// # Errors
    ///
    /// Returns [`SilenceError::OddSamples`] for odd output storage.
    pub fn drain(&mut self, output: &mut [f32]) -> Result<usize, SilenceError> {
        if output.len() & 1 != 0 {
            return Err(SilenceError::OddSamples);
        }
        let samples = output.len().min(self.ready.len()) & !1;
        for destination in &mut output[..samples] {
            *destination = self.ready.pop_front().unwrap_or(0.0);
        }
        Ok(samples / 2)
    }

    /// Returns complete stereo frames ready for delivery.
    #[must_use]
    pub fn ready_frames(&self) -> usize {
        self.ready.len() / 2
    }

    /// Reports whether the configured trailing duration was confirmed.
    #[must_use]
    pub const fn ended(&self) -> bool {
        self.ended
    }

    /// Restores the initial detector state without changing its policy.
    pub fn reset(&mut self) {
        self.heard_audio = false;
        self.ended = false;
        self.candidate.clear();
        self.ready.clear();
    }

    fn commit_candidate(&mut self) -> Result<(), SilenceError> {
        self.ready
            .try_reserve(self.candidate.len())
            .map_err(|_| SilenceError::Allocation)?;
        self.ready.append(&mut self.candidate);
        Ok(())
    }
}

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

    /// Advances the timeline without converting samples.
    ///
    /// The returned count is limited to the declared length-plus-fade boundary.
    ///
    /// # Errors
    ///
    /// Returns [`PostMixError::PositionOverflow`] if the new position cannot be
    /// represented.
    pub fn advance(&mut self, requested: u64) -> Result<u64, PostMixError> {
        let frames = self.available_frames(requested);
        self.position = self
            .position
            .checked_add(frames)
            .ok_or(PostMixError::PositionOverflow)?;
        Ok(frames)
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
    use super::{PostMixError, PostMixer, SilenceDetector, SilenceError};

    fn drain_all(detector: &mut SilenceDetector) -> Vec<f32> {
        let mut output = vec![0.0; detector.ready_frames() * 2];
        assert_eq!(detector.drain(&mut output).unwrap(), output.len() / 2);
        output
    }

    #[test]
    fn trailing_silence_is_withheld_and_short_quiet_passages_are_exact() {
        let mut detector = SilenceDetector::new(0.01, 3).unwrap();
        detector
            .push(&[0.5, -0.5, 0.001, -0.002, 0.0, 0.0, 0.25, 0.5])
            .unwrap();
        assert!(!detector.ended());
        assert_eq!(
            drain_all(&mut detector),
            [0.5, -0.5, 0.001, -0.002, 0.0, 0.0, 0.25, 0.5]
        );

        detector
            .push(&[0.01, -0.01, 0.0, 0.0, -0.001, 0.001, 0.75, 0.75])
            .unwrap();
        assert!(detector.ended());
        assert!(drain_all(&mut detector).is_empty());
    }

    #[test]
    fn leading_silence_is_delivered_and_does_not_end_playback() {
        let mut detector = SilenceDetector::new(0.0, 2).unwrap();
        detector.push(&[0.0; 8]).unwrap();
        assert!(!detector.ended());
        assert_eq!(drain_all(&mut detector), [0.0; 8]);
        detector.push(&[1.0, 0.0, 0.0, 0.0]).unwrap();
        detector.finish().unwrap();
        assert_eq!(drain_all(&mut detector), [1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn detector_validation_reset_and_partitioning_are_stable() {
        assert_eq!(
            SilenceDetector::new(f32::NAN, 1),
            Err(SilenceError::InvalidThreshold)
        );
        assert_eq!(
            SilenceDetector::new(0.0, 0),
            Err(SilenceError::InvalidDuration)
        );
        let samples = [0.5, 0.5, 0.0, 0.0, 0.25, 0.25, 0.0, 0.0, 0.0, 0.0];
        let mut whole = SilenceDetector::new(0.0, 2).unwrap();
        let mut chunked = whole.clone();
        whole.push(&samples).unwrap();
        for chunk in samples.chunks_exact(2) {
            chunked.push(chunk).unwrap();
        }
        assert_eq!(whole, chunked);
        assert!(whole.ended());
        whole.reset();
        assert!(!whole.ended());
        assert_eq!(whole.ready_frames(), 0);
    }

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
    fn timeline_advance_stops_at_the_end_without_converting_samples() {
        let mut mixer = PostMixer::new(2.0, Some(3), 2);
        assert_eq!(mixer.advance(4), Ok(4));
        assert_eq!(mixer.position(), 4);
        assert_eq!(mixer.advance(10), Ok(1));
        assert_eq!(mixer.position(), 5);
        assert!(mixer.ended());
        assert_eq!(mixer.advance(1), Ok(0));
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
