// SPDX-License-Identifier: LGPL-2.1-or-later
//! Unoptimized-reference mixer smoke benchmark.

use std::hint::black_box;
use std::time::Instant;

use upse_ps1_spu::{SAMPLE_RATE, Spu};

fn main() {
    let mut spu = Spu::new();
    let mut output = vec![0_i16; usize::try_from(SAMPLE_RATE).unwrap_or(44_100) * 2];
    let start = Instant::now();
    spu.render(output.len() / 2, &mut output)
        .expect("fixed benchmark buffer has stereo length");
    black_box(&output);
    eprintln!("reference mixer: {:?} per silent second", start.elapsed());
}
