// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS1 reverb datapath and its 44.1 kHz/22.05 kHz resamplers.

use upse_spu_common::{RingBufferAddress, clamp_i16, multiply_q15};

use super::SOUND_RAM_SIZE;

const D_APF1: usize = 0;
const D_APF2: usize = 1;
const V_IIR: usize = 2;
const V_COMB1: usize = 3;
const V_COMB2: usize = 4;
const V_COMB3: usize = 5;
const V_COMB4: usize = 6;
const V_WALL: usize = 7;
const V_APF1: usize = 8;
const V_APF2: usize = 9;
const M_LSAME: usize = 10;
const M_RSAME: usize = 11;
const M_LCOMB1: usize = 12;
const M_RCOMB1: usize = 13;
const M_LCOMB2: usize = 14;
const M_RCOMB2: usize = 15;
const D_LSAME: usize = 16;
const D_RSAME: usize = 17;
const M_LDIFF: usize = 18;
const M_RDIFF: usize = 19;
const M_LCOMB3: usize = 20;
const M_RCOMB3: usize = 21;
const M_LCOMB4: usize = 22;
const M_RCOMB4: usize = 23;
const D_LDIFF: usize = 24;
const D_RDIFF: usize = 25;
const M_LAPF1: usize = 26;
const M_RAPF1: usize = 27;
const M_LAPF2: usize = 28;
const M_RAPF2: usize = 29;
const V_LIN: usize = 30;
const V_RIN: usize = 31;

const RESAMPLE_RING_MASK: usize = 63;
const UPSAMPLE_RING_MASK: usize = 31;

// The zero-valued taps in the documented 39-tap filter are omitted. The
// centre 0x4000 tap is handled separately because it belongs to the opposite
// polyphase arm.
const RESAMPLE_COEFFICIENTS: [i16; 20] = [
    -0x0001, 0x0002, -0x000a, 0x0023, -0x0067, 0x010a, -0x0268, 0x0534, -0x0b90, 0x2806, 0x2806,
    -0x0b90, 0x0534, -0x0268, 0x010a, -0x0067, 0x0023, -0x000a, 0x0002, -0x0001,
];

/// Instance-owned reverb cursor, input history, and output history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Reverb {
    current_address: usize,
    resample_position: usize,
    downsample_history: [[i16; 64]; 2],
    upsample_history: [[i16; 32]; 2],
}

impl Reverb {
    pub(super) fn new() -> Self {
        Self {
            current_address: 0,
            resample_position: 0,
            downsample_history: [[0; 64]; 2],
            upsample_history: [[0; 32]; 2],
        }
    }

    pub(super) fn set_base(&mut self, base_register: u16) {
        self.current_address = usize::from(base_register) * 8;
    }

    /// Accepts one 44.1 kHz stereo wet-send sample and returns one wet sample.
    pub(super) fn process(
        &mut self,
        ram: &mut [u8],
        base_register: u16,
        registers: &[u16; 32],
        writes_enabled: bool,
        input: [i16; 2],
    ) -> [i16; 2] {
        for (channel, sample) in input.into_iter().enumerate() {
            self.downsample_history[channel][self.resample_position] = sample;
        }

        let output = if self.resample_position & 1 == 0 {
            self.centre_upsample_phase()
        } else {
            let downsampled = self.downsample();
            let effect_output =
                self.effect_tick(ram, base_register, registers, writes_enabled, downsampled);
            let output_position = self.resample_position >> 1;
            for (channel, sample) in effect_output.into_iter().enumerate() {
                self.upsample_history[channel][output_position] = sample;
            }
            self.fir_upsample_phase()
        };

        self.resample_position = (self.resample_position + 1) & RESAMPLE_RING_MASK;
        output
    }

    fn downsample(&self) -> [i16; 2] {
        let start = self.resample_position.wrapping_sub(38) & RESAMPLE_RING_MASK;
        std::array::from_fn(|channel| {
            let mut accumulator = 0_i64;
            for (tap, coefficient) in RESAMPLE_COEFFICIENTS.iter().enumerate() {
                let index = (start + tap * 2) & RESAMPLE_RING_MASK;
                accumulator +=
                    i64::from(self.downsample_history[channel][index]) * i64::from(*coefficient);
            }
            let centre = (start + 19) & RESAMPLE_RING_MASK;
            accumulator += i64::from(self.downsample_history[channel][centre]) * 0x4000;
            clamp_i64_to_i16(accumulator >> 15)
        })
    }

    fn centre_upsample_phase(&self) -> [i16; 2] {
        let index = ((self.resample_position >> 1).wrapping_sub(10)) & UPSAMPLE_RING_MASK;
        std::array::from_fn(|channel| self.upsample_history[channel][index])
    }

    fn fir_upsample_phase(&self) -> [i16; 2] {
        let start = ((self.resample_position >> 1).wrapping_sub(19)) & UPSAMPLE_RING_MASK;
        std::array::from_fn(|channel| {
            let accumulator =
                RESAMPLE_COEFFICIENTS
                    .iter()
                    .enumerate()
                    .fold(0_i64, |sum, (tap, coefficient)| {
                        let index = (start + tap) & UPSAMPLE_RING_MASK;
                        sum + i64::from(self.upsample_history[channel][index])
                            * i64::from(*coefficient)
                    });
            // Upsampling inserts an implicit zero between source samples, so
            // this phase uses twice the normal Q15 gain.
            clamp_i64_to_i16(accumulator >> 14)
        })
    }

    fn effect_tick(
        &mut self,
        ram: &mut [u8],
        base_register: u16,
        registers: &[u16; 32],
        writes_enabled: bool,
        input: [i16; 2],
    ) -> [i16; 2] {
        let volume = |index| signed(registers[index]);
        let scaled_input = [
            clamp_i16(multiply_q15(input[0], volume(V_LIN))),
            clamp_i16(multiply_q15(input[1], volume(V_RIN))),
        ];

        if writes_enabled {
            let left_same = self.reflection(
                ram,
                base_register,
                scaled_input[0],
                registers[D_LSAME],
                registers[M_LSAME],
                volume(V_WALL),
                volume(V_IIR),
            );
            let right_same = self.reflection(
                ram,
                base_register,
                scaled_input[1],
                registers[D_RSAME],
                registers[M_RSAME],
                volume(V_WALL),
                volume(V_IIR),
            );
            let left_different = self.reflection(
                ram,
                base_register,
                scaled_input[0],
                registers[D_RDIFF],
                registers[M_LDIFF],
                volume(V_WALL),
                volume(V_IIR),
            );
            let right_different = self.reflection(
                ram,
                base_register,
                scaled_input[1],
                registers[D_LDIFF],
                registers[M_RDIFF],
                volume(V_WALL),
                volume(V_IIR),
            );
            self.write(ram, base_register, registers[M_LSAME], left_same);
            self.write(ram, base_register, registers[M_RSAME], right_same);
            self.write(ram, base_register, registers[M_LDIFF], left_different);
            self.write(ram, base_register, registers[M_RDIFF], right_different);
        }

        let comb_volumes = [
            volume(V_COMB1),
            volume(V_COMB2),
            volume(V_COMB3),
            volume(V_COMB4),
        ];
        let left_comb = self.comb(
            ram,
            base_register,
            [
                registers[M_LCOMB1],
                registers[M_LCOMB2],
                registers[M_LCOMB3],
                registers[M_LCOMB4],
            ],
            comb_volumes,
        );
        let right_comb = self.comb(
            ram,
            base_register,
            [
                registers[M_RCOMB1],
                registers[M_RCOMB2],
                registers[M_RCOMB3],
                registers[M_RCOMB4],
            ],
            comb_volumes,
        );

        let left_apf1 = registers[M_LAPF1];
        let right_apf1 = registers[M_RAPF1];
        let left_apf2 = registers[M_LAPF2];
        let right_apf2 = registers[M_RAPF2];
        let left = self.all_pass(
            ram,
            base_register,
            left_comb,
            left_apf1,
            left_apf1.wrapping_sub(registers[D_APF1]),
            volume(V_APF1),
            writes_enabled,
        );
        let right = self.all_pass(
            ram,
            base_register,
            right_comb,
            right_apf1,
            right_apf1.wrapping_sub(registers[D_APF1]),
            volume(V_APF1),
            writes_enabled,
        );
        let left = self.all_pass(
            ram,
            base_register,
            left,
            left_apf2,
            left_apf2.wrapping_sub(registers[D_APF2]),
            volume(V_APF2),
            writes_enabled,
        );
        let right = self.all_pass(
            ram,
            base_register,
            right,
            right_apf2,
            right_apf2.wrapping_sub(registers[D_APF2]),
            volume(V_APF2),
            writes_enabled,
        );

        self.current_address += 2;
        if self.current_address >= SOUND_RAM_SIZE {
            self.current_address = usize::from(base_register) * 8;
        }
        [left, right]
    }

    #[allow(clippy::too_many_arguments)]
    fn reflection(
        &self,
        ram: &[u8],
        base_register: u16,
        input: i16,
        source: u16,
        destination: u16,
        wall_volume: i16,
        iir_volume: i16,
    ) -> i16 {
        let wall = multiply_q15(self.read(ram, base_register, source, 0), wall_volume);
        let old = self.read(ram, base_register, destination, -2);
        let incident = clamp_i16(i32::from(input).saturating_add(wall));
        let delta = clamp_i16(i32::from(incident) - i32::from(old));
        clamp_i16(multiply_q15(delta, iir_volume).saturating_add(i32::from(old)))
    }

    fn comb(&self, ram: &[u8], base_register: u16, sources: [u16; 4], volumes: [i16; 4]) -> i16 {
        let accumulator = sources
            .into_iter()
            .zip(volumes)
            .fold(0_i64, |sum, (source, volume)| {
                sum + i64::from(multiply_q15(
                    self.read(ram, base_register, source, 0),
                    volume,
                ))
            });
        clamp_i64_to_i16(accumulator)
    }

    #[allow(clippy::too_many_arguments)]
    fn all_pass(
        &self,
        ram: &mut [u8],
        base_register: u16,
        input: i16,
        destination: u16,
        feedback_source: u16,
        volume: i16,
        writes_enabled: bool,
    ) -> i16 {
        let feedback = self.read(ram, base_register, feedback_source, 0);
        let stored = clamp_i16(i32::from(input) - multiply_q15(feedback, volume));
        if writes_enabled {
            self.write(ram, base_register, destination, stored);
        }
        clamp_i16(multiply_q15(stored, volume).saturating_add(i32::from(feedback)))
    }

    fn read(&self, ram: &[u8], base_register: u16, offset: u16, extra: i64) -> i16 {
        let address = self.memory_address(base_register, offset, extra);
        i16::from_le_bytes([ram[address], ram[address + 1]])
    }

    fn write(&self, ram: &mut [u8], base_register: u16, offset: u16, value: i16) {
        let address = self.memory_address(base_register, offset, 0);
        let bytes = value.to_le_bytes();
        ram[address] = bytes[0];
        ram[address + 1] = bytes[1];
    }

    fn memory_address(&self, base_register: u16, offset: u16, extra: i64) -> usize {
        let base = usize::from(base_register) * 8;
        let length = SOUND_RAM_SIZE - base;
        let mut address = RingBufferAddress::new(base, length)
            .expect("the PS1 reverb work area always contains at least eight bytes");
        let current = self.current_address.saturating_sub(base);
        address.advance(i64::try_from(current).unwrap_or(0) + i64::from(offset) * 8 + extra);
        address
            .get()
            .expect("a wrapped PS1 reverb address remains inside sound RAM")
    }
}

fn signed(value: u16) -> i16 {
    i16::from_le_bytes(value.to_le_bytes())
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
