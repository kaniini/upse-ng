// SPDX-License-Identifier: LGPL-2.1-or-later
//! Deterministic high-level emulation of the PS2 IOP BIOS foundation.
//!
//! The crate owns no CPU or hardware device. It operates through an abstract
//! guest-memory contract and deliberately has no firmware-image input path.

mod allocator;
mod dispatch;
mod error;
mod kernel;
mod memory;
mod module;
mod table;

pub use allocator::{Allocation, AllocationMode, SYSMEM_QUANTUM, SystemMemory};
pub use dispatch::{
    CallbackRequest, ControlBlock, CpuContext, DispatchBoundary, DispatchCall, ExceptionCode,
    HandlerRegistry, ImportCall, Trampoline,
};
pub use error::{BiosError, KernelError};
pub use kernel::{
    EventFlagSpec, FixedPoolSpec, Kernel, KernelEvent, MessageBoxSpec, RescheduleReason,
    ScheduleAction, SemaphoreSpec, Thread, ThreadSpec, ThreadState, VariablePoolSpec, WaitReason,
};
pub use memory::{GuestMemory, GuestMemoryError, GuestRange};
pub use module::{
    ExportRegistration, ModuleInfo, ModuleInvocation, ModuleRecord, ModuleRegistry, ModuleState,
    ResolvedImport,
};
pub use table::FixedTable;

/// First byte available to the IOP system-memory allocator.
pub const DEFAULT_HEAP_START: u32 = 0x0001_0000;
/// Exclusive end of the default two-megabyte IOP system-memory arena.
pub const DEFAULT_HEAP_END: u32 = 0x0020_0000;
/// Maximum simultaneous system-memory allocations.
pub const DEFAULT_ALLOCATION_CAPACITY: usize = 256;
/// Maximum resident module records.
pub const DEFAULT_MODULE_CAPACITY: usize = 64;
/// Maximum registered export libraries.
pub const DEFAULT_LIBRARY_CAPACITY: usize = 256;

/// Complete instance-owned PS2 IOP BIOS foundation.
#[derive(Clone, Debug)]
pub struct BiosHle {
    memory: SystemMemory,
    dispatch: DispatchBoundary,
    handlers: HandlerRegistry,
    kernel: Kernel,
    modules: ModuleRegistry,
}

impl BiosHle {
    /// Constructs reset BIOS state for the standard two-megabyte IOP map.
    ///
    /// # Errors
    ///
    /// Returns [`KernelError`] if the configured arena is invalid.
    pub fn new() -> Result<Self, KernelError> {
        Ok(Self {
            memory: SystemMemory::new(DEFAULT_HEAP_START, DEFAULT_HEAP_END)?,
            dispatch: DispatchBoundary::new(),
            handlers: HandlerRegistry::new(),
            kernel: Kernel::new(),
            modules: ModuleRegistry::new(),
        })
    }

    /// Resets all BIOS-owned host state and writes the guest control region.
    ///
    /// # Errors
    ///
    /// Returns [`BiosError`] if guest RAM cannot contain the reserved region.
    pub fn reset<M: GuestMemory>(&mut self, guest: &mut M) -> Result<(), BiosError> {
        self.memory.reset();
        self.dispatch.reset(guest)?;
        self.handlers.reset();
        self.kernel.reset();
        self.modules.reset();
        Ok(())
    }

    /// Returns the system-memory allocator.
    #[must_use]
    pub const fn memory(&self) -> &SystemMemory {
        &self.memory
    }

    /// Returns mutable system-memory allocator state.
    #[must_use]
    pub const fn memory_mut(&mut self) -> &mut SystemMemory {
        &mut self.memory
    }

    /// Returns the HLE dispatch boundary.
    #[must_use]
    pub const fn dispatch(&self) -> &DispatchBoundary {
        &self.dispatch
    }

    /// Returns mutable HLE dispatch state.
    #[must_use]
    pub const fn dispatch_mut(&mut self) -> &mut DispatchBoundary {
        &mut self.dispatch
    }

    /// Returns registered interrupt and `VBlank` handlers.
    #[must_use]
    pub const fn handlers(&self) -> &HandlerRegistry {
        &self.handlers
    }

    /// Returns mutable interrupt and `VBlank` handler state.
    #[must_use]
    pub const fn handlers_mut(&mut self) -> &mut HandlerRegistry {
        &mut self.handlers
    }

    /// Returns thread and synchronization state.
    #[must_use]
    pub const fn kernel(&self) -> &Kernel {
        &self.kernel
    }

    /// Returns mutable thread and synchronization state.
    #[must_use]
    pub const fn kernel_mut(&mut self) -> &mut Kernel {
        &mut self.kernel
    }

    /// Returns the module registry.
    #[must_use]
    pub const fn modules(&self) -> &ModuleRegistry {
        &self.modules
    }

    /// Returns mutable module registry state.
    #[must_use]
    pub const fn modules_mut(&mut self) -> &mut ModuleRegistry {
        &mut self.modules
    }

    /// Returns the allocator and module registry as disjoint mutable parts.
    #[must_use]
    pub fn memory_and_modules_mut(&mut self) -> (&mut SystemMemory, &mut ModuleRegistry) {
        (&mut self.memory, &mut self.modules)
    }

    /// Relocates and registers one parsed IOP module.
    ///
    /// # Errors
    ///
    /// Returns a structured module, allocation, or guest-memory diagnostic.
    pub fn load_module<M: GuestMemory>(
        &mut self,
        irx: &upse_irx::IrxModule,
        guest: &mut M,
    ) -> Result<u32, BiosError> {
        self.modules.load(irx, &mut self.memory, guest)
    }

    /// Unloads a permitted module and releases all of its system memory.
    ///
    /// # Errors
    ///
    /// Returns a lifecycle, library-use, or guest-memory diagnostic.
    pub fn unload_module<M: GuestMemory>(
        &mut self,
        id: u32,
        guest: &mut M,
    ) -> Result<ModuleRecord, BiosError> {
        self.modules.unload(id, &mut self.memory, guest)
    }
}

impl Default for BiosHle {
    fn default() -> Self {
        Self::new().expect("the fixed default IOP heap is valid")
    }
}
