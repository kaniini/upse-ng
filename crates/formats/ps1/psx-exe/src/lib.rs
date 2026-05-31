// SPDX-License-Identifier: LGPL-2.1-or-later
//! Checked PS-X EXE parsing and deterministic PSF1 overlay application.

use thiserror::Error;
use upse_psf::{Psf1LoadPlan, RefreshRate};

/// Fixed PS-X EXE header size.
pub const HEADER_SIZE: usize = 0x800;
/// Physical PS1 main RAM size used by PSF1.
pub const RAM_SIZE: usize = 2 * 1024 * 1024;

/// Region marker decoded from the executable header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Region {
    /// Japan, using 60 Hz refresh.
    Japan,
    /// North America, using 60 Hz refresh.
    NorthAmerica,
    /// Europe, using 50 Hz refresh.
    Europe,
    /// Marker was absent or unrecognized.
    Unknown,
}

impl Region {
    /// Returns the refresh implied by a recognized region marker.
    #[must_use]
    pub const fn refresh(self) -> Option<RefreshRate> {
        match self {
            Self::Japan | Self::NorthAmerica => Some(RefreshRate::Hz60),
            Self::Europe => Some(RefreshRate::Hz50),
            Self::Unknown => None,
        }
    }
}

/// A validated executable borrowing its text bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PsxExe<'a> {
    /// Initial program counter.
    pub pc: u32,
    /// Guest virtual text address.
    pub text_address: u32,
    /// Initial stack pointer.
    pub sp: u32,
    /// Header region marker.
    pub region: Region,
    physical_text_offset: usize,
    text: &'a [u8],
}

impl<'a> PsxExe<'a> {
    /// Parses a complete uncompressed PS-X EXE payload.
    ///
    /// # Errors
    ///
    /// Returns [`ExeError`] for a truncated header/text section, invalid
    /// signature, arithmetic overflow, or text range outside 2 MiB RAM.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ExeError> {
        if bytes.len() < HEADER_SIZE {
            return Err(ExeError::TruncatedHeader);
        }
        if &bytes[..8] != b"PS-X EXE" {
            return Err(ExeError::InvalidSignature);
        }
        let pc = read_u32(bytes, 0x10);
        let text_address = read_u32(bytes, 0x18);
        let text_size =
            usize::try_from(read_u32(bytes, 0x1c)).map_err(|_| ExeError::AddressOverflow)?;
        let sp = read_u32(bytes, 0x30);
        let text_end = HEADER_SIZE
            .checked_add(text_size)
            .ok_or(ExeError::AddressOverflow)?;
        if text_end > bytes.len() {
            return Err(ExeError::TruncatedText {
                declared: text_size,
                available: bytes.len() - HEADER_SIZE,
            });
        }
        let physical_text_offset = checked_ram_range(text_address, text_size)?.start;
        let marker = &bytes[0x4c..HEADER_SIZE];
        let region = if contains(marker, b"North America") {
            Region::NorthAmerica
        } else if contains(marker, b"Japan") {
            Region::Japan
        } else if contains(marker, b"Europe") {
            Region::Europe
        } else {
            Region::Unknown
        };
        Ok(Self {
            pc,
            text_address,
            sp,
            region,
            physical_text_offset,
            text: &bytes[HEADER_SIZE..text_end],
        })
    }

    /// Returns the validated text section.
    #[must_use]
    pub const fn text(self) -> &'a [u8] {
        self.text
    }

    /// Returns the physical RAM offset of the text section.
    #[must_use]
    pub const fn physical_text_offset(self) -> usize {
        self.physical_text_offset
    }
}

/// Fully overlaid PSF1 executable image and reset state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutableImage {
    ram: Vec<u8>,
    /// Initial program counter selected from `_lib` traversal.
    pub pc: u32,
    /// Initial stack pointer selected from `_lib` traversal.
    pub sp: u32,
    /// Root region unless overridden by the first `_refresh` tag.
    pub refresh: RefreshRate,
}

impl ExecutableImage {
    /// Applies a validated PSF1 plan with last-overlay-wins behavior.
    ///
    /// # Errors
    ///
    /// Returns [`ImageError`] when any payload is not a valid PS-X EXE, the
    /// root/initial layer is absent, or no refresh rate can be established.
    pub fn from_plan(plan: &Psf1LoadPlan) -> Result<Self, ImageError> {
        let mut ram = vec![0_u8; RAM_SIZE];
        let mut initial = None;
        let mut root_region = None;
        for layer in &plan.layers {
            let exe = PsxExe::parse(layer.container.program()).map_err(|source| {
                ImageError::Executable {
                    origin: layer.origin.clone(),
                    source,
                }
            })?;
            if layer.origin == plan.initial_state_origin {
                initial = Some((exe.pc, exe.sp));
            }
            if layer.origin == plan.root_origin {
                root_region = Some(exe.region);
            }
            let start = exe.physical_text_offset();
            let end = start + exe.text().len();
            ram[start..end].copy_from_slice(exe.text());
        }
        let (pc, sp) = initial.ok_or(ImageError::MissingInitialLayer)?;
        let refresh = plan
            .refresh_override
            .or_else(|| root_region.and_then(Region::refresh))
            .ok_or(ImageError::UnknownRootRegion)?;
        Ok(Self {
            ram,
            pc,
            sp,
            refresh,
        })
    }

    /// Returns the complete two-megabyte RAM image.
    #[must_use]
    pub fn ram(&self) -> &[u8] {
        &self.ram
    }
}

/// Invalid PS-X EXE payload.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ExeError {
    /// Fewer than 0x800 header bytes were available.
    #[error("truncated PS-X EXE header")]
    TruncatedHeader,
    /// Header did not start with `PS-X EXE`.
    #[error("invalid PS-X EXE signature")]
    InvalidSignature,
    /// Declared text exceeded the available payload.
    #[error("truncated text: declared {declared} bytes, only {available} available")]
    TruncatedText {
        /// Header-declared size.
        declared: usize,
        /// Bytes following the header.
        available: usize,
    },
    /// Address or length arithmetic overflowed.
    #[error("executable address arithmetic overflow")]
    AddressOverflow,
    /// Text falls outside PSF1's two-megabyte physical RAM.
    #[error("text range {address:#010x}+{size:#x} is outside PS1 RAM")]
    TextOutsideRam {
        /// Guest virtual start address.
        address: u32,
        /// Text byte count.
        size: usize,
    },
}

/// Failure while applying a multi-file overlay plan.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ImageError {
    /// A named layer contained an invalid executable.
    #[error("{origin}: {source}")]
    Executable {
        /// Logical origin.
        origin: String,
        /// Executable parser error.
        source: ExeError,
    },
    /// Plan did not contain its selected PC/SP layer.
    #[error("PSF1 plan is missing its initial-state layer")]
    MissingInitialLayer,
    /// Root executable had no known region and no `_refresh` override.
    #[error("root executable has no recognized refresh region")]
    UnknownRootRegion,
}

fn checked_ram_range(address: u32, size: usize) -> Result<std::ops::Range<usize>, ExeError> {
    let physical = usize::try_from(address & 0x1fff_ffff).map_err(|_| ExeError::AddressOverflow)?;
    let end = physical
        .checked_add(size)
        .ok_or(ExeError::AddressOverflow)?;
    if physical >= RAM_SIZE || end > RAM_SIZE {
        return Err(ExeError::TextOutsideRam { address, size });
    }
    Ok(physical..end)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("checked header"),
    )
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|candidate| candidate == needle)
}

#[cfg(test)]
mod tests {
    use upse_psf::{
        DependencyLimits, LoadPlan, MemoryResolver, ParseLimits, PsfBuilder, PsfVersion, load_plan,
    };

    use super::{ExeError, ExecutableImage, PsxExe, RAM_SIZE, Region};

    fn exe(address: u32, pc: u32, sp: u32, text: &[u8], region: &str) -> Vec<u8> {
        let mut bytes = vec![0_u8; 0x800 + text.len()];
        bytes[..8].copy_from_slice(b"PS-X EXE");
        bytes[0x10..0x14].copy_from_slice(&pc.to_le_bytes());
        bytes[0x18..0x1c].copy_from_slice(&address.to_le_bytes());
        let text_size = u32::try_from(text.len()).expect("synthetic test executable fits u32");
        bytes[0x1c..0x20].copy_from_slice(&text_size.to_le_bytes());
        bytes[0x30..0x34].copy_from_slice(&sp.to_le_bytes());
        bytes[0x4c..0x4c + region.len()].copy_from_slice(region.as_bytes());
        bytes[0x800..].copy_from_slice(text);
        bytes
    }

    fn container(program: Vec<u8>, tags: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = PsfBuilder::new(PsfVersion::Psf1).program(program);
        for (key, value) in tags {
            builder = builder.tag(*key, *value);
        }
        builder.build()
    }

    #[test]
    fn parses_header_region_and_checked_text() {
        let bytes = exe(
            0x8001_0000,
            0x8001_0010,
            0x801f_ff00,
            &[1, 2, 3],
            "Europe area",
        );
        let parsed = PsxExe::parse(&bytes).unwrap();
        assert_eq!(parsed.pc, 0x8001_0010);
        assert_eq!(parsed.sp, 0x801f_ff00);
        assert_eq!(parsed.physical_text_offset(), 0x10000);
        assert_eq!(parsed.text(), [1, 2, 3]);
        assert_eq!(parsed.region, Region::Europe);
    }

    #[test]
    fn rejects_all_payload_and_ram_boundaries() {
        assert_eq!(PsxExe::parse(&[]), Err(ExeError::TruncatedHeader));
        let mut bad = vec![0_u8; 0x800];
        assert_eq!(PsxExe::parse(&bad), Err(ExeError::InvalidSignature));
        bad = exe(0x8001_0000, 0, 0, &[1], "Japan");
        bad[0x1c..0x20].copy_from_slice(&2_u32.to_le_bytes());
        assert!(matches!(
            PsxExe::parse(&bad),
            Err(ExeError::TruncatedText { .. })
        ));
        let outside = exe(0x801f_ffff, 0, 0, &[1, 2], "North America area");
        assert!(matches!(
            PsxExe::parse(&outside),
            Err(ExeError::TextOutsideRam { .. })
        ));
    }

    #[test]
    fn plan_overlay_selects_lib_state_root_region_and_last_bytes() {
        let root = container(
            exe(0x8001_0002, 0x1111, 0x2222, &[9, 9], "Europe area"),
            &[("_lib", "base.psflib"), ("_lib2", "patch.psflib")],
        );
        let mut resolver = MemoryResolver::new();
        resolver
            .insert(
                "set/base.psflib",
                container(
                    exe(
                        0x8001_0000,
                        0x8001_0000,
                        0x801f_ff00,
                        &[1, 2, 3, 4],
                        "Japan",
                    ),
                    &[],
                ),
            )
            .unwrap();
        resolver
            .insert(
                "set/patch.psflib",
                container(exe(0x8001_0001, 0, 0, &[7, 8], "Japan"), &[]),
            )
            .unwrap();
        let LoadPlan::Psf1(plan) = load_plan(
            "set/root.minipsf",
            &root,
            &mut resolver,
            ParseLimits::default(),
            DependencyLimits::default(),
        )
        .unwrap() else {
            panic!("wrong plan")
        };
        let image = ExecutableImage::from_plan(&plan).unwrap();
        assert_eq!(&image.ram()[0x10000..0x10004], &[1, 7, 8, 9]);
        assert_eq!(image.pc, 0x8001_0000);
        assert_eq!(image.sp, 0x801f_ff00);
        assert_eq!(image.refresh.hz(), 50);
        assert_eq!(image.ram().len(), RAM_SIZE);
    }

    #[test]
    fn refresh_override_beats_root_region() {
        let root = container(
            exe(0x8001_0000, 0, 0, &[0], "Europe area"),
            &[("_refresh", "60")],
        );
        let mut resolver = MemoryResolver::new();
        let LoadPlan::Psf1(plan) = load_plan(
            "root.psf",
            &root,
            &mut resolver,
            ParseLimits::default(),
            DependencyLimits::default(),
        )
        .unwrap() else {
            panic!("wrong plan")
        };
        assert_eq!(ExecutableImage::from_plan(&plan).unwrap().refresh.hz(), 60);
    }
}
