// SPDX-License-Identifier: LGPL-2.1-or-later
//! Unoptimized-reference two-core mixer smoke benchmark.

use std::hint::black_box;
use std::time::Instant;

use upse_ps2_spu2::{CORE_COUNT, CoreInput, SAMPLE_RATE, SPU2_BASE, Spu2};

fn constant_block(nibble: u8) -> [u8; 16] {
    let mut block = [0_u8; 16];
    block[1] = 3;
    for byte in &mut block[2..] {
        *byte = nibble | (nibble << 4);
    }
    block
}

fn write_address(spu2: &mut Spu2, register: u32, address: u32) {
    spu2.write_register(register, (address >> 17) as u16)
        .expect("fixture address high register is valid");
    spu2.write_register(register + 2, ((address >> 1) & 0xfff8) as u16)
        .expect("fixture address low register is valid");
}

fn audible_spu2() -> Spu2 {
    let mut spu2 = Spu2::new();
    for (core, ram_address, nibble) in [(0_u32, 0_usize, 1_u8), (1, 0x1000, 2)] {
        let base = SPU2_BASE + core * 0x400;
        spu2.load_ram(ram_address, &constant_block(nibble))
            .expect("fixture block fits sound RAM");
        for (offset, value) in [
            (0, 0x3fff),
            (2, 0x3fff),
            (4, 0x1000),
            (6, 0x00ff),
            (8, 0x1f00),
        ] {
            spu2.write_register(base + offset, value)
                .expect("fixture voice register is valid");
        }
        write_address(
            &mut spu2,
            base + 0x1c0,
            u32::try_from(ram_address).unwrap_or(0),
        );
        spu2.write_register(base + 0x188, 1)
            .expect("fixture left route is valid");
        spu2.write_register(base + 0x190, 1)
            .expect("fixture right route is valid");
        spu2.write_register(base + 0x198, if core == 0 { 0x0c00 } else { 0x0c0c })
            .expect("fixture mixer register is valid");
        spu2.write_register(base + 0x19a, if core == 0 { 0xc000 } else { 0xc001 })
            .expect("fixture core attribute is valid");
        spu2.write_register(base + 0x1a0, 1)
            .expect("fixture key-on register is valid");
        let primary = SPU2_BASE + 0x760 + core * 0x28;
        spu2.write_register(primary, 0x7fff)
            .expect("fixture master-left register is valid");
        spu2.write_register(primary + 2, 0x7fff)
            .expect("fixture master-right register is valid");
        if core == 1 {
            spu2.write_register(primary + 8, 0x7fff)
                .expect("fixture input-left register is valid");
            spu2.write_register(primary + 0x0a, 0x7fff)
                .expect("fixture input-right register is valid");
        }
    }
    spu2
}

fn main() {
    let frames = usize::try_from(SAMPLE_RATE).unwrap_or(48_000);
    let mut reference = audible_spu2();
    let mut expected = vec![0_i16; frames * 2];
    let start = Instant::now();
    for frame in expected.chunks_exact_mut(2) {
        let output = reference
            .render_frame([CoreInput::default(); CORE_COUNT])
            .expect("silent voices have valid state")
            .final_output;
        frame[0] = output.left;
        frame[1] = output.right;
    }
    let reference_elapsed = start.elapsed();

    let mut block = audible_spu2();
    let mut actual = vec![0_i16; frames * 2];
    let start = Instant::now();
    block
        .render(frames, &mut actual)
        .expect("fixed benchmark buffer has stereo length");
    let block_elapsed = start.elapsed();
    assert_eq!(actual, expected);
    black_box(&actual);
    eprintln!(
        "frame reference: {:?} ({:.1}x realtime)",
        reference_elapsed,
        1.0 / reference_elapsed.as_secs_f64()
    );
    eprintln!(
        "block mixer: {:?} ({:.1}x realtime)",
        block_elapsed,
        1.0 / block_elapsed.as_secs_f64()
    );
}
