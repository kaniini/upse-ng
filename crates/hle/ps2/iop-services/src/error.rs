// SPDX-License-Identifier: LGPL-2.1-or-later

use thiserror::Error;

use crate::ServiceMemoryError;

/// Machine/kernel adapter failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct BackendError {
    message: String,
}

impl BackendError {
    /// Constructs an adapter diagnostic.
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

/// Structured import-service failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ServiceError {
    /// The named library is outside the PSF2 service matrix.
    #[error(
        "unknown IOP import library {library} version {version:#06x} ordinal {ordinal:#06x} for module {module_id} at PC {pc:#010x}"
    )]
    UnknownImport {
        /// Requested library name.
        library: String,
        /// Required packed version.
        version: u16,
        /// Requested ordinal.
        ordinal: u16,
        /// Calling module.
        module_id: u32,
        /// Original call site.
        pc: u32,
    },
    /// The library exists but cannot satisfy the requested version.
    #[error(
        "IOP import library {library} provides {provided:#06x}, not requested {required:#06x}, for module {module_id} at PC {pc:#010x}"
    )]
    VersionMismatch {
        /// Requested library name.
        library: String,
        /// Provided packed version.
        provided: u16,
        /// Required packed version.
        required: u16,
        /// Calling module.
        module_id: u32,
        /// Original call site.
        pc: u32,
    },
    /// The ordinal is known but intentionally unsupported in the IOP-only profile.
    #[error(
        "unsupported IOP import {library} {symbol} ordinal {ordinal:#06x} for module {module_id} at PC {pc:#010x}"
    )]
    UnsupportedImport {
        /// Requested library name.
        library: String,
        /// Public symbol name.
        symbol: &'static str,
        /// Requested ordinal.
        ordinal: u16,
        /// Calling module.
        module_id: u32,
        /// Original call site.
        pc: u32,
    },
    /// Guest pointer or memory operation failed.
    #[error("IOP service guest memory failed at {address:#010x} for {size:#x} bytes: {source}")]
    GuestMemory {
        /// First guest address.
        address: u32,
        /// Requested byte count.
        size: usize,
        /// Machine-owned detail.
        source: ServiceMemoryError,
    },
    /// A guest string was not terminated within its bound.
    #[error("unterminated IOP guest string at {address:#010x}")]
    UnterminatedString {
        /// First guest byte.
        address: u32,
    },
    /// A guest argument was invalid.
    #[error("invalid IOP service argument for {operation}: {detail}")]
    InvalidArgument {
        /// Operation name.
        operation: &'static str,
        /// Specific reason.
        detail: &'static str,
    },
    /// Read-only VFS lookup failed.
    #[error("PSF2 virtual filesystem operation failed: {0}")]
    Vfs(String),
    /// The machine/kernel adapter rejected the operation.
    #[error("IOP BIOS service adapter failed: {0}")]
    Backend(#[from] BackendError),
    /// A fixed service-owned resource table is exhausted.
    #[error("IOP service resource limit exceeded: {0}")]
    ResourceLimit(&'static str),
}
