// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS1 memory regions with no firmware-image input path.

use thiserror::Error;
use upse_psx_exe::{ExecutableImage, RAM_SIZE};

const RAM_MIRROR_END: u32 = 0x0080_0000;
const SCRATCH_START: u32 = 0x1f80_0000;
const SCRATCH_SIZE: usize = 1024;
const SCRATCH_SIZE_U32: u32 = 1024;
const MMIO_START: u32 = 0x1f80_1000;
const MMIO_END: u32 = 0x1f80_3000;
const ROM_START: u32 = 0x1fc0_0000;
const ROM_END: u32 = 0x1fc8_0000;

/// First memory-control register address.
pub const MEMORY_CONTROL_START: u32 = 0x1f80_1000;
/// Last byte occupied by the memory-control register block.
pub const MEMORY_CONTROL_END: u32 = 0x1f80_1023;
const MEMORY_CONTROL_RESET: [u32; 9] = [
    0x1f00_0000,
    0x1f80_2000,
    0x0013_243f,
    0x0000_3022,
    0x0013_243f,
    0x2009_31e1,
    0x0002_0843,
    0x0007_0777,
    0x0003_1125,
];

/// Handling for otherwise unmapped physical addresses.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OpenBusPolicy {
    /// Report an explicit error.
    #[default]
    Strict,
    /// Return all-one values and discard writes.
    Ones,
}

/// Decoded address region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryRegion {
    /// Mirrored two-megabyte main RAM.
    Ram {
        /// Offset into the physical two-megabyte RAM allocation.
        offset: usize,
    },
    /// One-kilobyte scratchpad.
    Scratchpad {
        /// Offset into the scratchpad allocation.
        offset: usize,
    },
    /// Machine-routed I/O registers.
    Mmio {
        /// Translated physical I/O address.
        physical: u32,
    },
    /// Firmware range intentionally implemented only through HLE.
    HleRom {
        /// Translated physical ROM address.
        physical: u32,
    },
    /// Address has no modeled region.
    Unmapped {
        /// Translated physical address.
        physical: u32,
    },
}

/// Memory access failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum MemoryError {
    /// KSEG2/3 address translation is not available in the PSF1 profile.
    #[error("unsupported virtual segment at {address:#010x}")]
    UnsupportedSegment {
        /// Guest virtual address.
        address: u32,
    },
    /// Host pointer width cannot represent a decoded physical offset.
    #[error("physical address does not fit host pointer width at {address:#010x}")]
    UnsupportedAddressWidth {
        /// Guest virtual address.
        address: u32,
    },
    /// Access belongs to machine-level MMIO routing.
    #[error("MMIO access at {address:#010x}")]
    Mmio {
        /// Physical I/O address.
        address: u32,
    },
    /// Guest attempted to read or write unmodeled console ROM contents.
    #[error("unmodeled HLE-only ROM access at {address:#010x}")]
    HleRom {
        /// Physical ROM address.
        address: u32,
    },
    /// Strict mode rejected an unmapped address.
    #[error("unmapped access at {address:#010x}")]
    Unmapped {
        /// Physical address.
        address: u32,
    },
    /// Multi-byte access crossed a region boundary.
    #[error("access crosses a memory-region boundary at {address:#010x}")]
    CrossesBoundary {
        /// First guest virtual address.
        address: u32,
    },
    /// Executable image had an unexpected RAM size.
    #[error("invalid executable RAM image size {actual}")]
    InvalidImageSize {
        /// Supplied byte count.
        actual: usize,
    },
}

/// Instance-owned PS1 RAM and scratchpad.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ps1Memory {
    ram: Vec<u8>,
    scratchpad: [u8; SCRATCH_SIZE],
    memory_control: [u32; 9],
    open_bus: OpenBusPolicy,
}

impl Ps1Memory {
    /// Constructs zeroed memory with the selected unmapped-access policy.
    #[must_use]
    pub fn new(open_bus: OpenBusPolicy) -> Self {
        Self {
            ram: vec![0; RAM_SIZE],
            scratchpad: [0; SCRATCH_SIZE],
            memory_control: MEMORY_CONTROL_RESET,
            open_bus,
        }
    }

    /// Constructs memory from an applied executable image.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::InvalidImageSize`] if the image does not contain
    /// exactly two megabytes.
    pub fn from_image(
        image: &ExecutableImage,
        open_bus: OpenBusPolicy,
    ) -> Result<Self, MemoryError> {
        if image.ram().len() != RAM_SIZE {
            return Err(MemoryError::InvalidImageSize {
                actual: image.ram().len(),
            });
        }
        Ok(Self {
            ram: image.ram().to_vec(),
            scratchpad: [0; SCRATCH_SIZE],
            memory_control: MEMORY_CONTROL_RESET,
            open_bus,
        })
    }

    /// Reads one aligned memory-control register.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Mmio`] outside the nine-register block.
    pub fn read_control(&self, address: u32) -> Result<u32, MemoryError> {
        let index = memory_control_index(address)?;
        Ok(self.memory_control[index])
    }

    /// Writes one aligned memory-control register.
    ///
    /// Timing fields are retained for guest-visible readback; the PSF profile
    /// does not use them to stall the host-side memory implementation.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Mmio`] outside the nine-register block.
    pub fn write_control(&mut self, address: u32, value: u32) -> Result<(), MemoryError> {
        let index = memory_control_index(address)?;
        self.memory_control[index] = value;
        Ok(())
    }

    /// Decodes a guest virtual address without performing an access.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::UnsupportedSegment`] for KSEG2/3.
    pub fn classify(address: u32) -> Result<MemoryRegion, MemoryError> {
        let physical = translate(address)?;
        Ok(if physical < RAM_MIRROR_END {
            MemoryRegion::Ram {
                offset: usize::try_from(physical)
                    .map_err(|_| MemoryError::UnsupportedAddressWidth { address })?
                    % RAM_SIZE,
            }
        } else if (SCRATCH_START..SCRATCH_START + SCRATCH_SIZE_U32).contains(&physical) {
            MemoryRegion::Scratchpad {
                offset: usize::try_from(physical - SCRATCH_START)
                    .map_err(|_| MemoryError::UnsupportedAddressWidth { address })?,
            }
        } else if (MMIO_START..MMIO_END).contains(&physical) {
            MemoryRegion::Mmio { physical }
        } else if (ROM_START..ROM_END).contains(&physical) {
            MemoryRegion::HleRom { physical }
        } else {
            MemoryRegion::Unmapped { physical }
        })
    }

    /// Reads one byte.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] for strict unmapped, MMIO, ROM, or segment access.
    pub fn read_u8(&self, address: u32) -> Result<u8, MemoryError> {
        match Self::classify(address)? {
            MemoryRegion::Ram { offset } => Ok(self.ram[offset]),
            MemoryRegion::Scratchpad { offset } => Ok(self.scratchpad[offset]),
            MemoryRegion::Mmio { physical } => Err(MemoryError::Mmio { address: physical }),
            MemoryRegion::HleRom { physical } => Err(MemoryError::HleRom { address: physical }),
            MemoryRegion::Unmapped { physical } => match self.open_bus {
                OpenBusPolicy::Strict => Err(MemoryError::Unmapped { address: physical }),
                OpenBusPolicy::Ones => Ok(u8::MAX),
            },
        }
    }

    /// Reads one little-endian halfword without crossing a region boundary.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] under the same policy as [`Ps1Memory::read_u8`].
    pub fn read_u16(&self, address: u32) -> Result<u16, MemoryError> {
        let region = Self::classify(address)?;
        self.read_decoded_u16(address, region)
    }

    /// Reads one little-endian word without crossing a region boundary.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] under the same policy as [`Ps1Memory::read_u8`].
    pub fn read_u32(&self, address: u32) -> Result<u32, MemoryError> {
        let region = Self::classify(address)?;
        self.read_decoded_u32(address, region)
    }

    /// Reads a halfword using an address classification already obtained by a
    /// machine-level bus router.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] when the access crosses the supplied region or
    /// when the region is not directly backed by this component.
    pub fn read_decoded_u16(&self, address: u32, region: MemoryRegion) -> Result<u16, MemoryError> {
        Self::validate_decoded_access::<2>(address, region)?;
        match region {
            MemoryRegion::Ram { offset } => {
                Ok(u16::from(self.ram[offset])
                    | (u16::from(self.ram[(offset + 1) % RAM_SIZE]) << 8))
            }
            MemoryRegion::Scratchpad { offset } => {
                Ok(u16::from(self.scratchpad[offset])
                    | (u16::from(self.scratchpad[offset + 1]) << 8))
            }
            MemoryRegion::Mmio { physical } => Err(MemoryError::Mmio { address: physical }),
            MemoryRegion::HleRom { physical } => Err(MemoryError::HleRom { address: physical }),
            MemoryRegion::Unmapped { physical } => match self.open_bus {
                OpenBusPolicy::Strict => Err(MemoryError::Unmapped { address: physical }),
                OpenBusPolicy::Ones => Ok(u16::MAX),
            },
        }
    }

    /// Reads a word using an address classification already obtained by a
    /// machine-level bus router.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] when the access crosses the supplied region or
    /// when the region is not directly backed by this component.
    pub fn read_decoded_u32(&self, address: u32, region: MemoryRegion) -> Result<u32, MemoryError> {
        Self::validate_decoded_access::<4>(address, region)?;
        match region {
            MemoryRegion::Ram { offset } => Ok(u32::from(self.ram[offset])
                | (u32::from(self.ram[(offset + 1) % RAM_SIZE]) << 8)
                | (u32::from(self.ram[(offset + 2) % RAM_SIZE]) << 16)
                | (u32::from(self.ram[(offset + 3) % RAM_SIZE]) << 24)),
            MemoryRegion::Scratchpad { offset } => Ok(u32::from(self.scratchpad[offset])
                | (u32::from(self.scratchpad[offset + 1]) << 8)
                | (u32::from(self.scratchpad[offset + 2]) << 16)
                | (u32::from(self.scratchpad[offset + 3]) << 24)),
            MemoryRegion::Mmio { physical } => Err(MemoryError::Mmio { address: physical }),
            MemoryRegion::HleRom { physical } => Err(MemoryError::HleRom { address: physical }),
            MemoryRegion::Unmapped { physical } => match self.open_bus {
                OpenBusPolicy::Strict => Err(MemoryError::Unmapped { address: physical }),
                OpenBusPolicy::Ones => Ok(u32::MAX),
            },
        }
    }

    /// Writes one byte.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] for strict unmapped, MMIO, ROM, or segment access.
    pub fn write_u8(&mut self, address: u32, value: u8) -> Result<(), MemoryError> {
        match Self::classify(address)? {
            MemoryRegion::Ram { offset } => self.ram[offset] = value,
            MemoryRegion::Scratchpad { offset } => self.scratchpad[offset] = value,
            MemoryRegion::Mmio { physical } => {
                return Err(MemoryError::Mmio { address: physical });
            }
            MemoryRegion::HleRom { physical } => {
                return Err(MemoryError::HleRom { address: physical });
            }
            MemoryRegion::Unmapped { physical } => {
                if self.open_bus == OpenBusPolicy::Strict {
                    return Err(MemoryError::Unmapped { address: physical });
                }
            }
        }
        Ok(())
    }

    /// Writes one little-endian halfword without crossing a region boundary.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] under the same policy as [`Ps1Memory::write_u8`].
    pub fn write_u16(&mut self, address: u32, value: u16) -> Result<(), MemoryError> {
        let region = Self::classify(address)?;
        self.write_decoded_bytes(address, region, value.to_le_bytes())
    }

    /// Writes one little-endian word without crossing a region boundary.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] under the same policy as [`Ps1Memory::write_u8`].
    pub fn write_u32(&mut self, address: u32, value: u32) -> Result<(), MemoryError> {
        let region = Self::classify(address)?;
        self.write_decoded_bytes(address, region, value.to_le_bytes())
    }

    /// Writes a halfword using an address classification already obtained by
    /// a machine-level bus router.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] when the access crosses the supplied region or
    /// when the region is not directly backed by this component.
    pub fn write_decoded_u16(
        &mut self,
        address: u32,
        region: MemoryRegion,
        value: u16,
    ) -> Result<(), MemoryError> {
        self.write_decoded_bytes(address, region, value.to_le_bytes())
    }

    /// Writes a word using an address classification already obtained by a
    /// machine-level bus router.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] when the access crosses the supplied region or
    /// when the region is not directly backed by this component.
    pub fn write_decoded_u32(
        &mut self,
        address: u32,
        region: MemoryRegion,
        value: u32,
    ) -> Result<(), MemoryError> {
        self.write_decoded_bytes(address, region, value.to_le_bytes())
    }

    /// Returns the physical main-RAM bytes for checked DMA integration.
    #[must_use]
    pub fn ram(&self) -> &[u8] {
        &self.ram
    }

    /// Returns mutable physical main-RAM bytes for checked DMA integration.
    #[must_use]
    pub fn ram_mut(&mut self) -> &mut [u8] {
        &mut self.ram
    }

    fn validate_decoded_access<const N: usize>(
        address: u32,
        region: MemoryRegion,
    ) -> Result<(), MemoryError> {
        let last = address
            .checked_add(u32::try_from(N.saturating_sub(1)).expect("small access"))
            .ok_or(MemoryError::CrossesBoundary { address })?;
        if !same_region(region, Self::classify(last)?) {
            return Err(MemoryError::CrossesBoundary { address });
        }
        Ok(())
    }

    fn write_decoded_bytes<const N: usize>(
        &mut self,
        address: u32,
        region: MemoryRegion,
        bytes: [u8; N],
    ) -> Result<(), MemoryError> {
        let last = address
            .checked_add(u32::try_from(N.saturating_sub(1)).expect("small access"))
            .ok_or(MemoryError::CrossesBoundary { address })?;
        if !same_region(region, Self::classify(last)?) {
            return Err(MemoryError::CrossesBoundary { address });
        }
        match region {
            MemoryRegion::Ram { offset } => {
                for (index, value) in bytes.into_iter().enumerate() {
                    self.ram[(offset + index) % RAM_SIZE] = value;
                }
            }
            MemoryRegion::Scratchpad { offset } => {
                let end = offset
                    .checked_add(N)
                    .ok_or(MemoryError::CrossesBoundary { address })?;
                self.scratchpad
                    .get_mut(offset..end)
                    .ok_or(MemoryError::CrossesBoundary { address })?
                    .copy_from_slice(&bytes);
            }
            MemoryRegion::Mmio { physical } => {
                return Err(MemoryError::Mmio { address: physical });
            }
            MemoryRegion::HleRom { physical } => {
                return Err(MemoryError::HleRom { address: physical });
            }
            MemoryRegion::Unmapped { physical } => {
                if self.open_bus == OpenBusPolicy::Strict {
                    return Err(MemoryError::Unmapped { address: physical });
                }
            }
        }
        Ok(())
    }
}

fn translate(address: u32) -> Result<u32, MemoryError> {
    match address >> 29 {
        0..=5 => Ok(address & 0x1fff_ffff),
        _ => Err(MemoryError::UnsupportedSegment { address }),
    }
}

fn memory_control_index(address: u32) -> Result<usize, MemoryError> {
    if !(MEMORY_CONTROL_START..=MEMORY_CONTROL_END).contains(&address) || address & 3 != 0 {
        return Err(MemoryError::Mmio { address });
    }
    usize::try_from((address - MEMORY_CONTROL_START) / 4)
        .map_err(|_| MemoryError::UnsupportedAddressWidth { address })
}

const fn same_region(left: MemoryRegion, right: MemoryRegion) -> bool {
    matches!(
        (left, right),
        (MemoryRegion::Ram { .. }, MemoryRegion::Ram { .. })
            | (
                MemoryRegion::Scratchpad { .. },
                MemoryRegion::Scratchpad { .. }
            )
            | (MemoryRegion::Mmio { .. }, MemoryRegion::Mmio { .. })
            | (MemoryRegion::HleRom { .. }, MemoryRegion::HleRom { .. })
            | (MemoryRegion::Unmapped { .. }, MemoryRegion::Unmapped { .. })
    )
}

#[cfg(test)]
mod tests {
    use super::{
        MEMORY_CONTROL_START, MemoryError, MemoryRegion, OpenBusPolicy, Ps1Memory, RAM_SIZE,
    };

    #[test]
    fn ram_is_mirrored_through_first_eight_megabytes_and_kseg_aliases() {
        let mut memory = Ps1Memory::new(OpenBusPolicy::Strict);
        memory.write_u32(0x0000_0120, 0x1234_5678).unwrap();
        assert_eq!(memory.read_u32(0x0020_0120).unwrap(), 0x1234_5678);
        assert_eq!(memory.read_u32(0x8040_0120).unwrap(), 0x1234_5678);
        assert_eq!(memory.read_u32(0xa060_0120).unwrap(), 0x1234_5678);
        assert_eq!(memory.ram().len(), RAM_SIZE);
    }

    #[test]
    fn scratchpad_is_distinct_and_boundary_checked() {
        let mut memory = Ps1Memory::new(OpenBusPolicy::Strict);
        memory.write_u16(0x1f80_0002, 0xbeef).unwrap();
        assert_eq!(memory.read_u16(0x9f80_0002).unwrap(), 0xbeef);
        assert!(matches!(
            memory.read_u32(0x1f80_03fe),
            Err(MemoryError::CrossesBoundary { .. })
        ));
    }

    #[test]
    fn memory_control_reset_values_and_readback_are_instance_owned() {
        let mut first = Ps1Memory::new(OpenBusPolicy::Strict);
        let second = Ps1Memory::new(OpenBusPolicy::Strict);
        assert_eq!(first.read_control(0x1f80_1014).unwrap(), 0x2009_31e1);
        first.write_control(0x1f80_1014, 0x2209_31e1).unwrap();
        assert_eq!(first.read_control(0x1f80_1014).unwrap(), 0x2209_31e1);
        assert_eq!(second.read_control(0x1f80_1014).unwrap(), 0x2009_31e1);
        assert!(matches!(
            first.read_control(MEMORY_CONTROL_START + 2),
            Err(MemoryError::Mmio { .. })
        ));
    }

    #[test]
    fn mmio_and_hle_rom_are_explicit_and_no_bios_can_be_loaded() {
        assert!(matches!(
            Ps1Memory::classify(0x1f80_1c00).unwrap(),
            MemoryRegion::Mmio { .. }
        ));
        let mut memory = Ps1Memory::new(OpenBusPolicy::Strict);
        assert!(matches!(
            memory.read_u8(0xbfc0_0000),
            Err(MemoryError::HleRom { .. })
        ));
        assert!(matches!(
            memory.write_u8(0xbfc0_0000, 1),
            Err(MemoryError::HleRom { .. })
        ));
    }

    #[test]
    fn strict_and_ones_open_bus_policies_differ_without_global_state() {
        let strict = Ps1Memory::new(OpenBusPolicy::Strict);
        let ones = Ps1Memory::new(OpenBusPolicy::Ones);
        assert!(matches!(
            strict.read_u8(0x1e00_0000),
            Err(MemoryError::Unmapped { .. })
        ));
        assert_eq!(ones.read_u32(0x1e00_0000).unwrap(), u32::MAX);
        assert!(matches!(
            strict.read_u8(0xc000_0000),
            Err(MemoryError::UnsupportedSegment { .. })
        ));
    }
}
