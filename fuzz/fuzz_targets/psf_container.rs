// SPDX-License-Identifier: LGPL-2.1-or-later
#![no_main]

use libfuzzer_sys::fuzz_target;
use upse_psf::{ParseLimits, PsfContainer};

fuzz_target!(|bytes: &[u8]| {
    let limits = ParseLimits {
        max_input_bytes: 1 << 20,
        max_reserved_bytes: 1 << 19,
        max_decompressed_bytes: 1 << 20,
        max_tag_bytes: 50_000,
    };
    let _ = PsfContainer::parse_with_limits("fuzz-input", bytes, limits);
});
