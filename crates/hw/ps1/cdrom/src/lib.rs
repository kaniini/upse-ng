// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS1 CD-ROM banked register interface.
//!
//! PSF playback has no disc image, but rip drivers still initialize the CD-ROM
//! controller. This component models its four byte-wide registers and reset
//! state without pretending that media is present.

use thiserror::Error;

/// First physical address in the CD-ROM register window.
pub const CDROM_BASE: u32 = 0x1f80_1800;
/// Last physical address in the CD-ROM register window.
pub const CDROM_END: u32 = CDROM_BASE + 3;

const FIFO_CAPACITY: usize = 16;
const REG1: u32 = CDROM_BASE + 1;
const REG2: u32 = CDROM_BASE + 2;
const REG3: u32 = CDROM_BASE + 3;

/// Invalid CD-ROM register access.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CdRomError {
    /// Address is outside the four-byte CD-ROM register window.
    #[error("invalid PS1 CD-ROM register address {address:#010x}")]
    InvalidRegister {
        /// Physical address supplied by the machine.
        address: u32,
    },
}

/// Instance-owned PS1 CD-ROM register state for a machine without media.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CdRom {
    index: u8,
    parameters: [u8; FIFO_CAPACITY],
    parameter_count: usize,
    response: [u8; FIFO_CAPACITY],
    response_read: usize,
    response_count: usize,
    interrupt_enable: u8,
    interrupt_flags: u8,
    request: u8,
    audio_volume: [u8; 4],
    applied_audio_volume: [u8; 4],
}

impl CdRom {
    /// Constructs the reset state of a drive with no disc inserted.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            index: 0,
            parameters: [0; FIFO_CAPACITY],
            parameter_count: 0,
            response: [0; FIFO_CAPACITY],
            response_read: 0,
            response_count: 0,
            interrupt_enable: 0,
            interrupt_flags: 0,
            request: 0,
            audio_volume: [0, 0, 0, 0],
            applied_audio_volume: [0, 0, 0, 0],
        }
    }

    /// Returns the currently selected register bank.
    #[must_use]
    pub const fn index(&self) -> u8 {
        self.index
    }

    /// Reports whether an enabled CD-ROM interrupt is pending.
    #[must_use]
    pub const fn interrupt_pending(&self) -> bool {
        self.interrupt_flags & self.interrupt_enable & 0x1f != 0
    }

    /// Reads one byte from a physical CD-ROM register address.
    ///
    /// # Errors
    ///
    /// Returns [`CdRomError::InvalidRegister`] outside the register window.
    pub fn read_register(&mut self, address: u32) -> Result<u8, CdRomError> {
        match address {
            CDROM_BASE => {
                let mut status = self.index | 0x10;
                if self.parameter_count == 0 {
                    status |= 0x08;
                }
                if self.parameter_count == FIFO_CAPACITY {
                    status &= !0x10;
                }
                if self.response_count != 0 {
                    status |= 0x20;
                }
                Ok(status)
            }
            REG1 => Ok(self.pop_response()),
            REG2 => Ok(0),
            REG3 => match self.index {
                1 | 3 => Ok(0xe0 | self.interrupt_flags),
                _ => Ok(0xe0 | self.interrupt_enable),
            },
            _ => Err(CdRomError::InvalidRegister { address }),
        }
    }

    /// Writes one byte to a physical CD-ROM register address.
    ///
    /// # Errors
    ///
    /// Returns [`CdRomError::InvalidRegister`] outside the register window.
    pub fn write_register(&mut self, address: u32, value: u8) -> Result<(), CdRomError> {
        match address {
            CDROM_BASE => self.index = value & 3,
            REG1 => match self.index {
                0 => self.accept_command(value),
                3 => self.audio_volume[2] = value,
                _ => {}
            },
            REG2 => match self.index {
                0 => {
                    if self.parameter_count < FIFO_CAPACITY {
                        self.parameters[self.parameter_count] = value;
                        self.parameter_count += 1;
                    }
                }
                1 => self.interrupt_enable = value & 0x1f,
                2 => self.audio_volume[0] = value,
                3 => self.audio_volume[3] = value,
                _ => unreachable!(),
            },
            REG3 => match self.index {
                0 => self.request = value,
                1 => {
                    self.interrupt_flags &= !(value & 0x1f);
                    if value & 0x40 != 0 {
                        self.parameter_count = 0;
                    }
                }
                2 => self.audio_volume[1] = value,
                3 if value & 0x20 != 0 => self.applied_audio_volume = self.audio_volume,
                3 => {}
                _ => unreachable!(),
            },
            _ => return Err(CdRomError::InvalidRegister { address }),
        }
        Ok(())
    }

    fn accept_command(&mut self, _command: u8) {
        // A PSF machine has no CD medium or sector source. Retaining the command
        // boundary and consuming parameters is sufficient for initialization;
        // media operations cannot contribute audio to the module.
        self.parameter_count = 0;
    }

    fn pop_response(&mut self) -> u8 {
        if self.response_count == 0 {
            return 0;
        }
        let value = self.response[self.response_read];
        self.response_read = (self.response_read + 1) % FIFO_CAPACITY;
        self.response_count -= 1;
        value
    }
}

impl Default for CdRom {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{CDROM_BASE, CdRom};

    #[test]
    fn reset_status_describes_empty_ready_fifos() {
        let mut drive = CdRom::new();
        assert_eq!(drive.read_register(CDROM_BASE).unwrap(), 0x18);
        assert_eq!(drive.read_register(CDROM_BASE + 1).unwrap(), 0);
        assert!(!drive.interrupt_pending());
    }

    #[test]
    fn bank_selection_controls_interrupt_registers() {
        let mut drive = CdRom::new();
        drive.write_register(CDROM_BASE, 1).unwrap();
        drive.write_register(CDROM_BASE + 2, 0x04).unwrap();
        assert_eq!(drive.index(), 1);
        assert_eq!(drive.read_register(CDROM_BASE + 3).unwrap(), 0xe0);
        drive.write_register(CDROM_BASE, 0).unwrap();
        assert_eq!(drive.read_register(CDROM_BASE + 3).unwrap(), 0xe4);
    }

    #[test]
    fn parameter_fifo_status_and_clear_follow_register_protocol() {
        let mut drive = CdRom::new();
        drive.write_register(CDROM_BASE + 2, 0xaa).unwrap();
        assert_eq!(drive.read_register(CDROM_BASE).unwrap() & 0x08, 0);
        drive.write_register(CDROM_BASE, 1).unwrap();
        drive.write_register(CDROM_BASE + 3, 0x40).unwrap();
        drive.write_register(CDROM_BASE, 0).unwrap();
        assert_ne!(drive.read_register(CDROM_BASE).unwrap() & 0x08, 0);
    }

    #[test]
    fn audio_volume_changes_are_latched_only_when_applied() {
        let mut drive = CdRom::new();
        drive.write_register(CDROM_BASE, 2).unwrap();
        drive.write_register(CDROM_BASE + 2, 0x11).unwrap();
        drive.write_register(CDROM_BASE + 3, 0x22).unwrap();
        drive.write_register(CDROM_BASE, 3).unwrap();
        drive.write_register(CDROM_BASE + 1, 0x33).unwrap();
        drive.write_register(CDROM_BASE + 2, 0x44).unwrap();
        drive.write_register(CDROM_BASE + 3, 0x20).unwrap();
        assert_eq!(drive.applied_audio_volume, [0x11, 0x22, 0x33, 0x44]);
    }
}
