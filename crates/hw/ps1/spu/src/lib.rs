// SPDX-License-Identifier: LGPL-2.1-or-later
//! Standalone, instance-owned PS1 sound processing unit.

#![allow(clippy::too_many_lines)]

mod reverb;

use thiserror::Error;
use upse_ps1_dma::{EndpointError, SoundDmaEndpoint};
use upse_ps1_irq::{InterruptController, InterruptSource};
use upse_spu_common::{
    AdpcmError, AdpcmFlags, AdpcmHistory, Envelope, EnvelopeConfig, EnvelopePhase,
    GaussianInterpolator, NoiseGenerator, PitchCounter, clamp_i16,
};

use reverb::Reverb;

/// Number of independently mixed hardware voices.
pub const VOICE_COUNT: usize = 24;
/// Size of sound RAM in bytes.
pub const SOUND_RAM_SIZE: usize = 512 * 1024;
/// Native integer output rate.
pub const SAMPLE_RATE: u32 = 44_100;
/// First SPU register address.
pub const SPU_BASE: u32 = 0x1f80_1c00;
/// Final SPU register address, inclusive.
pub const SPU_END: u32 = 0x1f80_1dff;

const VOICE_STRIDE: u32 = 0x10;
const VOICE_REGISTER_END: u32 = SPU_BASE + 0x180;
const MAIN_VOLUME_LEFT: u32 = 0x1f80_1d80;
const MAIN_VOLUME_RIGHT: u32 = 0x1f80_1d82;
const REVERB_VOLUME_LEFT: u32 = 0x1f80_1d84;
const REVERB_VOLUME_RIGHT: u32 = 0x1f80_1d86;
const KEY_ON_LOW: u32 = 0x1f80_1d88;
const KEY_ON_HIGH: u32 = 0x1f80_1d8a;
const KEY_OFF_LOW: u32 = 0x1f80_1d8c;
const KEY_OFF_HIGH: u32 = 0x1f80_1d8e;
const PITCH_MOD_LOW: u32 = 0x1f80_1d90;
const PITCH_MOD_HIGH: u32 = 0x1f80_1d92;
const NOISE_LOW: u32 = 0x1f80_1d94;
const NOISE_HIGH: u32 = 0x1f80_1d96;
const REVERB_ON_LOW: u32 = 0x1f80_1d98;
const REVERB_ON_HIGH: u32 = 0x1f80_1d9a;
const ENDX_LOW: u32 = 0x1f80_1d9c;
const ENDX_HIGH: u32 = 0x1f80_1d9e;
const UNKNOWN_DA0: u32 = 0x1f80_1da0;
const REVERB_BASE: u32 = 0x1f80_1da2;
const IRQ_ADDRESS: u32 = 0x1f80_1da4;
const TRANSFER_ADDRESS: u32 = 0x1f80_1da6;
const TRANSFER_FIFO: u32 = 0x1f80_1da8;
const CONTROL: u32 = 0x1f80_1daa;
const TRANSFER_CONTROL: u32 = 0x1f80_1dac;
const STATUS: u32 = 0x1f80_1dae;
const CD_VOLUME_LEFT: u32 = 0x1f80_1db0;
const CD_VOLUME_RIGHT: u32 = 0x1f80_1db2;
const EXTERNAL_VOLUME_LEFT: u32 = 0x1f80_1db4;
const EXTERNAL_VOLUME_RIGHT: u32 = 0x1f80_1db6;
const CURRENT_MAIN_VOLUME_LEFT: u32 = 0x1f80_1db8;
const CURRENT_MAIN_VOLUME_RIGHT: u32 = 0x1f80_1dba;
const UNKNOWN_DBC: u32 = 0x1f80_1dbc;
const UNKNOWN_DBE: u32 = 0x1f80_1dbe;
const REVERB_REGISTERS_START: u32 = 0x1f80_1dc0;
const CONTROL_ENABLE: u16 = 1 << 15;
const CONTROL_REVERB_ENABLE: u16 = 1 << 7;
const CONTROL_IRQ_ENABLE: u16 = 1 << 6;
const STATUS_IRQ: u16 = 1 << 6;
const RAM_MASK: usize = SOUND_RAM_SIZE - 1;

/// Typed interrupt output consumed by the standalone SPU.
pub trait InterruptSink {
    /// Latches or records an SPU interrupt request.
    fn request(&mut self, source: InterruptSource);
}

impl InterruptSink for InterruptController {
    fn request(&mut self, source: InterruptSource) {
        InterruptController::request(self, source);
    }
}

/// Invalid register, buffer, RAM, or ADPCM operation.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SpuError {
    /// Address is not a modeled 16-bit SPU register.
    #[error("invalid PS1 SPU register address {address:#010x}")]
    InvalidRegister {
        /// Physical register address.
        address: u32,
    },
    /// Sound-RAM host access left the 512 KiB allocation.
    #[error("sound RAM range {offset:#x}+{size:#x} is outside 512 KiB")]
    RamRange {
        /// Byte offset in sound RAM.
        offset: usize,
        /// Requested byte count.
        size: usize,
    },
    /// Interleaved stereo output length does not equal twice the frame count.
    #[error("output has {actual} samples, expected {expected}")]
    OutputSize {
        /// Required scalar sample count.
        expected: usize,
        /// Supplied scalar sample count.
        actual: usize,
    },
    /// One voice encountered an undefined ADPCM block header.
    #[error("voice {voice} ADPCM failure at sound RAM {address:#x}: {source}")]
    Adpcm {
        /// Voice index from zero through 23.
        voice: usize,
        /// Byte address of the block.
        address: usize,
        /// Decoder diagnostic.
        source: AdpcmError,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Voice {
    volume_left: u16,
    volume_right: u16,
    pitch: u16,
    start_address: u16,
    adsr_low: u16,
    adsr_high: u16,
    repeat_address: u16,
    current_address: usize,
    current_block_address: usize,
    envelope: Envelope,
    history: AdpcmHistory,
    pitch_counter: PitchCounter,
    decoded: [i16; 28],
    decoded_flags: AdpcmFlags,
    sample_index: usize,
    decoded_valid: bool,
    active: bool,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            volume_left: 0,
            volume_right: 0,
            pitch: 0,
            start_address: 0,
            adsr_low: 0,
            adsr_high: 0,
            repeat_address: 0,
            current_address: 0,
            current_block_address: 0,
            envelope: Envelope::new(),
            history: AdpcmHistory::default(),
            pitch_counter: PitchCounter::new(),
            decoded: [0; 28],
            decoded_flags: AdpcmFlags {
                end: false,
                repeat: false,
                loop_start: false,
            },
            sample_index: 0,
            decoded_valid: false,
            active: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct VoiceOutput {
    dry: i16,
    fetched: Option<usize>,
    loop_end: bool,
}

impl Voice {
    fn key_on(&mut self) {
        self.current_address = usize::from(self.start_address) * 8;
        self.current_block_address = self.current_address;
        self.envelope.key_on();
        self.history = AdpcmHistory::default();
        self.pitch_counter.reset();
        self.sample_index = 0;
        self.decoded_valid = false;
        self.active = true;
    }

    fn key_off(&mut self) {
        self.envelope.key_off();
    }

    fn render(
        &mut self,
        ram: &[u8],
        effective_pitch: u16,
        noise: Option<i16>,
    ) -> Result<VoiceOutput, (usize, AdpcmError)> {
        if !self.active {
            return Ok(VoiceOutput::default());
        }
        let mut fetched = None;
        let mut loop_end = false;
        let source = if let Some(noise) = noise {
            noise
        } else {
            if !self.decoded_valid {
                fetched = Some(self.decode_current(ram)?);
            }
            let phase = self.pitch_counter.phase() >> 4;
            let phase = phase.to_le_bytes()[0];
            let samples = self.interpolation_window();
            let sample = GaussianInterpolator::interpolate(samples, phase);
            let step = self.pitch_counter.advance(effective_pitch);
            for _ in 0..step.whole_samples {
                self.sample_index += 1;
                if self.sample_index == self.decoded.len() {
                    loop_end |= self.decoded_flags.end;
                    let stopped = self.finish_block();
                    if stopped {
                        return Ok(VoiceOutput {
                            dry: self.apply_envelope(sample),
                            fetched,
                            loop_end,
                        });
                    }
                    fetched = Some(self.decode_current(ram)?);
                }
            }
            sample
        };
        Ok(VoiceOutput {
            dry: self.apply_envelope(source),
            fetched,
            loop_end,
        })
    }

    fn apply_envelope(&mut self, sample: i16) -> i16 {
        let config = EnvelopeConfig::from_registers(self.adsr_low, self.adsr_high);
        self.envelope.advance(&config, 1);
        if self.envelope.phase() == EnvelopePhase::Off {
            self.active = false;
        }
        let scaled = (i32::from(sample) * i32::from(self.envelope.level())) >> 15;
        clamp_i16(scaled)
    }

    fn interpolation_window(&self) -> [i16; 4] {
        let sample = |relative: isize| {
            let index = self.sample_index.saturating_add_signed(relative);
            self.decoded[index.min(self.decoded.len() - 1)]
        };
        [sample(-1), sample(0), sample(1), sample(2)]
    }

    fn decode_current(&mut self, ram: &[u8]) -> Result<usize, (usize, AdpcmError)> {
        let address = self.current_address & RAM_MASK;
        let mut block = [0_u8; 16];
        for (index, output) in block.iter_mut().enumerate() {
            *output = ram[(address + index) & RAM_MASK];
        }
        let decoded = upse_spu_common::decode_block(&block, &mut self.history)
            .map_err(|error| (address, error))?;
        self.decoded = decoded.samples;
        self.decoded_flags = decoded.flags;
        self.current_block_address = address;
        if decoded.flags.loop_start {
            self.repeat_address =
                u16::try_from(address / 8).expect("sound RAM address fits repeat register");
        }
        self.sample_index = 0;
        self.decoded_valid = true;
        Ok(address)
    }

    fn finish_block(&mut self) -> bool {
        let flags = self.decoded_flags;
        self.decoded_valid = false;
        self.sample_index = 0;
        if flags.end {
            if flags.repeat {
                self.current_address = usize::from(self.repeat_address) * 8;
            } else {
                self.envelope = Envelope::new();
                self.active = false;
                return true;
            }
        } else {
            self.current_address = (self.current_block_address + 16) & RAM_MASK;
        }
        false
    }
}

/// Standalone 24-voice PS1 SPU.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Spu {
    ram: Vec<u8>,
    voices: [Voice; VOICE_COUNT],
    main_volume_left: u16,
    main_volume_right: u16,
    reverb_volume_left: u16,
    reverb_volume_right: u16,
    pitch_mod_mask: u32,
    noise_mask: u32,
    reverb_mask: u32,
    endx: u32,
    unknown_da0: u16,
    reverb_base: u16,
    irq_address: u16,
    transfer_address: usize,
    control: u16,
    transfer_control: u16,
    status: u16,
    cd_volume_left: u16,
    cd_volume_right: u16,
    external_volume_left: u16,
    external_volume_right: u16,
    unknown_dbc: u16,
    unknown_dbe: u16,
    reverb_registers: [u16; 32],
    reverb: Reverb,
    irq_request: bool,
    noise: NoiseGenerator,
}

impl Default for Spu {
    fn default() -> Self {
        Self::new()
    }
}

impl Spu {
    /// Constructs reset sound RAM, registers, and voices.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ram: vec![0; SOUND_RAM_SIZE],
            voices: std::array::from_fn(|_| Voice::default()),
            main_volume_left: 0,
            main_volume_right: 0,
            reverb_volume_left: 0,
            reverb_volume_right: 0,
            pitch_mod_mask: 0,
            noise_mask: 0,
            reverb_mask: 0,
            endx: 0,
            unknown_da0: 0,
            reverb_base: 0,
            irq_address: 0,
            transfer_address: 0,
            control: 0,
            transfer_control: 0,
            status: 0,
            cd_volume_left: 0,
            cd_volume_right: 0,
            external_volume_left: 0,
            external_volume_right: 0,
            unknown_dbc: 0,
            unknown_dbe: 0,
            reverb_registers: [0; 32],
            reverb: Reverb::new(),
            irq_request: false,
            noise: NoiseGenerator::default(),
        }
    }

    /// Returns immutable sound RAM for diagnostics.
    #[must_use]
    pub fn ram(&self) -> &[u8] {
        &self.ram
    }

    /// Loads an original or synthetic sound-RAM range without register side effects.
    ///
    /// # Errors
    ///
    /// Returns [`SpuError::RamRange`] if the host-supplied range leaves sound RAM.
    pub fn load_ram(&mut self, offset: usize, bytes: &[u8]) -> Result<(), SpuError> {
        let end = offset.checked_add(bytes.len()).ok_or(SpuError::RamRange {
            offset,
            size: bytes.len(),
        })?;
        let Some(destination) = self.ram.get_mut(offset..end) else {
            return Err(SpuError::RamRange {
                offset,
                size: bytes.len(),
            });
        };
        destination.copy_from_slice(bytes);
        Ok(())
    }

    /// Reads one 16-bit SPU register.
    ///
    /// # Errors
    ///
    /// Returns [`SpuError::InvalidRegister`] for odd or unmodeled addresses.
    pub fn read_register(&self, address: u32) -> Result<u16, SpuError> {
        ensure_register(address)?;
        if address < VOICE_REGISTER_END {
            let (voice, register) = decode_voice_register(address);
            let voice = &self.voices[voice];
            return match register {
                0 => Ok(voice.volume_left),
                2 => Ok(voice.volume_right),
                4 => Ok(voice.pitch),
                6 => Ok(voice.start_address),
                8 => Ok(voice.adsr_low),
                10 => Ok(voice.adsr_high),
                12 => Ok(voice.envelope.level()),
                14 => Ok(voice.repeat_address),
                _ => Err(SpuError::InvalidRegister { address }),
            };
        }
        match address {
            MAIN_VOLUME_LEFT | CURRENT_MAIN_VOLUME_LEFT => Ok(self.main_volume_left),
            MAIN_VOLUME_RIGHT | CURRENT_MAIN_VOLUME_RIGHT => Ok(self.main_volume_right),
            REVERB_VOLUME_LEFT => Ok(self.reverb_volume_left),
            REVERB_VOLUME_RIGHT => Ok(self.reverb_volume_right),
            PITCH_MOD_LOW => Ok(low_half(self.pitch_mod_mask)),
            PITCH_MOD_HIGH => Ok(high_half(self.pitch_mod_mask)),
            NOISE_LOW => Ok(low_half(self.noise_mask)),
            NOISE_HIGH => Ok(high_half(self.noise_mask)),
            REVERB_ON_LOW => Ok(low_half(self.reverb_mask)),
            REVERB_ON_HIGH => Ok(high_half(self.reverb_mask)),
            ENDX_LOW => Ok(low_half(self.endx)),
            ENDX_HIGH => Ok(high_half(self.endx)),
            UNKNOWN_DA0 => Ok(self.unknown_da0),
            REVERB_BASE => Ok(self.reverb_base),
            IRQ_ADDRESS => Ok(self.irq_address),
            TRANSFER_ADDRESS => Ok(u16::try_from(self.transfer_address / 8).unwrap_or(0)),
            CONTROL => Ok(self.control),
            TRANSFER_CONTROL => Ok(self.transfer_control),
            STATUS => Ok(self.status),
            CD_VOLUME_LEFT => Ok(self.cd_volume_left),
            CD_VOLUME_RIGHT => Ok(self.cd_volume_right),
            EXTERNAL_VOLUME_LEFT => Ok(self.external_volume_left),
            EXTERNAL_VOLUME_RIGHT => Ok(self.external_volume_right),
            UNKNOWN_DBC => Ok(self.unknown_dbc),
            UNKNOWN_DBE => Ok(self.unknown_dbe),
            REVERB_REGISTERS_START..=SPU_END => Ok(self.reverb_registers[reverb_index(address)]),
            _ => Err(SpuError::InvalidRegister { address }),
        }
    }

    /// Writes one 16-bit SPU register.
    ///
    /// # Errors
    ///
    /// Returns [`SpuError::InvalidRegister`] for odd or unmodeled addresses.
    pub fn write_register(&mut self, address: u32, value: u16) -> Result<(), SpuError> {
        ensure_register(address)?;
        if address < VOICE_REGISTER_END {
            let (voice, register) = decode_voice_register(address);
            let voice = &mut self.voices[voice];
            match register {
                0 => voice.volume_left = value,
                2 => voice.volume_right = value,
                4 => voice.pitch = value.min(0x3fff),
                6 => voice.start_address = value,
                8 => voice.adsr_low = value,
                10 => voice.adsr_high = value,
                12 => {}
                14 => voice.repeat_address = value,
                _ => return Err(SpuError::InvalidRegister { address }),
            }
            return Ok(());
        }
        match address {
            MAIN_VOLUME_LEFT => self.main_volume_left = value,
            MAIN_VOLUME_RIGHT => self.main_volume_right = value,
            REVERB_VOLUME_LEFT => self.reverb_volume_left = value,
            REVERB_VOLUME_RIGHT => self.reverb_volume_right = value,
            KEY_ON_LOW => self.key_on(u32::from(value)),
            KEY_ON_HIGH => self.key_on(u32::from(value) << 16),
            KEY_OFF_LOW => self.key_off(u32::from(value)),
            KEY_OFF_HIGH => self.key_off(u32::from(value) << 16),
            PITCH_MOD_LOW => set_low_half(&mut self.pitch_mod_mask, value),
            PITCH_MOD_HIGH => set_high_half(&mut self.pitch_mod_mask, value),
            NOISE_LOW => set_low_half(&mut self.noise_mask, value),
            NOISE_HIGH => set_high_half(&mut self.noise_mask, value),
            REVERB_ON_LOW => set_low_half(&mut self.reverb_mask, value),
            REVERB_ON_HIGH => set_high_half(&mut self.reverb_mask, value),
            ENDX_LOW => self.endx &= !u32::from(value),
            ENDX_HIGH => self.endx &= !(u32::from(value) << 16),
            UNKNOWN_DA0 => self.unknown_da0 = value,
            REVERB_BASE => {
                self.reverb_base = value;
                self.reverb.set_base(value);
            }
            IRQ_ADDRESS => self.irq_address = value,
            TRANSFER_ADDRESS => self.transfer_address = usize::from(value) * 8,
            TRANSFER_FIFO => self.write_transfer_halfword(value),
            CONTROL => {
                self.control = value;
                if value & CONTROL_IRQ_ENABLE == 0 {
                    self.status &= !STATUS_IRQ;
                    self.irq_request = false;
                }
            }
            TRANSFER_CONTROL => self.transfer_control = value,
            STATUS | CURRENT_MAIN_VOLUME_LEFT | CURRENT_MAIN_VOLUME_RIGHT => {}
            CD_VOLUME_LEFT => self.cd_volume_left = value,
            CD_VOLUME_RIGHT => self.cd_volume_right = value,
            EXTERNAL_VOLUME_LEFT => self.external_volume_left = value,
            EXTERNAL_VOLUME_RIGHT => self.external_volume_right = value,
            UNKNOWN_DBC => self.unknown_dbc = value,
            UNKNOWN_DBE => self.unknown_dbe = value,
            REVERB_REGISTERS_START..=SPU_END => {
                self.reverb_registers[reverb_index(address)] = value;
            }
            _ => return Err(SpuError::InvalidRegister { address }),
        }
        Ok(())
    }

    /// Renders interleaved signed 16-bit stereo frames at [`SAMPLE_RATE`].
    ///
    /// # Errors
    ///
    /// Returns [`SpuError::OutputSize`] for a mismatched buffer or
    /// [`SpuError::Adpcm`] for an undefined voice block header.
    pub fn render(&mut self, frames: usize, output: &mut [i16]) -> Result<(), SpuError> {
        let expected = frames.checked_mul(2).ok_or(SpuError::OutputSize {
            expected: usize::MAX,
            actual: output.len(),
        })?;
        if output.len() != expected {
            return Err(SpuError::OutputSize {
                expected,
                actual: output.len(),
            });
        }
        for frame in output.chunks_exact_mut(2) {
            let (left, right) = self.render_frame()?;
            frame[0] = left;
            frame[1] = right;
        }
        Ok(())
    }

    /// Delivers one latched SPU interrupt request to a machine sink.
    pub fn drain_irq<S: InterruptSink>(&mut self, sink: &mut S) -> bool {
        if !self.irq_request {
            return false;
        }
        self.irq_request = false;
        sink.request(InterruptSource::Spu);
        true
    }

    fn render_frame(&mut self) -> Result<(i16, i16), SpuError> {
        if self.control & CONTROL_ENABLE == 0 {
            return Ok((0, 0));
        }
        let mut left = 0_i64;
        let mut right = 0_i64;
        let mut reverb_left = 0_i64;
        let mut reverb_right = 0_i64;
        let mut previous = 0_i16;
        for index in 0..VOICE_COUNT {
            let bit = 1_u32 << index;
            let pitch = if index != 0 && self.pitch_mod_mask & bit != 0 {
                modulated_pitch(self.voices[index].pitch, previous)
            } else {
                self.voices[index].pitch
            };
            let noise = if self.noise_mask & bit != 0 {
                Some(self.noise.step())
            } else {
                None
            };
            let result = self.voices[index].render(&self.ram, pitch, noise).map_err(
                |(address, source)| SpuError::Adpcm {
                    voice: index,
                    address,
                    source,
                },
            )?;
            if let Some(address) = result.fetched {
                self.check_irq_fetch(address);
            }
            if result.loop_end {
                self.endx |= bit;
            }
            previous = result.dry;
            let voice_left = apply_volume(result.dry, self.voices[index].volume_left);
            let voice_right = apply_volume(result.dry, self.voices[index].volume_right);
            left += i64::from(voice_left);
            right += i64::from(voice_right);
            if self.reverb_mask & bit != 0 {
                reverb_left += i64::from(voice_left);
                reverb_right += i64::from(voice_right);
            }
        }
        let reverb = self.reverb.process(
            &mut self.ram,
            self.reverb_base,
            &self.reverb_registers,
            self.control & CONTROL_REVERB_ENABLE != 0,
            [
                clamp_i64_to_i16(reverb_left),
                clamp_i64_to_i16(reverb_right),
            ],
        );
        left += i64::from(apply_signed_volume(reverb[0], self.reverb_volume_left));
        right += i64::from(apply_signed_volume(reverb[1], self.reverb_volume_right));
        let left = apply_volume(clamp_i64_to_i16(left), self.main_volume_left);
        let right = apply_volume(clamp_i64_to_i16(right), self.main_volume_right);
        Ok((left, right))
    }

    fn key_on(&mut self, mask: u32) {
        for (index, voice) in self.voices.iter_mut().enumerate() {
            if mask & (1_u32 << index) != 0 {
                voice.key_on();
                self.endx &= !(1_u32 << index);
            }
        }
    }

    fn key_off(&mut self, mask: u32) {
        for (index, voice) in self.voices.iter_mut().enumerate() {
            if mask & (1_u32 << index) != 0 {
                voice.key_off();
            }
        }
    }

    fn write_transfer_halfword(&mut self, value: u16) {
        let address = self.transfer_address & RAM_MASK;
        self.check_irq_address(address);
        let bytes = value.to_le_bytes();
        self.ram[address] = bytes[0];
        self.ram[(address + 1) & RAM_MASK] = bytes[1];
        self.transfer_address = (address + 2) & RAM_MASK;
    }

    fn read_transfer_halfword(&mut self) -> u16 {
        let address = self.transfer_address & RAM_MASK;
        self.check_irq_address(address);
        let value = u16::from_le_bytes([self.ram[address], self.ram[(address + 1) & RAM_MASK]]);
        self.transfer_address = (address + 2) & RAM_MASK;
        value
    }

    fn check_irq_fetch(&mut self, address: usize) {
        self.check_irq_address(address);
        self.check_irq_address((address + 8) & RAM_MASK);
    }

    fn check_irq_address(&mut self, address: usize) {
        if self.control & CONTROL_IRQ_ENABLE != 0
            && address & !7 == (usize::from(self.irq_address) * 8) & RAM_MASK
        {
            self.status |= STATUS_IRQ;
            self.irq_request = true;
        }
    }
}

impl SoundDmaEndpoint for Spu {
    fn write_word(&mut self, value: u32) -> Result<(), EndpointError> {
        let bytes = value.to_le_bytes();
        self.write_transfer_halfword(u16::from_le_bytes([bytes[0], bytes[1]]));
        self.write_transfer_halfword(u16::from_le_bytes([bytes[2], bytes[3]]));
        Ok(())
    }

    fn read_word(&mut self) -> Result<u32, EndpointError> {
        let low = self.read_transfer_halfword();
        let high = self.read_transfer_halfword();
        Ok(u32::from(low) | (u32::from(high) << 16))
    }
}

fn ensure_register(address: u32) -> Result<(), SpuError> {
    if !(SPU_BASE..=SPU_END).contains(&address) || address & 1 != 0 {
        return Err(SpuError::InvalidRegister { address });
    }
    Ok(())
}

fn decode_voice_register(address: u32) -> (usize, u32) {
    let relative = address - SPU_BASE;
    let index = usize::try_from(relative / VOICE_STRIDE).expect("voice register index fits usize");
    (index, relative % VOICE_STRIDE)
}

fn reverb_index(address: u32) -> usize {
    usize::try_from((address - REVERB_REGISTERS_START) / 2)
        .expect("reverb register index fits usize")
}

fn low_half(value: u32) -> u16 {
    let bytes = value.to_le_bytes();
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn high_half(value: u32) -> u16 {
    let bytes = value.to_le_bytes();
    u16::from_le_bytes([bytes[2], bytes[3]])
}

fn set_low_half(target: &mut u32, value: u16) {
    *target = (*target & 0xffff_0000) | u32::from(value);
}

fn set_high_half(target: &mut u32, value: u16) {
    *target = (*target & 0x0000_ffff) | (u32::from(value) << 16);
}

fn apply_volume(sample: i16, register: u16) -> i16 {
    let direct = register & 0x7fff;
    let signed = if direct & 0x4000 != 0 {
        i32::from(direct) - 0x8000
    } else {
        i32::from(direct)
    };
    clamp_i16((i32::from(sample) * signed) >> 14)
}

fn apply_signed_volume(sample: i16, register: u16) -> i16 {
    let volume = i16::from_le_bytes(register.to_le_bytes());
    clamp_i16((i32::from(sample) * i32::from(volume)) >> 15)
}

fn modulated_pitch(base: u16, previous: i16) -> u16 {
    let factor = i64::from(i32::from(previous) + 0x8000);
    let pitch = (i64::from(base) * factor) >> 15;
    u16::try_from(pitch.clamp(0, 0x3fff)).unwrap_or(0)
}

fn clamp_i64_to_i16(value: i64) -> i16 {
    if value < i64::from(i16::MIN) {
        i16::MIN
    } else if value > i64::from(i16::MAX) {
        i16::MAX
    } else {
        i16::try_from(value).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use upse_ps1_dma::SoundDmaEndpoint;
    use upse_ps1_irq::{InterruptController, InterruptSource};

    use super::{
        CD_VOLUME_LEFT, CD_VOLUME_RIGHT, CONTROL, CONTROL_ENABLE, CONTROL_IRQ_ENABLE,
        CONTROL_REVERB_ENABLE, ENDX_LOW, EXTERNAL_VOLUME_LEFT, EXTERNAL_VOLUME_RIGHT, IRQ_ADDRESS,
        KEY_OFF_LOW, KEY_ON_LOW, MAIN_VOLUME_LEFT, MAIN_VOLUME_RIGHT, NOISE_LOW, PITCH_MOD_LOW,
        REVERB_BASE, REVERB_ON_HIGH, REVERB_ON_LOW, REVERB_REGISTERS_START, REVERB_VOLUME_LEFT,
        REVERB_VOLUME_RIGHT, SOUND_RAM_SIZE, SPU_BASE, STATUS, STATUS_IRQ, Spu, SpuError,
        TRANSFER_ADDRESS, TRANSFER_CONTROL, UNKNOWN_DA0, UNKNOWN_DBC, UNKNOWN_DBE, VOICE_COUNT,
    };

    const ROOM_REVERB: [u16; 32] = [
        0x007d, 0x005b, 0x6d80, 0x54b8, 0xbed0, 0x0000, 0x0000, 0xba80, 0x5800, 0x5300, 0x04d6,
        0x0333, 0x03f0, 0x0227, 0x0374, 0x01ef, 0x0334, 0x01b5, 0x0000, 0x0000, 0x0000, 0x0000,
        0x0000, 0x0000, 0x0000, 0x0000, 0x01b4, 0x0136, 0x00b8, 0x005c, 0x8000, 0x8000,
    ];

    fn constant_block(nibble: u8, flags: u8) -> [u8; 16] {
        let mut block = [0_u8; 16];
        block[0] = 0;
        block[1] = flags;
        for byte in &mut block[2..] {
            *byte = nibble | (nibble << 4);
        }
        block
    }

    fn configure_voice(spu: &mut Spu, index: usize, start: u16) {
        let base = SPU_BASE + u32::try_from(index).unwrap() * 0x10;
        spu.write_register(base, 0x3fff).unwrap();
        spu.write_register(base + 2, 0x3fff).unwrap();
        spu.write_register(base + 4, 0x1000).unwrap();
        spu.write_register(base + 6, start).unwrap();
        spu.write_register(base + 8, 0x00ff).unwrap();
        spu.write_register(base + 10, 0x1f00).unwrap();
        spu.write_register(base + 14, start).unwrap();
    }

    fn configured_spu() -> Spu {
        let mut spu = Spu::new();
        spu.load_ram(0, &constant_block(1, 3)).unwrap();
        configure_voice(&mut spu, 0, 0);
        spu.write_register(MAIN_VOLUME_LEFT, 0x3fff).unwrap();
        spu.write_register(MAIN_VOLUME_RIGHT, 0x3fff).unwrap();
        spu.write_register(CONTROL, CONTROL_ENABLE).unwrap();
        spu.write_register(KEY_ON_LOW, 1).unwrap();
        spu
    }

    fn configure_room_reverb(spu: &mut Spu, master_enable: bool) {
        let base = u16::try_from((SOUND_RAM_SIZE - 0x26c0) / 8).unwrap();
        spu.write_register(REVERB_BASE, base).unwrap();
        spu.write_register(REVERB_VOLUME_LEFT, 0x7fff).unwrap();
        spu.write_register(REVERB_VOLUME_RIGHT, 0x7fff).unwrap();
        spu.write_register(REVERB_ON_LOW, 1).unwrap();
        spu.write_register(TRANSFER_CONTROL, 4).unwrap();
        for (index, value) in ROOM_REVERB.into_iter().enumerate() {
            let address = REVERB_REGISTERS_START + u32::try_from(index).unwrap() * 2;
            spu.write_register(address, value).unwrap();
        }
        let control = CONTROL_ENABLE | (u16::from(master_enable) * CONTROL_REVERB_ENABLE);
        spu.write_register(CONTROL, control).unwrap();
    }

    #[test]
    fn standalone_register_script_produces_audible_integer_golden() {
        let mut spu = configured_spu();
        let mut output = [0_i16; 16];
        spu.render(8, &mut output).unwrap();
        assert_eq!(
            output,
            [
                1_790, 1_790, 3_582, 3_582, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093,
                4_093, 4_093, 4_093, 4_093
            ]
        );
    }

    #[test]
    fn render_is_chunk_independent_and_rejects_wrong_output_size() {
        let mut whole = configured_spu();
        let mut chunked = whole.clone();
        let mut expected = [0_i16; 128];
        whole.render(64, &mut expected).unwrap();
        let mut actual = [0_i16; 128];
        for chunk in actual.chunks_exact_mut(16) {
            chunked.render(8, chunk).unwrap();
        }
        assert_eq!(actual, expected);
        assert_eq!(
            whole.render(2, &mut [0; 3]),
            Err(SpuError::OutputSize {
                expected: 4,
                actual: 3
            })
        );
    }

    #[test]
    fn room_reverb_writes_its_ring_and_adds_a_delayed_wet_signal() {
        let mut dry = configured_spu();
        let mut disabled = dry.clone();
        let mut wet = dry.clone();
        configure_room_reverb(&mut disabled, false);
        configure_room_reverb(&mut wet, true);

        let mut dry_output = vec![0_i16; 16_384];
        let mut disabled_output = vec![0_i16; 16_384];
        let mut wet_output = vec![0_i16; 16_384];
        dry.render(8_192, &mut dry_output).unwrap();
        disabled.render(8_192, &mut disabled_output).unwrap();
        wet.render(8_192, &mut wet_output).unwrap();

        assert_eq!(disabled_output, dry_output);
        assert_ne!(wet_output, dry_output);
        let base = SOUND_RAM_SIZE - 0x26c0;
        assert!(disabled.ram()[base..].iter().all(|&byte| byte == 0));
        assert!(wet.ram()[base..].iter().any(|&byte| byte != 0));
    }

    #[test]
    fn key_end_loop_noise_pitch_modulation_and_clipping_are_deterministic() {
        let mut spu = Spu::new();
        spu.load_ram(0, &constant_block(7, 1)).unwrap();
        spu.load_ram(16, &constant_block(7, 3)).unwrap();
        for index in 0..VOICE_COUNT {
            configure_voice(&mut spu, index, if index == 0 { 0 } else { 2 });
        }
        spu.write_register(MAIN_VOLUME_LEFT, 0x3fff).unwrap();
        spu.write_register(MAIN_VOLUME_RIGHT, 0x3fff).unwrap();
        spu.write_register(PITCH_MOD_LOW, 0xfffe).unwrap();
        spu.write_register(NOISE_LOW, 1 << 1).unwrap();
        spu.write_register(REVERB_ON_LOW, u16::MAX).unwrap();
        spu.write_register(CONTROL, CONTROL_ENABLE).unwrap();
        spu.write_register(KEY_ON_LOW, u16::MAX).unwrap();
        spu.write_register(super::KEY_ON_HIGH, 0xff).unwrap();
        let mut output = vec![0_i16; 80];
        spu.render(40, &mut output).unwrap();
        assert!(output.iter().any(|&sample| sample >= 32_760));
        assert_ne!(spu.read_register(ENDX_LOW).unwrap() & 1, 0);
        assert_eq!(spu.read_register(ENDX_LOW).unwrap(), 0xfffd);
        assert_eq!(spu.read_register(REVERB_ON_LOW).unwrap(), u16::MAX);
        assert_eq!(spu.read_register(SPU_BASE + 12).unwrap(), 0);
        spu.write_register(KEY_OFF_LOW, 2).unwrap();
        let mut tail = [0_i16; 8];
        spu.render(4, &mut tail).unwrap();
        assert_ne!(tail, [0; 8]);
    }

    #[test]
    fn transfer_fifo_dma_and_irq_address_wrap_sound_ram() {
        let mut spu = Spu::new();
        spu.write_register(IRQ_ADDRESS, 0).unwrap();
        spu.write_register(TRANSFER_ADDRESS, u16::MAX).unwrap();
        spu.write_register(CONTROL, CONTROL_ENABLE | CONTROL_IRQ_ENABLE)
            .unwrap();
        spu.write_word(0x1122_3344).unwrap();
        assert_eq!(
            &spu.ram()[SOUND_RAM_SIZE - 8..SOUND_RAM_SIZE - 4],
            &[0x44, 0x33, 0x22, 0x11]
        );
        spu.write_register(TRANSFER_ADDRESS, u16::MAX).unwrap();
        assert_eq!(spu.read_word().unwrap(), 0x1122_3344);

        spu.write_register(TRANSFER_ADDRESS, 0).unwrap();
        spu.write_word(0xaabb_ccdd).unwrap();
        assert_ne!(spu.read_register(STATUS).unwrap() & STATUS_IRQ, 0);
        let mut irq = InterruptController::new();
        assert!(spu.drain_irq(&mut irq));
        assert!(!spu.drain_irq(&mut irq));
        assert_eq!(irq.status(), InterruptSource::Spu.bit());
    }

    #[test]
    fn register_and_ram_boundaries_are_checked() {
        let mut spu = Spu::new();
        spu.write_register(REVERB_VOLUME_LEFT, 0x1234).unwrap();
        spu.write_register(REVERB_VOLUME_RIGHT, 0xfedc).unwrap();
        spu.write_register(REVERB_ON_LOW, 0x4567).unwrap();
        spu.write_register(REVERB_ON_HIGH, 0x00ab).unwrap();
        spu.write_register(CD_VOLUME_LEFT, 0x1111).unwrap();
        spu.write_register(CD_VOLUME_RIGHT, 0x2222).unwrap();
        spu.write_register(EXTERNAL_VOLUME_LEFT, 0x3333).unwrap();
        spu.write_register(EXTERNAL_VOLUME_RIGHT, 0x4444).unwrap();
        spu.write_register(UNKNOWN_DA0, 0x5555).unwrap();
        spu.write_register(UNKNOWN_DBC, 0x6666).unwrap();
        spu.write_register(UNKNOWN_DBE, 0x7777).unwrap();
        assert_eq!(spu.read_register(REVERB_VOLUME_LEFT).unwrap(), 0x1234);
        assert_eq!(spu.read_register(REVERB_VOLUME_RIGHT).unwrap(), 0xfedc);
        assert_eq!(spu.read_register(REVERB_ON_LOW).unwrap(), 0x4567);
        assert_eq!(spu.read_register(REVERB_ON_HIGH).unwrap(), 0x00ab);
        assert_eq!(spu.read_register(CD_VOLUME_LEFT).unwrap(), 0x1111);
        assert_eq!(spu.read_register(CD_VOLUME_RIGHT).unwrap(), 0x2222);
        assert_eq!(spu.read_register(EXTERNAL_VOLUME_LEFT).unwrap(), 0x3333);
        assert_eq!(spu.read_register(EXTERNAL_VOLUME_RIGHT).unwrap(), 0x4444);
        assert_eq!(spu.read_register(UNKNOWN_DA0).unwrap(), 0x5555);
        assert_eq!(spu.read_register(UNKNOWN_DBC).unwrap(), 0x6666);
        assert_eq!(spu.read_register(UNKNOWN_DBE).unwrap(), 0x7777);
        assert!(matches!(
            spu.write_register(SPU_BASE + 1, 0),
            Err(SpuError::InvalidRegister { .. })
        ));
        assert!(matches!(
            spu.load_ram(SOUND_RAM_SIZE - 1, &[1, 2]),
            Err(SpuError::RamRange { .. })
        ));
    }
}
