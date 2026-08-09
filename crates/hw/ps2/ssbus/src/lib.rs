// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS2 IOP subsystem-bus address and delay registers.

use thiserror::Error;

/// Number of SSBUS device selectors.
pub const DEVICE_COUNT: usize = 13;
/// Shared delay-configuration register.
pub const COMMON_DELAY: u32 = 0x1f80_1020;

const BASE_REGISTERS: [Option<u32>; DEVICE_COUNT] = [
    Some(0x1f80_1000),
    Some(0x1f80_1400),
    None,
    None,
    Some(0x1f80_1404),
    Some(0x1f80_1408),
    None,
    None,
    Some(0x1f80_1004),
    Some(0x1f80_140c),
    None,
    Some(0x1f80_1410),
    None,
];

const DELAY_REGISTERS: [Option<u32>; DEVICE_COUNT] = [
    Some(0x1f80_1008),
    Some(0x1f80_100c),
    Some(0x1f80_1010),
    None,
    Some(0x1f80_1014),
    Some(0x1f80_1018),
    None,
    None,
    Some(0x1f80_101c),
    Some(0x1f80_1414),
    Some(0x1f80_1418),
    Some(0x1f80_141c),
    Some(0x1f80_1420),
];

/// Invalid SSBUS register or device selector.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SsbusError {
    /// The physical address is not an implemented SSBUS register.
    #[error("invalid IOP SSBUS register address {address:#010x}")]
    InvalidRegister {
        /// Physical register address.
        address: u32,
    },
    /// The device has no register of the requested kind.
    #[error("IOP SSBUS device {device} has no {kind} register")]
    InvalidDevice {
        /// Numeric SSBUS device selector.
        device: u32,
        /// Register group requested by the caller.
        kind: &'static str,
    },
}

/// Instance-owned subsystem-bus configuration.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SsbusController {
    bases: [u32; DEVICE_COUNT],
    delays: [u32; DEVICE_COUNT],
    common_delay: u32,
}

impl SsbusController {
    /// Constructs reset register state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bases: [0; DEVICE_COUNT],
            delays: [0; DEVICE_COUNT],
            common_delay: 0,
        }
    }

    /// Reports whether an aligned word belongs to this component.
    #[must_use]
    pub fn contains(address: u32) -> bool {
        address == COMMON_DELAY
            || BASE_REGISTERS
                .iter()
                .flatten()
                .any(|value| *value == address)
            || DELAY_REGISTERS
                .iter()
                .flatten()
                .any(|value| *value == address)
    }

    /// Reads one aligned physical register.
    ///
    /// # Errors
    ///
    /// Returns [`SsbusError::InvalidRegister`] for an unknown address.
    pub fn read_u32(&self, address: u32) -> Result<u32, SsbusError> {
        if address == COMMON_DELAY {
            return Ok(self.common_delay);
        }
        if let Some(device) = register_device(&BASE_REGISTERS, address) {
            return Ok(self.bases[device]);
        }
        if let Some(device) = register_device(&DELAY_REGISTERS, address) {
            return Ok(self.delays[device]);
        }
        Err(SsbusError::InvalidRegister { address })
    }

    /// Writes one aligned physical register.
    ///
    /// # Errors
    ///
    /// Returns [`SsbusError::InvalidRegister`] for an unknown address.
    pub fn write_u32(&mut self, address: u32, value: u32) -> Result<(), SsbusError> {
        if address == COMMON_DELAY {
            self.common_delay = value;
            return Ok(());
        }
        if let Some(device) = register_device(&BASE_REGISTERS, address) {
            self.bases[device] = value;
            return Ok(());
        }
        if let Some(device) = register_device(&DELAY_REGISTERS, address) {
            self.delays[device] = value;
            return Ok(());
        }
        Err(SsbusError::InvalidRegister { address })
    }

    /// Reads a device delay value.
    ///
    /// # Errors
    ///
    /// Returns [`SsbusError::InvalidDevice`] when no delay register exists.
    pub fn delay(&self, device: u32) -> Result<u32, SsbusError> {
        let index = device_index(&DELAY_REGISTERS, device, "delay")?;
        Ok(self.delays[index])
    }

    /// Replaces a device delay and returns its old value.
    ///
    /// # Errors
    ///
    /// Returns [`SsbusError::InvalidDevice`] when no delay register exists.
    pub fn set_delay(&mut self, device: u32, value: u32) -> Result<u32, SsbusError> {
        let index = device_index(&DELAY_REGISTERS, device, "delay")?;
        Ok(std::mem::replace(&mut self.delays[index], value))
    }

    /// Reads a device base-address value.
    ///
    /// # Errors
    ///
    /// Returns [`SsbusError::InvalidDevice`] when no base register exists.
    pub fn base(&self, device: u32) -> Result<u32, SsbusError> {
        let index = device_index(&BASE_REGISTERS, device, "base-address")?;
        Ok(self.bases[index])
    }

    /// Replaces a device base address and returns its old value.
    ///
    /// # Errors
    ///
    /// Returns [`SsbusError::InvalidDevice`] when no base register exists.
    pub fn set_base(&mut self, device: u32, value: u32) -> Result<u32, SsbusError> {
        let index = device_index(&BASE_REGISTERS, device, "base-address")?;
        Ok(std::mem::replace(&mut self.bases[index], value))
    }

    /// Reads the shared delay register.
    #[must_use]
    pub const fn common_delay(&self) -> u32 {
        self.common_delay
    }

    /// Replaces the shared delay register and returns its old value.
    pub fn set_common_delay(&mut self, value: u32) -> u32 {
        std::mem::replace(&mut self.common_delay, value)
    }
}

fn register_device(registers: &[Option<u32>; DEVICE_COUNT], address: u32) -> Option<usize> {
    registers
        .iter()
        .position(|register| *register == Some(address))
}

fn device_index(
    registers: &[Option<u32>; DEVICE_COUNT],
    device: u32,
    kind: &'static str,
) -> Result<usize, SsbusError> {
    usize::try_from(device)
        .ok()
        .filter(|index| registers.get(*index).is_some_and(Option::is_some))
        .ok_or(SsbusError::InvalidDevice { device, kind })
}

#[cfg(test)]
mod tests {
    use super::{COMMON_DELAY, SsbusController, SsbusError};

    #[test]
    fn direct_and_typed_register_access_share_state() {
        let mut bus = SsbusController::new();
        bus.write_u32(0x1f80_1404, 0xbf90_0000).unwrap();
        bus.write_u32(0x1f80_140c, 0xbf90_0800).unwrap();
        bus.write_u32(0x1f80_1014, 0x200b_31e1).unwrap();
        bus.write_u32(0x1f80_1414, 0x200b_31e1).unwrap();
        assert_eq!(bus.base(4).unwrap(), 0xbf90_0000);
        assert_eq!(bus.base(9).unwrap(), 0xbf90_0800);
        assert_eq!(bus.delay(4).unwrap(), 0x200b_31e1);
        assert_eq!(bus.delay(9).unwrap(), 0x200b_31e1);
        assert_eq!(bus.set_common_delay(0x1234), 0);
        assert_eq!(bus.read_u32(COMMON_DELAY).unwrap(), 0x1234);
    }

    #[test]
    fn holes_and_unknown_addresses_are_diagnostic() {
        let mut bus = SsbusController::new();
        assert!(matches!(
            bus.set_base(2, 1),
            Err(SsbusError::InvalidDevice { device: 2, .. })
        ));
        assert!(matches!(
            bus.write_u32(0x1f80_1424, 1),
            Err(SsbusError::InvalidRegister {
                address: 0x1f80_1424
            })
        ));
    }
}
