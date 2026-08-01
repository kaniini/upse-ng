// SPDX-License-Identifier: LGPL-2.1-or-later

use thiserror::Error;

use crate::{BiosError, KernelError};

/// Half-open guest address range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestRange {
    /// Inclusive first byte.
    pub start: u32,
    /// Exclusive end byte.
    pub end: u32,
}

impl GuestRange {
    /// Creates a validated non-wrapping range.
    #[must_use]
    pub const fn new(start: u32, end: u32) -> Option<Self> {
        if start <= end {
            Some(Self { start, end })
        } else {
            None
        }
    }

    /// Reports whether the range contains the complete byte interval.
    #[must_use]
    pub fn contains(self, address: u32, size: usize) -> bool {
        u32::try_from(size)
            .ok()
            .and_then(|size| address.checked_add(size))
            .is_some_and(|end| address >= self.start && end <= self.end)
    }

    /// Validates a non-null pointer, alignment, and byte count.
    ///
    /// # Errors
    ///
    /// Returns a guest-compatible address or size error.
    pub fn validate(self, address: u32, size: usize, alignment: u32) -> Result<(), KernelError> {
        if size == 0 {
            return Err(KernelError::IllegalSize);
        }
        if address == 0
            || alignment == 0
            || !alignment.is_power_of_two()
            || address % alignment != 0
        {
            return Err(KernelError::IllegalObject);
        }
        if !self.contains(address, size) {
            return Err(KernelError::IllegalObject);
        }
        Ok(())
    }
}

/// Failure returned by a machine-owned guest-memory implementation.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct GuestMemoryError {
    message: String,
}

impl GuestMemoryError {
    /// Constructs a guest-memory diagnostic.
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

/// Narrow byte-oriented memory interface consumed by IOP BIOS services.
pub trait GuestMemory {
    /// Returns the complete directly accessible guest RAM range.
    fn range(&self) -> GuestRange;

    /// Reads a complete byte interval without partial success.
    ///
    /// # Errors
    ///
    /// Returns [`GuestMemoryError`] when the interval is not readable.
    fn read(&self, address: u32, output: &mut [u8]) -> Result<(), GuestMemoryError>;

    /// Writes a complete byte interval without partial success.
    ///
    /// # Errors
    ///
    /// Returns [`GuestMemoryError`] when the interval is not writable.
    fn write(&mut self, address: u32, input: &[u8]) -> Result<(), GuestMemoryError>;
}

pub(crate) fn read_guest<M: GuestMemory>(
    guest: &M,
    address: u32,
    output: &mut [u8],
) -> Result<(), BiosError> {
    guest.range().validate(address, output.len(), 1)?;
    guest
        .read(address, output)
        .map_err(|source| BiosError::GuestMemory {
            operation: "read",
            address,
            size: output.len(),
            source,
        })
}

pub(crate) fn write_guest<M: GuestMemory>(
    guest: &mut M,
    address: u32,
    input: &[u8],
) -> Result<(), BiosError> {
    guest.range().validate(address, input.len(), 1)?;
    guest
        .write(address, input)
        .map_err(|source| BiosError::GuestMemory {
            operation: "write",
            address,
            size: input.len(),
            source,
        })
}

pub(crate) fn write_u16<M: GuestMemory>(
    guest: &mut M,
    address: u32,
    value: u16,
) -> Result<(), BiosError> {
    write_guest(guest, address, &value.to_le_bytes())
}

pub(crate) fn write_u32<M: GuestMemory>(
    guest: &mut M,
    address: u32,
    value: u32,
) -> Result<(), BiosError> {
    write_guest(guest, address, &value.to_le_bytes())
}
