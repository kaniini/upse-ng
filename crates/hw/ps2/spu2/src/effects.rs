// SPDX-License-Identifier: LGPL-2.1-or-later
//! SPU2 effect-unit arithmetic and sound-RAM addressing.

use upse_spu_common::{RingBufferAddress, clamp_i16, multiply_q15};

use super::SOUND_RAM_SIZE;

const D_APF1: usize = 0;
const D_APF2: usize = 1;
const M_LSAME: usize = 2;
const M_RSAME: usize = 3;
const M_LCOMB1: usize = 4;
const M_RCOMB1: usize = 5;
const M_LCOMB2: usize = 6;
const M_RCOMB2: usize = 7;
const D_LSAME: usize = 8;
const D_RSAME: usize = 9;
const M_LDIFF: usize = 10;
const M_RDIFF: usize = 11;
const M_LCOMB3: usize = 12;
const M_RCOMB3: usize = 13;
const M_LCOMB4: usize = 14;
const M_RCOMB4: usize = 15;
const D_LDIFF: usize = 16;
const D_RDIFF: usize = 17;
const M_LAPF1: usize = 18;
const M_RAPF1: usize = 19;
const M_LAPF2: usize = 20;
const M_RAPF2: usize = 21;

const V_IIR: usize = 0;
const V_COMB1: usize = 1;
const V_COMB2: usize = 2;
const V_COMB3: usize = 3;
const V_COMB4: usize = 4;
const V_WALL: usize = 5;
const V_APF1: usize = 6;
const V_APF2: usize = 7;
const V_LIN: usize = 8;
const V_RIN: usize = 9;

/// Instance-owned cursor for one SPU2 core's effects work area.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Effects {
    current: usize,
}

impl Effects {
    pub(super) const fn new() -> Self {
        Self { current: 0 }
    }

    pub(super) fn set_start(&mut self, start: u32) {
        self.current = normalize_address(start);
    }

    /// Runs the PS2 effect graph at its native 48 kHz rate.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn process(
        &mut self,
        ram: &mut [u8],
        start: u32,
        end: u32,
        addresses: &[u32; 22],
        coefficients: &[u16; 10],
        writes_enabled: bool,
        input: [i16; 2],
    ) -> [i16; 2] {
        let start = normalize_address(start);
        let end = normalize_address(end) | 1;
        if start >= SOUND_RAM_SIZE || end < start {
            return [0; 2];
        }
        if self.current < start || self.current > end {
            self.current = start;
        }

        let volume = |index| signed(coefficients[index]);
        let scaled_input = [
            clamp_i16(multiply_q15(input[0], volume(V_LIN))),
            clamp_i16(multiply_q15(input[1], volume(V_RIN))),
        ];

        if writes_enabled {
            let left_same = self.reflection(
                ram,
                start,
                end,
                scaled_input[0],
                addresses[D_LSAME],
                addresses[M_LSAME],
                volume(V_WALL),
                volume(V_IIR),
            );
            let right_same = self.reflection(
                ram,
                start,
                end,
                scaled_input[1],
                addresses[D_RSAME],
                addresses[M_RSAME],
                volume(V_WALL),
                volume(V_IIR),
            );
            let left_different = self.reflection(
                ram,
                start,
                end,
                scaled_input[0],
                addresses[D_RDIFF],
                addresses[M_LDIFF],
                volume(V_WALL),
                volume(V_IIR),
            );
            let right_different = self.reflection(
                ram,
                start,
                end,
                scaled_input[1],
                addresses[D_LDIFF],
                addresses[M_RDIFF],
                volume(V_WALL),
                volume(V_IIR),
            );
            self.write(ram, start, end, addresses[M_LSAME], left_same);
            self.write(ram, start, end, addresses[M_RSAME], right_same);
            self.write(ram, start, end, addresses[M_LDIFF], left_different);
            self.write(ram, start, end, addresses[M_RDIFF], right_different);
        }

        let comb_volumes = [
            volume(V_COMB1),
            volume(V_COMB2),
            volume(V_COMB3),
            volume(V_COMB4),
        ];
        let left_comb = self.comb(
            ram,
            start,
            end,
            [
                addresses[M_LCOMB1],
                addresses[M_LCOMB2],
                addresses[M_LCOMB3],
                addresses[M_LCOMB4],
            ],
            comb_volumes,
        );
        let right_comb = self.comb(
            ram,
            start,
            end,
            [
                addresses[M_RCOMB1],
                addresses[M_RCOMB2],
                addresses[M_RCOMB3],
                addresses[M_RCOMB4],
            ],
            comb_volumes,
        );

        let left = self.all_pass(
            ram,
            start,
            end,
            left_comb,
            addresses[M_LAPF1],
            addresses[M_LAPF1].wrapping_sub(addresses[D_APF1]),
            volume(V_APF1),
            writes_enabled,
        );
        let right = self.all_pass(
            ram,
            start,
            end,
            right_comb,
            addresses[M_RAPF1],
            addresses[M_RAPF1].wrapping_sub(addresses[D_APF1]),
            volume(V_APF1),
            writes_enabled,
        );
        let left = self.all_pass(
            ram,
            start,
            end,
            left,
            addresses[M_LAPF2],
            addresses[M_LAPF2].wrapping_sub(addresses[D_APF2]),
            volume(V_APF2),
            writes_enabled,
        );
        let right = self.all_pass(
            ram,
            start,
            end,
            right,
            addresses[M_RAPF2],
            addresses[M_RAPF2].wrapping_sub(addresses[D_APF2]),
            volume(V_APF2),
            writes_enabled,
        );

        self.current += 2;
        if self.current > end {
            self.current = start;
        }
        [left, right]
    }

    #[allow(clippy::too_many_arguments)]
    fn reflection(
        &self,
        ram: &[u8],
        start: usize,
        end: usize,
        input: i16,
        source: u32,
        destination: u32,
        wall_volume: i16,
        iir_volume: i16,
    ) -> i16 {
        let wall = multiply_q15(self.read(ram, start, end, source, 0), wall_volume);
        let old = self.read(ram, start, end, destination, -2);
        let incident = clamp_i16(i32::from(input).saturating_add(wall));
        let delta = clamp_i16(i32::from(incident) - i32::from(old));
        clamp_i16(multiply_q15(delta, iir_volume).saturating_add(i32::from(old)))
    }

    fn comb(
        &self,
        ram: &[u8],
        start: usize,
        end: usize,
        sources: [u32; 4],
        volumes: [i16; 4],
    ) -> i16 {
        let accumulator = sources
            .into_iter()
            .zip(volumes)
            .fold(0_i64, |sum, (source, volume)| {
                sum + i64::from(multiply_q15(self.read(ram, start, end, source, 0), volume))
            });
        clamp_i64_to_i16(accumulator)
    }

    #[allow(clippy::too_many_arguments)]
    fn all_pass(
        &self,
        ram: &mut [u8],
        start: usize,
        end: usize,
        input: i16,
        destination: u32,
        feedback_source: u32,
        volume: i16,
        writes_enabled: bool,
    ) -> i16 {
        let feedback = self.read(ram, start, end, feedback_source, 0);
        let stored = clamp_i16(i32::from(input) - multiply_q15(feedback, volume));
        if writes_enabled {
            self.write(ram, start, end, destination, stored);
        }
        clamp_i16(multiply_q15(stored, volume).saturating_add(i32::from(feedback)))
    }

    fn read(&self, ram: &[u8], start: usize, end: usize, offset: u32, extra: i64) -> i16 {
        let address = self.memory_address(start, end, offset, extra);
        i16::from_le_bytes([ram[address], ram[address + 1]])
    }

    fn write(&self, ram: &mut [u8], start: usize, end: usize, offset: u32, value: i16) {
        let address = self.memory_address(start, end, offset, 0);
        let bytes = value.to_le_bytes();
        ram[address] = bytes[0];
        ram[address + 1] = bytes[1];
    }

    fn memory_address(&self, start: usize, end: usize, offset: u32, extra: i64) -> usize {
        let length = end - start + 1;
        let mut address = RingBufferAddress::new(start, length)
            .expect("a validated SPU2 effects work area is nonempty");
        let current = self.current.saturating_sub(start);
        let signed_offset = i64::from(i32::from_ne_bytes(offset.to_ne_bytes())) * 8;
        address.advance(i64::try_from(current).unwrap_or(0) + signed_offset + extra);
        address
            .get()
            .expect("a wrapped SPU2 effect address remains inside sound RAM")
    }
}

fn normalize_address(address: u32) -> usize {
    usize::try_from(address).unwrap_or(0) & (SOUND_RAM_SIZE - 1) & !1
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
