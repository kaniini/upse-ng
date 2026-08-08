// SPDX-License-Identifier: LGPL-2.1-or-later
//! Generated PS2SDK-layout IRX fixtures and hostile-input tests.

use upse_irx::{
    IrxErrorKind, IrxLimits, IrxModule, IrxVariant, MemoryRange, ResidentState, TargetError,
    TargetMemory,
};

const PHOFF: usize = 52;
const IOPMOD_OFFSET: usize = 0xa0;
const IMAGE_OFFSET: usize = 0x100;
const IMAGE_SIZE: usize = 0xc0;
const MEMORY_SIZE: usize = 0xd0;
const REL_OFFSET: usize = 0x1c0;
const SHOFF: usize = 0x220;

#[derive(Default)]
struct FixtureTarget {
    range: Option<MemoryRange>,
    writes: usize,
    address: u32,
    image: Vec<u8>,
    fail: bool,
}

impl FixtureTarget {
    fn new(range: MemoryRange) -> Self {
        Self {
            range: Some(range),
            ..Self::default()
        }
    }
}

impl TargetMemory for FixtureTarget {
    fn range(&self) -> MemoryRange {
        self.range.unwrap()
    }

    fn write_image(&mut self, address: u32, image: &[u8]) -> Result<(), TargetError> {
        self.writes += 1;
        if self.fail {
            return Err(TargetError::new("injected target failure"));
        }
        self.address = address;
        self.image = image.to_vec();
        Ok(())
    }
}

fn ps2sdk_irx_fixture() -> Vec<u8> {
    let relocations = [
        (0x00, 1),
        (0x04, 2),
        (0x08, 4),
        (0x0c, 5),
        (0x10, 6),
        (0x14, 6),
        (0x18, 250),
        (0x44, 251),
        (0x40, 2),
        (0xa4, 2),
    ];
    let mut elf = vec![0_u8; SHOFF + 3 * 40];
    elf[..16].copy_from_slice(b"\x7fELF\x01\x01\x01\0\0\0\0\0\0\0\0\0");
    put_u16(&mut elf, 16, 0xff81);
    put_u16(&mut elf, 18, 8);
    put_u32(&mut elf, 20, 1);
    put_u32(&mut elf, 24, 0);
    put_u32(&mut elf, 28, PHOFF);
    put_u32(&mut elf, 32, SHOFF);
    put_u16(&mut elf, 40, 52);
    put_u16(&mut elf, 42, 32);
    put_u16(&mut elf, 44, 2);
    put_u16(&mut elf, 46, 40);
    put_u16(&mut elf, 48, 3);

    program_header(&mut elf, 0, 0x7000_0080, IOPMOD_OFFSET, 0, 27, 27, 4);
    program_header(&mut elf, 1, 1, IMAGE_OFFSET, 0, IMAGE_SIZE, MEMORY_SIZE, 16);

    put_u32(&mut elf, IOPMOD_OFFSET, 0x40);
    put_u32(&mut elf, IOPMOD_OFFSET + 4, 0);
    put_u32(&mut elf, IOPMOD_OFFSET + 8, 0x30);
    put_u32(&mut elf, IOPMOD_OFFSET + 12, 0xb0);
    put_u32(&mut elf, IOPMOD_OFFSET + 16, 0x10);
    put_u32(&mut elf, IOPMOD_OFFSET + 20, 0x10);
    put_u16(&mut elf, IOPMOD_OFFSET + 24, 0x0102);

    put_u32(&mut elf, IMAGE_OFFSET, 0x2402_0001);
    put_u32(&mut elf, IMAGE_OFFSET + 4, 0x20);
    put_u32(&mut elf, IMAGE_OFFSET + 8, 0x0800_0010);
    put_u32(&mut elf, IMAGE_OFFSET + 12, 0x3c08_0000);
    put_u32(&mut elf, IMAGE_OFFSET + 16, 0x2508_0040);
    put_u32(&mut elf, IMAGE_OFFSET + 20, 0x2442_0030);
    put_u32(&mut elf, IMAGE_OFFSET + 24, 0x3c09_0002);
    put_u32(&mut elf, IMAGE_OFFSET + 28, 0x0000_0000);
    put_u32(&mut elf, IMAGE_OFFSET + 32, 0x3c0a_0000);
    put_u32(&mut elf, IMAGE_OFFSET + 0x40, 0x48);
    put_u16(&mut elf, IMAGE_OFFSET + 0x44, 0x0102);
    elf[IMAGE_OFFSET + 0x48..IMAGE_OFFSET + 0x50].copy_from_slice(b"fixture\0");

    let import = IMAGE_OFFSET + 0x60;
    put_u32(&mut elf, import, 0x41e0_0000);
    put_u16(&mut elf, import + 8, 0x0103);
    elf[import + 12..import + 20].copy_from_slice(b"sysclib\0");
    put_u32(&mut elf, import + 20, 0x03e0_0008);
    put_u32(&mut elf, import + 24, 0x2400_000c);

    let export = IMAGE_OFFSET + 0x90;
    put_u32(&mut elf, export, 0x41c0_0000);
    put_u16(&mut elf, export + 8, 0x0102);
    elf[export + 12..export + 20].copy_from_slice(b"fixture\0");
    put_u32(&mut elf, export + 20, 0x10);

    for (index, (offset, kind)) in relocations.iter().copied().enumerate() {
        put_u32(&mut elf, REL_OFFSET + index * 8, offset);
        put_u32(&mut elf, REL_OFFSET + index * 8 + 4, kind);
    }
    section_header(&mut elf, 1, 1, 6, 0, IMAGE_OFFSET, IMAGE_SIZE, 0, 0, 16, 0);
    section_header(
        &mut elf,
        2,
        9,
        0,
        0,
        REL_OFFSET,
        relocations.len() * 8,
        0,
        1,
        4,
        8,
    );
    elf
}

fn fixed_executable_fixture() -> Vec<u8> {
    let mut elf = ps2sdk_irx_fixture();
    put_u16(&mut elf, 16, 2);
    put_u32(&mut elf, 24, 0x1_0000);
    put_u32(&mut elf, 32, 0);
    put_u16(&mut elf, 46, 0);
    put_u16(&mut elf, 48, 0);
    put_u32(&mut elf, PHOFF + 32 + 8, 0x1_0000);
    put_u32(&mut elf, PHOFF + 32 + 12, 0x1_0000);
    put_u32(&mut elf, IOPMOD_OFFSET, 0x1_0040);
    put_u32(&mut elf, IOPMOD_OFFSET + 4, 0x1_0000);
    put_u32(&mut elf, IOPMOD_OFFSET + 8, 0x1_0030);
    put_u32(&mut elf, IMAGE_OFFSET + 0x40, 0x1_0048);
    put_u32(&mut elf, IMAGE_OFFSET + 0xa4, 0x1_0010);
    elf
}

#[allow(clippy::too_many_arguments)]
fn program_header(
    elf: &mut [u8],
    index: usize,
    kind: u32,
    offset: usize,
    address: u32,
    file_size: usize,
    memory_size: usize,
    alignment: u32,
) {
    let at = PHOFF + index * 32;
    put_u32(elf, at, kind);
    put_u32(elf, at + 4, offset);
    put_u32(elf, at + 8, address);
    put_u32(elf, at + 12, address);
    put_u32(elf, at + 16, file_size);
    put_u32(elf, at + 20, memory_size);
    put_u32(elf, at + 24, 7);
    put_u32(elf, at + 28, alignment);
}

#[allow(clippy::too_many_arguments)]
fn section_header(
    elf: &mut [u8],
    index: usize,
    kind: u32,
    flags: u32,
    address: u32,
    offset: usize,
    size: usize,
    link: u32,
    info: u32,
    alignment: u32,
    entry_size: u32,
) {
    let at = SHOFF + index * 40;
    put_u32(elf, at + 4, kind);
    put_u32(elf, at + 8, flags);
    put_u32(elf, at + 12, address);
    put_u32(elf, at + 16, offset);
    put_u32(elf, at + 20, size);
    put_u32(elf, at + 24, link);
    put_u32(elf, at + 28, info);
    put_u32(elf, at + 32, alignment);
    put_u32(elf, at + 36, entry_size);
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut [u8], offset: usize, value: impl TryInto<u32>) {
    let value = value.try_into().ok().unwrap();
    output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn word(input: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(input[offset..offset + 4].try_into().unwrap())
}

#[test]
fn ps2sdk_layout_relocates_to_byte_exact_image_and_describes_links() {
    let irx = IrxModule::parse("fixture.irx", &ps2sdk_irx_fixture()).unwrap();
    assert_eq!(irx.description().name, "fixture");
    assert_eq!(irx.description().version, 0x0102);
    assert_eq!(irx.description().variant, IrxVariant::RelocatableV2);
    assert_eq!(irx.load_ranges().len(), 1);
    assert_eq!(irx.allocation_size(), 0xd0);

    let mut target = FixtureTarget::new(MemoryRange {
        start: 0x8000,
        end: 0xb000,
    });
    let loaded = irx.load_into(0x9000, &mut target).unwrap();
    assert_eq!(target.writes, 1);
    assert_eq!(target.address, 0x9000);
    assert_eq!(target.image.len(), MEMORY_SIZE);
    assert_eq!(word(&target.image, 0x00), 0x2402_9001);
    assert_eq!(word(&target.image, 0x04), 0x0000_9020);
    assert_eq!(word(&target.image, 0x08), 0x0800_2410);
    assert_eq!(word(&target.image, 0x0c), 0x3c08_0001);
    assert_eq!(word(&target.image, 0x10), 0x2508_9040);
    assert_eq!(word(&target.image, 0x14), 0x2442_9030);
    assert_eq!(word(&target.image, 0x18), 0x3c09_0001);
    assert_eq!(word(&target.image, 0x20), 0x3c0a_0001);
    assert_eq!(word(&target.image, 0x40), 0x0000_9048);
    assert_eq!(word(&target.image, 0xa4), 0x0000_9010);
    assert!(target.image[IMAGE_SIZE..].iter().all(|byte| *byte == 0));

    assert_eq!(loaded.entry, 0x9000);
    assert_eq!(loaded.global_pointer, 0x9030);
    assert_eq!(loaded.module_id, Some(0x9040));
    assert_eq!(loaded.resident_state, ResidentState::Unstarted);
    assert_eq!(loaded.imports.len(), 1);
    assert_eq!(loaded.imports[0].name, "sysclib");
    assert_eq!(loaded.imports[0].version, 0x0103);
    assert_eq!(loaded.imports[0].stubs[0].ordinal, 12);
    assert_eq!(loaded.imports[0].stubs[0].address, 0x9074);
    assert_eq!(loaded.exports.len(), 1);
    assert_eq!(loaded.exports[0].name, "fixture");
    assert_eq!(loaded.exports[0].functions, [0x9010]);
}

#[test]
fn version_one_and_stripped_section_tables_are_identified() {
    let mut v1 = ps2sdk_irx_fixture();
    put_u16(&mut v1, 16, 0xff80);
    assert_eq!(
        IrxModule::parse("v1.irx", &v1)
            .unwrap()
            .description()
            .variant,
        IrxVariant::RelocatableV1
    );

    put_u32(&mut v1, IOPMOD_OFFSET, u32::MAX);
    v1[IOPMOD_OFFSET + 26] = 0;
    put_u16(&mut v1, IOPMOD_OFFSET + 24, 0);
    let legacy = IrxModule::parse("legacy.irx", &v1).unwrap();
    assert_eq!(legacy.description().name, "legacy");
    assert_eq!(legacy.description().version, 0);

    put_u32(&mut v1, IOPMOD_OFFSET + 8, 0x8030);
    let module = IrxModule::parse("legacy-gp.irx", &v1).unwrap();
    let mut target = FixtureTarget::new(MemoryRange {
        start: 0x9000,
        end: 0x9000 + u32::try_from(MEMORY_SIZE).unwrap(),
    });
    let loaded = module.load_into(0x9000, &mut target).unwrap();
    assert_eq!(loaded.global_pointer, 0x1_1030);
    assert_eq!(target.writes, 1);

    let mut stripped = ps2sdk_irx_fixture();
    put_u32(&mut stripped, 32, 0);
    put_u16(&mut stripped, 46, 0);
    put_u16(&mut stripped, 48, 0);
    let module = IrxModule::parse("stripped.irx", &stripped).unwrap();
    assert_eq!(module.description().name, "fixture");
}

#[test]
fn fixed_address_iop_executable_loads_only_at_its_declared_range() {
    let module = IrxModule::parse("fixed.elf", &fixed_executable_fixture()).unwrap();
    assert_eq!(module.description().variant, IrxVariant::Executable);
    assert_eq!(module.preferred_address(), 0x1_0000);
    let mut target = FixtureTarget::new(MemoryRange {
        start: 0xf000,
        end: 0x1_2000,
    });
    let loaded = module.load_into(0x1_0000, &mut target).unwrap();
    assert_eq!(loaded.entry, 0x1_0000);
    assert_eq!(loaded.global_pointer, 0x1_0030);
    assert_eq!(loaded.exports[0].functions, [0x1_0010]);

    let mut wrong = FixtureTarget::new(MemoryRange {
        start: 0x2_0000,
        end: 0x2_2000,
    });
    assert_eq!(
        module.load_into(0x2_0000, &mut wrong).unwrap_err().kind,
        IrxErrorKind::InvalidLoadAddress
    );
    assert_eq!(wrong.writes, 0);
}

#[test]
fn malformed_pairs_symbols_and_unsupported_relocations_are_rejected() {
    let mut pair = ps2sdk_irx_fixture();
    put_u32(&mut pair, REL_OFFSET + 4 * 8 + 4, 2);
    assert_eq!(
        IrxModule::parse("pair.irx", &pair).unwrap_err().kind,
        IrxErrorKind::MalformedRelocationPair
    );

    let mut sony_pair = ps2sdk_irx_fixture();
    put_u32(&mut sony_pair, REL_OFFSET + 7 * 8 + 4, 6);
    assert_eq!(
        IrxModule::parse("sony.irx", &sony_pair).unwrap_err().kind,
        IrxErrorKind::MalformedRelocationPair
    );

    let mut symbol = ps2sdk_irx_fixture();
    put_u32(&mut symbol, REL_OFFSET + 4, 0x0101);
    assert_eq!(
        IrxModule::parse("symbol.irx", &symbol).unwrap_err().kind,
        IrxErrorKind::InvalidSymbol
    );

    let mut unsupported = ps2sdk_irx_fixture();
    put_u32(&mut unsupported, REL_OFFSET + 4, 7);
    assert_eq!(
        IrxModule::parse("gprel.irx", &unsupported)
            .unwrap_err()
            .kind,
        IrxErrorKind::UnsupportedRelocation(7)
    );
}

#[test]
fn overlapping_ranges_arithmetic_overflow_and_limits_are_rejected() {
    let mut overlap = ps2sdk_irx_fixture();
    put_u16(&mut overlap, 44, 3);
    program_header(&mut overlap, 2, 1, IMAGE_OFFSET, 0x80, 0x20, 0x40, 16);
    assert_eq!(
        IrxModule::parse("overlap.irx", &overlap).unwrap_err().kind,
        IrxErrorKind::OverlappingRanges
    );

    let mut overflow = ps2sdk_irx_fixture();
    put_u16(&mut overflow, 16, 2);
    put_u32(&mut overflow, PHOFF + 32 + 8, 0xffff_ff80_u32);
    assert_eq!(
        IrxModule::parse("overflow.irx", &overflow)
            .unwrap_err()
            .kind,
        IrxErrorKind::Overflow
    );

    let fixture = ps2sdk_irx_fixture();
    for (limits, expected) in [
        (
            IrxLimits {
                max_input_bytes: fixture.len() - 1,
                ..IrxLimits::default()
            },
            "input bytes",
        ),
        (
            IrxLimits {
                max_image_bytes: MEMORY_SIZE - 1,
                ..IrxLimits::default()
            },
            "image bytes",
        ),
        (
            IrxLimits {
                max_relocations: 2,
                ..IrxLimits::default()
            },
            "relocations",
        ),
    ] {
        assert_eq!(
            IrxModule::parse_with_limits("limited.irx", &fixture, limits)
                .unwrap_err()
                .kind,
            IrxErrorKind::LimitExceeded(expected)
        );
    }
}

#[test]
fn malformed_modules_never_write_to_the_abstract_target() {
    let module = IrxModule::parse("fixture.irx", &ps2sdk_irx_fixture()).unwrap();
    let mut too_small = FixtureTarget::new(MemoryRange {
        start: 0x9000,
        end: 0x9080,
    });
    assert_eq!(
        module.load_into(0x9000, &mut too_small).unwrap_err().kind,
        IrxErrorKind::TargetRange
    );
    assert_eq!(too_small.writes, 0);

    let mut malformed_link = ps2sdk_irx_fixture();
    put_u32(&mut malformed_link, IMAGE_OFFSET + 0x74, 0xdead_beef_u32);
    let module = IrxModule::parse("bad-link.irx", &malformed_link).unwrap();
    let mut target = FixtureTarget::new(MemoryRange {
        start: 0x8000,
        end: 0xb000,
    });
    assert_eq!(
        module.load_into(0x9000, &mut target).unwrap_err().kind,
        IrxErrorKind::InvalidLinkTable
    );
    assert_eq!(target.writes, 0);

    target.fail = true;
    assert!(matches!(
        IrxModule::parse("fixture.irx", &ps2sdk_irx_fixture())
            .unwrap()
            .load_into(0x9000, &mut target)
            .unwrap_err()
            .kind,
        IrxErrorKind::Target(_)
    ));
}

#[test]
fn truncated_headers_sections_and_random_inputs_never_panic() {
    let fixture = ps2sdk_irx_fixture();
    for length in 0..fixture.len() {
        let _ = IrxModule::parse("truncated.irx", &fixture[..length]);
    }

    let limits = IrxLimits {
        max_input_bytes: 4096,
        max_program_headers: 8,
        max_section_headers: 32,
        max_image_bytes: 2048,
        max_relocations: 64,
        max_link_tables: 16,
        max_link_entries: 64,
        max_name_bytes: 32,
        max_alignment: 256,
    };
    let mut state = 0xbb67_ae85_84ca_a73b_u64;
    for length in 0..2048 {
        let mut input = vec![0_u8; length];
        for byte in &mut input {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state.to_le_bytes()[0];
        }
        let _ = IrxModule::parse_with_limits("arbitrary.irx", &input, limits);
    }
}
