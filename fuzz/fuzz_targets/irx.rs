// SPDX-License-Identifier: LGPL-2.1-or-later
#![no_main]

use libfuzzer_sys::fuzz_target;
use upse_irx::{IrxLimits, IrxModule, MemoryRange, TargetError, TargetMemory};

struct FuzzMemory {
    bytes: Vec<u8>,
}

impl TargetMemory for FuzzMemory {
    fn range(&self) -> MemoryRange {
        MemoryRange {
            start: 0x1000,
            end: 0x20_0000,
        }
    }

    fn write_image(&mut self, address: u32, image: &[u8]) -> Result<(), TargetError> {
        let offset = address.saturating_sub(0x1000) as usize;
        let Some(output) = self.bytes.get_mut(offset..offset.saturating_add(image.len())) else {
            return Err(TargetError::new("outside fuzz memory"));
        };
        output.copy_from_slice(image);
        Ok(())
    }
}

fuzz_target!(|bytes: &[u8]| {
    let limits = IrxLimits {
        max_input_bytes: 1 << 20,
        max_program_headers: 16,
        max_section_headers: 256,
        max_image_bytes: 1 << 20,
        max_relocations: 4096,
        max_link_tables: 256,
        max_link_entries: 4096,
        max_name_bytes: 127,
        max_alignment: 4096,
    };
    if let Ok(module) = IrxModule::parse_with_limits("fuzz.irx", bytes, limits) {
        let address = if module.description().variant.is_relocatable() {
            0x1000
        } else {
            module.preferred_address()
        };
        let mut memory = FuzzMemory {
            bytes: vec![0; 0x1f_f000],
        };
        let _ = module.load_into(address, &mut memory);
    }
});
