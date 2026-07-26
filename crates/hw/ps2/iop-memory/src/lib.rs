// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS2 IOP RAM and address decoding with no firmware-image input path.

use thiserror::Error;

/// Installed IOP main RAM size.
pub const RAM_SIZE: usize = 2 * 1024 * 1024;
const RAM_SIZE_U32: u32 = 2 * 1024 * 1024;
const RAM_MIRROR_END: u32 = 8 * 1024 * 1024;
/// First physical scratchpad byte.
pub const SCRATCHPAD_START: u32 = 0x1f80_0000;
/// Size of the two IOP scratchpad/cache banks exposed before MMIO.
pub const SCRATCHPAD_SIZE: usize = 0x800;
const SCRATCHPAD_SIZE_U32: u32 = 0x800;
/// First physical IOP hardware register.
pub const MMIO_START: u32 = 0x1f80_1000;
/// Last physical register in the main IOP hardware window.
pub const MMIO_END: u32 = 0x1f80_ffff;
/// First SPU2 register byte.
pub const SPU2_MMIO_START: u32 = 0x1f90_0000;
/// Last SPU2 register byte.
pub const SPU2_MMIO_END: u32 = 0x1f90_07ff;
/// First byte of the firmware range reserved for BIOS HLE.
pub const HLE_ROM_START: u32 = 0x1fc0_0000;
/// Last byte of the firmware range reserved for BIOS HLE.
pub const HLE_ROM_END: u32 = 0x1fc7_ffff;

/// Handling for addresses outside modeled IOP regions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OpenBusPolicy {
    /// Return an explicit diagnostic.
    #[default]
    Strict,
    /// Return all-one values and discard writes.
    Ones,
}

/// Decoded IOP address region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryRegion {
    /// Main RAM, including its low eight-megabyte mirrors.
    Ram {
        /// Offset into the two-megabyte allocation.
        offset: usize,
    },
    /// IOP scratchpad/cache storage.
    Scratchpad {
        /// Offset into scratchpad storage.
        offset: usize,
    },
    /// Machine-routed IOP register.
    Mmio {
        /// Physical register address.
        physical: u32,
    },
    /// Machine-routed SPU2 register.
    Spu2 {
        /// Physical register address.
        physical: u32,
    },
    /// Firmware range intentionally owned by BIOS HLE.
    HleRom {
        /// Physical firmware address.
        physical: u32,
    },
    /// Address has no modeled region.
    Unmapped {
        /// Translated physical address.
        physical: u32,
    },
}

/// IOP memory access failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum MemoryError {
    /// An address requires an IOP TLB mapping or lies in KSEG2/3.
    #[error("unsupported IOP virtual segment at {address:#010x}")]
    UnsupportedSegment {
        /// Guest virtual address.
        address: u32,
    },
    /// Host pointer width cannot represent a decoded physical offset.
    #[error("IOP address does not fit host pointer width at {address:#010x}")]
    UnsupportedAddressWidth {
        /// Guest virtual address.
        address: u32,
    },
    /// Access belongs to the machine-level IOP register router.
    #[error("IOP MMIO access at {address:#010x}")]
    Mmio {
        /// Physical register address.
        address: u32,
    },
    /// Access belongs to the machine-level SPU2 router.
    #[error("SPU2 MMIO access at {address:#010x}")]
    Spu2 {
        /// Physical register address.
        address: u32,
    },
    /// Guest attempted to fetch or read an absent Sony firmware image.
    #[error("IOP firmware address {address:#010x} is HLE-only")]
    HleRom {
        /// Physical firmware address.
        address: u32,
    },
    /// Strict mode rejected an unmapped address.
    #[error("unmapped IOP access at {address:#010x}")]
    Unmapped {
        /// Physical address.
        address: u32,
    },
    /// Multi-byte access crossed a region boundary.
    #[error("IOP access crosses a memory-region boundary at {address:#010x}")]
    CrossesBoundary {
        /// First guest virtual address.
        address: u32,
    },
    /// A host load request leaves physical main RAM.
    #[error("IOP RAM range {address:#010x}..+{length:#x} is invalid")]
    InvalidRamRange {
        /// Physical base address.
        address: u32,
        /// Requested byte count.
        length: usize,
    },
}

/// Instance-owned PS2 IOP memory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IopMemory {
    ram: Vec<u8>,
    scratchpad: [u8; SCRATCHPAD_SIZE],
    open_bus: OpenBusPolicy,
}

impl IopMemory {
    /// Constructs reset memory.
    #[must_use]
    pub fn new(open_bus: OpenBusPolicy) -> Self {
        Self {
            ram: vec![0; RAM_SIZE],
            scratchpad: [0; SCRATCHPAD_SIZE],
            open_bus,
        }
    }

    /// Returns immutable physical main RAM.
    #[must_use]
    pub fn ram(&self) -> &[u8] {
        &self.ram
    }

    /// Returns mutable physical main RAM.
    #[must_use]
    pub fn ram_mut(&mut self) -> &mut [u8] {
        &mut self.ram
    }

    /// Copies a validated byte image into physical main RAM.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::InvalidRamRange`] without changing RAM when the
    /// complete range does not fit.
    pub fn load_ram(&mut self, address: u32, bytes: &[u8]) -> Result<(), MemoryError> {
        let start = usize::try_from(address).map_err(|_| MemoryError::InvalidRamRange {
            address,
            length: bytes.len(),
        })?;
        let end = start
            .checked_add(bytes.len())
            .filter(|&end| end <= RAM_SIZE)
            .ok_or(MemoryError::InvalidRamRange {
                address,
                length: bytes.len(),
            })?;
        self.ram[start..end].copy_from_slice(bytes);
        Ok(())
    }

    /// Translates a direct-mapped IOP address to its physical bus address.
    ///
    /// The IOP accepts the low cached and uncached aliases beginning at zero,
    /// as well as KSEG0 and KSEG1. TLB-backed KUSEG ranges and KSEG2/3 are not
    /// available in the PSF2 machine profile.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::UnsupportedSegment`] for a non-direct segment.
    pub const fn translate(address: u32) -> Result<u32, MemoryError> {
        match address {
            0x0000_0000..=0x3fff_ffff | 0x8000_0000..=0xbfff_ffff => Ok(address & 0x1fff_ffff),
            _ => Err(MemoryError::UnsupportedSegment { address }),
        }
    }

    /// Classifies a guest address without performing an access.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::UnsupportedSegment`] for a non-direct segment.
    pub fn classify(address: u32) -> Result<MemoryRegion, MemoryError> {
        let physical = Self::translate(address)?;
        Ok(if physical < RAM_MIRROR_END {
            MemoryRegion::Ram {
                offset: usize::try_from(physical % RAM_SIZE_U32)
                    .map_err(|_| MemoryError::UnsupportedAddressWidth { address })?,
            }
        } else if (SCRATCHPAD_START..SCRATCHPAD_START + SCRATCHPAD_SIZE_U32).contains(&physical) {
            MemoryRegion::Scratchpad {
                offset: usize::try_from(physical - SCRATCHPAD_START)
                    .map_err(|_| MemoryError::UnsupportedAddressWidth { address })?,
            }
        } else if (MMIO_START..=MMIO_END).contains(&physical) {
            MemoryRegion::Mmio { physical }
        } else if (SPU2_MMIO_START..=SPU2_MMIO_END).contains(&physical) {
            MemoryRegion::Spu2 { physical }
        } else if (HLE_ROM_START..=HLE_ROM_END).contains(&physical) {
            MemoryRegion::HleRom { physical }
        } else {
            MemoryRegion::Unmapped { physical }
        })
    }

    /// Reads one byte from a directly backed region.
    ///
    /// # Errors
    ///
    /// Returns a structured region or segment diagnostic.
    pub fn read_u8(&self, address: u32) -> Result<u8, MemoryError> {
        self.read_decoded_u8(Self::classify(address)?)
    }

    /// Reads one little-endian halfword from a directly backed region.
    ///
    /// # Errors
    ///
    /// Returns a structured region, boundary, or segment diagnostic.
    pub fn read_u16(&self, address: u32) -> Result<u16, MemoryError> {
        let region = Self::classify(address)?;
        Self::validate_span(address, region, 2)?;
        let bytes = [
            self.read_decoded_u8(region)?,
            self.read_u8(address.wrapping_add(1))?,
        ];
        Ok(u16::from_le_bytes(bytes))
    }

    /// Reads one little-endian word from a directly backed region.
    ///
    /// # Errors
    ///
    /// Returns a structured region, boundary, or segment diagnostic.
    pub fn read_u32(&self, address: u32) -> Result<u32, MemoryError> {
        let region = Self::classify(address)?;
        Self::validate_span(address, region, 4)?;
        let mut bytes = [0; 4];
        for (offset, byte) in [0_u32, 1, 2, 3].into_iter().zip(&mut bytes) {
            *byte = self.read_u8(address.wrapping_add(offset))?;
        }
        Ok(u32::from_le_bytes(bytes))
    }

    /// Writes one byte to a directly backed region.
    ///
    /// # Errors
    ///
    /// Returns a structured region or segment diagnostic.
    pub fn write_u8(&mut self, address: u32, value: u8) -> Result<(), MemoryError> {
        self.write_decoded_u8(Self::classify(address)?, value)
    }

    /// Writes one little-endian halfword to a directly backed region.
    ///
    /// # Errors
    ///
    /// Returns a structured region, boundary, or segment diagnostic.
    pub fn write_u16(&mut self, address: u32, value: u16) -> Result<(), MemoryError> {
        let region = Self::classify(address)?;
        Self::validate_span(address, region, 2)?;
        for (offset, byte) in [0_u32, 1].into_iter().zip(value.to_le_bytes()) {
            self.write_u8(address.wrapping_add(offset), byte)?;
        }
        Ok(())
    }

    /// Writes one little-endian word to a directly backed region.
    ///
    /// # Errors
    ///
    /// Returns a structured region, boundary, or segment diagnostic.
    pub fn write_u32(&mut self, address: u32, value: u32) -> Result<(), MemoryError> {
        let region = Self::classify(address)?;
        Self::validate_span(address, region, 4)?;
        for (offset, byte) in [0_u32, 1, 2, 3].into_iter().zip(value.to_le_bytes()) {
            self.write_u8(address.wrapping_add(offset), byte)?;
        }
        Ok(())
    }

    fn read_decoded_u8(&self, region: MemoryRegion) -> Result<u8, MemoryError> {
        match region {
            MemoryRegion::Ram { offset } => Ok(self.ram[offset]),
            MemoryRegion::Scratchpad { offset } => Ok(self.scratchpad[offset]),
            MemoryRegion::Mmio { physical } => Err(MemoryError::Mmio { address: physical }),
            MemoryRegion::Spu2 { physical } => Err(MemoryError::Spu2 { address: physical }),
            MemoryRegion::HleRom { physical } => Err(MemoryError::HleRom { address: physical }),
            MemoryRegion::Unmapped { physical } => match self.open_bus {
                OpenBusPolicy::Strict => Err(MemoryError::Unmapped { address: physical }),
                OpenBusPolicy::Ones => Ok(u8::MAX),
            },
        }
    }

    fn write_decoded_u8(&mut self, region: MemoryRegion, value: u8) -> Result<(), MemoryError> {
        match region {
            MemoryRegion::Ram { offset } => self.ram[offset] = value,
            MemoryRegion::Scratchpad { offset } => self.scratchpad[offset] = value,
            MemoryRegion::Mmio { physical } => {
                return Err(MemoryError::Mmio { address: physical });
            }
            MemoryRegion::Spu2 { physical } => {
                return Err(MemoryError::Spu2 { address: physical });
            }
            MemoryRegion::HleRom { physical } => {
                return Err(MemoryError::HleRom { address: physical });
            }
            MemoryRegion::Unmapped { physical } => match self.open_bus {
                OpenBusPolicy::Strict => return Err(MemoryError::Unmapped { address: physical }),
                OpenBusPolicy::Ones => {}
            },
        }
        Ok(())
    }

    fn validate_span(address: u32, first: MemoryRegion, length: u32) -> Result<(), MemoryError> {
        let last_address = address
            .checked_add(length - 1)
            .ok_or(MemoryError::CrossesBoundary { address })?;
        let last = Self::classify(last_address)?;
        if region_kind(first) != region_kind(last) {
            return Err(MemoryError::CrossesBoundary { address });
        }
        match (first, last) {
            (MemoryRegion::Ram { offset: first }, MemoryRegion::Ram { offset: last })
                if last < first =>
            {
                Err(MemoryError::CrossesBoundary { address })
            }
            (
                MemoryRegion::Scratchpad { offset: first },
                MemoryRegion::Scratchpad { offset: last },
            ) if last < first => Err(MemoryError::CrossesBoundary { address }),
            _ => Ok(()),
        }
    }
}

fn region_kind(region: MemoryRegion) -> u8 {
    match region {
        MemoryRegion::Ram { .. } => 0,
        MemoryRegion::Scratchpad { .. } => 1,
        MemoryRegion::Mmio { .. } => 2,
        MemoryRegion::Spu2 { .. } => 3,
        MemoryRegion::HleRom { .. } => 4,
        MemoryRegion::Unmapped { .. } => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HLE_ROM_START, IopMemory, MMIO_START, MemoryError, MemoryRegion, OpenBusPolicy, RAM_SIZE,
        SCRATCHPAD_START, SPU2_MMIO_START,
    };

    #[test]
    fn ram_is_mirrored_through_all_direct_aliases() {
        let mut memory = IopMemory::new(OpenBusPolicy::Strict);
        memory.write_u32(0x001f_fffc, 0x1234_5678).unwrap();
        for address in [
            0x001f_fffc,
            0x003f_fffc,
            0x201f_fffc,
            0x801f_fffc,
            0xa01f_fffc,
        ] {
            assert_eq!(memory.read_u32(address).unwrap(), 0x1234_5678);
        }
        assert_eq!(memory.ram().len(), RAM_SIZE);
    }

    #[test]
    fn scratchpad_and_device_windows_are_distinct() {
        let mut memory = IopMemory::new(OpenBusPolicy::Strict);
        memory.write_u16(SCRATCHPAD_START + 0x7fe, 0xabcd).unwrap();
        assert_eq!(memory.read_u16(0xbf80_07fe).unwrap(), 0xabcd);
        assert_eq!(
            IopMemory::classify(MMIO_START).unwrap(),
            MemoryRegion::Mmio {
                physical: MMIO_START
            }
        );
        assert_eq!(
            memory.read_u16(SPU2_MMIO_START),
            Err(MemoryError::Spu2 {
                address: SPU2_MMIO_START
            })
        );
        assert_eq!(
            memory.read_u32(HLE_ROM_START),
            Err(MemoryError::HleRom {
                address: HLE_ROM_START
            })
        );
    }

    #[test]
    fn invalid_segments_boundaries_and_loads_are_diagnostic() {
        let mut memory = IopMemory::new(OpenBusPolicy::Strict);
        assert_eq!(
            memory.read_u8(0x4000_0000),
            Err(MemoryError::UnsupportedSegment {
                address: 0x4000_0000
            })
        );
        assert_eq!(
            memory.write_u32(0x007f_fffe, 1),
            Err(MemoryError::CrossesBoundary {
                address: 0x007f_fffe
            })
        );
        let before = memory.ram().to_vec();
        assert_eq!(
            memory.load_ram(0x001f_ffff, &[1, 2]),
            Err(MemoryError::InvalidRamRange {
                address: 0x001f_ffff,
                length: 2
            })
        );
        assert_eq!(memory.ram(), before);
    }

    #[test]
    fn open_bus_policy_is_explicit() {
        let mut memory = IopMemory::new(OpenBusPolicy::Ones);
        assert_eq!(memory.read_u32(0x0100_0000).unwrap(), u32::MAX);
        memory.write_u32(0x0100_0000, 0).unwrap();
    }
}
