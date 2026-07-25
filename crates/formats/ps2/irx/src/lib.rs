// SPDX-License-Identifier: LGPL-2.1-or-later
//! Safe parsing and relocation of Sony IOP ELF/IRX modules.
//!
//! Parsing and relocation complete in owned staging memory before the supplied
//! target sees a write. This crate has no dependency on an IOP BIOS or machine.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use thiserror::Error;

const ELF_HEADER_SIZE: usize = 52;
const PROGRAM_HEADER_SIZE: usize = 32;
const SECTION_HEADER_SIZE: usize = 40;
const SYMBOL_SIZE: usize = 16;
const REL_SIZE: usize = 8;
const PT_LOAD: u32 = 1;
const PT_SCE_IOPMOD: u32 = 0x7000_0080;
const SHT_NOBITS: u32 = 8;
const SHT_REL: u32 = 9;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const IMPORT_MAGIC: u32 = 0x41e0_0000;
const EXPORT_MAGIC: u32 = 0x41c0_0000;

/// Resource limits for one IOP ELF/IRX module.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IrxLimits {
    /// Maximum source file size.
    pub max_input_bytes: usize,
    /// Maximum number of ELF program headers.
    pub max_program_headers: usize,
    /// Maximum number of ELF section headers.
    pub max_section_headers: usize,
    /// Maximum contiguous target allocation.
    pub max_image_bytes: usize,
    /// Maximum relocation entries.
    pub max_relocations: usize,
    /// Maximum import and export tables in aggregate.
    pub max_link_tables: usize,
    /// Maximum import stubs and export pointers in aggregate.
    pub max_link_entries: usize,
    /// Maximum module or library name length.
    pub max_name_bytes: usize,
    /// Maximum accepted segment alignment.
    pub max_alignment: u32,
}

impl Default for IrxLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_program_headers: 32,
            max_section_headers: 4096,
            max_image_bytes: 2 * 1024 * 1024,
            max_relocations: 262_144,
            max_link_tables: 4096,
            max_link_entries: 65_536,
            max_name_bytes: 127,
            max_alignment: 4096,
        }
    }
}

/// Supported Sony IOP ELF variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrxVariant {
    /// Original relocatable IRX (`ET_SCE_IOPRELEXEC`).
    RelocatableV1,
    /// Revised relocatable IRX (`ET_SCE_IOPRELEXEC2`).
    RelocatableV2,
    /// Fixed-address IOP executable (`ET_EXEC`).
    Executable,
}

impl IrxVariant {
    /// Reports whether the module is relocated at load time.
    #[must_use]
    pub const fn is_relocatable(self) -> bool {
        matches!(self, Self::RelocatableV1 | Self::RelocatableV2)
    }
}

/// Runtime residency state returned by an IOP module entry point.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ResidentState {
    /// The module has been loaded but not started.
    #[default]
    Unstarted,
    /// `MODULE_RESIDENT_END`.
    Resident,
    /// `MODULE_NO_RESIDENT_END`.
    NotResident,
    /// `MODULE_REMOVABLE_END`.
    Removable,
}

/// One loadable ELF range.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadRange {
    /// Byte offset in the source IRX.
    pub source_offset: u32,
    /// Link-time virtual address.
    pub virtual_address: u32,
    /// Bytes copied from the source.
    pub file_size: u32,
    /// Bytes occupied in target memory, including zeroed BSS.
    pub memory_size: u32,
    /// ELF program flags.
    pub flags: u32,
    /// Required address alignment.
    pub alignment: u32,
}

/// Contiguous allocation selected for a loaded module.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Allocation {
    /// First target address.
    pub address: u32,
    /// Total byte length, including gaps and BSS.
    pub size: u32,
    /// Required starting-address alignment.
    pub alignment: u32,
}

/// Module metadata available before execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleDescription {
    /// Module identifier name.
    pub name: String,
    /// Packed major/minor module version.
    pub version: u16,
    /// ELF/IRX variant.
    pub variant: IrxVariant,
    /// Link-time entry address or offset.
    pub entry: u32,
    /// Link-time global-pointer address or offset.
    pub global_pointer: u32,
    /// Declared text byte count.
    pub text_size: u32,
    /// Declared initialized-data byte count.
    pub data_size: u32,
    /// Declared BSS byte count.
    pub bss_size: u32,
}

/// One imported function stub.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportStub {
    /// Imported ordinal.
    pub ordinal: u16,
    /// Address of the two-word caller stub.
    pub address: u32,
}

/// One IOP import library table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportLibrary {
    /// Eight-byte IOP library name, normalized only by null termination.
    pub name: String,
    /// Required packed major/minor version.
    pub version: u16,
    /// Import-table mode word.
    pub mode: u16,
    /// Address of the table header.
    pub table_address: u32,
    /// Imported ordinal stubs in source order.
    pub stubs: Vec<ImportStub>,
}

/// One IOP export library table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportLibrary {
    /// Eight-byte IOP library name, normalized only by null termination.
    pub name: String,
    /// Provided packed major/minor version.
    pub version: u16,
    /// Export-table mode word.
    pub mode: u16,
    /// Address of the table header.
    pub table_address: u32,
    /// Exported function addresses indexed by ordinal.
    pub functions: Vec<u32>,
}

/// Result of loading and relocating one module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedModule {
    /// Module name and static layout description.
    pub description: ModuleDescription,
    /// Absolute entry point.
    pub entry: u32,
    /// Absolute global pointer.
    pub global_pointer: u32,
    /// Absolute module-ID structure, when present.
    pub module_id: Option<u32>,
    /// Target allocation occupied by the module.
    pub allocation: Allocation,
    /// Imports found in the relocated image.
    pub imports: Vec<ImportLibrary>,
    /// Exports found in the relocated image.
    pub exports: Vec<ExportLibrary>,
    /// Initial lifecycle state.
    pub resident_state: ResidentState,
}

/// Half-open target memory range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryRange {
    /// Inclusive first address.
    pub start: u32,
    /// Exclusive ending address.
    pub end: u32,
}

impl MemoryRange {
    fn contains(self, address: u32, size: usize) -> bool {
        u32::try_from(size)
            .ok()
            .and_then(|size| address.checked_add(size))
            .is_some_and(|end| address >= self.start && end <= self.end)
    }
}

/// Error returned by an abstract target memory implementation.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct TargetError {
    message: String,
}

impl TargetError {
    /// Constructs a target diagnostic.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns the diagnostic text.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Abstract destination for a completely staged module image.
pub trait TargetMemory {
    /// Returns the target range available to module allocations.
    fn range(&self) -> MemoryRange;

    /// Writes one complete, validated module image.
    ///
    /// # Errors
    ///
    /// Returns [`TargetError`] when the target cannot accept the write.
    fn write_image(&mut self, address: u32, image: &[u8]) -> Result<(), TargetError>;
}

/// Specific IOP module failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum IrxErrorKind {
    /// The input ended before a complete structure.
    #[error("truncated input")]
    Truncated,
    /// The ELF identification, class, byte order, or ABI version was invalid.
    #[error("invalid ELF identification")]
    InvalidIdentification,
    /// The ELF type is not a supported IOP executable variant.
    #[error("unsupported ELF type {0:#06x}")]
    UnsupportedType(u16),
    /// The ELF machine is not 32-bit MIPS.
    #[error("unsupported ELF machine {0}")]
    UnsupportedMachine(u16),
    /// A fixed-size ELF table used an unsupported entry size.
    #[error("invalid ELF table entry size")]
    InvalidEntrySize,
    /// An integer offset, address, or relocation calculation overflowed.
    #[error("integer or address overflow")]
    Overflow,
    /// A configured resource limit was exceeded.
    #[error("resource limit exceeded: {0}")]
    LimitExceeded(&'static str),
    /// Required IOP module or load program headers were missing or duplicated.
    #[error("invalid IOP program-header layout")]
    InvalidProgramLayout,
    /// Two loadable memory ranges overlap.
    #[error("overlapping loadable ranges")]
    OverlappingRanges,
    /// A segment, section, or relocation offset was outside its declared range.
    #[error("range lies outside its containing image")]
    OutOfRange,
    /// A section header or symbol table was inconsistent.
    #[error("invalid ELF section or symbol table")]
    InvalidSection,
    /// A relocation references a non-null or invalid final-IRX symbol.
    #[error("invalid symbol index in final IRX relocation")]
    InvalidSymbol,
    /// A relocation type is not supported by the IOP loader contract.
    #[error("unsupported IOP relocation {0}")]
    UnsupportedRelocation(u8),
    /// A HI16 or Sony relocation did not have its required paired entry.
    #[error("malformed paired relocation")]
    MalformedRelocationPair,
    /// A Sony HI16 chain was malformed, cyclic, unaligned, or out of range.
    #[error("malformed Sony HI16 chain")]
    MalformedRelocationChain,
    /// A relocation result could not be represented by its instruction field.
    #[error("relocation result overflow")]
    RelocationOverflow,
    /// Module metadata was missing or inconsistent with the load image.
    #[error("invalid IOP module metadata")]
    InvalidModuleMetadata,
    /// An import or export table was malformed.
    #[error("invalid IOP import/export table")]
    InvalidLinkTable,
    /// A requested base does not satisfy a fixed address or alignment.
    #[error("invalid module load address")]
    InvalidLoadAddress,
    /// The target range cannot contain the complete allocation.
    #[error("module allocation is outside target memory")]
    TargetRange,
    /// The target rejected the staged image.
    #[error("target memory failure: {0}")]
    Target(String),
}

/// Structured IOP module diagnostic.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{origin}:{offset}: {kind}")]
pub struct IrxError {
    /// Logical module origin.
    pub origin: String,
    /// Source-file offset associated with the failure.
    pub offset: usize,
    /// Specific failure.
    pub kind: IrxErrorKind,
}

impl IrxError {
    fn new(origin: &str, offset: usize, kind: IrxErrorKind) -> Self {
        Self {
            origin: origin.to_owned(),
            offset,
            kind,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ProgramHeader {
    kind: u32,
    offset: u32,
    virtual_address: u32,
    file_size: u32,
    memory_size: u32,
    flags: u32,
    alignment: u32,
}

#[derive(Clone, Copy, Debug)]
struct SectionHeader {
    kind: u32,
    offset: u32,
    size: u32,
    link: u32,
    info: u32,
    alignment: u32,
    entry_size: u32,
}

#[derive(Clone, Copy, Debug)]
enum RelocationKind {
    Half,
    Word,
    Jump,
    High { low_offset: u32 },
    Low,
    SonyHigh { addend: u32 },
}

#[derive(Clone, Copy, Debug)]
struct Relocation {
    offset: u32,
    kind: RelocationKind,
    source_offset: usize,
}

#[derive(Clone, Debug)]
struct RawMetadata {
    module_id: Option<u32>,
    entry: u32,
    global_pointer: u32,
}

/// Parsed, owned IOP module ready to be relocated into a target.
#[derive(Clone, Debug)]
pub struct IrxModule {
    origin: String,
    limits: IrxLimits,
    description: ModuleDescription,
    raw_metadata: RawMetadata,
    ranges: Vec<LoadRange>,
    preferred_address: u32,
    alignment: u32,
    image: Vec<u8>,
    relocations: Vec<Relocation>,
}

impl IrxModule {
    /// Parses a module with conservative defaults.
    ///
    /// # Errors
    ///
    /// Returns [`IrxError`] for malformed or unsupported input.
    pub fn parse(origin: impl AsRef<str>, input: &[u8]) -> Result<Self, IrxError> {
        Self::parse_with_limits(origin, input, IrxLimits::default())
    }

    /// Parses a module with caller-supplied resource bounds.
    ///
    /// # Errors
    ///
    /// Returns [`IrxError`] for malformed or unsupported input.
    pub fn parse_with_limits(
        origin: impl AsRef<str>,
        input: &[u8],
        limits: IrxLimits,
    ) -> Result<Self, IrxError> {
        Parser::new(origin.as_ref(), input, limits).parse()
    }

    /// Returns static module metadata.
    #[must_use]
    pub const fn description(&self) -> &ModuleDescription {
        &self.description
    }

    /// Returns validated loadable ranges in ascending virtual-address order.
    #[must_use]
    pub fn load_ranges(&self) -> &[LoadRange] {
        &self.ranges
    }

    /// Returns the fixed address for an executable, or zero for a relocatable IRX.
    #[must_use]
    pub const fn preferred_address(&self) -> u32 {
        self.preferred_address
    }

    /// Returns the required allocation alignment.
    #[must_use]
    pub const fn alignment(&self) -> u32 {
        self.alignment
    }

    /// Returns the contiguous allocation byte count.
    #[must_use]
    pub fn allocation_size(&self) -> u32 {
        u32::try_from(self.image.len()).unwrap_or(u32::MAX)
    }

    /// Relocates and writes one complete module image to abstract target memory.
    ///
    /// All module validation, relocation, and import/export discovery occur in
    /// owned staging memory before the single target write.
    ///
    /// # Errors
    ///
    /// Returns [`IrxError`] if the address, relocation, link tables, target
    /// range, or target write is invalid.
    pub fn load_into<T: TargetMemory>(
        &self,
        address: u32,
        target: &mut T,
    ) -> Result<LoadedModule, IrxError> {
        if address % self.alignment != 0
            || (!self.description.variant.is_relocatable() && address != self.preferred_address)
        {
            return Err(self.error(0, IrxErrorKind::InvalidLoadAddress));
        }
        if !target.range().contains(address, self.image.len()) {
            return Err(self.error(0, IrxErrorKind::TargetRange));
        }
        let mut image = self.image.clone();
        let delta = if self.description.variant.is_relocatable() {
            address
        } else {
            0
        };
        for relocation in &self.relocations {
            apply_relocation(
                &self.origin,
                &mut image,
                self.preferred_address,
                delta,
                *relocation,
            )?;
        }
        let entry = self
            .raw_metadata
            .entry
            .checked_add(delta)
            .ok_or_else(|| self.error(0, IrxErrorKind::Overflow))?;
        let global_pointer = self
            .raw_metadata
            .global_pointer
            .checked_add(delta)
            .ok_or_else(|| self.error(0, IrxErrorKind::Overflow))?;
        if !(MemoryRange {
            start: address,
            end: address
                .checked_add(self.allocation_size())
                .ok_or_else(|| self.error(0, IrxErrorKind::Overflow))?,
        })
        .contains(entry, 4)
            || !target.range().contains(global_pointer, 1)
        {
            return Err(self.error(0, IrxErrorKind::TargetRange));
        }
        let module_id = match self.raw_metadata.module_id {
            Some(value) => Some(
                value
                    .checked_add(delta)
                    .ok_or_else(|| self.error(0, IrxErrorKind::Overflow))?,
            ),
            None => None,
        };
        let (imports, exports) = parse_link_tables(&self.origin, &image, address, self.limits)?;
        target
            .write_image(address, &image)
            .map_err(|error| self.error(0, IrxErrorKind::Target(error.to_string())))?;
        Ok(LoadedModule {
            description: self.description.clone(),
            entry,
            global_pointer,
            module_id,
            allocation: Allocation {
                address,
                size: self.allocation_size(),
                alignment: self.alignment,
            },
            imports,
            exports,
            resident_state: ResidentState::Unstarted,
        })
    }

    fn error(&self, offset: usize, kind: IrxErrorKind) -> IrxError {
        IrxError::new(&self.origin, offset, kind)
    }
}

struct Parser<'a> {
    origin: &'a str,
    input: &'a [u8],
    limits: IrxLimits,
}

impl<'a> Parser<'a> {
    const fn new(origin: &'a str, input: &'a [u8], limits: IrxLimits) -> Self {
        Self {
            origin,
            input,
            limits,
        }
    }

    fn parse(self) -> Result<IrxModule, IrxError> {
        if self.input.len() > self.limits.max_input_bytes {
            return Err(self.error(0, IrxErrorKind::LimitExceeded("input bytes")));
        }
        let ident = self
            .input
            .get(..16)
            .ok_or_else(|| self.error(self.input.len(), IrxErrorKind::Truncated))?;
        if &ident[..7] != b"\x7fELF\x01\x01\x01" {
            return Err(self.error(0, IrxErrorKind::InvalidIdentification));
        }
        if self.input.len() < ELF_HEADER_SIZE {
            return Err(self.error(self.input.len(), IrxErrorKind::Truncated));
        }
        let variant = match self.u16(16)? {
            0xff80 => IrxVariant::RelocatableV1,
            0xff81 => IrxVariant::RelocatableV2,
            2 => IrxVariant::Executable,
            other => return Err(self.error(16, IrxErrorKind::UnsupportedType(other))),
        };
        let machine = self.u16(18)?;
        if machine != 8 {
            return Err(self.error(18, IrxErrorKind::UnsupportedMachine(machine)));
        }
        if self.u32(20)? != 1 || self.u16(40)? as usize != ELF_HEADER_SIZE {
            return Err(self.error(20, IrxErrorKind::InvalidIdentification));
        }
        let elf_entry = self.u32(24)?;
        let phoff = self.u32(28)? as usize;
        let shoff = self.u32(32)? as usize;
        let phentsize = self.u16(42)? as usize;
        let phnum = self.u16(44)? as usize;
        let shentsize = self.u16(46)? as usize;
        let shnum = self.u16(48)? as usize;
        if phnum == 0 || phnum > self.limits.max_program_headers {
            return Err(self.error(44, IrxErrorKind::LimitExceeded("program headers")));
        }
        if phentsize != PROGRAM_HEADER_SIZE {
            return Err(self.error(42, IrxErrorKind::InvalidEntrySize));
        }
        self.table(phoff, phnum, PROGRAM_HEADER_SIZE)?;
        if shnum > self.limits.max_section_headers {
            return Err(self.error(48, IrxErrorKind::LimitExceeded("section headers")));
        }
        if shnum == 0 {
            if shoff != 0 {
                return Err(self.error(32, IrxErrorKind::InvalidSection));
            }
        } else {
            if shentsize != SECTION_HEADER_SIZE {
                return Err(self.error(46, IrxErrorKind::InvalidEntrySize));
            }
            self.table(shoff, shnum, SECTION_HEADER_SIZE)?;
        }

        let headers = self.program_headers(phoff, phnum)?;
        let metadata_header = headers
            .iter()
            .filter(|header| header.kind == PT_SCE_IOPMOD)
            .copied()
            .collect::<Vec<_>>();
        let mut loads = headers
            .iter()
            .filter(|header| header.kind == PT_LOAD)
            .copied()
            .collect::<Vec<_>>();
        if metadata_header.len() != 1 || loads.is_empty() {
            return Err(self.error(phoff, IrxErrorKind::InvalidProgramLayout));
        }
        loads.sort_by_key(|header| header.virtual_address);
        self.validate_loads(&loads, variant)?;
        let preferred_address = loads[0].virtual_address;
        if variant.is_relocatable() && preferred_address != 0 {
            return Err(self.error(phoff, IrxErrorKind::InvalidProgramLayout));
        }
        let image_end = loads
            .iter()
            .map(|header| header.virtual_address + header.memory_size)
            .max()
            .expect("nonempty loads");
        let image_size = image_end
            .checked_sub(preferred_address)
            .ok_or_else(|| self.error(phoff, IrxErrorKind::Overflow))?
            as usize;
        if image_size > self.limits.max_image_bytes {
            return Err(self.error(phoff, IrxErrorKind::LimitExceeded("image bytes")));
        }
        let mut image = vec![0_u8; image_size];
        for header in &loads {
            let destination = (header.virtual_address - preferred_address) as usize;
            let file_size = header.file_size as usize;
            image[destination..destination + file_size]
                .copy_from_slice(self.slice(header.offset as usize, file_size)?);
        }
        let alignment = loads
            .iter()
            .map(|header| header.alignment.max(1))
            .max()
            .expect("nonempty loads");
        let (raw_metadata, description) = self.module_metadata(
            metadata_header[0],
            &image,
            preferred_address,
            variant,
            elf_entry,
        )?;
        let sections = self.section_headers(shoff, shnum)?;
        self.validate_symbols(&sections)?;
        let relocations = self.relocations(&sections, variant, image_size, preferred_address)?;
        let ranges = loads
            .into_iter()
            .map(|header| LoadRange {
                source_offset: header.offset,
                virtual_address: header.virtual_address,
                file_size: header.file_size,
                memory_size: header.memory_size,
                flags: header.flags,
                alignment: header.alignment,
            })
            .collect();
        Ok(IrxModule {
            origin: self.origin.to_owned(),
            limits: self.limits,
            description,
            raw_metadata,
            ranges,
            preferred_address,
            alignment,
            image,
            relocations,
        })
    }

    fn program_headers(&self, offset: usize, count: usize) -> Result<Vec<ProgramHeader>, IrxError> {
        let mut headers = Vec::with_capacity(count);
        for index in 0..count {
            let at = offset + index * PROGRAM_HEADER_SIZE;
            headers.push(ProgramHeader {
                kind: self.u32(at)?,
                offset: self.u32(at + 4)?,
                virtual_address: self.u32(at + 8)?,
                file_size: self.u32(at + 16)?,
                memory_size: self.u32(at + 20)?,
                flags: self.u32(at + 24)?,
                alignment: self.u32(at + 28)?,
            });
        }
        Ok(headers)
    }

    fn validate_loads(&self, loads: &[ProgramHeader], variant: IrxVariant) -> Result<(), IrxError> {
        let mut prior_end = None;
        for header in loads {
            if header.file_size > header.memory_size {
                return Err(self.error(header.offset as usize, IrxErrorKind::OutOfRange));
            }
            self.slice(header.offset as usize, header.file_size as usize)?;
            let end = header
                .virtual_address
                .checked_add(header.memory_size)
                .ok_or_else(|| self.error(header.offset as usize, IrxErrorKind::Overflow))?;
            if prior_end.is_some_and(|prior| header.virtual_address < prior) {
                return Err(self.error(header.offset as usize, IrxErrorKind::OverlappingRanges));
            }
            prior_end = Some(end);
            let alignment = header.alignment;
            if alignment != 0
                && (!alignment.is_power_of_two()
                    || alignment > self.limits.max_alignment
                    || header.virtual_address % alignment != 0)
            {
                return Err(self.error(header.offset as usize, IrxErrorKind::InvalidProgramLayout));
            }
            if variant.is_relocatable()
                && header.virtual_address >= self.limits.max_image_bytes as u32
            {
                return Err(self.error(header.offset as usize, IrxErrorKind::InvalidProgramLayout));
            }
        }
        Ok(())
    }

    fn module_metadata(
        &self,
        header: ProgramHeader,
        image: &[u8],
        preferred_address: u32,
        variant: IrxVariant,
        elf_entry: u32,
    ) -> Result<(RawMetadata, ModuleDescription), IrxError> {
        if header.file_size < 26 {
            return Err(self.error(header.offset as usize, IrxErrorKind::InvalidModuleMetadata));
        }
        let bytes = self.slice(header.offset as usize, header.file_size as usize)?;
        let module_id_raw = read_u32(bytes, 0).expect("metadata length");
        let entry = read_u32(bytes, 4).expect("metadata length");
        let global_pointer = read_u32(bytes, 8).expect("metadata length");
        let text_size = read_u32(bytes, 12).expect("metadata length");
        let data_size = read_u32(bytes, 16).expect("metadata length");
        let bss_size = read_u32(bytes, 20).expect("metadata length");
        let header_version = read_u16(bytes, 24).expect("metadata length");
        if entry != elf_entry {
            return Err(self.error(
                header.offset as usize + 4,
                IrxErrorKind::InvalidModuleMetadata,
            ));
        }
        if image_offset(entry, preferred_address, image.len()).is_none_or(|offset| offset % 4 != 0)
        {
            return Err(self.error(
                header.offset as usize + 4,
                IrxErrorKind::InvalidModuleMetadata,
            ));
        }
        let declared = text_size
            .checked_add(data_size)
            .and_then(|size| size.checked_add(bss_size))
            .ok_or_else(|| self.error(header.offset as usize + 12, IrxErrorKind::Overflow))?;
        if declared as usize > image.len() {
            return Err(self.error(
                header.offset as usize + 12,
                IrxErrorKind::InvalidModuleMetadata,
            ));
        }
        let header_name = parse_c_name(
            bytes.get(26..).unwrap_or_default(),
            self.limits.max_name_bytes,
            true,
        )
        .map_err(|kind| self.error(header.offset as usize + 26, kind))?;
        let (module_id, name, version) = if module_id_raw == u32::MAX {
            let Some(name) = header_name else {
                return Err(self.error(
                    header.offset as usize + 26,
                    IrxErrorKind::InvalidModuleMetadata,
                ));
            };
            (None, name, header_version)
        } else {
            let module_offset = image_offset(module_id_raw, preferred_address, image.len())
                .ok_or_else(|| {
                    self.error(header.offset as usize, IrxErrorKind::InvalidModuleMetadata)
                })?;
            let name_pointer = read_u32(image, module_offset).ok_or_else(|| {
                self.error(header.offset as usize, IrxErrorKind::InvalidModuleMetadata)
            })?;
            let id_version = read_u16(image, module_offset + 4).ok_or_else(|| {
                self.error(header.offset as usize, IrxErrorKind::InvalidModuleMetadata)
            })?;
            let name_offset = image_offset(name_pointer, preferred_address, image.len())
                .ok_or_else(|| {
                    self.error(header.offset as usize, IrxErrorKind::InvalidModuleMetadata)
                })?;
            let name = parse_c_name(&image[name_offset..], self.limits.max_name_bytes, false)
                .map_err(|_| {
                    self.error(header.offset as usize, IrxErrorKind::InvalidModuleMetadata)
                })?
                .ok_or_else(|| {
                    self.error(header.offset as usize, IrxErrorKind::InvalidModuleMetadata)
                })?;
            if header_version != 0 && header_version != id_version {
                return Err(self.error(
                    header.offset as usize + 24,
                    IrxErrorKind::InvalidModuleMetadata,
                ));
            }
            (Some(module_id_raw), name, id_version)
        };
        Ok((
            RawMetadata {
                module_id,
                entry,
                global_pointer,
            },
            ModuleDescription {
                name,
                version,
                variant,
                entry,
                global_pointer,
                text_size,
                data_size,
                bss_size,
            },
        ))
    }

    fn section_headers(&self, offset: usize, count: usize) -> Result<Vec<SectionHeader>, IrxError> {
        let mut headers = Vec::with_capacity(count);
        for index in 0..count {
            let at = offset + index * SECTION_HEADER_SIZE;
            let header = SectionHeader {
                kind: self.u32(at + 4)?,
                offset: self.u32(at + 16)?,
                size: self.u32(at + 20)?,
                link: self.u32(at + 24)?,
                info: self.u32(at + 28)?,
                alignment: self.u32(at + 32)?,
                entry_size: self.u32(at + 36)?,
            };
            if header.kind != SHT_NOBITS {
                self.slice(header.offset as usize, header.size as usize)?;
            }
            if header.alignment != 0 && !header.alignment.is_power_of_two() {
                return Err(self.error(at + 32, IrxErrorKind::InvalidSection));
            }
            headers.push(header);
        }
        Ok(headers)
    }

    fn validate_symbols(&self, sections: &[SectionHeader]) -> Result<(), IrxError> {
        for (index, section) in sections.iter().enumerate() {
            if section.kind != SHT_SYMTAB {
                continue;
            }
            if section.entry_size as usize != SYMBOL_SIZE
                || section.size as usize % SYMBOL_SIZE != 0
                || section.link as usize >= sections.len()
                || sections[section.link as usize].kind != SHT_STRTAB
            {
                return Err(self.error(index * SECTION_HEADER_SIZE, IrxErrorKind::InvalidSection));
            }
            let symbols = self.slice(section.offset as usize, section.size as usize)?;
            let strings = &sections[section.link as usize];
            let strings = self.slice(strings.offset as usize, strings.size as usize)?;
            for symbol in symbols.chunks_exact(SYMBOL_SIZE) {
                let name = read_u32(symbol, 0).expect("symbol") as usize;
                let shndx = read_u16(symbol, 14).expect("symbol") as usize;
                if name >= strings.len()
                    || (name != 0 && !strings[name..].contains(&0))
                    || (shndx < 0xff00 && shndx >= sections.len())
                {
                    return Err(self.error(section.offset as usize, IrxErrorKind::InvalidSection));
                }
            }
        }
        Ok(())
    }

    fn relocations(
        &self,
        sections: &[SectionHeader],
        variant: IrxVariant,
        image_size: usize,
        preferred_address: u32,
    ) -> Result<Vec<Relocation>, IrxError> {
        let mut output = Vec::new();
        for section in sections {
            if section.kind != SHT_REL {
                continue;
            }
            if !variant.is_relocatable() {
                return Err(self.error(section.offset as usize, IrxErrorKind::InvalidSection));
            }
            if section.entry_size as usize != REL_SIZE
                || section.size as usize % REL_SIZE != 0
                || section.info as usize >= sections.len()
            {
                return Err(self.error(section.offset as usize, IrxErrorKind::InvalidSection));
            }
            let count = section.size as usize / REL_SIZE;
            if output.len().saturating_add(count) > self.limits.max_relocations {
                return Err(self.error(
                    section.offset as usize,
                    IrxErrorKind::LimitExceeded("relocations"),
                ));
            }
            let data = self.slice(section.offset as usize, section.size as usize)?;
            let mut index = 0;
            while index < count {
                let at = index * REL_SIZE;
                let offset = read_u32(data, at).expect("relocation");
                let info = read_u32(data, at + 4).expect("relocation");
                if info >> 8 != 0 {
                    self.validate_relocation_symbol(sections, *section, info >> 8)?;
                    return Err(self.error(
                        section.offset as usize + at + 4,
                        IrxErrorKind::InvalidSymbol,
                    ));
                }
                let kind = (info & 0xff) as u8;
                if kind == 0 {
                    index += 1;
                    continue;
                }
                let image_at = image_offset(offset, preferred_address, image_size)
                    .filter(|offset| offset.checked_add(4).is_some_and(|end| end <= image_size))
                    .filter(|offset| offset % 4 == 0)
                    .ok_or_else(|| {
                        self.error(section.offset as usize + at, IrxErrorKind::OutOfRange)
                    })?;
                let relocation_kind = match kind {
                    1 => RelocationKind::Half,
                    2 => RelocationKind::Word,
                    4 => RelocationKind::Jump,
                    5 => {
                        let next = data.get(at + REL_SIZE..at + 2 * REL_SIZE).ok_or_else(|| {
                            self.error(
                                section.offset as usize + at,
                                IrxErrorKind::MalformedRelocationPair,
                            )
                        })?;
                        let next_info = read_u32(next, 4).expect("paired relocation");
                        if (next_info & 0xff) != 6 || next_info >> 8 != 0 {
                            return Err(self.error(
                                section.offset as usize + at,
                                IrxErrorKind::MalformedRelocationPair,
                            ));
                        }
                        let low_offset = read_u32(next, 0).expect("paired relocation");
                        image_offset(low_offset, preferred_address, image_size)
                            .filter(|offset| {
                                offset % 4 == 0
                                    && offset.checked_add(4).is_some_and(|end| end <= image_size)
                            })
                            .ok_or_else(|| {
                                self.error(
                                    section.offset as usize + at + REL_SIZE,
                                    IrxErrorKind::OutOfRange,
                                )
                            })?;
                        RelocationKind::High { low_offset }
                    }
                    6 => RelocationKind::Low,
                    250 => {
                        let next = data.get(at + REL_SIZE..at + 2 * REL_SIZE).ok_or_else(|| {
                            self.error(
                                section.offset as usize + at,
                                IrxErrorKind::MalformedRelocationPair,
                            )
                        })?;
                        let next_info = read_u32(next, 4).expect("Sony addend");
                        if (next_info & 0xff) != 251 || next_info >> 8 != 0 {
                            return Err(self.error(
                                section.offset as usize + at,
                                IrxErrorKind::MalformedRelocationPair,
                            ));
                        }
                        let addend = read_u32(next, 0).expect("Sony addend");
                        index += 1;
                        RelocationKind::SonyHigh { addend }
                    }
                    251 => {
                        return Err(self.error(
                            section.offset as usize + at,
                            IrxErrorKind::MalformedRelocationPair,
                        ));
                    }
                    other => {
                        return Err(self.error(
                            section.offset as usize + at + 4,
                            IrxErrorKind::UnsupportedRelocation(other),
                        ));
                    }
                };
                let _ = image_at;
                output.push(Relocation {
                    offset,
                    kind: relocation_kind,
                    source_offset: section.offset as usize + at,
                });
                index += 1;
            }
        }
        Ok(output)
    }

    fn validate_relocation_symbol(
        &self,
        sections: &[SectionHeader],
        relocation_section: SectionHeader,
        symbol: u32,
    ) -> Result<(), IrxError> {
        let symtab = sections
            .get(relocation_section.link as usize)
            .filter(|section| section.kind == SHT_SYMTAB)
            .ok_or_else(|| {
                self.error(
                    relocation_section.offset as usize,
                    IrxErrorKind::InvalidSymbol,
                )
            })?;
        if symtab.entry_size as usize != SYMBOL_SIZE
            || symbol as usize >= symtab.size as usize / SYMBOL_SIZE
        {
            return Err(self.error(
                relocation_section.offset as usize,
                IrxErrorKind::InvalidSymbol,
            ));
        }
        Ok(())
    }

    fn table(&self, offset: usize, count: usize, size: usize) -> Result<(), IrxError> {
        let bytes = count
            .checked_mul(size)
            .ok_or_else(|| self.error(offset, IrxErrorKind::Overflow))?;
        self.slice(offset, bytes).map(|_| ())
    }

    fn slice(&self, offset: usize, size: usize) -> Result<&'a [u8], IrxError> {
        let end = offset
            .checked_add(size)
            .ok_or_else(|| self.error(offset, IrxErrorKind::Overflow))?;
        self.input
            .get(offset..end)
            .ok_or_else(|| self.error(self.input.len(), IrxErrorKind::Truncated))
    }

    fn u16(&self, offset: usize) -> Result<u16, IrxError> {
        read_u16(self.input, offset)
            .ok_or_else(|| self.error(self.input.len(), IrxErrorKind::Truncated))
    }

    fn u32(&self, offset: usize) -> Result<u32, IrxError> {
        read_u32(self.input, offset)
            .ok_or_else(|| self.error(self.input.len(), IrxErrorKind::Truncated))
    }

    fn error(&self, offset: usize, kind: IrxErrorKind) -> IrxError {
        IrxError::new(self.origin, offset, kind)
    }
}

fn apply_relocation(
    origin: &str,
    image: &mut [u8],
    preferred_address: u32,
    delta: u32,
    relocation: Relocation,
) -> Result<(), IrxError> {
    let at = image_offset(relocation.offset, preferred_address, image.len())
        .ok_or_else(|| IrxError::new(origin, relocation.source_offset, IrxErrorKind::OutOfRange))?;
    let word = read_u32(image, at)
        .ok_or_else(|| IrxError::new(origin, relocation.source_offset, IrxErrorKind::OutOfRange))?;
    let patched = match relocation.kind {
        RelocationKind::Half => {
            let addend = i32::from((word as u16) as i16);
            let value = i64::from(delta) + i64::from(addend);
            if !(-32_768..=65_535).contains(&value) {
                return Err(IrxError::new(
                    origin,
                    relocation.source_offset,
                    IrxErrorKind::RelocationOverflow,
                ));
            }
            (word & 0xffff_0000) | u32::from(value as u16)
        }
        RelocationKind::Word => word.checked_add(delta).ok_or_else(|| {
            IrxError::new(
                origin,
                relocation.source_offset,
                IrxErrorKind::RelocationOverflow,
            )
        })?,
        RelocationKind::Jump => {
            let addend = (word & 0x03ff_ffff) << 2;
            let target = delta.checked_add(addend).ok_or_else(|| {
                IrxError::new(
                    origin,
                    relocation.source_offset,
                    IrxErrorKind::RelocationOverflow,
                )
            })?;
            if target & 3 != 0 {
                return Err(IrxError::new(
                    origin,
                    relocation.source_offset,
                    IrxErrorKind::RelocationOverflow,
                ));
            }
            (word & 0xfc00_0000) | ((target >> 2) & 0x03ff_ffff)
        }
        RelocationKind::High { low_offset } => {
            let low_at =
                image_offset(low_offset, preferred_address, image.len()).ok_or_else(|| {
                    IrxError::new(
                        origin,
                        relocation.source_offset,
                        IrxErrorKind::MalformedRelocationPair,
                    )
                })?;
            let next = read_u32(image, low_at).ok_or_else(|| {
                IrxError::new(
                    origin,
                    relocation.source_offset,
                    IrxErrorKind::MalformedRelocationPair,
                )
            })?;
            let addend = i64::from((word & 0xffff) << 16) + i64::from((next as u16) as i16);
            let value = i64::from(delta) + addend;
            if !(0..=i64::from(u32::MAX)).contains(&value) {
                return Err(IrxError::new(
                    origin,
                    relocation.source_offset,
                    IrxErrorKind::RelocationOverflow,
                ));
            }
            (word & 0xffff_0000) | (((value as u32).wrapping_add(0x8000) >> 16) & 0xffff)
        }
        RelocationKind::Low => (word & 0xffff_0000) | (word.wrapping_add(delta) & 0xffff),
        RelocationKind::SonyHigh { addend } => {
            apply_sony_high(origin, image, at, delta, addend, relocation.source_offset)?;
            return Ok(());
        }
    };
    image[at..at + 4].copy_from_slice(&patched.to_le_bytes());
    Ok(())
}

fn apply_sony_high(
    origin: &str,
    image: &mut [u8],
    start: usize,
    delta: u32,
    addend: u32,
    source_offset: usize,
) -> Result<(), IrxError> {
    let value = delta
        .checked_add(addend)
        .ok_or_else(|| IrxError::new(origin, source_offset, IrxErrorKind::RelocationOverflow))?;
    let immediate = value.wrapping_add(0x8000) >> 16;
    let mut at = start;
    let mut visited = 0;
    loop {
        let word = read_u32(image, at).ok_or_else(|| {
            IrxError::new(
                origin,
                source_offset,
                IrxErrorKind::MalformedRelocationChain,
            )
        })?;
        let step = i32::from((word as u16) as i16) * 4;
        let patched = (word & 0xffff_0000) | (immediate & 0xffff);
        image[at..at + 4].copy_from_slice(&patched.to_le_bytes());
        visited += 1;
        if visited > image.len() / 4 + 1 {
            return Err(IrxError::new(
                origin,
                source_offset,
                IrxErrorKind::MalformedRelocationChain,
            ));
        }
        if step == 0 {
            return Ok(());
        }
        at = if step.is_negative() {
            at.checked_sub(step.unsigned_abs() as usize)
        } else {
            at.checked_add(step as usize)
        }
        .filter(|offset| {
            offset % 4 == 0 && offset.checked_add(4).is_some_and(|end| end <= image.len())
        })
        .ok_or_else(|| {
            IrxError::new(
                origin,
                source_offset,
                IrxErrorKind::MalformedRelocationChain,
            )
        })?;
    }
}

fn parse_link_tables(
    origin: &str,
    image: &[u8],
    address: u32,
    limits: IrxLimits,
) -> Result<(Vec<ImportLibrary>, Vec<ExportLibrary>), IrxError> {
    let image_size =
        u32::try_from(image.len()).map_err(|_| IrxError::new(origin, 0, IrxErrorKind::Overflow))?;
    let image_end = address
        .checked_add(image_size)
        .ok_or_else(|| IrxError::new(origin, 0, IrxErrorKind::Overflow))?;
    let mut imports = Vec::new();
    let mut exports = Vec::new();
    let mut entries = 0_usize;
    for at in (0..image.len().saturating_sub(3)).step_by(4) {
        let magic = read_u32(image, at).expect("four-byte scan");
        if magic != IMPORT_MAGIC && magic != EXPORT_MAGIC {
            continue;
        }
        if imports.len() + exports.len() >= limits.max_link_tables {
            return Err(IrxError::new(
                origin,
                at,
                IrxErrorKind::LimitExceeded("link tables"),
            ));
        }
        let header = image
            .get(at..at + 20)
            .ok_or_else(|| IrxError::new(origin, at, IrxErrorKind::InvalidLinkTable))?;
        if read_u32(header, 4) != Some(0) {
            return Err(IrxError::new(
                origin,
                at + 4,
                IrxErrorKind::InvalidLinkTable,
            ));
        }
        let version = read_u16(header, 8).expect("link header");
        let mode = read_u16(header, 10).expect("link header");
        let name = parse_fixed_name(&header[12..20])
            .ok_or_else(|| IrxError::new(origin, at + 12, IrxErrorKind::InvalidLinkTable))?;
        let table_address = address
            .checked_add(at as u32)
            .ok_or_else(|| IrxError::new(origin, at, IrxErrorKind::Overflow))?;
        if magic == IMPORT_MAGIC {
            let mut stubs = Vec::new();
            let mut cursor = at + 20;
            loop {
                let stub = image
                    .get(cursor..cursor + 8)
                    .ok_or_else(|| IrxError::new(origin, cursor, IrxErrorKind::InvalidLinkTable))?;
                let jump = read_u32(stub, 0).expect("import stub");
                let ordinal_word = read_u32(stub, 4).expect("import stub");
                if jump == 0 && ordinal_word == 0 {
                    break;
                }
                if jump != 0x03e0_0008 || ordinal_word & 0xffff_0000 != 0x2400_0000 {
                    return Err(IrxError::new(
                        origin,
                        cursor,
                        IrxErrorKind::InvalidLinkTable,
                    ));
                }
                entries = entries
                    .checked_add(1)
                    .ok_or_else(|| IrxError::new(origin, cursor, IrxErrorKind::Overflow))?;
                if entries > limits.max_link_entries {
                    return Err(IrxError::new(
                        origin,
                        cursor,
                        IrxErrorKind::LimitExceeded("link entries"),
                    ));
                }
                stubs.push(ImportStub {
                    ordinal: ordinal_word as u16,
                    address: address
                        .checked_add(cursor as u32)
                        .ok_or_else(|| IrxError::new(origin, cursor, IrxErrorKind::Overflow))?,
                });
                cursor += 8;
            }
            imports.push(ImportLibrary {
                name,
                version,
                mode,
                table_address,
                stubs,
            });
        } else {
            let mut functions = Vec::new();
            let mut cursor = at + 20;
            loop {
                let function = read_u32(image, cursor)
                    .ok_or_else(|| IrxError::new(origin, cursor, IrxErrorKind::InvalidLinkTable))?;
                if function == 0 {
                    break;
                }
                if !(MemoryRange {
                    start: address,
                    end: image_end,
                })
                .contains(function, 4)
                {
                    return Err(IrxError::new(
                        origin,
                        cursor,
                        IrxErrorKind::InvalidLinkTable,
                    ));
                }
                entries = entries
                    .checked_add(1)
                    .ok_or_else(|| IrxError::new(origin, cursor, IrxErrorKind::Overflow))?;
                if entries > limits.max_link_entries {
                    return Err(IrxError::new(
                        origin,
                        cursor,
                        IrxErrorKind::LimitExceeded("link entries"),
                    ));
                }
                functions.push(function);
                cursor += 4;
            }
            exports.push(ExportLibrary {
                name,
                version,
                mode,
                table_address,
                functions,
            });
        }
    }
    Ok((imports, exports))
}

fn parse_c_name(
    bytes: &[u8],
    max: usize,
    empty_allowed: bool,
) -> Result<Option<String>, IrxErrorKind> {
    let end = bytes
        .iter()
        .take(max.saturating_add(1))
        .position(|byte| *byte == 0)
        .ok_or(IrxErrorKind::InvalidModuleMetadata)?;
    if end == 0 {
        return if empty_allowed {
            Ok(None)
        } else {
            Err(IrxErrorKind::InvalidModuleMetadata)
        };
    }
    if bytes[..end].iter().any(|byte| !(32..=126).contains(byte)) {
        return Err(IrxErrorKind::InvalidModuleMetadata);
    }
    Ok(Some(
        String::from_utf8(bytes[..end].to_vec()).expect("ASCII name"),
    ))
}

fn parse_fixed_name(bytes: &[u8]) -> Option<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    if end == 0
        || bytes[end..].iter().any(|byte| *byte != 0)
        || bytes[..end].iter().any(|byte| !(33..=126).contains(byte))
    {
        return None;
    }
    Some(String::from_utf8(bytes[..end].to_vec()).expect("ASCII library name"))
}

fn image_offset(address: u32, preferred_address: u32, image_size: usize) -> Option<usize> {
    let offset = address.checked_sub(preferred_address)? as usize;
    (offset < image_size).then_some(offset)
}

fn read_u16(input: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        input.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn read_u32(input: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        input.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}
