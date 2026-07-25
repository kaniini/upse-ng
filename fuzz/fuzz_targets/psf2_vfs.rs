// SPDX-License-Identifier: LGPL-2.1-or-later
#![no_main]

use libfuzzer_sys::fuzz_target;
use upse_psf::{
    DependencyLimits, LoadPlan, MemoryResolver, ParseLimits, PsfBuilder, PsfVersion, load_plan,
};
use upse_psf2_vfs::{Psf2Vfs, VfsLimits};

fuzz_target!(|bytes: &[u8]| {
    let root = PsfBuilder::new(PsfVersion::Psf2)
        .reserved(bytes)
        .build();
    let Ok(LoadPlan::Psf2(plan)) = load_plan(
        "fuzz.psf2",
        &root,
        &mut MemoryResolver::new(),
        ParseLimits {
            max_input_bytes: 2 << 20,
            max_reserved_bytes: 1 << 20,
            max_decompressed_bytes: 1 << 20,
            max_tag_bytes: 50_000,
        },
        DependencyLimits::default(),
    ) else {
        return;
    };
    let limits = VfsLimits {
        max_depth: 8,
        max_entries: 4096,
        max_path_bytes: 255,
        max_blocks: 4096,
        max_file_bytes: 1 << 20,
        max_block_bytes: 1 << 18,
        max_aggregate_bytes: 2 << 20,
    };
    if let Ok(vfs) = Psf2Vfs::from_load_plan(&plan, limits) {
        for (path, data) in vfs.files() {
            let mut output = [0_u8; 64];
            let _ = vfs.node_kind(path);
            let _ = vfs.read(path, data.len() / 2, &mut output);
        }
        for path in vfs.directories() {
            let _ = vfs.node_kind(path);
        }
    }
});
