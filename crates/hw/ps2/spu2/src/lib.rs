// SPDX-License-Identifier: LGPL-2.1-or-later
//! Standalone, instance-owned PS2 sound processing unit.

#![allow(clippy::too_many_lines)]

mod effects;

use thiserror::Error;
use upse_iop_dma::{EndpointError, SoundDmaChannel, Spu2DmaEndpoint, Spu2MmioEndpoint};
use upse_iop_irq::{InterruptSink, InterruptSource};
use upse_spu_common::{
    AdpcmError, AdpcmFlags, AdpcmHistory, DecodedBlock, Envelope, EnvelopeConfig, EnvelopePhase,
    GaussianInterpolator, NoiseGenerator, PitchCounter, clamp_i16,
};

use effects::Effects;

/// Number of hardware cores.
pub const CORE_COUNT: usize = 2;
/// Number of independently mixed voices in each core.
pub const VOICES_PER_CORE: usize = 24;
/// Shared SPU2 sound RAM size in bytes.
pub const SOUND_RAM_SIZE: usize = 2 * 1024 * 1024;
/// Native signed-integer output rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// First physical SPU2 register address.
pub const SPU2_BASE: u32 = 0x1f90_0000;
/// Final physical SPU2 register address, inclusive.
pub const SPU2_END: u32 = 0x1f90_07ff;

/// Mix input A into the dry left bus.
pub const MMIX_INPUT_A_DRY_LEFT: u16 = 1 << 0;
/// Mix input A into the dry right bus.
pub const MMIX_INPUT_A_DRY_RIGHT: u16 = 1 << 1;
/// Mix input B into the dry left bus.
pub const MMIX_INPUT_B_DRY_LEFT: u16 = 1 << 2;
/// Mix input B into the dry right bus.
pub const MMIX_INPUT_B_DRY_RIGHT: u16 = 1 << 3;
/// Mix voices into the dry left bus.
pub const MMIX_VOICE_DRY_LEFT: u16 = 1 << 4;
/// Mix voices into the dry right bus.
pub const MMIX_VOICE_DRY_RIGHT: u16 = 1 << 5;
/// Mix input A into the effect left bus.
pub const MMIX_INPUT_A_EFFECT_LEFT: u16 = 1 << 6;
/// Mix input A into the effect right bus.
pub const MMIX_INPUT_A_EFFECT_RIGHT: u16 = 1 << 7;
/// Mix input B into the effect left bus.
pub const MMIX_INPUT_B_EFFECT_LEFT: u16 = 1 << 8;
/// Mix input B into the effect right bus.
pub const MMIX_INPUT_B_EFFECT_RIGHT: u16 = 1 << 9;
/// Mix voices into the effect left bus.
pub const MMIX_VOICE_EFFECT_LEFT: u16 = 1 << 10;
/// Mix voices into the effect right bus.
pub const MMIX_VOICE_EFFECT_RIGHT: u16 = 1 << 11;

/// Enables one SPU2 core.
pub const CORE_ATTR_ENABLE: u16 = 1 << 15;
/// Enables audible output from one SPU2 core.
pub const CORE_ATTR_UNMUTE: u16 = 1 << 14;
/// Enables writes by one core's effect processor.
pub const CORE_ATTR_EFFECT_ENABLE: u16 = 1 << 7;
/// Enables sound-RAM address interrupts from one core.
pub const CORE_ATTR_IRQ_ENABLE: u16 = 1 << 6;
/// Enables the external core input path.
pub const CORE_ATTR_EXTERNAL_ENABLE: u16 = 1;

const REGISTER_HALFWORDS: usize = 0x400;
const CORE_STRIDE: u32 = 0x400;
const VOICE_PARAMETER_END: u32 = 0x180;
const VOICE_STRIDE: u32 = 0x10;
const VOICE_ADDRESS_BASE: u32 = 0x1c0;
const VOICE_ADDRESS_STRIDE: u32 = 0x0c;
const PRIMARY_BASE: u32 = 0x760;
const PRIMARY_STRIDE: u32 = 0x28;
const IRQ_INFO: u32 = 0x7c2;
const RAM_MASK: usize = SOUND_RAM_SIZE - 1;

const PMON_HIGH: u32 = 0x180;
const PMON_LOW: u32 = 0x182;
const NON_HIGH: u32 = 0x184;
const NON_LOW: u32 = 0x186;
const VMIXL_HIGH: u32 = 0x188;
const VMIXL_LOW: u32 = 0x18a;
const VMIXEL_HIGH: u32 = 0x18c;
const VMIXEL_LOW: u32 = 0x18e;
const VMIXR_HIGH: u32 = 0x190;
const VMIXR_LOW: u32 = 0x192;
const VMIXER_HIGH: u32 = 0x194;
const VMIXER_LOW: u32 = 0x196;
const MMIX: u32 = 0x198;
const CORE_ATTR: u32 = 0x19a;
const IRQA_HIGH: u32 = 0x19c;
const IRQA_LOW: u32 = 0x19e;
const KEY_ON_HIGH: u32 = 0x1a0;
const KEY_ON_LOW: u32 = 0x1a2;
const KEY_OFF_HIGH: u32 = 0x1a4;
const KEY_OFF_LOW: u32 = 0x1a6;
const TSA_HIGH: u32 = 0x1a8;
const TSA_LOW: u32 = 0x1aa;
const TRANSFER_DATA: u32 = 0x1ac;
const EFFECT_START_HIGH: u32 = 0x2e0;
const EFFECT_START_LOW: u32 = 0x2e2;
const EFFECT_ADDRESS_FIRST: u32 = 0x2e4;
const EFFECT_ADDRESS_LAST: u32 = 0x33a;
const EFFECT_END_HIGH: u32 = 0x33c;
const EFFECT_END_LOW: u32 = 0x33e;
const ENDX_HIGH: u32 = 0x340;
const ENDX_LOW: u32 = 0x342;
const STATUS: u32 = 0x344;

/// One signed-integer stereo sample.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StereoFrame {
    /// Left sample.
    pub left: i16,
    /// Right sample.
    pub right: i16,
}

impl StereoFrame {
    /// Constructs a stereo frame.
    #[must_use]
    pub const fn new(left: i16, right: i16) -> Self {
        Self { left, right }
    }

    const fn samples(self) -> [i16; 2] {
        [self.left, self.right]
    }
}

/// The two serial/input streams visible to one core for one frame.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CoreInput {
    /// Input A. Core 1 also receives core 0's output through this path.
    pub input_a: StereoFrame,
    /// Input B.
    pub input_b: StereoFrame,
}

/// Per-core and final output from one time-driven SPU2 step.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RenderedFrame {
    /// Output produced independently by cores 0 and 1.
    pub cores: [StereoFrame; CORE_COUNT],
    /// Hardware final output, taken after core 0 is routed through core 1.
    pub final_output: StereoFrame,
}

/// Invalid register, buffer, RAM, or ADPCM operation.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum Spu2Error {
    /// Address is not an aligned SPU2 register.
    #[error("invalid SPU2 register address {address:#010x}")]
    InvalidRegister {
        /// Physical register address.
        address: u32,
    },
    /// Host access left the shared 2 MiB sound RAM.
    #[error("SPU2 sound-RAM range {offset:#x}..+{size:#x} is out of bounds")]
    RamRange {
        /// First byte offset.
        offset: usize,
        /// Byte count.
        size: usize,
    },
    /// Interleaved stereo output length does not match the requested frames.
    #[error("SPU2 output has {actual} samples, expected {expected}")]
    OutputSize {
        /// Required sample count.
        expected: usize,
        /// Supplied sample count.
        actual: usize,
    },
    /// Per-frame input length does not match the requested frames.
    #[error("SPU2 input has {actual} frames, expected {expected}")]
    InputSize {
        /// Required frame count.
        expected: usize,
        /// Supplied frame count.
        actual: usize,
    },
    /// A voice encountered an undefined ADPCM header.
    #[error("SPU2 core {core} voice {voice} ADPCM failure at {address:#08x}: {source}")]
    Adpcm {
        /// Sound core.
        core: usize,
        /// Voice within the core.
        voice: usize,
        /// Sound-RAM block address.
        address: usize,
        /// Decoder failure.
        source: AdpcmError,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreparedBlock {
    address: usize,
    decoded: DecodedBlock,
    history_before: AdpcmHistory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Voice {
    volume_left: u16,
    volume_right: u16,
    pitch: u16,
    adsr_low: u16,
    adsr_high: u16,
    start_address: u32,
    repeat_address: u32,
    current_address: usize,
    current_block_address: usize,
    decoded: [i16; 28],
    decoded_flags: AdpcmFlags,
    decoded_valid: bool,
    prepared: Option<PreparedBlock>,
    sample_index: usize,
    previous_sample: i16,
    history: AdpcmHistory,
    envelope: Envelope,
    pitch_counter: PitchCounter,
    active: bool,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            volume_left: 0,
            volume_right: 0,
            pitch: 0,
            adsr_low: 0,
            adsr_high: 0,
            start_address: 0,
            repeat_address: 0,
            current_address: 0,
            current_block_address: 0,
            decoded: [0; 28],
            decoded_flags: AdpcmFlags::default(),
            decoded_valid: false,
            prepared: None,
            sample_index: 0,
            previous_sample: 0,
            history: AdpcmHistory::default(),
            envelope: Envelope::new(),
            pitch_counter: PitchCounter::new(),
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
        self.current_address = normalize_ram_address(self.start_address);
        self.current_block_address = self.current_address;
        self.decoded_valid = false;
        self.prepared = None;
        self.sample_index = 0;
        self.previous_sample = 0;
        self.history = AdpcmHistory::default();
        self.envelope.key_on();
        self.pitch_counter.reset();
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
            if self.sample_index + 2 >= self.decoded.len()
                && let Some(address) = self.prepare_next(ram)?
            {
                fetched = Some(address);
            }
            let phase = self.pitch_counter.phase() >> 4;
            let sample = GaussianInterpolator::interpolate(
                self.interpolation_window(),
                phase.to_le_bytes()[0],
            );
            let step = self.pitch_counter.advance(effective_pitch);
            for _ in 0..step.whole_samples {
                self.sample_index += 1;
                if self.sample_index == self.decoded.len() {
                    loop_end |= self.decoded_flags.end;
                    if self.finish_block() {
                        return Ok(VoiceOutput {
                            dry: self.apply_envelope(sample),
                            fetched,
                            loop_end,
                        });
                    }
                    if !self.decoded_valid {
                        fetched = Some(self.decode_current(ram)?);
                    }
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
        clamp_i16((i32::from(sample) * i32::from(self.envelope.level())) >> 15)
    }

    fn interpolation_window(&self) -> [i16; 4] {
        let sample = |relative: isize| {
            let index = isize::try_from(self.sample_index).unwrap_or(0) + relative;
            if index < 0 {
                return self.previous_sample;
            }
            let index = usize::try_from(index).unwrap_or(0);
            if let Some(sample) = self.decoded.get(index) {
                return *sample;
            }
            self.prepared
                .as_ref()
                .and_then(|prepared| prepared.decoded.samples.get(index - self.decoded.len()))
                .copied()
                .unwrap_or(self.decoded[self.decoded.len() - 1])
        };
        [sample(-1), sample(0), sample(1), sample(2)]
    }

    fn decode_current(&mut self, ram: &[u8]) -> Result<usize, (usize, AdpcmError)> {
        let address = self.current_address & RAM_MASK;
        let decoded = decode_block_at(ram, address, &mut self.history)?;
        self.install_block(address, decoded);
        Ok(address)
    }

    fn prepare_next(&mut self, ram: &[u8]) -> Result<Option<usize>, (usize, AdpcmError)> {
        if self.prepared.is_some() {
            return Ok(None);
        }
        let Some(address) = self.next_block_address() else {
            return Ok(None);
        };
        let history_before = self.history;
        let decoded = decode_block_at(ram, address, &mut self.history)?;
        self.prepared = Some(PreparedBlock {
            address,
            decoded,
            history_before,
        });
        Ok(Some(address))
    }

    fn next_block_address(&self) -> Option<usize> {
        if self.decoded_flags.end {
            self.decoded_flags
                .repeat
                .then_some(normalize_ram_address(self.repeat_address))
        } else {
            Some((self.current_block_address + 16) & RAM_MASK)
        }
    }

    fn install_block(&mut self, address: usize, decoded: DecodedBlock) {
        self.decoded = decoded.samples;
        self.decoded_flags = decoded.flags;
        self.current_block_address = address;
        self.current_address = address;
        if decoded.flags.loop_start {
            self.repeat_address = u32::try_from(address).unwrap_or(0);
        }
        self.sample_index = 0;
        self.decoded_valid = true;
    }

    fn finish_block(&mut self) -> bool {
        let flags = self.decoded_flags;
        self.previous_sample = self.decoded[self.decoded.len() - 1];
        self.decoded_valid = false;
        self.sample_index = 0;
        let next_address = if flags.end {
            if flags.repeat {
                normalize_ram_address(self.repeat_address)
            } else {
                self.envelope = Envelope::new();
                self.active = false;
                self.prepared = None;
                return true;
            }
        } else {
            (self.current_block_address + 16) & RAM_MASK
        };
        self.current_address = next_address;
        if let Some(prepared) = self.prepared.take() {
            if prepared.address == next_address {
                self.install_block(prepared.address, prepared.decoded);
            } else {
                self.history = prepared.history_before;
            }
        }
        false
    }
}

fn decode_block_at(
    ram: &[u8],
    address: usize,
    history: &mut AdpcmHistory,
) -> Result<DecodedBlock, (usize, AdpcmError)> {
    let mut block = [0_u8; 16];
    for (index, output) in block.iter_mut().enumerate() {
        *output = ram[(address + index) & RAM_MASK];
    }
    upse_spu_common::decode_block(&block, history).map_err(|error| (address, error))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Core {
    voices: [Voice; VOICES_PER_CORE],
    pitch_mod_mask: u32,
    noise_mask: u32,
    voice_dry_left: u32,
    voice_dry_right: u32,
    voice_effect_left: u32,
    voice_effect_right: u32,
    mix: u16,
    attributes: u16,
    endx: u32,
    irq_address: u32,
    transfer_address: u32,
    effect_start: u32,
    effect_end: u32,
    effect_addresses: [u32; 22],
    effect_coefficients: [u16; 10],
    master_volume: [u16; 2],
    effect_volume: [u16; 2],
    input_a_volume: [u16; 2],
    input_b_volume: [u16; 2],
    effects: Effects,
    noise: NoiseGenerator,
    irq_request: bool,
    status: u16,
}

impl Default for Core {
    fn default() -> Self {
        Self {
            voices: std::array::from_fn(|_| Voice::default()),
            pitch_mod_mask: 0,
            noise_mask: 0,
            voice_dry_left: 0,
            voice_dry_right: 0,
            voice_effect_left: 0,
            voice_effect_right: 0,
            mix: 0,
            attributes: 0,
            endx: 0,
            irq_address: 0,
            transfer_address: 0,
            effect_start: 0,
            effect_end: u32::try_from(SOUND_RAM_SIZE - 1).unwrap_or(0),
            effect_addresses: [0; 22],
            effect_coefficients: [0; 10],
            master_volume: [0; 2],
            effect_volume: [0; 2],
            input_a_volume: [0; 2],
            input_b_volume: [0; 2],
            effects: Effects::new(),
            noise: NoiseGenerator::default(),
            irq_request: false,
            status: 0,
        }
    }
}

impl Core {
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

    fn render(
        &mut self,
        core_index: usize,
        ram: &mut [u8],
        input: CoreInput,
    ) -> Result<StereoFrame, Spu2Error> {
        if self.attributes & CORE_ATTR_ENABLE == 0 {
            return Ok(StereoFrame::default());
        }
        let mut dry = [0_i64; 2];
        let mut wet = [0_i64; 2];
        let mut previous = 0_i16;
        for index in 0..VOICES_PER_CORE {
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
            let result =
                self.voices[index]
                    .render(ram, pitch, noise)
                    .map_err(|(address, source)| Spu2Error::Adpcm {
                        core: core_index,
                        voice: index,
                        address,
                        source,
                    })?;
            if let Some(address) = result.fetched {
                self.check_irq_fetch(address);
            }
            if result.loop_end {
                self.endx |= bit;
            }
            previous = result.dry;
            let voice_left = apply_volume(result.dry, self.voices[index].volume_left);
            let voice_right = apply_volume(result.dry, self.voices[index].volume_right);
            if self.voice_dry_left & bit != 0 && self.mix & MMIX_VOICE_DRY_LEFT != 0 {
                dry[0] += i64::from(voice_left);
            }
            if self.voice_dry_right & bit != 0 && self.mix & MMIX_VOICE_DRY_RIGHT != 0 {
                dry[1] += i64::from(voice_right);
            }
            if self.voice_effect_left & bit != 0 && self.mix & MMIX_VOICE_EFFECT_LEFT != 0 {
                wet[0] += i64::from(voice_left);
            }
            if self.voice_effect_right & bit != 0 && self.mix & MMIX_VOICE_EFFECT_RIGHT != 0 {
                wet[1] += i64::from(voice_right);
            }
        }

        if self.attributes & CORE_ATTR_EXTERNAL_ENABLE != 0 {
            self.mix_input(input.input_a, self.input_a_volume, true, &mut dry, &mut wet);
        }
        self.mix_input(
            input.input_b,
            self.input_b_volume,
            false,
            &mut dry,
            &mut wet,
        );
        let effect = if self.attributes & CORE_ATTR_EFFECT_ENABLE != 0 {
            self.effects.process(
                ram,
                self.effect_start,
                self.effect_end,
                &self.effect_addresses,
                &self.effect_coefficients,
                true,
                [clamp_i64_to_i16(wet[0]), clamp_i64_to_i16(wet[1])],
            )
        } else {
            [0; 2]
        };
        dry[0] += i64::from(apply_signed_volume(effect[0], self.effect_volume[0]));
        dry[1] += i64::from(apply_signed_volume(effect[1], self.effect_volume[1]));

        if self.attributes & CORE_ATTR_UNMUTE == 0 {
            return Ok(StereoFrame::default());
        }
        Ok(StereoFrame::new(
            apply_signed_volume(clamp_i64_to_i16(dry[0]), self.master_volume[0]),
            apply_signed_volume(clamp_i64_to_i16(dry[1]), self.master_volume[1]),
        ))
    }

    fn mix_input(
        &self,
        input: StereoFrame,
        volume: [u16; 2],
        input_a: bool,
        dry: &mut [i64; 2],
        wet: &mut [i64; 2],
    ) {
        let input = input.samples();
        let scaled = [
            apply_signed_volume(input[0], volume[0]),
            apply_signed_volume(input[1], volume[1]),
        ];
        let dry_bits = if input_a {
            [MMIX_INPUT_A_DRY_LEFT, MMIX_INPUT_A_DRY_RIGHT]
        } else {
            [MMIX_INPUT_B_DRY_LEFT, MMIX_INPUT_B_DRY_RIGHT]
        };
        let wet_bits = if input_a {
            [MMIX_INPUT_A_EFFECT_LEFT, MMIX_INPUT_A_EFFECT_RIGHT]
        } else {
            [MMIX_INPUT_B_EFFECT_LEFT, MMIX_INPUT_B_EFFECT_RIGHT]
        };
        for channel in 0..2 {
            if self.mix & dry_bits[channel] != 0 {
                dry[channel] += i64::from(scaled[channel]);
            }
            if self.mix & wet_bits[channel] != 0 {
                wet[channel] += i64::from(scaled[channel]);
            }
        }
    }

    fn write_transfer_halfword(&mut self, ram: &mut [u8], value: u16) {
        let address = normalize_ram_address(self.transfer_address);
        self.check_irq_address(address);
        let bytes = value.to_le_bytes();
        ram[address] = bytes[0];
        ram[(address + 1) & RAM_MASK] = bytes[1];
        self.transfer_address = u32::try_from((address + 2) & RAM_MASK).unwrap_or(0);
    }

    fn read_transfer_halfword(&mut self, ram: &[u8]) -> u16 {
        let address = normalize_ram_address(self.transfer_address);
        self.check_irq_address(address);
        let value = u16::from_le_bytes([ram[address], ram[(address + 1) & RAM_MASK]]);
        self.transfer_address = u32::try_from((address + 2) & RAM_MASK).unwrap_or(0);
        value
    }

    fn check_irq_fetch(&mut self, address: usize) {
        self.check_irq_address(address);
        self.check_irq_address((address + 8) & RAM_MASK);
    }

    fn check_irq_address(&mut self, address: usize) {
        if self.attributes & CORE_ATTR_IRQ_ENABLE != 0
            && address & !0xf == normalize_ram_address(self.irq_address) & !0xf
        {
            self.irq_request = true;
        }
    }
}

/// Standalone two-core SPU2.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Spu2 {
    ram: Vec<u8>,
    cores: [Core; CORE_COUNT],
    registers: Vec<u16>,
    irq_info: u16,
}

impl Default for Spu2 {
    fn default() -> Self {
        Self::new()
    }
}

impl Spu2 {
    /// Constructs reset sound RAM, registers, cores, and voices.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ram: vec![0; SOUND_RAM_SIZE],
            cores: std::array::from_fn(|_| Core::default()),
            registers: vec![0; REGISTER_HALFWORDS],
            irq_info: 0,
        }
    }

    /// Returns immutable shared sound RAM for diagnostics.
    #[must_use]
    pub fn ram(&self) -> &[u8] {
        &self.ram
    }

    /// Loads a synthetic or locally supplied sound-RAM range without MMIO side effects.
    ///
    /// # Errors
    ///
    /// Returns [`Spu2Error::RamRange`] when the range leaves sound RAM.
    pub fn load_ram(&mut self, offset: usize, bytes: &[u8]) -> Result<(), Spu2Error> {
        let end = offset.checked_add(bytes.len()).ok_or(Spu2Error::RamRange {
            offset,
            size: bytes.len(),
        })?;
        let Some(destination) = self.ram.get_mut(offset..end) else {
            return Err(Spu2Error::RamRange {
                offset,
                size: bytes.len(),
            });
        };
        destination.copy_from_slice(bytes);
        Ok(())
    }

    /// Reads one aligned 16-bit register.
    ///
    /// # Errors
    ///
    /// Returns [`Spu2Error::InvalidRegister`] outside the SPU2 window.
    pub fn read_register(&self, address: u32) -> Result<u16, Spu2Error> {
        let (offset, index) = decode_register(address)?;
        if offset == IRQ_INFO {
            return Ok(self.irq_info);
        }
        if let Some((core_index, local)) = decode_core_register(offset) {
            let core = &self.cores[core_index];
            if local < VOICE_PARAMETER_END {
                let voice = usize::try_from(local / VOICE_STRIDE).unwrap_or(0);
                return match local % VOICE_STRIDE {
                    0 => Ok(core.voices[voice].volume_left),
                    2 => Ok(core.voices[voice].volume_right),
                    4 => Ok(core.voices[voice].pitch),
                    6 => Ok(core.voices[voice].adsr_low),
                    8 => Ok(core.voices[voice].adsr_high),
                    0x0a => Ok(core.voices[voice].envelope.level()),
                    0x0c => Ok(current_volume(core.voices[voice].volume_left)),
                    0x0e => Ok(current_volume(core.voices[voice].volume_right)),
                    _ => Ok(self.registers[index]),
                };
            }
            if let Some((voice, register)) = decode_voice_address(local) {
                let voice = &core.voices[voice];
                return Ok(match register {
                    0 | 2 => address_half(voice.start_address, register == 0),
                    4 | 6 => address_half(voice.repeat_address, register == 4),
                    8 | 10 => address_half(
                        u32::try_from(voice.current_address).unwrap_or(0),
                        register == 8,
                    ),
                    _ => self.registers[index],
                });
            }
            return Ok(match local {
                ENDX_HIGH => high_switch_half(core.endx),
                ENDX_LOW => low_switch_half(core.endx),
                STATUS => core.status,
                _ => self.registers[index],
            });
        }
        Ok(self.registers[index])
    }

    /// Writes one aligned 16-bit register and applies its hardware side effects.
    ///
    /// Unknown aligned locations in the physical SPU2 window remain readable,
    /// allowing module drivers to probe revision-specific state safely.
    ///
    /// # Errors
    ///
    /// Returns [`Spu2Error::InvalidRegister`] outside the SPU2 window.
    pub fn write_register(&mut self, address: u32, value: u16) -> Result<(), Spu2Error> {
        let (offset, index) = decode_register(address)?;
        self.registers[index] = value;
        if offset == IRQ_INFO {
            self.irq_info &= value;
            return Ok(());
        }
        if let Some((core_index, local)) = decode_core_register(offset) {
            if local < VOICE_PARAMETER_END {
                let voice = usize::try_from(local / VOICE_STRIDE).unwrap_or(0);
                let voice = &mut self.cores[core_index].voices[voice];
                match local % VOICE_STRIDE {
                    0 => voice.volume_left = value,
                    2 => voice.volume_right = value,
                    4 => voice.pitch = value.min(0x3fff),
                    6 => voice.adsr_low = value,
                    8 => voice.adsr_high = value,
                    _ => {}
                }
                return Ok(());
            }
            if let Some((voice_index, register)) = decode_voice_address(local) {
                let pair_base = offset - register;
                let pair_index = register_index(pair_base);
                let high = self.registers[pair_index];
                let low = self.registers[pair_index + 1];
                let voice = &mut self.cores[core_index].voices[voice_index];
                match register {
                    0 | 2 => voice.start_address = decode_sound_address(high, low),
                    4 | 6 => voice.repeat_address = decode_sound_address(high, low),
                    _ => {}
                }
                return Ok(());
            }
            self.write_core_register(core_index, local, offset, value);
            return Ok(());
        }
        if let Some((core_index, local)) = decode_primary_register(offset) {
            self.write_primary_register(core_index, local, value);
        }
        Ok(())
    }

    /// Renders one native-rate frame, retaining both core outputs for tests and tooling.
    ///
    /// # Errors
    ///
    /// Returns an ADPCM diagnostic when an active voice has an invalid block header.
    pub fn render_frame(
        &mut self,
        mut inputs: [CoreInput; CORE_COUNT],
    ) -> Result<RenderedFrame, Spu2Error> {
        let core0 = self.cores[0].render(0, &mut self.ram, inputs[0])?;
        inputs[1].input_a.left =
            clamp_i16(i32::from(inputs[1].input_a.left).saturating_add(i32::from(core0.left)));
        inputs[1].input_a.right =
            clamp_i16(i32::from(inputs[1].input_a.right).saturating_add(i32::from(core0.right)));
        let core1 = self.cores[1].render(1, &mut self.ram, inputs[1])?;
        self.refresh_irq_info();
        Ok(RenderedFrame {
            cores: [core0, core1],
            final_output: core1,
        })
    }

    /// Renders silent-input, interleaved signed 16-bit stereo frames at 48 kHz.
    ///
    /// # Errors
    ///
    /// Returns an output-size or ADPCM diagnostic.
    pub fn render(&mut self, frames: usize, output: &mut [i16]) -> Result<(), Spu2Error> {
        let expected = frames.checked_mul(2).ok_or(Spu2Error::OutputSize {
            expected: usize::MAX,
            actual: output.len(),
        })?;
        if output.len() != expected {
            return Err(Spu2Error::OutputSize {
                expected,
                actual: output.len(),
            });
        }
        for frame in output.chunks_exact_mut(2) {
            let rendered = self
                .render_frame([CoreInput::default(); CORE_COUNT])?
                .final_output;
            frame[0] = rendered.left;
            frame[1] = rendered.right;
        }
        Ok(())
    }

    /// Renders time-aligned input frames to interleaved signed 16-bit stereo output.
    ///
    /// # Errors
    ///
    /// Returns an input-size, output-size, or ADPCM diagnostic.
    pub fn render_with_inputs(
        &mut self,
        inputs: &[[CoreInput; CORE_COUNT]],
        output: &mut [i16],
    ) -> Result<(), Spu2Error> {
        let expected = inputs.len().checked_mul(2).ok_or(Spu2Error::OutputSize {
            expected: usize::MAX,
            actual: output.len(),
        })?;
        if output.len() != expected {
            return Err(Spu2Error::OutputSize {
                expected,
                actual: output.len(),
            });
        }
        for (input, frame) in inputs.iter().copied().zip(output.chunks_exact_mut(2)) {
            let rendered = self.render_frame(input)?.final_output;
            frame[0] = rendered.left;
            frame[1] = rendered.right;
        }
        Ok(())
    }

    /// Delivers one coalesced SPU2 address interrupt to the IOP interrupt sink.
    pub fn drain_irq<S: InterruptSink>(&mut self, sink: &mut S) -> bool {
        let requested = self.cores.iter().any(|core| core.irq_request);
        for core in &mut self.cores {
            core.irq_request = false;
        }
        if requested {
            sink.request(InterruptSource::Spu2);
        }
        requested
    }

    fn write_core_register(&mut self, core_index: usize, local: u32, offset: u32, value: u16) {
        let sound_address = matches!(
            local,
            IRQA_HIGH | IRQA_LOW | TSA_HIGH | TSA_LOW | EFFECT_START_HIGH | EFFECT_START_LOW
        )
        .then(|| self.decode_register_pair(offset, local));
        let effect_address = (EFFECT_ADDRESS_FIRST..=EFFECT_ADDRESS_LAST)
            .contains(&local)
            .then(|| {
                let pair_local = local & !3;
                self.decode_effect_register_pair(offset, local, pair_local)
            });
        let effect_end = matches!(local, EFFECT_END_HIGH | EFFECT_END_LOW)
            .then(|| decode_effect_end(self.registers[register_index(offset & !2)]));
        let core = &mut self.cores[core_index];
        match local {
            PMON_HIGH => set_high_switch_half(&mut core.pitch_mod_mask, value),
            PMON_LOW => set_low_switch_half(&mut core.pitch_mod_mask, value),
            NON_HIGH => set_high_switch_half(&mut core.noise_mask, value),
            NON_LOW => set_low_switch_half(&mut core.noise_mask, value),
            VMIXL_HIGH => set_high_switch_half(&mut core.voice_dry_left, value),
            VMIXL_LOW => set_low_switch_half(&mut core.voice_dry_left, value),
            VMIXEL_HIGH => set_high_switch_half(&mut core.voice_effect_left, value),
            VMIXEL_LOW => set_low_switch_half(&mut core.voice_effect_left, value),
            VMIXR_HIGH => set_high_switch_half(&mut core.voice_dry_right, value),
            VMIXR_LOW => set_low_switch_half(&mut core.voice_dry_right, value),
            VMIXER_HIGH => set_high_switch_half(&mut core.voice_effect_right, value),
            VMIXER_LOW => set_low_switch_half(&mut core.voice_effect_right, value),
            MMIX => core.mix = value & 0x0fff,
            CORE_ATTR => {
                core.attributes = value;
                if value & CORE_ATTR_IRQ_ENABLE == 0 {
                    core.irq_request = false;
                    self.irq_info &= !(1 << (core_index + 2));
                }
            }
            IRQA_HIGH | IRQA_LOW => {
                core.irq_address = sound_address.unwrap_or(0);
            }
            KEY_ON_HIGH => core.key_on(u32::from(value)),
            KEY_ON_LOW => core.key_on(u32::from(value & 0xff) << 16),
            KEY_OFF_HIGH => core.key_off(u32::from(value)),
            KEY_OFF_LOW => core.key_off(u32::from(value & 0xff) << 16),
            TSA_HIGH | TSA_LOW => {
                core.transfer_address = sound_address.unwrap_or(0);
            }
            TRANSFER_DATA => core.write_transfer_halfword(&mut self.ram, value),
            EFFECT_START_HIGH | EFFECT_START_LOW => {
                core.effect_start = sound_address.unwrap_or(0);
                core.effects.set_start(core.effect_start);
            }
            EFFECT_ADDRESS_FIRST..=EFFECT_ADDRESS_LAST => {
                let relative = local - EFFECT_ADDRESS_FIRST;
                if relative % 4 <= 2 {
                    let pair_local = local & !3;
                    let address_index = usize::try_from(relative / 4).unwrap_or(0);
                    let _ = pair_local;
                    core.effect_addresses[address_index] = effect_address.unwrap_or(0);
                }
            }
            EFFECT_END_HIGH | EFFECT_END_LOW => {
                core.effect_end = effect_end.unwrap_or(0);
            }
            ENDX_HIGH => core.endx &= !u32::from(value),
            ENDX_LOW => core.endx &= !(u32::from(value & 0xff) << 16),
            _ => {}
        }
    }

    fn write_primary_register(&mut self, core_index: usize, local: u32, value: u16) {
        let core = &mut self.cores[core_index];
        match local {
            0 => core.master_volume[0] = value,
            2 => core.master_volume[1] = value,
            4 => core.effect_volume[0] = value,
            6 => core.effect_volume[1] = value,
            8 => core.input_a_volume[0] = value,
            0x0a => core.input_a_volume[1] = value,
            0x0c => core.input_b_volume[0] = value,
            0x0e => core.input_b_volume[1] = value,
            0x14..=0x26 if local & 1 == 0 => {
                let coefficient = usize::try_from((local - 0x14) / 2).unwrap_or(0);
                core.effect_coefficients[coefficient] = value;
            }
            _ => {}
        }
    }

    fn decode_register_pair(&self, offset: u32, local: u32) -> u32 {
        let high_offset = if local & 2 == 0 { offset } else { offset - 2 };
        let high = self.registers[register_index(high_offset)];
        let low = self.registers[register_index(high_offset + 2)];
        decode_sound_address(high, low)
    }

    fn decode_effect_register_pair(&self, offset: u32, local: u32, pair_local: u32) -> u32 {
        let core_base = offset - local;
        let high_offset = core_base + pair_local;
        let high = self.registers[register_index(high_offset)];
        let low = self.registers[register_index(high_offset + 2)];
        decode_effect_offset(high, low)
    }

    fn refresh_irq_info(&mut self) {
        for (index, core) in self.cores.iter().enumerate() {
            if core.irq_request {
                self.irq_info |= 1 << (index + 2);
            }
        }
    }
}

impl Spu2DmaEndpoint for Spu2 {
    fn write_word(&mut self, channel: SoundDmaChannel, value: u32) -> Result<(), EndpointError> {
        let core = &mut self.cores[channel as usize];
        let bytes = value.to_le_bytes();
        core.write_transfer_halfword(&mut self.ram, u16::from_le_bytes([bytes[0], bytes[1]]));
        core.write_transfer_halfword(&mut self.ram, u16::from_le_bytes([bytes[2], bytes[3]]));
        self.refresh_irq_info();
        Ok(())
    }

    fn read_word(&mut self, channel: SoundDmaChannel) -> Result<u32, EndpointError> {
        let core = &mut self.cores[channel as usize];
        let low = core.read_transfer_halfword(&self.ram);
        let high = core.read_transfer_halfword(&self.ram);
        self.refresh_irq_info();
        Ok(u32::from(low) | (u32::from(high) << 16))
    }
}

impl Spu2MmioEndpoint for Spu2 {
    fn read_register(&mut self, address: u32) -> Result<u16, EndpointError> {
        Spu2::read_register(self, address).map_err(|error| EndpointError::new(error.to_string()))
    }

    fn write_register(&mut self, address: u32, value: u16) -> Result<(), EndpointError> {
        Spu2::write_register(self, address, value)
            .map_err(|error| EndpointError::new(error.to_string()))
    }
}

fn decode_register(address: u32) -> Result<(u32, usize), Spu2Error> {
    if !(SPU2_BASE..=SPU2_END).contains(&address) || address & 1 != 0 {
        return Err(Spu2Error::InvalidRegister { address });
    }
    let offset = address - SPU2_BASE;
    Ok((offset, register_index(offset)))
}

fn register_index(offset: u32) -> usize {
    usize::try_from(offset / 2).unwrap_or(0)
}

fn decode_core_register(offset: u32) -> Option<(usize, u32)> {
    if offset < CORE_STRIDE {
        Some((0, offset))
    } else if offset < PRIMARY_BASE {
        Some((1, offset - CORE_STRIDE))
    } else {
        None
    }
}

fn decode_primary_register(offset: u32) -> Option<(usize, u32)> {
    if (PRIMARY_BASE..PRIMARY_BASE + PRIMARY_STRIDE).contains(&offset) {
        Some((0, offset - PRIMARY_BASE))
    } else if (PRIMARY_BASE + PRIMARY_STRIDE..PRIMARY_BASE + PRIMARY_STRIDE * 2).contains(&offset) {
        Some((1, offset - PRIMARY_BASE - PRIMARY_STRIDE))
    } else {
        None
    }
}

fn decode_voice_address(local: u32) -> Option<(usize, u32)> {
    let end = VOICE_ADDRESS_BASE + VOICE_ADDRESS_STRIDE * u32::try_from(VOICES_PER_CORE).ok()?;
    if !(VOICE_ADDRESS_BASE..end).contains(&local) {
        return None;
    }
    let relative = local - VOICE_ADDRESS_BASE;
    Some((
        usize::try_from(relative / VOICE_ADDRESS_STRIDE).ok()?,
        relative % VOICE_ADDRESS_STRIDE,
    ))
}

fn decode_sound_address(high: u16, low: u16) -> u32 {
    ((u32::from(high) << 17) | (u32::from(low & 0xfff8) << 1))
        & u32::try_from(RAM_MASK).unwrap_or(u32::MAX)
}

fn decode_effect_offset(high: u16, low: u16) -> u32 {
    (u32::from(high) << 14) | (u32::from(low) >> 2)
}

fn decode_effect_end(high: u16) -> u32 {
    ((u32::from(high) << 17) | 0x1ffff) & u32::try_from(RAM_MASK).unwrap_or(u32::MAX)
}

fn address_half(address: u32, high: bool) -> u16 {
    if high {
        u16::try_from(address >> 17).unwrap_or(0)
    } else {
        u16::try_from((address >> 1) & 0xfff8).unwrap_or(0)
    }
}

fn normalize_ram_address(address: u32) -> usize {
    usize::try_from(address).unwrap_or(0) & RAM_MASK & !1
}

fn high_switch_half(value: u32) -> u16 {
    u16::try_from(value & 0xffff).unwrap_or(0)
}

fn low_switch_half(value: u32) -> u16 {
    u16::try_from((value >> 16) & 0xff).unwrap_or(0)
}

fn set_high_switch_half(target: &mut u32, value: u16) {
    *target = (*target & 0x00ff_0000) | u32::from(value);
}

fn set_low_switch_half(target: &mut u32, value: u16) {
    *target = (*target & 0x0000_ffff) | (u32::from(value & 0xff) << 16);
}

fn current_volume(register: u16) -> u16 {
    let volume = direct_volume(register);
    u16::from_le_bytes(i16::try_from(volume).unwrap_or_default().to_le_bytes())
}

fn direct_volume(register: u16) -> i32 {
    let direct = register & 0x7fff;
    if direct & 0x4000 != 0 {
        i32::from(direct) - 0x8000
    } else {
        i32::from(direct)
    }
}

fn apply_volume(sample: i16, register: u16) -> i16 {
    clamp_i16((i32::from(sample) * direct_volume(register)) >> 14)
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
    use upse_iop_dma::{SoundDmaChannel, Spu2DmaEndpoint, Spu2MmioEndpoint};
    use upse_iop_irq::{InterruptController, InterruptSource};

    use super::{
        CORE_ATTR, CORE_ATTR_EFFECT_ENABLE, CORE_ATTR_ENABLE, CORE_ATTR_IRQ_ENABLE,
        CORE_ATTR_UNMUTE, CORE_COUNT, CoreInput, EFFECT_END_HIGH, EFFECT_START_HIGH,
        EFFECT_START_LOW, ENDX_HIGH, IRQA_HIGH, IRQA_LOW, KEY_ON_HIGH, MMIX, MMIX_INPUT_A_DRY_LEFT,
        MMIX_INPUT_A_DRY_RIGHT, MMIX_VOICE_DRY_LEFT, MMIX_VOICE_DRY_RIGHT, MMIX_VOICE_EFFECT_LEFT,
        MMIX_VOICE_EFFECT_RIGHT, PRIMARY_BASE, PRIMARY_STRIDE, SAMPLE_RATE, SOUND_RAM_SIZE,
        SPU2_BASE, SPU2_END, Spu2, Spu2Error, StereoFrame, TSA_HIGH, TSA_LOW, VMIXEL_HIGH,
        VMIXER_HIGH, VMIXL_HIGH, VMIXR_HIGH, VOICE_ADDRESS_BASE, VOICE_STRIDE, VOICES_PER_CORE,
    };

    fn core_register(core: usize, local: u32) -> u32 {
        SPU2_BASE + u32::try_from(core).unwrap() * 0x400 + local
    }

    fn primary_register(core: usize, local: u32) -> u32 {
        SPU2_BASE + PRIMARY_BASE + u32::try_from(core).unwrap() * PRIMARY_STRIDE + local
    }

    fn constant_block(nibble: u8, flags: u8) -> [u8; 16] {
        let mut block = [0_u8; 16];
        block[0] = 0;
        block[1] = flags;
        for byte in &mut block[2..] {
            *byte = nibble | (nibble << 4);
        }
        block
    }

    fn write_address(spu2: &mut Spu2, core: usize, local: u32, address: u32) {
        spu2.write_register(core_register(core, local), (address >> 17) as u16)
            .unwrap();
        spu2.write_register(
            core_register(core, local + 2),
            ((address >> 1) & 0xfff8) as u16,
        )
        .unwrap();
    }

    fn configure_voice(spu2: &mut Spu2, core: usize, voice: usize, start: u32) {
        let base = core_register(core, u32::try_from(voice).unwrap() * VOICE_STRIDE);
        spu2.write_register(base, 0x3fff).unwrap();
        spu2.write_register(base + 2, 0x3fff).unwrap();
        spu2.write_register(base + 4, 0x1000).unwrap();
        spu2.write_register(base + 6, 0x00ff).unwrap();
        spu2.write_register(base + 8, 0x1f00).unwrap();
        write_address(
            spu2,
            core,
            VOICE_ADDRESS_BASE + u32::try_from(voice).unwrap() * 0x0c,
            start,
        );
    }

    fn enable_voice_mix(spu2: &mut Spu2, core: usize, voice_mask: u16) {
        spu2.write_register(core_register(core, VMIXL_HIGH), voice_mask)
            .unwrap();
        spu2.write_register(core_register(core, VMIXR_HIGH), voice_mask)
            .unwrap();
        spu2.write_register(
            core_register(core, MMIX),
            MMIX_VOICE_DRY_LEFT | MMIX_VOICE_DRY_RIGHT,
        )
        .unwrap();
        spu2.write_register(primary_register(core, 0), 0x7fff)
            .unwrap();
        spu2.write_register(primary_register(core, 2), 0x7fff)
            .unwrap();
        spu2.write_register(
            core_register(core, CORE_ATTR),
            CORE_ATTR_ENABLE | CORE_ATTR_UNMUTE,
        )
        .unwrap();
    }

    fn enable_core0_passthrough(spu2: &mut Spu2) {
        spu2.write_register(
            core_register(1, MMIX),
            MMIX_INPUT_A_DRY_LEFT | MMIX_INPUT_A_DRY_RIGHT,
        )
        .unwrap();
        spu2.write_register(primary_register(1, 0), 0x7fff).unwrap();
        spu2.write_register(primary_register(1, 2), 0x7fff).unwrap();
        spu2.write_register(primary_register(1, 8), 0x7fff).unwrap();
        spu2.write_register(primary_register(1, 0x0a), 0x7fff)
            .unwrap();
        spu2.write_register(
            core_register(1, CORE_ATTR),
            CORE_ATTR_ENABLE | CORE_ATTR_UNMUTE | super::CORE_ATTR_EXTERNAL_ENABLE,
        )
        .unwrap();
    }

    fn configured_spu2() -> Spu2 {
        let mut spu2 = Spu2::new();
        spu2.load_ram(0x1000, &constant_block(1, 3)).unwrap();
        configure_voice(&mut spu2, 0, 0, 0x1000);
        enable_voice_mix(&mut spu2, 0, 1);
        enable_core0_passthrough(&mut spu2);
        spu2.write_register(core_register(0, KEY_ON_HIGH), 1)
            .unwrap();
        spu2
    }

    #[test]
    fn register_script_produces_audible_two_core_integer_golden() {
        let mut spu2 = configured_spu2();
        let mut core0 = [0_i16; 8];
        let mut core1 = [0_i16; 8];
        let mut output = [0_i16; 16];
        for (index, frame) in output.chunks_exact_mut(2).enumerate() {
            let rendered = spu2
                .render_frame([CoreInput::default(); CORE_COUNT])
                .unwrap();
            core0[index] = rendered.cores[0].left;
            core1[index] = rendered.cores[1].left;
            frame[0] = rendered.final_output.left;
            frame[1] = rendered.final_output.right;
        }
        assert_eq!(
            core0,
            [1_491, 3_582, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093]
        );
        assert_eq!(
            core1,
            [1_489, 3_580, 4_091, 4_091, 4_091, 4_091, 4_091, 4_091]
        );
        assert_eq!(
            output,
            [
                1_489, 1_489, 3_580, 3_580, 4_091, 4_091, 4_091, 4_091, 4_091, 4_091, 4_091, 4_091,
                4_091, 4_091, 4_091, 4_091,
            ]
        );
    }

    #[test]
    fn simultaneous_cores_and_input_paths_mix_before_final_saturation() {
        let mut spu2 = configured_spu2();
        spu2.load_ram(0x2000, &constant_block(2, 3)).unwrap();
        configure_voice(&mut spu2, 1, 0, 0x2000);
        enable_voice_mix(&mut spu2, 1, 1);
        spu2.write_register(
            core_register(1, MMIX),
            MMIX_INPUT_A_DRY_LEFT
                | MMIX_INPUT_A_DRY_RIGHT
                | MMIX_VOICE_DRY_LEFT
                | MMIX_VOICE_DRY_RIGHT,
        )
        .unwrap();
        spu2.write_register(primary_register(1, 8), 0x7fff).unwrap();
        spu2.write_register(primary_register(1, 0x0a), 0x7fff)
            .unwrap();
        spu2.write_register(core_register(1, KEY_ON_HIGH), 1)
            .unwrap();

        let first = spu2
            .render_frame([
                CoreInput::default(),
                CoreInput {
                    input_a: StereoFrame::new(100, -100),
                    input_b: StereoFrame::default(),
                },
            ])
            .unwrap();
        assert_ne!(first.cores[0], StereoFrame::default());
        assert_ne!(first.cores[1], first.cores[0]);
        assert_eq!(first.final_output, first.cores[1]);

        let mut loud = Spu2::new();
        loud.load_ram(0, &constant_block(7, 3)).unwrap();
        for voice in 0..VOICES_PER_CORE {
            configure_voice(&mut loud, 1, voice, 0);
        }
        enable_voice_mix(&mut loud, 1, u16::MAX);
        loud.write_register(core_register(1, super::VMIXL_LOW), 0xff)
            .unwrap();
        loud.write_register(core_register(1, super::VMIXR_LOW), 0xff)
            .unwrap();
        loud.write_register(core_register(1, KEY_ON_HIGH), u16::MAX)
            .unwrap();
        loud.write_register(core_register(1, super::KEY_ON_LOW), 0xff)
            .unwrap();
        let mut saturated = [0; 80];
        loud.render(40, &mut saturated).unwrap();
        assert!(saturated.iter().any(|&sample| sample >= 32_760));
    }

    #[test]
    fn dma_directions_irq_edges_and_sound_ram_wrap_are_deterministic() {
        let mut spu2 = Spu2::new();
        write_address(&mut spu2, 1, IRQA_HIGH, 0x1f_fff0);
        write_address(&mut spu2, 1, TSA_HIGH, 0x1f_fff0);
        spu2.write_register(
            core_register(1, CORE_ATTR),
            CORE_ATTR_ENABLE | CORE_ATTR_IRQ_ENABLE,
        )
        .unwrap();
        spu2.write_word(SoundDmaChannel::Core1, 0x1122_3344)
            .unwrap();
        spu2.write_word(SoundDmaChannel::Core1, 0xaabb_ccdd)
            .unwrap();
        assert_eq!(
            &spu2.ram()[SOUND_RAM_SIZE - 16..SOUND_RAM_SIZE - 12],
            &[0x44, 0x33, 0x22, 0x11]
        );
        assert_eq!(
            &spu2.ram()[SOUND_RAM_SIZE - 12..SOUND_RAM_SIZE - 8],
            &[0xdd, 0xcc, 0xbb, 0xaa]
        );
        let mut irq = InterruptController::new();
        assert!(spu2.drain_irq(&mut irq));
        assert!(!spu2.drain_irq(&mut irq));
        assert_eq!(irq.status(), InterruptSource::Spu2.bit());

        write_address(&mut spu2, 1, TSA_HIGH, 0x1f_fff0);
        assert_eq!(spu2.read_word(SoundDmaChannel::Core1).unwrap(), 0x1122_3344);
        assert_eq!(spu2.read_word(SoundDmaChannel::Core1).unwrap(), 0xaabb_ccdd);
    }

    #[test]
    fn voice_loops_endx_noise_pitch_and_chunk_boundaries_are_stable() {
        let mut whole = configured_spu2();
        configure_voice(&mut whole, 0, 1, 0x1000);
        whole
            .write_register(core_register(0, VMIXL_HIGH), 3)
            .unwrap();
        whole
            .write_register(core_register(0, VMIXR_HIGH), 3)
            .unwrap();
        whole
            .write_register(core_register(0, KEY_ON_HIGH), 2)
            .unwrap();
        whole
            .write_register(core_register(0, super::PMON_HIGH), 2)
            .unwrap();
        whole
            .write_register(core_register(0, super::NON_HIGH), 2)
            .unwrap();
        let mut chunked = whole.clone();
        let mut expected = [0_i16; 256];
        whole.render(128, &mut expected).unwrap();
        let mut actual = [0_i16; 256];
        for chunk in actual.chunks_exact_mut(16) {
            chunked.render(8, chunk).unwrap();
        }
        assert_eq!(actual, expected);
        assert_ne!(
            whole.read_register(core_register(0, ENDX_HIGH)).unwrap() & 1,
            0
        );
        assert_eq!(SAMPLE_RATE, 48_000);
        assert_eq!(CORE_COUNT, 2);
    }

    #[test]
    fn effect_unit_writes_its_ring_and_contributes_to_core_mix() {
        let mut dry = configured_spu2();
        let mut wet = dry.clone();
        wet.write_register(core_register(0, VMIXEL_HIGH), 1)
            .unwrap();
        wet.write_register(core_register(0, VMIXER_HIGH), 1)
            .unwrap();
        wet.write_register(
            core_register(0, MMIX),
            MMIX_VOICE_DRY_LEFT
                | MMIX_VOICE_DRY_RIGHT
                | MMIX_VOICE_EFFECT_LEFT
                | MMIX_VOICE_EFFECT_RIGHT,
        )
        .unwrap();
        write_address(&mut wet, 0, EFFECT_START_HIGH, 0x1f_0000);
        wet.write_register(core_register(0, EFFECT_END_HIGH), 0x000f)
            .unwrap();
        wet.write_register(primary_register(0, 4), 0x7fff).unwrap();
        wet.write_register(primary_register(0, 6), 0x7fff).unwrap();
        wet.write_register(primary_register(0, 0x14), 0x7fff)
            .unwrap();
        wet.write_register(primary_register(0, 0x16), 0x7fff)
            .unwrap();
        wet.write_register(primary_register(0, 0x24), 0x7fff)
            .unwrap();
        wet.write_register(primary_register(0, 0x26), 0x7fff)
            .unwrap();
        wet.write_register(
            core_register(0, CORE_ATTR),
            CORE_ATTR_ENABLE | CORE_ATTR_UNMUTE | CORE_ATTR_EFFECT_ENABLE,
        )
        .unwrap();

        let mut dry_output = [0_i16; 128];
        let mut wet_output = [0_i16; 128];
        dry.render(64, &mut dry_output).unwrap();
        wet.render(64, &mut wet_output).unwrap();
        assert_ne!(wet_output, dry_output);
        assert_eq!(
            &wet_output[..16],
            &[
                2_977, 2_978, 7_159, 7_160, 8_181, 8_182, 8_181, 8_182, 8_181, 8_182, 8_181, 8_182,
                8_181, 8_182, 8_181, 8_182
            ]
        );
        assert!(wet.ram()[0x1f_0000..].iter().any(|&byte| byte != 0));
    }

    #[test]
    fn register_endpoint_and_host_ranges_are_checked() {
        let mut spu2 = Spu2::new();
        Spu2MmioEndpoint::write_register(&mut spu2, SPU2_BASE + 0x7fe, 0x1234).unwrap();
        assert_eq!(
            Spu2MmioEndpoint::read_register(&mut spu2, SPU2_BASE + 0x7fe).unwrap(),
            0x1234
        );
        assert!(matches!(
            spu2.write_register(SPU2_BASE + 1, 0),
            Err(Spu2Error::InvalidRegister { .. })
        ));
        assert!(matches!(
            spu2.read_register(SPU2_END + 1),
            Err(Spu2Error::InvalidRegister { .. })
        ));
        assert!(matches!(
            spu2.load_ram(SOUND_RAM_SIZE - 1, &[1, 2]),
            Err(Spu2Error::RamRange { .. })
        ));
        assert_eq!(EFFECT_START_LOW, EFFECT_START_HIGH + 2);
        assert_eq!(IRQA_LOW, IRQA_HIGH + 2);
        assert_eq!(TSA_LOW, TSA_HIGH + 2);
    }
}
