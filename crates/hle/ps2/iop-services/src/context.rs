// SPDX-License-Identifier: LGPL-2.1-or-later

use thiserror::Error;

const REGISTER_COUNT: usize = 32;
const SP: usize = 29;

/// Contiguous guest address range exposed to import services.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestAddressRange {
    /// Inclusive first byte.
    pub start: u32,
    /// Exclusive end byte.
    pub end: u32,
}

impl GuestAddressRange {
    /// Validates one bounded, aligned guest range.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceMemoryError`] for null, unaligned, overflowing, or
    /// out-of-range accesses.
    pub fn validate(
        self,
        address: u32,
        size: usize,
        alignment: u32,
    ) -> Result<(), ServiceMemoryError> {
        if alignment == 0 || !alignment.is_power_of_two() || address % alignment != 0 {
            return Err(ServiceMemoryError::new("unaligned guest address"));
        }
        let size = u32::try_from(size).map_err(|_| ServiceMemoryError::new("size width"))?;
        let end = address
            .checked_add(size)
            .ok_or_else(|| ServiceMemoryError::new("guest address overflow"))?;
        if address < self.start || end > self.end {
            return Err(ServiceMemoryError::new("guest range is outside IOP RAM"));
        }
        Ok(())
    }
}

/// Machine-owned guest-memory failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct ServiceMemoryError {
    message: String,
}

impl ServiceMemoryError {
    /// Constructs a diagnostic.
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

/// Guest memory contract required by IOP services.
pub trait ServiceMemory {
    /// Returns the directly addressable IOP RAM range.
    fn range(&self) -> GuestAddressRange;

    /// Reads an exact byte range.
    ///
    /// # Errors
    ///
    /// Returns a machine-owned diagnostic for an invalid access.
    fn read(&self, address: u32, output: &mut [u8]) -> Result<(), ServiceMemoryError>;

    /// Writes an exact byte range.
    ///
    /// # Errors
    ///
    /// Returns a machine-owned diagnostic for an invalid access.
    fn write(&mut self, address: u32, input: &[u8]) -> Result<(), ServiceMemoryError>;
}

/// Complete register state visible at an IOP import boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceContext {
    registers: [u32; REGISTER_COUNT],
    /// Multiply/divide high register.
    pub hi: u32,
    /// Multiply/divide low register.
    pub lo: u32,
    /// Coprocessor-zero status.
    pub status: u32,
    /// Coprocessor-zero cause.
    pub cause: u32,
    /// Coprocessor-zero exception PC.
    pub epc: u32,
    /// Current guest PC.
    pub pc: u32,
}

impl ServiceContext {
    /// Constructs zeroed state at an entry point and stack.
    #[must_use]
    pub const fn reset(entry: u32, stack: u32) -> Self {
        let mut registers = [0; REGISTER_COUNT];
        registers[SP] = stack;
        Self {
            registers,
            hi: 0,
            lo: 0,
            status: 0,
            cause: 0,
            epc: 0,
            pc: entry,
        }
    }

    /// Reads one register.
    #[must_use]
    pub fn register(&self, index: usize) -> Option<u32> {
        self.registers.get(index).copied()
    }

    /// Writes one register while preserving register zero.
    pub fn set_register(&mut self, index: usize, value: u32) -> bool {
        let Some(register) = self.registers.get_mut(index) else {
            return false;
        };
        if index != 0 {
            *register = value;
        }
        true
    }

    /// Returns all registers.
    #[must_use]
    pub const fn registers(&self) -> &[u32; REGISTER_COUNT] {
        &self.registers
    }

    /// Returns the four conventional argument registers.
    #[must_use]
    pub fn arguments(&self) -> [u32; 4] {
        std::array::from_fn(|index| self.registers[4 + index])
    }
}
