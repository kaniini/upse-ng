// SPDX-License-Identifier: LGPL-2.1-or-later
//! High-level caller-driven PSF playback.
//!
//! In-memory use:
//!
//! ```no_run
//! use upse::{AudioAction, PlayerBuilder};
//!
//! # fn example(psf: &[u8]) -> Result<(), upse::PlayerError> {
//! let mut player = PlayerBuilder::new()
//!     .callback(|block| {
//!         consume_audio(block.samples());
//!         AudioAction::Continue
//!     })
//!     .open_memory("music.psf", psf)?;
//! player.render(4096)?;
//! # Ok(())
//! # }
//! # fn consume_audio(_: &[f32]) {}
//! ```
//!
//! Path-based use resolves `_lib*` references relative to the root file:
//!
//! ```no_run
//! use upse::{AudioAction, PlayerBuilder};
//!
//! # fn example() -> Result<(), upse::PlayerError> {
//! let mut player = PlayerBuilder::new()
//!     .callback(|_| AudioAction::Continue)
//!     .open_path("music.psf")?;
//! let format = player.audio_format();
//! assert_eq!(format.channels(), 2);
//! player.render(u64::from(format.sample_rate()))?;
//! # Ok(())
//! # }
//! ```

use std::{fs, path::Path, time::Duration};

use thiserror::Error;
pub use upse_audio::SilenceError;
use upse_audio::{PostMixError, PostMixer, SilenceDetector};
pub use upse_audio_types::{AudioAction, AudioBlock, AudioFormat, ChannelOrder, RenderOutcome};
pub use upse_psf::{
    DependencyLimits, LoadError, ParseLimits, PlaybackMetadata as Metadata, ResolvedFile, Resolver,
    ResolverError,
};
use upse_psf::{DurationError, FileResolver, LoadPlan, MemoryResolver, load_plan};

#[cfg(feature = "psf1")]
use upse_ps1_machine::{
    MachineConfig as Ps1MachineConfig, MachineError as Ps1MachineError, Ps1Machine,
};
#[cfg(feature = "psf2")]
use upse_ps2_machine::{
    MachineConfig as Ps2MachineConfig, MachineError as Ps2MachineError, Ps2Machine,
};

const DEFAULT_QUANTUM: usize = 1024;
const MAX_QUANTUM: usize = 65_536;
#[cfg(any(feature = "psf1", feature = "psf2"))]
const NANOS_PER_SECOND: u64 = 1_000_000_000;

type Callback = Box<dyn for<'a> FnMut(AudioBlock<'a>) -> AudioAction + Send>;

/// Parser, resolver, and callback-allocation bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Per-container parser bounds.
    pub parse: ParseLimits,
    /// Whole dependency-graph bounds.
    pub dependencies: DependencyLimits,
    /// Maximum accepted callback quantum.
    pub maximum_quantum: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            parse: ParseLimits::default(),
            dependencies: DependencyLimits::default(),
            maximum_quantum: MAX_QUANTUM,
        }
    }
}

/// Optional trailing-silence termination policy.
///
/// Detection begins only after an audible frame, so leading silence remains
/// observable. Quiet frames are compared after configured gain and fade
/// processing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SilenceDetection {
    /// Maximum absolute normalized amplitude considered quiet.
    pub threshold: f32,
    /// Continuous quiet duration required to end playback.
    pub duration: Duration,
}

impl SilenceDetection {
    /// Constructs a trailing-silence policy.
    #[must_use]
    pub const fn new(threshold: f32, duration: Duration) -> Self {
        Self {
            threshold,
            duration,
        }
    }
}

impl Default for SilenceDetection {
    fn default() -> Self {
        Self {
            threshold: 1.0 / 32_768.0,
            duration: Duration::from_secs(5),
        }
    }
}

/// Selects the gain applied after emulation and before delivery.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum GainPolicy {
    /// Apply the PSF `volume` tag, including its implicit unity default.
    #[default]
    Tag,
    /// Ignore the tag and apply the supplied linear amplitude coefficient.
    Override(f64),
}

/// Selects how a PSF `length` or `fade` tag controls playback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DurationPolicy {
    /// Use the tag and retain its format-defined default when absent.
    #[default]
    Tag,
    /// Use the tag when present, otherwise use the supplied duration.
    TagOr(Duration),
    /// Ignore the tag and always use the supplied duration.
    Override(Duration),
    /// Ignore the tag and disable this part of the timeline.
    ///
    /// For length this means indefinite playback. For fade this means no fade.
    Ignore,
}

/// Gain, length, and fade policy for one PSF format version.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlaybackConfig {
    /// Post-emulation gain policy.
    pub gain: GainPolicy,
    /// Playback length policy.
    pub length: DurationPolicy,
    /// Fade duration policy.
    pub fade: DurationPolicy,
}

/// High-level open, timeline, callback, or emulation failure.
#[derive(Debug, Error)]
pub enum PlayerError {
    /// Root file could not be read.
    #[error("cannot read root PSF {}: {source}", path.display())]
    Io {
        /// Requested root path.
        path: std::path::PathBuf,
        /// Host I/O diagnostic.
        source: std::io::Error,
    },
    /// PSF parsing or dependency resolution failed.
    #[error("PSF load failure: {0}")]
    Load(#[from] LoadError),
    /// Parsed format has no machine enabled in this facade build.
    #[error("PSF version is not enabled in this build")]
    UnsupportedVersion,
    /// PSF duration could not map to native frames.
    #[error("PSF timeline failure: {0}")]
    Duration(#[from] DurationError),
    /// Callback quantum is zero or exceeds its configured maximum.
    #[error("invalid callback quantum {quantum}; maximum is {maximum}")]
    InvalidQuantum {
        /// Requested frames per callback.
        quantum: usize,
        /// Configured maximum frames per callback.
        maximum: usize,
    },
    /// A configured gain override was not finite.
    #[error("playback gain override must be finite")]
    InvalidGain,
    /// A configured playback duration could not map to native frames.
    #[error("configured playback duration exceeds the native timeline")]
    PlaybackDurationOverflow,
    /// End-to-end PSF1 machine execution failed.
    #[cfg(feature = "psf1")]
    #[error("PSF1 machine failure: {0}")]
    Psf1Machine(#[from] Ps1MachineError),
    /// End-to-end PSF2 machine execution failed.
    #[cfg(feature = "psf2")]
    #[error("PSF2 machine failure: {0}")]
    Psf2Machine(#[from] Ps2MachineError),
    /// Final sample conversion failed.
    #[error("audio post-mix failure: {0}")]
    PostMix(#[from] PostMixError),
    /// Silence detection configuration or render-ahead buffering failed.
    #[error("audio silence detection failure: {0}")]
    Silence(#[from] SilenceError),
    /// Audio callback returned [`AudioAction::Error`].
    #[error("audio callback reported failure after {frames} frames")]
    Callback {
        /// Frames delivered during the current render call.
        frames: u64,
    },
}

/// Configures limits, per-format playback policy, and the initial callback.
pub struct PlayerBuilder {
    limits: Limits,
    quantum: usize,
    silence_detection: Option<SilenceDetection>,
    psf1_playback: PlaybackConfig,
    psf2_playback: PlaybackConfig,
    callback: Callback,
}

impl Default for PlayerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PlayerBuilder {
    /// Constructs defaults with a synchronous discard callback.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limits: Limits::default(),
            quantum: DEFAULT_QUANTUM,
            silence_detection: None,
            psf1_playback: PlaybackConfig::default(),
            psf2_playback: PlaybackConfig::default(),
            callback: Box::new(|_| AudioAction::Continue),
        }
    }

    /// Replaces all parser, dependency, and allocation limits.
    #[must_use]
    pub const fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Selects the maximum frames delivered in one callback invocation.
    #[must_use]
    pub const fn callback_quantum(mut self, frames: usize) -> Self {
        self.quantum = frames;
        self
    }

    /// Enables render-ahead trailing-silence termination.
    #[must_use]
    pub const fn silence_detection(mut self, detection: SilenceDetection) -> Self {
        self.silence_detection = Some(detection);
        self
    }

    /// Sets gain and timeline handling for PSF1 files.
    #[must_use]
    pub const fn psf1_playback(mut self, config: PlaybackConfig) -> Self {
        self.psf1_playback = config;
        self
    }

    /// Sets gain and timeline handling for PSF2 files.
    #[must_use]
    pub const fn psf2_playback(mut self, config: PlaybackConfig) -> Self {
        self.psf2_playback = config;
        self
    }

    /// Installs the synchronous callback used after opening.
    #[must_use]
    pub fn callback<F>(mut self, callback: F) -> Self
    where
        F: for<'a> FnMut(AudioBlock<'a>) -> AudioAction + Send + 'static,
    {
        self.callback = Box::new(callback);
        self
    }

    /// Opens a root file and resolves libraries relative to its canonical path.
    ///
    /// # Errors
    ///
    /// Returns [`PlayerError`] for host I/O, parsing, dependency, timeline, or
    /// machine construction failures.
    pub fn open_path(self, path: impl AsRef<Path>) -> Result<Player, PlayerError> {
        self.validate()?;
        let requested = path.as_ref();
        let canonical = requested.canonicalize().map_err(|source| PlayerError::Io {
            path: requested.to_path_buf(),
            source,
        })?;
        let bytes = fs::read(&canonical).map_err(|source| PlayerError::Io {
            path: canonical.clone(),
            source,
        })?;
        let origin = canonical.to_string_lossy().into_owned();
        self.open_with_resolver(origin, &bytes, &mut FileResolver)
    }

    /// Opens root bytes with no external dependencies available.
    ///
    /// # Errors
    ///
    /// Returns [`PlayerError`] for parsing, a referenced dependency, timeline,
    /// or machine construction failure.
    pub fn open_memory(
        self,
        origin: impl Into<String>,
        bytes: &[u8],
    ) -> Result<Player, PlayerError> {
        self.open_with_resolver(origin, bytes, &mut MemoryResolver::new())
    }

    /// Opens root bytes through a caller-owned dependency resolver.
    ///
    /// # Errors
    ///
    /// Returns [`PlayerError`] for invalid configuration, parsing, dependency,
    /// timeline, or machine construction failure.
    pub fn open_with_resolver<R: Resolver>(
        self,
        origin: impl Into<String>,
        bytes: &[u8],
        resolver: &mut R,
    ) -> Result<Player, PlayerError> {
        self.validate()?;
        let plan = load_plan(
            origin,
            bytes,
            resolver,
            self.limits.parse,
            self.limits.dependencies,
        )?;
        Player::from_plan(
            plan,
            self.quantum,
            self.silence_detection,
            self.psf1_playback,
            self.psf2_playback,
            self.callback,
        )
    }

    fn validate(&self) -> Result<(), PlayerError> {
        if self.quantum == 0 || self.quantum > self.limits.maximum_quantum {
            return Err(PlayerError::InvalidQuantum {
                quantum: self.quantum,
                maximum: self.limits.maximum_quantum,
            });
        }
        for config in [self.psf1_playback, self.psf2_playback] {
            if let GainPolicy::Override(gain) = config.gain
                && !gain.is_finite()
            {
                return Err(PlayerError::InvalidGain);
            }
        }
        Ok(())
    }
}

/// Opened, resettable, caller-driven PSF player.
pub struct Player {
    machine: Machine,
    metadata: Metadata,
    format: AudioFormat,
    post_mix: PostMixer,
    silence_detector: Option<SilenceDetector>,
    delivered_frames: u64,
    callback: Callback,
    #[cfg(any(feature = "psf1", feature = "psf2"))]
    quantum: usize,
    #[cfg(any(feature = "psf1", feature = "psf2"))]
    integer_buffer: Vec<i16>,
    #[cfg(any(feature = "psf1", feature = "psf2"))]
    float_buffer: Vec<f32>,
}

enum Machine {
    #[cfg(feature = "psf1")]
    Psf1(Box<Ps1Machine>),
    #[cfg(feature = "psf2")]
    Psf2(Box<Ps2Machine>),
    #[cfg(not(any(feature = "psf1", feature = "psf2")))]
    #[allow(dead_code)]
    Disabled,
}

impl Player {
    #[cfg(any(feature = "psf1", feature = "psf2"))]
    fn from_plan(
        plan: LoadPlan,
        quantum: usize,
        silence_detection: Option<SilenceDetection>,
        psf1_playback: PlaybackConfig,
        psf2_playback: PlaybackConfig,
        callback: Callback,
    ) -> Result<Self, PlayerError> {
        #[cfg(not(feature = "psf1"))]
        let _ = psf1_playback;
        #[cfg(not(feature = "psf2"))]
        let _ = psf2_playback;
        match plan {
            #[cfg(feature = "psf1")]
            LoadPlan::Psf1(plan) => {
                let metadata = plan.metadata.clone();
                let format = AudioFormat::stereo(44_100);
                let (gain, length, fade) = resolve_playback(&metadata, format, psf1_playback)?;
                let machine = Ps1Machine::from_plan(&plan, Ps1MachineConfig::default())?;
                Ok(Self {
                    machine: Machine::Psf1(Box::new(machine)),
                    post_mix: PostMixer::new(gain, length, fade),
                    silence_detector: make_silence_detector(silence_detection, format)?,
                    delivered_frames: 0,
                    metadata,
                    format,
                    callback,
                    quantum,
                    integer_buffer: vec![0; quantum * 2],
                    float_buffer: vec![0.0; quantum * 2],
                })
            }
            #[cfg(feature = "psf2")]
            LoadPlan::Psf2(plan) => {
                let metadata = plan.metadata.clone();
                let format = AudioFormat::stereo(48_000);
                let (gain, length, fade) = resolve_playback(&metadata, format, psf2_playback)?;
                let machine = Ps2Machine::from_plan(&plan, Ps2MachineConfig::default())?;
                Ok(Self {
                    machine: Machine::Psf2(Box::new(machine)),
                    post_mix: PostMixer::new(gain, length, fade),
                    silence_detector: make_silence_detector(silence_detection, format)?,
                    delivered_frames: 0,
                    metadata,
                    format,
                    callback,
                    quantum,
                    integer_buffer: vec![0; quantum * 2],
                    float_buffer: vec![0.0; quantum * 2],
                })
            }
            #[allow(unreachable_patterns)]
            _ => Err(PlayerError::UnsupportedVersion),
        }
    }

    #[cfg(not(any(feature = "psf1", feature = "psf2")))]
    fn from_plan(
        _plan: LoadPlan,
        _quantum: usize,
        _silence_detection: Option<SilenceDetection>,
        _psf1_playback: PlaybackConfig,
        _psf2_playback: PlaybackConfig,
        _callback: Callback,
    ) -> Result<Self, PlayerError> {
        Err(PlayerError::UnsupportedVersion)
    }

    /// Returns parsed descriptive and exact timeline metadata.
    #[must_use]
    pub const fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Returns native sample rate, stereo channel count, and channel order.
    #[must_use]
    pub const fn audio_format(&self) -> AudioFormat {
        self.format
    }

    /// Returns the resolved gain applied to delivered samples.
    #[must_use]
    pub const fn effective_gain(&self) -> f64 {
        self.post_mix.volume()
    }

    /// Returns the resolved pre-fade length, or no automatic ending.
    #[must_use]
    pub const fn effective_length_frames(&self) -> Option<u64> {
        self.post_mix.length_frames()
    }

    /// Returns the resolved fade duration in native frames.
    #[must_use]
    pub const fn effective_fade_frames(&self) -> u64 {
        self.post_mix.fade_frames()
    }

    /// Returns timeline frames rendered or advanced since open/reset.
    #[must_use]
    pub const fn frames_rendered(&self) -> u64 {
        self.delivered_frames
    }

    /// Replaces the callback without changing emulation or timeline position.
    pub fn set_callback<F>(&mut self, callback: F)
    where
        F: for<'a> FnMut(AudioBlock<'a>) -> AudioAction + Send + 'static,
    {
        self.callback = Box::new(callback);
    }

    /// Restores initial machine and timeline state.
    pub fn reset(&mut self) {
        match &mut self.machine {
            #[cfg(feature = "psf1")]
            Machine::Psf1(machine) => machine.reset(),
            #[cfg(feature = "psf2")]
            Machine::Psf2(machine) => machine.reset(),
            #[cfg(not(any(feature = "psf1", feature = "psf2")))]
            Machine::Disabled => {}
        }
        self.post_mix.reset();
        if let Some(detector) = &mut self.silence_detector {
            detector.reset();
        }
        self.delivered_frames = 0;
    }

    /// Delivers at most `max_frames` synchronously in bounded callback blocks.
    ///
    /// A length policy which resolves to no duration means this method never
    /// returns [`RenderOutcome::End`] from the timeline. A callback stop
    /// includes the current block in its returned frame count. There is
    /// intentionally no seek method; consumers reset and advance or render to
    /// a discard callback.
    ///
    /// # Errors
    ///
    /// Returns [`PlayerError`] for emulation/post-mix failures or
    /// [`AudioAction::Error`].
    #[cfg(any(feature = "psf1", feature = "psf2"))]
    pub fn render(&mut self, max_frames: u64) -> Result<RenderOutcome, PlayerError> {
        if self.silence_detector.is_some() {
            self.render_detected(max_frames)
        } else {
            self.render_direct(max_frames)
        }
    }

    /// Advances emulation without converting or delivering audio.
    ///
    /// All machine and sound state is retained exactly. Trailing-silence
    /// detection restarts at the new position because discarded samples are
    /// intentionally not passed through the post-mix detector.
    ///
    /// # Errors
    ///
    /// Returns [`PlayerError`] for emulation, timeline, or clock failure.
    #[cfg(any(feature = "psf1", feature = "psf2"))]
    pub fn advance(&mut self, max_frames: u64) -> Result<RenderOutcome, PlayerError> {
        if let Some(detector) = &mut self.silence_detector {
            detector.reset();
        }
        let frames = self.post_mix.available_frames(max_frames);
        let mut remaining = frames;
        while remaining != 0 {
            let maximum = u64::try_from(usize::MAX).unwrap_or(u64::MAX);
            let chunk = usize::try_from(remaining.min(maximum))
                .map_err(|_| PostMixError::PositionOverflow)?;
            match &mut self.machine {
                #[cfg(feature = "psf1")]
                Machine::Psf1(machine) => machine.advance(chunk)?,
                #[cfg(feature = "psf2")]
                Machine::Psf2(machine) => machine.advance(chunk)?,
                #[cfg(not(any(feature = "psf1", feature = "psf2")))]
                Machine::Disabled => return Err(PlayerError::UnsupportedVersion),
            }
            remaining -= u64::try_from(chunk).map_err(|_| PostMixError::PositionOverflow)?;
        }
        let advanced = self.post_mix.advance(frames)?;
        debug_assert_eq!(advanced, frames);
        self.delivered_frames = self.post_mix.position();
        if self.post_mix.ended() {
            Ok(RenderOutcome::End { frames })
        } else {
            Ok(RenderOutcome::Complete { frames })
        }
    }

    #[cfg(any(feature = "psf1", feature = "psf2"))]
    fn render_direct(&mut self, max_frames: u64) -> Result<RenderOutcome, PlayerError> {
        let mut delivered = 0_u64;
        while delivered < max_frames {
            if self.post_mix.ended() {
                return Ok(RenderOutcome::End { frames: delivered });
            }
            let requested = max_frames - delivered;
            let quantum = u64::try_from(self.quantum).unwrap_or(u64::MAX);
            let frames = self.post_mix.available_frames(requested.min(quantum));
            if frames == 0 {
                return Ok(RenderOutcome::End { frames: delivered });
            }
            let (frames_usize, samples) = self.generate(frames)?;
            delivered = delivered
                .checked_add(frames)
                .ok_or(PostMixError::PositionOverflow)?;
            self.delivered_frames = self
                .delivered_frames
                .checked_add(frames)
                .ok_or(PostMixError::PositionOverflow)?;
            let block = AudioBlock::new(&self.float_buffer[..samples], frames_usize)
                .map_err(|_| PostMixError::OutputSize)?;
            match (self.callback)(block) {
                AudioAction::Continue => {}
                AudioAction::Stop => return Ok(RenderOutcome::Stopped { frames: delivered }),
                AudioAction::Error => return Err(PlayerError::Callback { frames: delivered }),
            }
        }
        if self.post_mix.ended() {
            Ok(RenderOutcome::End { frames: delivered })
        } else {
            Ok(RenderOutcome::Complete { frames: delivered })
        }
    }

    #[cfg(any(feature = "psf1", feature = "psf2"))]
    fn render_detected(&mut self, max_frames: u64) -> Result<RenderOutcome, PlayerError> {
        let mut delivered = 0_u64;
        while delivered < max_frames {
            if self.post_mix.ended() {
                self.silence_detector
                    .as_mut()
                    .expect("selected detected rendering")
                    .finish()?;
            }
            let ready = self
                .silence_detector
                .as_ref()
                .expect("selected detected rendering")
                .ready_frames();
            if ready != 0 {
                let remaining = usize::try_from(max_frames - delivered).unwrap_or(usize::MAX);
                let frames = ready.min(remaining).min(self.quantum);
                let samples = frames
                    .checked_mul(2)
                    .ok_or(PostMixError::PositionOverflow)?;
                let drained = self
                    .silence_detector
                    .as_mut()
                    .expect("selected detected rendering")
                    .drain(&mut self.float_buffer[..samples])?;
                debug_assert_eq!(drained, frames);
                let frames = u64::try_from(frames).map_err(|_| PostMixError::PositionOverflow)?;
                delivered = delivered
                    .checked_add(frames)
                    .ok_or(PostMixError::PositionOverflow)?;
                self.delivered_frames = self
                    .delivered_frames
                    .checked_add(frames)
                    .ok_or(PostMixError::PositionOverflow)?;
                let block = AudioBlock::new(&self.float_buffer[..samples], drained)
                    .map_err(|_| PostMixError::OutputSize)?;
                match (self.callback)(block) {
                    AudioAction::Continue => {}
                    AudioAction::Stop => {
                        return Ok(RenderOutcome::Stopped { frames: delivered });
                    }
                    AudioAction::Error => return Err(PlayerError::Callback { frames: delivered }),
                }
                continue;
            }

            let detector_ended = self
                .silence_detector
                .as_ref()
                .expect("selected detected rendering")
                .ended();
            if detector_ended || self.post_mix.ended() {
                return Ok(RenderOutcome::End { frames: delivered });
            }
            let quantum = u64::try_from(self.quantum).unwrap_or(u64::MAX);
            let frames = self.post_mix.available_frames(quantum);
            if frames == 0 {
                continue;
            }
            let (_, samples) = self.generate(frames)?;
            self.silence_detector
                .as_mut()
                .expect("selected detected rendering")
                .push(&self.float_buffer[..samples])?;
        }

        if self.post_mix.ended() {
            self.silence_detector
                .as_mut()
                .expect("selected detected rendering")
                .finish()?;
        }
        let detector = self
            .silence_detector
            .as_ref()
            .expect("selected detected rendering");
        if detector.ready_frames() == 0 && (detector.ended() || self.post_mix.ended()) {
            Ok(RenderOutcome::End { frames: delivered })
        } else {
            Ok(RenderOutcome::Complete { frames: delivered })
        }
    }

    #[cfg(any(feature = "psf1", feature = "psf2"))]
    fn generate(&mut self, frames: u64) -> Result<(usize, usize), PlayerError> {
        let frames = usize::try_from(frames).map_err(|_| PostMixError::PositionOverflow)?;
        let samples = frames
            .checked_mul(2)
            .ok_or(PostMixError::PositionOverflow)?;
        match &mut self.machine {
            #[cfg(feature = "psf1")]
            Machine::Psf1(machine) => {
                machine.render(frames, &mut self.integer_buffer[..samples])?;
            }
            #[cfg(feature = "psf2")]
            Machine::Psf2(machine) => {
                machine.render(frames, &mut self.integer_buffer[..samples])?;
            }
            #[cfg(not(any(feature = "psf1", feature = "psf2")))]
            Machine::Disabled => return Err(PlayerError::UnsupportedVersion),
        }
        self.post_mix.process(
            &self.integer_buffer[..samples],
            &mut self.float_buffer[..samples],
        )?;
        Ok((frames, samples))
    }

    /// Rejects rendering when no machine feature is enabled.
    ///
    /// # Errors
    ///
    /// Always returns [`PlayerError::UnsupportedVersion`].
    #[cfg(not(any(feature = "psf1", feature = "psf2")))]
    pub fn render(&mut self, _max_frames: u64) -> Result<RenderOutcome, PlayerError> {
        Err(PlayerError::UnsupportedVersion)
    }

    /// Rejects advancement when no machine feature is enabled.
    ///
    /// # Errors
    ///
    /// Always returns [`PlayerError::UnsupportedVersion`].
    #[cfg(not(any(feature = "psf1", feature = "psf2")))]
    pub fn advance(&mut self, _max_frames: u64) -> Result<RenderOutcome, PlayerError> {
        Err(PlayerError::UnsupportedVersion)
    }
}

#[cfg(any(feature = "psf1", feature = "psf2"))]
fn resolve_playback(
    metadata: &Metadata,
    format: AudioFormat,
    config: PlaybackConfig,
) -> Result<(f64, Option<u64>, u64), PlayerError> {
    let gain = match config.gain {
        GainPolicy::Tag => metadata.volume,
        GainPolicy::Override(gain) => gain,
    };
    let sample_rate = format.sample_rate();
    let length = resolve_duration(config.length, metadata.length, sample_rate)?;
    let tagged_fade = metadata
        .raw_tags()
        .iter()
        .any(|tag| tag.key().eq_ignore_ascii_case("fade"))
        .then_some(metadata.fade);
    let fade = resolve_duration(config.fade, tagged_fade, sample_rate)?.unwrap_or(0);
    Ok((gain, length, fade))
}

#[cfg(any(feature = "psf1", feature = "psf2"))]
fn resolve_duration(
    policy: DurationPolicy,
    tagged: Option<upse_psf::Duration>,
    sample_rate: u32,
) -> Result<Option<u64>, PlayerError> {
    match policy {
        DurationPolicy::Tag => tagged
            .map(|duration| duration.to_frames_floor(sample_rate))
            .transpose()
            .map_err(PlayerError::from),
        DurationPolicy::TagOr(fallback) => tagged.map_or_else(
            || configured_duration_frames(fallback, sample_rate).map(Some),
            |duration| {
                duration
                    .to_frames_floor(sample_rate)
                    .map(Some)
                    .map_err(PlayerError::from)
            },
        ),
        DurationPolicy::Override(duration) => {
            configured_duration_frames(duration, sample_rate).map(Some)
        }
        DurationPolicy::Ignore => Ok(None),
    }
}

#[cfg(any(feature = "psf1", feature = "psf2"))]
fn configured_duration_frames(duration: Duration, sample_rate: u32) -> Result<u64, PlayerError> {
    let whole = duration
        .as_secs()
        .checked_mul(u64::from(sample_rate))
        .ok_or(PlayerError::PlaybackDurationOverflow)?;
    let fractional = u64::from(duration.subsec_nanos()) * u64::from(sample_rate) / NANOS_PER_SECOND;
    whole
        .checked_add(fractional)
        .ok_or(PlayerError::PlaybackDurationOverflow)
}

#[cfg(any(feature = "psf1", feature = "psf2"))]
fn make_silence_detector(
    detection: Option<SilenceDetection>,
    format: AudioFormat,
) -> Result<Option<SilenceDetector>, PlayerError> {
    detection
        .map(|detection| {
            let seconds = detection
                .duration
                .as_secs()
                .checked_mul(u64::from(format.sample_rate()))
                .ok_or(SilenceError::InvalidDuration)?;
            let fractional = u64::from(detection.duration.subsec_nanos())
                .checked_mul(u64::from(format.sample_rate()))
                .ok_or(SilenceError::InvalidDuration)?
                .div_ceil(NANOS_PER_SECOND);
            let frames = seconds
                .checked_add(fractional)
                .ok_or(SilenceError::InvalidDuration)?;
            SilenceDetector::new(detection.threshold, frames)
        })
        .transpose()
        .map_err(PlayerError::from)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use upse_psf::{MemoryResolver, PsfBuilder, PsfVersion};

    use super::{
        AudioAction, DurationPolicy, GainPolicy, Limits, PlaybackConfig, PlayerBuilder,
        PlayerError, RenderOutcome, ResolverError, SilenceDetection, SilenceError,
    };

    fn instruction_lui(rt: u32, immediate: u16) -> u32 {
        (0x0f << 26) | (rt << 16) | u32::from(immediate)
    }

    fn instruction_addiu(rt: u32, immediate: i16) -> u32 {
        instruction_addiu_from(rt, 0, immediate)
    }

    fn instruction_addiu_from(rt: u32, rs: u32, immediate: i16) -> u32 {
        (0x09 << 26)
            | (rs << 21)
            | (rt << 16)
            | u32::from(u16::from_ne_bytes(immediate.to_ne_bytes()))
    }

    fn instruction_bne(rs: u32, rt: u32, immediate: i16) -> u32 {
        (0x05 << 26)
            | (rs << 21)
            | (rt << 16)
            | u32::from(u16::from_ne_bytes(immediate.to_ne_bytes()))
    }

    fn instruction_sh(rt: u32, offset: u16) -> u32 {
        (0x29 << 26) | (8 << 21) | (rt << 16) | u32::from(offset)
    }

    fn noise_setup() -> Vec<u32> {
        let mut words = vec![instruction_lui(8, 0x1f80)];
        for (value, offset) in [
            (0x3fff_i16, 0x1c00_u16),
            (0x3fff, 0x1c02),
            (0x1000, 0x1c04),
            (0x00ff, 0x1c08),
            (0x1f00, 0x1c0a),
            (0x3fff, 0x1d80),
            (0x3fff, 0x1d82),
            (1, 0x1d94),
            (-32_768, 0x1daa),
            (1, 0x1d88),
        ] {
            words.push(instruction_addiu(9, value));
            words.push(instruction_sh(9, offset));
        }
        words
    }

    fn fixture(tags: &[(&str, &str)]) -> Vec<u8> {
        let mut words = noise_setup();
        let loop_address = 0x8001_0000_u32 + u32::try_from(words.len() * 4).unwrap();
        words.push(0x0800_0000 | ((loop_address >> 2) & 0x03ff_ffff));
        words.push(0);
        build_fixture(&words, tags)
    }

    fn ending_fixture() -> Vec<u8> {
        let mut words = noise_setup();
        words.push(instruction_addiu(10, 10_000));
        words.push(instruction_addiu_from(10, 10, -1));
        words.push(instruction_bne(10, 0, -2));
        words.push(0);
        words.push(instruction_addiu(9, 1));
        words.push(instruction_sh(9, 0x1d8c));
        let loop_address = 0x8001_0000_u32 + u32::try_from(words.len() * 4).unwrap();
        words.push(0x0800_0000 | ((loop_address >> 2) & 0x03ff_ffff));
        words.push(0);
        build_fixture(&words, &[])
    }

    fn build_fixture(words: &[u32], tags: &[(&str, &str)]) -> Vec<u8> {
        let mut text = vec![0_u8; words.len() * 4];
        for (index, word) in words.iter().copied().enumerate() {
            text[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        let mut exe = vec![0_u8; 0x800 + text.len()];
        exe[..8].copy_from_slice(b"PS-X EXE");
        exe[0x10..0x14].copy_from_slice(&0x8001_0000_u32.to_le_bytes());
        exe[0x18..0x1c].copy_from_slice(&0x8001_0000_u32.to_le_bytes());
        exe[0x1c..0x20].copy_from_slice(&u32::try_from(text.len()).unwrap().to_le_bytes());
        exe[0x30..0x34].copy_from_slice(&0x801f_ff00_u32.to_le_bytes());
        exe[0x4c..0x51].copy_from_slice(b"Japan");
        exe[0x800..].copy_from_slice(&text);
        let mut builder = PsfBuilder::new(PsfVersion::Psf1).program(exe);
        for (key, value) in tags {
            builder = builder.tag(*key, *value);
        }
        builder.build()
    }

    #[test]
    fn configurable_silence_detection_discards_only_the_confirmed_tail() {
        let bytes = ending_fixture();
        let whole_samples = Arc::new(Mutex::new(Vec::new()));
        let mut whole = collecting_builder(64, Arc::clone(&whole_samples))
            .silence_detection(SilenceDetection::new(0.0, Duration::from_millis(1)))
            .open_memory("ending.psf", &bytes)
            .unwrap();
        let outcome = whole.render(100_000).unwrap();
        let RenderOutcome::End { frames } = outcome else {
            panic!("silence detector did not end playback: {outcome:?}");
        };
        let samples = whole_samples.lock().unwrap();
        assert_eq!(samples.len(), usize::try_from(frames).unwrap() * 2);
        assert_eq!(whole.frames_rendered(), frames);
        assert!(samples.iter().any(|sample| *sample != 0.0));
        assert!(
            samples[samples.len() - 2..]
                .iter()
                .any(|sample| *sample != 0.0)
        );
        let expected = samples.clone();
        drop(samples);

        let chunked_samples = Arc::new(Mutex::new(Vec::new()));
        let mut chunked = collecting_builder(64, Arc::clone(&chunked_samples))
            .silence_detection(SilenceDetection::new(0.0, Duration::from_millis(1)))
            .open_memory("ending.psf", &bytes)
            .unwrap();
        loop {
            if matches!(chunked.render(17).unwrap(), RenderOutcome::End { .. }) {
                break;
            }
        }
        assert_eq!(*chunked_samples.lock().unwrap(), expected);
        assert_eq!(chunked.frames_rendered(), frames);

        whole.reset();
        assert_eq!(whole.frames_rendered(), 0);
    }

    #[test]
    fn invalid_silence_detection_is_rejected_during_open() {
        let bytes = fixture(&[]);
        for detection in [
            SilenceDetection::new(f32::NAN, Duration::from_secs(1)),
            SilenceDetection::new(0.0, Duration::ZERO),
        ] {
            assert!(matches!(
                PlayerBuilder::new()
                    .silence_detection(detection)
                    .open_memory("invalid.psf", &bytes),
                Err(PlayerError::Silence(
                    SilenceError::InvalidThreshold | SilenceError::InvalidDuration
                ))
            ));
        }
    }

    fn collecting_builder(quantum: usize, samples: Arc<Mutex<Vec<f32>>>) -> PlayerBuilder {
        PlayerBuilder::new()
            .callback_quantum(quantum)
            .callback(move |block| {
                samples.lock().unwrap().extend_from_slice(block.samples());
                AudioAction::Continue
            })
    }

    #[test]
    fn render_and_callback_partitions_have_identical_timeline_and_samples() {
        let bytes = fixture(&[
            ("title", "Synthetic noise"),
            ("length", "0.01"),
            ("fade", "0.005"),
        ]);
        let first_samples = Arc::new(Mutex::new(Vec::new()));
        let mut first = collecting_builder(17, Arc::clone(&first_samples))
            .open_memory("fixture.psf", &bytes)
            .unwrap();
        assert_eq!(first.metadata().title.as_deref(), Some("Synthetic noise"));
        assert_eq!(
            first.render(2_000).unwrap(),
            RenderOutcome::End { frames: 661 }
        );

        let second_samples = Arc::new(Mutex::new(Vec::new()));
        let mut second = collecting_builder(64, Arc::clone(&second_samples))
            .open_memory("fixture.psf", &bytes)
            .unwrap();
        let mut frames = 0;
        loop {
            let outcome = second.render(73).unwrap();
            frames += outcome.frames();
            if matches!(outcome, RenderOutcome::End { .. }) {
                break;
            }
        }
        assert_eq!(frames, 661);
        assert_eq!(
            *first_samples.lock().unwrap(),
            *second_samples.lock().unwrap()
        );
    }

    #[test]
    fn advance_bypasses_callbacks_and_preserves_following_samples() {
        const TARGET: u64 = 97;
        const WINDOW: u64 = 131;

        let bytes = fixture(&[]);
        let reference_samples = Arc::new(Mutex::new(Vec::new()));
        let mut reference = collecting_builder(31, Arc::clone(&reference_samples))
            .open_memory("reference.psf", &bytes)
            .unwrap();
        assert_eq!(
            reference.render(TARGET + WINDOW).unwrap(),
            RenderOutcome::Complete {
                frames: TARGET + WINDOW
            }
        );

        let advanced_samples = Arc::new(Mutex::new(Vec::new()));
        let mut advanced = collecting_builder(31, Arc::clone(&advanced_samples))
            .open_memory("advanced.psf", &bytes)
            .unwrap();
        assert_eq!(
            advanced.advance(TARGET).unwrap(),
            RenderOutcome::Complete { frames: TARGET }
        );
        assert!(advanced_samples.lock().unwrap().is_empty());
        assert_eq!(advanced.frames_rendered(), TARGET);
        assert_eq!(
            advanced.render(WINDOW).unwrap(),
            RenderOutcome::Complete { frames: WINDOW }
        );

        let reference = reference_samples.lock().unwrap();
        assert_eq!(
            &advanced_samples.lock().unwrap()[..],
            &reference[usize::try_from(TARGET * 2).unwrap()..]
        );
    }

    #[test]
    fn callback_stop_error_reset_and_missing_length_are_explicit() {
        let bytes = fixture(&[]);
        let mut stopped = PlayerBuilder::new()
            .callback_quantum(8)
            .callback(|_| AudioAction::Stop)
            .open_memory("fixture.psf", &bytes)
            .unwrap();
        assert_eq!(
            stopped.render(100).unwrap(),
            RenderOutcome::Stopped { frames: 8 }
        );
        assert_eq!(stopped.frames_rendered(), 8);
        stopped.reset();
        assert_eq!(stopped.frames_rendered(), 0);

        let mut failed = PlayerBuilder::new()
            .callback_quantum(7)
            .callback(|_| AudioAction::Error)
            .open_memory("fixture.psf", &bytes)
            .unwrap();
        assert!(matches!(
            failed.render(100),
            Err(PlayerError::Callback { frames: 7 })
        ));

        let mut endless = PlayerBuilder::new()
            .open_memory("fixture.psf", &bytes)
            .unwrap();
        assert_eq!(
            endless.render(100).unwrap(),
            RenderOutcome::Complete { frames: 100 }
        );
    }

    #[test]
    fn negative_and_over_unity_volumes_are_not_clipped() {
        let positive_bytes = fixture(&[("volume", "2")]);
        let negative_bytes = fixture(&[("volume", "-2")]);
        let positive = Arc::new(Mutex::new(Vec::new()));
        let negative = Arc::new(Mutex::new(Vec::new()));
        let mut positive_player = collecting_builder(32, Arc::clone(&positive))
            .open_memory("positive.psf", &positive_bytes)
            .unwrap();
        let mut negative_player = collecting_builder(32, Arc::clone(&negative))
            .open_memory("negative.psf", &negative_bytes)
            .unwrap();
        positive_player.render(32).unwrap();
        negative_player.render(32).unwrap();
        let positive = positive.lock().unwrap();
        let negative = negative.lock().unwrap();
        assert_eq!(positive.len(), negative.len());
        for (&left, &right) in positive.iter().zip(negative.iter()) {
            assert_eq!(left, -right);
        }
        assert!(positive.iter().any(|sample| sample.abs() > 1.0));
    }

    #[test]
    fn per_format_playback_policy_controls_gain_length_and_fade() {
        let tagged_bytes = fixture(&[("volume", "2"), ("length", "0.01"), ("fade", "0.005")]);
        let tagged_samples = Arc::new(Mutex::new(Vec::new()));
        let mut tagged = collecting_builder(32, Arc::clone(&tagged_samples))
            .psf1_playback(PlaybackConfig {
                gain: GainPolicy::Tag,
                length: DurationPolicy::TagOr(Duration::from_millis(100)),
                fade: DurationPolicy::TagOr(Duration::from_millis(100)),
            })
            .open_memory("tagged.psf", &tagged_bytes)
            .unwrap();
        assert_eq!(tagged.effective_gain(), 2.0);
        assert_eq!(tagged.effective_length_frames(), Some(441));
        assert_eq!(tagged.effective_fade_frames(), 220);
        assert_eq!(
            tagged.render(2_000).unwrap(),
            RenderOutcome::End { frames: 661 }
        );

        let overridden_samples = Arc::new(Mutex::new(Vec::new()));
        let mut overridden = collecting_builder(32, Arc::clone(&overridden_samples))
            .psf1_playback(PlaybackConfig {
                gain: GainPolicy::Override(1.0),
                length: DurationPolicy::Override(Duration::from_millis(4)),
                fade: DurationPolicy::Override(Duration::from_millis(1)),
            })
            .open_memory("overridden.psf", &tagged_bytes)
            .unwrap();
        assert_eq!(overridden.effective_gain(), 1.0);
        assert_eq!(overridden.effective_length_frames(), Some(176));
        assert_eq!(overridden.effective_fade_frames(), 44);
        assert_eq!(
            overridden.render(2_000).unwrap(),
            RenderOutcome::End { frames: 220 }
        );
        let tagged_samples = tagged_samples.lock().unwrap();
        let overridden_samples = overridden_samples.lock().unwrap();
        assert_eq!(overridden_samples.len(), 440);
        assert!(overridden_samples.iter().any(|sample| *sample != 0.0));
        for (&tagged, &overridden) in tagged_samples
            .iter()
            .zip(overridden_samples.iter())
            .take(352)
        {
            assert!((tagged - overridden * 2.0).abs() <= f32::EPSILON);
        }
        drop(tagged_samples);
        drop(overridden_samples);

        let untagged_bytes = fixture(&[]);
        let mut fallback = PlayerBuilder::new()
            .psf1_playback(PlaybackConfig {
                gain: GainPolicy::Tag,
                length: DurationPolicy::TagOr(Duration::from_millis(10)),
                fade: DurationPolicy::TagOr(Duration::from_millis(5)),
            })
            .open_memory("fallback.psf", &untagged_bytes)
            .unwrap();
        assert_eq!(
            fallback.render(2_000).unwrap(),
            RenderOutcome::End { frames: 661 }
        );

        let mut endless = PlayerBuilder::new()
            .psf1_playback(PlaybackConfig {
                gain: GainPolicy::Tag,
                length: DurationPolicy::Ignore,
                fade: DurationPolicy::Ignore,
            })
            .open_memory("endless.psf", &tagged_bytes)
            .unwrap();
        assert_eq!(endless.effective_length_frames(), None);
        assert_eq!(endless.effective_fade_frames(), 0);
        assert_eq!(
            endless.render(2_000).unwrap(),
            RenderOutcome::Complete { frames: 2_000 }
        );
    }

    #[test]
    fn invalid_playback_overrides_are_rejected() {
        let bytes = fixture(&[]);
        assert!(matches!(
            PlayerBuilder::new()
                .psf2_playback(PlaybackConfig {
                    gain: GainPolicy::Override(f64::NAN),
                    ..PlaybackConfig::default()
                })
                .open_memory("invalid.psf", &bytes),
            Err(PlayerError::InvalidGain)
        ));
        assert!(matches!(
            PlayerBuilder::new()
                .psf1_playback(PlaybackConfig {
                    length: DurationPolicy::Override(Duration::new(u64::MAX, 999_999_999)),
                    ..PlaybackConfig::default()
                })
                .open_memory("invalid.psf", &bytes),
            Err(PlayerError::PlaybackDurationOverflow)
        ));
    }

    #[test]
    fn path_memory_and_custom_resolver_construction_are_covered() {
        let bytes = fixture(&[]);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("upse-ng-{unique}.psf"));
        std::fs::write(&path, &bytes).unwrap();
        let player = PlayerBuilder::new().open_path(&path).unwrap();
        assert_eq!(player.audio_format().sample_rate(), 44_100);
        std::fs::remove_file(path).unwrap();

        let library = fixture(&[]);
        let root = fixture(&[("_lib", "library.psflib")]);
        let mut resolver = MemoryResolver::new();
        resolver.insert("library.psflib", library).unwrap();
        PlayerBuilder::new()
            .open_with_resolver("root.psf", &root, &mut resolver)
            .unwrap();

        assert!(matches!(
            PlayerBuilder::new()
                .limits(Limits {
                    maximum_quantum: 4,
                    ..Limits::default()
                })
                .callback_quantum(5)
                .open_memory("fixture.psf", &bytes),
            Err(PlayerError::InvalidQuantum { .. })
        ));
        let _ = ResolverError::new("custom resolver errors remain typed");
    }
}
