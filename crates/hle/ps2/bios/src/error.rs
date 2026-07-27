// SPDX-License-Identifier: LGPL-2.1-or-later

use thiserror::Error;

use crate::memory::GuestMemoryError;

/// IOP kernel result codes used by the HLE boundary.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[repr(i32)]
pub enum KernelError {
    /// Generic kernel failure.
    #[error("generic error")]
    Error = -1,
    /// Exception number is outside the IOP exception table.
    #[error("illegal exception code")]
    IllegalExceptionCode = -50,
    /// No handler occupies the exception slot.
    #[error("exception handler is not in use")]
    ExceptionHandlerNotInUse = -51,
    /// An exception handler already occupies the slot.
    #[error("exception handler is already in use")]
    ExceptionHandlerInUse = -52,
    /// The operation is invalid in the current CPU context.
    #[error("illegal execution context")]
    IllegalContext = -100,
    /// Interrupt number is outside the IOP interrupt table.
    #[error("illegal interrupt code")]
    IllegalInterruptCode = -101,
    /// An interrupt handler already exists.
    #[error("interrupt handler already found")]
    FoundHandler = -104,
    /// An interrupt handler was not registered.
    #[error("interrupt handler not found")]
    HandlerNotFound = -105,
    /// An object identifier or handle is invalid.
    #[error("illegal kernel object")]
    IllegalObject = -201,
    /// A module identifier or name is unknown.
    #[error("unknown module")]
    UnknownModule = -202,
    /// A memory range is still in use.
    #[error("memory is in use")]
    MemoryInUse = -205,
    /// Module has already started.
    #[error("module has already started")]
    AlreadyStarted = -206,
    /// Module has not started.
    #[error("module has not started")]
    NotStarted = -207,
    /// Module has already stopped.
    #[error("module has already stopped")]
    AlreadyStopped = -208,
    /// Module cannot be stopped in its current state.
    #[error("module cannot stop")]
    CannotStop = -209,
    /// Module is not stopped.
    #[error("module is not stopped")]
    NotStopped = -210,
    /// Module did not declare itself removable.
    #[error("module is not removable")]
    NotRemovable = -211,
    /// An export library with the same name is registered.
    #[error("library already found")]
    LibraryFound = -212,
    /// No compatible export library is registered.
    #[error("library not found")]
    LibraryNotFound = -213,
    /// Library header, name, version, or ordinal is invalid.
    #[error("illegal library")]
    IllegalLibrary = -214,
    /// Registered library still has users.
    #[error("library is in use")]
    LibraryInUse = -215,
    /// Module is already stopping.
    #[error("module is already stopping")]
    AlreadyStopping = -216,
    /// A fixed-capacity table or memory arena is exhausted.
    #[error("not enough memory")]
    NoMemory = -400,
    /// Entry point or handler address is invalid.
    #[error("illegal entry")]
    IllegalEntry = -402,
    /// Identifier is outside the supported range.
    #[error("illegal identifier")]
    IllegalId = -406,
    /// Guest pointer is null, unaligned, or outside RAM.
    #[error("illegal address")]
    IllegalAddress = -429,
    /// Allocation mode is not supported.
    #[error("illegal memory allocation mode")]
    IllegalMemoryMode = -431,
    /// A zero or overflowing size was supplied.
    #[error("illegal size")]
    IllegalSize = -432,
}

impl KernelError {
    /// Returns the guest-visible signed result.
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }
}

/// Structured PS2 BIOS HLE failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BiosError {
    /// Guest memory rejected an otherwise validated access.
    #[error("guest memory {operation} failed at {address:#010x} for {size:#x} bytes: {source}")]
    GuestMemory {
        /// Operation being attempted.
        operation: &'static str,
        /// First guest address.
        address: u32,
        /// Requested byte count.
        size: usize,
        /// Machine-owned diagnostic.
        source: GuestMemoryError,
    },
    /// IRX parsing, relocation, or target transfer failed.
    #[error("IOP module load failed: {0}")]
    Irx(#[from] upse_irx::IrxError),
    /// A BIOS operation returned a documented guest error.
    #[error("IOP kernel operation failed: {0}")]
    Kernel(#[from] KernelError),
    /// A syscall or import reached no implemented service.
    #[error(
        "unknown IOP operation {library} ordinal {ordinal:#06x} for module {module_id} at PC {pc:#010x}"
    )]
    UnknownOperation {
        /// Library name, or `syscall` for the system-call boundary.
        library: String,
        /// Import ordinal or syscall number.
        ordinal: u16,
        /// Calling module identifier.
        module_id: u32,
        /// Guest call-site PC.
        pc: u32,
    },
    /// Host-side lifecycle sequencing violated an explicit state transition.
    #[error("invalid BIOS HLE state: {0}")]
    InvalidState(&'static str),
}
