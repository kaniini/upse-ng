// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS1 interrupt status, mask, and CPU-line behavior.

use thiserror::Error;

/// Physical address of the interrupt status register.
pub const I_STAT: u32 = 0x1f80_1070;
/// Physical address of the interrupt mask register.
pub const I_MASK: u32 = 0x1f80_1074;

const VALID_BITS: u16 = 0x07ff;

/// A hardware source bit in `I_STAT` and `I_MASK`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum InterruptSource {
    /// Vertical blank.
    VBlank = 0,
    /// GPU interrupt.
    Gpu = 1,
    /// CD-ROM controller interrupt.
    CdRom = 2,
    /// DMA controller interrupt.
    Dma = 3,
    /// Root counter 0 interrupt.
    Timer0 = 4,
    /// Root counter 1 interrupt.
    Timer1 = 5,
    /// Root counter 2 interrupt.
    Timer2 = 6,
    /// Controller and memory-card interrupt.
    Controller = 7,
    /// Serial I/O interrupt.
    Sio = 8,
    /// Sound processing unit interrupt.
    Spu = 9,
    /// Light-pen interrupt.
    LightPen = 10,
}

impl InterruptSource {
    /// Returns the source's register bit.
    #[must_use]
    pub const fn bit(self) -> u16 {
        match self {
            Self::VBlank => 1 << 0,
            Self::Gpu => 1 << 1,
            Self::CdRom => 1 << 2,
            Self::Dma => 1 << 3,
            Self::Timer0 => 1 << 4,
            Self::Timer1 => 1 << 5,
            Self::Timer2 => 1 << 6,
            Self::Controller => 1 << 7,
            Self::Sio => 1 << 8,
            Self::Spu => 1 << 9,
            Self::LightPen => 1 << 10,
        }
    }
}

/// Invalid interrupt-controller register access.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum IrqError {
    /// Address is not a modeled interrupt register.
    #[error("invalid PS1 interrupt register address {address:#010x}")]
    InvalidRegister {
        /// Physical address supplied by the machine.
        address: u32,
    },
}

/// Instance-owned PS1 interrupt controller.
///
/// Pulsed sources latch their status bit until acknowledged. Level sources are
/// also latched and immediately reassert if software acknowledges them while
/// the producer's level remains high.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InterruptController {
    status: u16,
    mask: u16,
    levels: u16,
}

impl InterruptController {
    /// Constructs a reset controller with every source masked.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            status: 0,
            mask: 0,
            levels: 0,
        }
    }

    /// Latches a pulse from one source.
    pub fn request(&mut self, source: InterruptSource) {
        self.status |= source.bit();
    }

    /// Changes a producer's level and latches a rising or currently high level.
    pub fn set_level(&mut self, source: InterruptSource, asserted: bool) {
        let bit = source.bit();
        if asserted {
            self.levels |= bit;
            self.status |= bit;
        } else {
            self.levels &= !bit;
        }
    }

    /// Returns the latched status register.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Returns the source mask register.
    #[must_use]
    pub const fn mask(&self) -> u16 {
        self.mask
    }

    /// Reports whether the R3000 external interrupt line is asserted.
    #[must_use]
    pub const fn pending(&self) -> bool {
        self.status & self.mask != 0
    }

    /// Applies PS1 `I_STAT` acknowledgement semantics: zero clears, one keeps.
    pub fn acknowledge(&mut self, value: u16) {
        self.status &= value & VALID_BITS;
        self.status |= self.levels;
    }

    /// Replaces the implemented bits of `I_MASK`.
    pub fn set_mask(&mut self, value: u16) {
        self.mask = value & VALID_BITS;
    }

    /// Reads a 32-bit interrupt-controller register.
    ///
    /// # Errors
    ///
    /// Returns [`IrqError::InvalidRegister`] for any address other than
    /// [`I_STAT`] or [`I_MASK`].
    pub fn read(&self, address: u32) -> Result<u32, IrqError> {
        match address {
            I_STAT => Ok(u32::from(self.status)),
            I_MASK => Ok(u32::from(self.mask)),
            _ => Err(IrqError::InvalidRegister { address }),
        }
    }

    /// Writes a 32-bit interrupt-controller register.
    ///
    /// # Errors
    ///
    /// Returns [`IrqError::InvalidRegister`] for any address other than
    /// [`I_STAT`] or [`I_MASK`].
    pub fn write(&mut self, address: u32, value: u32) -> Result<(), IrqError> {
        let bytes = value.to_le_bytes();
        let value = u16::from_le_bytes([bytes[0], bytes[1]]);
        match address {
            I_STAT => self.acknowledge(value),
            I_MASK => self.set_mask(value),
            _ => return Err(IrqError::InvalidRegister { address }),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{I_MASK, I_STAT, InterruptController, InterruptSource, IrqError};

    #[test]
    fn masks_gate_the_cpu_line_without_discarding_status() {
        let mut irq = InterruptController::new();
        irq.request(InterruptSource::Timer0);
        assert_eq!(irq.status(), InterruptSource::Timer0.bit());
        assert!(!irq.pending());
        irq.write(I_MASK, u32::from(InterruptSource::Timer0.bit()))
            .unwrap();
        assert!(irq.pending());
        irq.set_mask(InterruptSource::VBlank.bit());
        assert!(!irq.pending());
        assert_eq!(irq.status(), InterruptSource::Timer0.bit());
    }

    #[test]
    fn zero_acknowledges_and_one_preserves_each_latched_bit() {
        let mut irq = InterruptController::new();
        irq.request(InterruptSource::VBlank);
        irq.request(InterruptSource::Timer0);
        irq.request(InterruptSource::Spu);
        let keep = InterruptSource::Timer0.bit() | InterruptSource::Spu.bit();
        irq.write(I_STAT, u32::from(keep)).unwrap();
        assert_eq!(irq.read(I_STAT).unwrap(), u32::from(keep));
        irq.acknowledge(0);
        assert_eq!(irq.status(), 0);
    }

    #[test]
    fn asserted_levels_reappear_until_the_producer_drops_them() {
        let mut irq = InterruptController::new();
        irq.set_level(InterruptSource::Spu, true);
        irq.acknowledge(0);
        assert_eq!(irq.status(), InterruptSource::Spu.bit());
        irq.set_level(InterruptSource::Spu, false);
        assert_eq!(irq.status(), InterruptSource::Spu.bit());
        irq.acknowledge(0);
        assert_eq!(irq.status(), 0);
    }

    #[test]
    fn simultaneous_sources_and_register_errors_are_deterministic() {
        let mut irq = InterruptController::new();
        for source in [
            InterruptSource::Timer2,
            InterruptSource::VBlank,
            InterruptSource::Timer1,
        ] {
            irq.request(source);
        }
        let expected = InterruptSource::VBlank.bit()
            | InterruptSource::Timer1.bit()
            | InterruptSource::Timer2.bit();
        irq.set_mask(u16::MAX);
        assert_eq!(irq.mask(), 0x07ff);
        assert_eq!(irq.status(), expected);
        assert!(irq.pending());
        assert_eq!(
            irq.read(I_STAT + 2),
            Err(IrqError::InvalidRegister {
                address: I_STAT + 2
            })
        );
    }
}
