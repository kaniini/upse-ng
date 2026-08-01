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
    /// Kernel object attributes contain unsupported bits.
    #[error("illegal attributes")]
    IllegalAttribute = -401,
    /// Entry point or handler address is invalid.
    #[error("illegal entry")]
    IllegalEntry = -402,
    /// Thread priority is outside the IOP range.
    #[error("illegal thread priority")]
    IllegalPriority = -403,
    /// Thread stack is smaller than the IOP minimum.
    #[error("illegal thread stack size")]
    IllegalStackSize = -404,
    /// Wait or dispatch mode contains unsupported bits.
    #[error("illegal mode")]
    IllegalMode = -405,
    /// Special or numeric thread identifier is invalid for the operation.
    #[error("illegal thread identifier")]
    IllegalThreadId = -406,
    /// Thread handle is not present.
    #[error("unknown thread identifier")]
    UnknownThreadId = -407,
    /// Semaphore handle is not present.
    #[error("unknown semaphore identifier")]
    UnknownSemaphoreId = -408,
    /// Event-flag handle is not present.
    #[error("unknown event-flag identifier")]
    UnknownEventFlagId = -409,
    /// Message-box handle is not present.
    #[error("unknown message-box identifier")]
    UnknownMessageBoxId = -410,
    /// Variable-pool handle is not present.
    #[error("unknown variable-pool identifier")]
    UnknownVariablePoolId = -411,
    /// Fixed-pool handle is not present.
    #[error("unknown fixed-pool identifier")]
    UnknownFixedPoolId = -412,
    /// Thread is dormant.
    #[error("thread is dormant")]
    Dormant = -413,
    /// Thread is not dormant.
    #[error("thread is not dormant")]
    NotDormant = -414,
    /// Thread is not suspended.
    #[error("thread is not suspended")]
    NotSuspended = -415,
    /// Thread is not waiting.
    #[error("thread is not waiting")]
    NotWaiting = -416,
    /// Current context cannot block.
    #[error("current context cannot wait")]
    CannotWait = -417,
    /// A wait was released by another thread.
    #[error("wait was released")]
    ReleaseWait = -418,
    /// Semaphore has no available count.
    #[error("semaphore count is zero")]
    SemaphoreZero = -419,
    /// Semaphore count would exceed its maximum.
    #[error("semaphore count overflow")]
    SemaphoreOverflow = -420,
    /// Event-flag condition is not currently satisfied.
    #[error("event-flag condition is false")]
    EventFlagCondition = -421,
    /// Single-waiter event flag already has a waiter.
    #[error("event flag does not permit multiple waiters")]
    EventFlagMultiple = -422,
    /// Event-flag wait pattern is empty.
    #[error("illegal event-flag pattern")]
    EventFlagIllegalPattern = -423,
    /// Message box contains no message.
    #[error("message box contains no message")]
    MessageBoxNoMessage = -424,
    /// A waited-on kernel object was deleted.
    #[error("waited-on object was deleted")]
    WaitDeleted = -425,
    /// Pool address is not an allocated block.
    #[error("illegal memory block")]
    IllegalMemoryBlock = -426,
    /// Pool size or allocation size is invalid.
    #[error("illegal memory size")]
    IllegalMemorySize = -427,
    /// Scratchpad address is invalid.
    #[error("illegal scratchpad address")]
    IllegalScratchpadAddress = -428,
    /// Scratchpad allocation is already in use.
    #[error("scratchpad is in use")]
    ScratchpadInUse = -429,
    /// Scratchpad allocation is not in use.
    #[error("scratchpad is not in use")]
    ScratchpadNotInUse = -430,
    /// Allocation or object type is invalid.
    #[error("illegal type")]
    IllegalType = -431,
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
    /// Checked emulated-time arithmetic overflowed.
    #[error("IOP kernel clock failed: {0}")]
    Clock(#[from] upse_clock::ClockError),
    /// Deterministic timed-event queue exhausted its insertion sequence.
    #[error("IOP kernel event scheduling failed: {0}")]
    Scheduler(#[from] upse_scheduler::SchedulerError),
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
