// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS2 IOP primary interrupt status, mask, and CPU-line behavior.

use thiserror::Error;

/// Physical address of the IOP interrupt status register.
pub const I_STAT: u32 = 0x1f80_1070;
/// Physical address of the IOP interrupt mask register.
pub const I_MASK: u32 = 0x1f80_1074;
const VALID_BITS: u32 = 0x03ff_ffff;

/// A primary IOP hardware interrupt source.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum InterruptSource {
    /// Start of vertical blank.
    VBlank = 0,
    /// Sub-bus interrupt.
    SBus = 1,
    /// CD/DVD controller interrupt.
    Cdvd = 2,
    /// Aggregate interrupt from the first and second DMA controllers.
    Dma = 3,
    /// Counter 0 interrupt.
    Timer0 = 4,
    /// Counter 1 interrupt.
    Timer1 = 5,
    /// Counter 2 interrupt.
    Timer2 = 6,
    /// First serial controller interrupt.
    Sio0 = 7,
    /// Second serial controller interrupt.
    Sio1 = 8,
    /// SPU2 address interrupt.
    Spu2 = 9,
    /// Parallel port interrupt.
    Pio = 10,
    /// End of vertical blank.
    VBlankEnd = 11,
    /// DVD interrupt.
    Dvd = 12,
    /// Expansion/DEV9 interrupt.
    Dev9 = 13,
    /// Counter 3 interrupt.
    Timer3 = 14,
    /// Counter 4 interrupt.
    Timer4 = 15,
    /// Counter 5 interrupt.
    Timer5 = 16,
    /// SIO2 interrupt.
    Sio2 = 17,
    /// Hold timer 0 interrupt.
    Hold0 = 18,
    /// Hold timer 1 interrupt.
    Hold1 = 19,
    /// Hold timer 2 interrupt.
    Hold2 = 20,
    /// Hold timer 3 interrupt.
    Hold3 = 21,
    /// USB controller interrupt.
    Usb = 22,
    /// Expansion interface interrupt.
    Expansion = 23,
    /// IEEE-1394 interrupt.
    ILink = 24,
    /// IEEE-1394 DMA interrupt.
    FireWireDma = 25,
}

impl InterruptSource {
    /// Returns the source's register bit.
    #[must_use]
    pub const fn bit(self) -> u32 {
        1_u32 << (self as u8)
    }

    /// Converts one implemented bit number.
    #[must_use]
    pub const fn from_index(index: u8) -> Option<Self> {
        match index {
            0 => Some(Self::VBlank),
            1 => Some(Self::SBus),
            2 => Some(Self::Cdvd),
            3 => Some(Self::Dma),
            4 => Some(Self::Timer0),
            5 => Some(Self::Timer1),
            6 => Some(Self::Timer2),
            7 => Some(Self::Sio0),
            8 => Some(Self::Sio1),
            9 => Some(Self::Spu2),
            10 => Some(Self::Pio),
            11 => Some(Self::VBlankEnd),
            12 => Some(Self::Dvd),
            13 => Some(Self::Dev9),
            14 => Some(Self::Timer3),
            15 => Some(Self::Timer4),
            16 => Some(Self::Timer5),
            17 => Some(Self::Sio2),
            18 => Some(Self::Hold0),
            19 => Some(Self::Hold1),
            20 => Some(Self::Hold2),
            21 => Some(Self::Hold3),
            22 => Some(Self::Usb),
            23 => Some(Self::Expansion),
            24 => Some(Self::ILink),
            25 => Some(Self::FireWireDma),
            _ => None,
        }
    }
}

/// Typed interrupt input used by independently composable devices.
pub trait InterruptSink {
    /// Latches a pulse from one primary IOP source.
    fn request(&mut self, source: InterruptSource);

    /// Changes a level-sensitive source.
    fn set_level(&mut self, source: InterruptSource, asserted: bool);
}

/// Invalid IOP interrupt-controller access.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("invalid IOP interrupt register address {address:#010x}")]
pub struct IrqError {
    /// Physical address supplied by the machine.
    pub address: u32,
}

/// Instance-owned IOP interrupt controller.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InterruptController {
    status: u32,
    mask: u32,
    levels: u32,
}

impl InterruptController {
    /// Constructs reset state with all sources masked.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            status: 0,
            mask: 0,
            levels: 0,
        }
    }

    /// Returns the latched status register.
    #[must_use]
    pub const fn status(&self) -> u32 {
        self.status
    }

    /// Returns the source mask register.
    #[must_use]
    pub const fn mask(&self) -> u32 {
        self.mask
    }

    /// Returns enabled, latched sources.
    #[must_use]
    pub const fn pending_bits(&self) -> u32 {
        self.status & self.mask
    }

    /// Returns the lowest-numbered enabled, latched source.
    #[must_use]
    pub fn first_pending(&self) -> Option<InterruptSource> {
        let bits = self.pending_bits();
        if bits == 0 {
            None
        } else {
            u8::try_from(bits.trailing_zeros())
                .ok()
                .and_then(InterruptSource::from_index)
        }
    }

    /// Reports whether the R3000 external interrupt line is asserted.
    #[must_use]
    pub const fn pending(&self) -> bool {
        self.pending_bits() != 0
    }

    /// Applies IOP `I_STAT` acknowledgement semantics: zero clears, one keeps.
    pub fn acknowledge(&mut self, value: u32) {
        self.status &= value & VALID_BITS;
        self.status |= self.levels;
    }

    /// Replaces the implemented bits of `I_MASK`.
    pub fn set_mask(&mut self, value: u32) {
        self.mask = value & VALID_BITS;
    }

    /// Reads a primary interrupt register.
    ///
    /// # Errors
    ///
    /// Returns [`IrqError`] for an address other than [`I_STAT`] or [`I_MASK`].
    pub fn read(&self, address: u32) -> Result<u32, IrqError> {
        match address {
            I_STAT => Ok(self.status),
            I_MASK => Ok(self.mask),
            _ => Err(IrqError { address }),
        }
    }

    /// Writes a primary interrupt register.
    ///
    /// # Errors
    ///
    /// Returns [`IrqError`] for an address other than [`I_STAT`] or [`I_MASK`].
    pub fn write(&mut self, address: u32, value: u32) -> Result<(), IrqError> {
        match address {
            I_STAT => self.acknowledge(value),
            I_MASK => self.set_mask(value),
            _ => return Err(IrqError { address }),
        }
        Ok(())
    }
}

impl InterruptSink for InterruptController {
    fn request(&mut self, source: InterruptSource) {
        self.status |= source.bit();
    }

    fn set_level(&mut self, source: InterruptSource, asserted: bool) {
        let bit = source.bit();
        if asserted {
            self.levels |= bit;
            self.status |= bit;
        } else {
            self.levels &= !bit;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{I_MASK, I_STAT, InterruptController, InterruptSink, InterruptSource, IrqError};

    #[test]
    fn every_primary_source_has_an_independent_bit() {
        let mut irq = InterruptController::new();
        for index in 0..26 {
            let source = InterruptSource::from_index(index).unwrap();
            irq.request(source);
        }
        assert_eq!(irq.status(), 0x03ff_ffff);
        irq.set_mask(u32::MAX);
        assert_eq!(irq.pending_bits(), 0x03ff_ffff);
        assert_eq!(irq.first_pending(), Some(InterruptSource::VBlank));
    }

    #[test]
    fn acknowledge_preserves_asserted_levels() {
        let mut irq = InterruptController::new();
        irq.request(InterruptSource::Timer3);
        irq.set_level(InterruptSource::Spu2, true);
        irq.acknowledge(0);
        assert_eq!(irq.status(), InterruptSource::Spu2.bit());
        irq.set_level(InterruptSource::Spu2, false);
        irq.acknowledge(0);
        assert_eq!(irq.status(), 0);
    }

    #[test]
    fn mask_gates_the_cpu_line_without_losing_status() {
        let mut irq = InterruptController::new();
        irq.request(InterruptSource::Timer5);
        assert!(!irq.pending());
        irq.write(I_MASK, InterruptSource::Timer5.bit()).unwrap();
        assert!(irq.pending());
        irq.write(I_STAT, 0).unwrap();
        assert!(!irq.pending());
        assert_eq!(irq.mask(), InterruptSource::Timer5.bit());
    }

    #[test]
    fn invalid_register_is_diagnostic() {
        let irq = InterruptController::new();
        assert_eq!(
            irq.read(I_MASK + 4),
            Err(IrqError {
                address: I_MASK + 4
            })
        );
    }
}
