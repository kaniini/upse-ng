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

use std::{fs, path::Path};

use thiserror::Error;
use upse_audio::{PostMixError, PostMixer};
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
    /// Audio callback returned [`AudioAction::Error`].
    #[error("audio callback reported failure after {frames} frames")]
    Callback {
        /// Frames delivered during the current render call.
        frames: u64,
    },
}

/// Configures limits, callback quantum, and the initial callback before open.
pub struct PlayerBuilder {
    limits: Limits,
    quantum: usize,
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
        Player::from_plan(plan, self.quantum, self.callback)
    }

    fn validate(&self) -> Result<(), PlayerError> {
        if self.quantum == 0 || self.quantum > self.limits.maximum_quantum {
            return Err(PlayerError::InvalidQuantum {
                quantum: self.quantum,
                maximum: self.limits.maximum_quantum,
            });
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
    fn from_plan(plan: LoadPlan, quantum: usize, callback: Callback) -> Result<Self, PlayerError> {
        match plan {
            #[cfg(feature = "psf1")]
            LoadPlan::Psf1(plan) => {
                let metadata = plan.metadata.clone();
                let format = AudioFormat::stereo(44_100);
                let length = metadata
                    .length
                    .map(|duration| duration.to_frames_floor(format.sample_rate()))
                    .transpose()?;
                let fade = metadata.fade.to_frames_floor(format.sample_rate())?;
                let machine = Ps1Machine::from_plan(&plan, Ps1MachineConfig::default())?;
                Ok(Self {
                    machine: Machine::Psf1(Box::new(machine)),
                    post_mix: PostMixer::new(metadata.volume, length, fade),
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
                let length = metadata
                    .length
                    .map(|duration| duration.to_frames_floor(format.sample_rate()))
                    .transpose()?;
                let fade = metadata.fade.to_frames_floor(format.sample_rate())?;
                let machine = Ps2Machine::from_plan(&plan, Ps2MachineConfig::default())?;
                Ok(Self {
                    machine: Machine::Psf2(Box::new(machine)),
                    post_mix: PostMixer::new(metadata.volume, length, fade),
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

    /// Returns frames already delivered since open/reset.
    #[must_use]
    pub const fn frames_rendered(&self) -> u64 {
        self.post_mix.position()
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
    }

    /// Delivers at most `max_frames` synchronously in bounded callback blocks.
    ///
    /// Missing `length` metadata means this method never returns
    /// [`RenderOutcome::End`] automatically. A callback stop includes the
    /// current block in its returned frame count. There is intentionally no
    /// seek method; consumers reset and render to a discard callback.
    ///
    /// # Errors
    ///
    /// Returns [`PlayerError`] for emulation/post-mix failures or
    /// [`AudioAction::Error`].
    #[cfg(any(feature = "psf1", feature = "psf2"))]
    pub fn render(&mut self, max_frames: u64) -> Result<RenderOutcome, PlayerError> {
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
            let frames_usize =
                usize::try_from(frames).map_err(|_| PostMixError::PositionOverflow)?;
            let samples = frames_usize
                .checked_mul(2)
                .ok_or(PostMixError::PositionOverflow)?;
            match &mut self.machine {
                #[cfg(feature = "psf1")]
                Machine::Psf1(machine) => {
                    machine.render(frames_usize, &mut self.integer_buffer[..samples])?;
                }
                #[cfg(feature = "psf2")]
                Machine::Psf2(machine) => {
                    machine.render(frames_usize, &mut self.integer_buffer[..samples])?;
                }
                #[cfg(not(any(feature = "psf1", feature = "psf2")))]
                Machine::Disabled => return Err(PlayerError::UnsupportedVersion),
            }
            self.post_mix.process(
                &self.integer_buffer[..samples],
                &mut self.float_buffer[..samples],
            )?;
            delivered = delivered
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

    /// Rejects rendering when no machine feature is enabled.
    ///
    /// # Errors
    ///
    /// Always returns [`PlayerError::UnsupportedVersion`].
    #[cfg(not(any(feature = "psf1", feature = "psf2")))]
    pub fn render(&mut self, _max_frames: u64) -> Result<RenderOutcome, PlayerError> {
        Err(PlayerError::UnsupportedVersion)
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    use upse_psf::{MemoryResolver, PsfBuilder, PsfVersion};

    use super::{AudioAction, Limits, PlayerBuilder, PlayerError, RenderOutcome, ResolverError};

    fn instruction_lui(rt: u32, immediate: u16) -> u32 {
        (0x0f << 26) | (rt << 16) | u32::from(immediate)
    }

    fn instruction_addiu(rt: u32, immediate: i16) -> u32 {
        (0x09 << 26) | (rt << 16) | u32::from(u16::from_ne_bytes(immediate.to_ne_bytes()))
    }

    fn instruction_sh(rt: u32, offset: u16) -> u32 {
        (0x29 << 26) | (8 << 21) | (rt << 16) | u32::from(offset)
    }

    fn fixture(tags: &[(&str, &str)]) -> Vec<u8> {
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
        let loop_address = 0x8001_0000_u32 + u32::try_from(words.len() * 4).unwrap();
        words.push(0x0800_0000 | ((loop_address >> 2) & 0x03ff_ffff));
        words.push(0);
        let mut text = vec![0_u8; words.len() * 4];
        for (index, word) in words.into_iter().enumerate() {
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
