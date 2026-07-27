// SPDX-License-Identifier: LGPL-2.1-or-later

use crate::memory::{read_guest, write_guest};
use crate::{BiosError, FixedTable, GuestMemory, GuestRange, KernelError};

const REGISTER_COUNT: usize = 32;
const A0: usize = 4;
const SP: usize = 29;
const RA: usize = 31;
const STATUS_MODE_MASK: u32 = 0x3f;
const CAUSE_EXCEPTION_MASK: u32 = 0x7c;
const CAUSE_INTERRUPT_PENDING: u32 = 1 << 10;
const CORE_ENTRY_COUNT: usize = 5;
const CORE_ENTRY_STRIDE: u32 = 16;
const IMPORT_TRAMPOLINE_COUNT: usize = 64;
const IMPORT_TRAMPOLINE_COUNT_U32: u32 = 64;
const IMPORT_TRAMPOLINE_STRIDE: u32 = 16;
const EXCEPTION_HANDLER_COUNT: usize = 16;
const INTERRUPT_HANDLER_COUNT: usize = 64;
const VBLANK_HANDLER_COUNT: usize = 8;

/// First byte of the guest-visible HLE control block.
pub const CONTROL_BLOCK_ADDRESS: u32 = 0x0000_0200;
/// Guest-visible control-block byte count.
pub const CONTROL_BLOCK_SIZE: usize = 0x80;
/// Fixed save area used at HLE exception entry.
pub const EXCEPTION_FRAME_ADDRESS: u32 = 0x0000_0280;
/// Encoded exception-frame byte count.
pub const EXCEPTION_FRAME_SIZE: usize = 38 * 4;
/// Exception entry guarded by the machine's HLE instruction trap.
pub const EXCEPTION_ENTRY: u32 = 0x0000_0400;
/// Interrupt entry guarded by the machine's HLE instruction trap.
pub const INTERRUPT_ENTRY: u32 = 0x0000_0410;
/// System-call dispatch entry.
pub const SYSCALL_ENTRY: u32 = 0x0000_0420;
/// Generic import dispatch entry.
pub const IMPORT_ENTRY: u32 = 0x0000_0430;
/// Callback/exception return entry.
pub const RETURN_ENTRY: u32 = 0x0000_0440;
/// First dynamically assigned import trampoline.
pub const IMPORT_TRAMPOLINE_BASE: u32 = 0x0000_0800;
/// Guest address containing the head of the module-info chain.
pub const MODULE_HEAD_ADDRESS: u32 = CONTROL_BLOCK_ADDRESS + 0x60;

const CONTROL_MAGIC: u32 = u32::from_le_bytes(*b"UP2H");
const CONTROL_ABI_VERSION: u32 = 1;
const CONTROL_BLOCK_SIZE_U32: u32 = 0x80;
const EXCEPTION_FRAME_SIZE_U32: u32 = 38 * 4;

/// Guest-visible description of the reserved HLE entry points.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlBlock {
    /// Four-byte `UP2H` signature.
    pub magic: u32,
    /// Project-defined control ABI revision.
    pub abi_version: u32,
    /// Encoded structure byte count.
    pub size: u32,
    /// Exception entry address.
    pub exception_entry: u32,
    /// Interrupt entry address.
    pub interrupt_entry: u32,
    /// Syscall entry address.
    pub syscall_entry: u32,
    /// Generic import entry address.
    pub import_entry: u32,
    /// Exception/callback return address.
    pub return_entry: u32,
    /// Guest exception-frame address.
    pub exception_frame: u32,
    /// Guest exception-frame byte count.
    pub exception_frame_size: u32,
    /// First dynamically allocated import trampoline.
    pub trampoline_base: u32,
    /// Byte stride between import trampolines.
    pub trampoline_stride: u32,
    /// Number of import trampolines.
    pub trampoline_count: u32,
    /// First allocatable system-memory byte.
    pub heap_start: u32,
    /// Exclusive end of system memory.
    pub heap_end: u32,
    /// Address of the guest module-list head word.
    pub module_head_address: u32,
}

impl ControlBlock {
    /// Returns the fixed HLE control-block layout.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            magic: CONTROL_MAGIC,
            abi_version: CONTROL_ABI_VERSION,
            size: CONTROL_BLOCK_SIZE_U32,
            exception_entry: EXCEPTION_ENTRY,
            interrupt_entry: INTERRUPT_ENTRY,
            syscall_entry: SYSCALL_ENTRY,
            import_entry: IMPORT_ENTRY,
            return_entry: RETURN_ENTRY,
            exception_frame: EXCEPTION_FRAME_ADDRESS,
            exception_frame_size: EXCEPTION_FRAME_SIZE_U32,
            trampoline_base: IMPORT_TRAMPOLINE_BASE,
            trampoline_stride: IMPORT_TRAMPOLINE_STRIDE,
            trampoline_count: IMPORT_TRAMPOLINE_COUNT_U32,
            heap_start: crate::DEFAULT_HEAP_START,
            heap_end: crate::DEFAULT_HEAP_END,
            module_head_address: MODULE_HEAD_ADDRESS,
        }
    }

    /// Encodes the complete little-endian guest structure.
    #[must_use]
    pub fn encode(self) -> [u8; CONTROL_BLOCK_SIZE] {
        let mut bytes = [0; CONTROL_BLOCK_SIZE];
        let words = [
            self.magic,
            self.abi_version,
            self.size,
            0,
            self.exception_entry,
            self.interrupt_entry,
            self.syscall_entry,
            self.import_entry,
            self.return_entry,
            self.exception_frame,
            self.exception_frame_size,
            self.trampoline_base,
            self.trampoline_stride,
            self.trampoline_count,
            self.heap_start,
            self.heap_end,
            self.module_head_address,
        ];
        for (index, word) in words.iter().copied().enumerate() {
            put_word(&mut bytes, index, word);
        }
        bytes
    }
}

impl Default for ControlBlock {
    fn default() -> Self {
        Self::new()
    }
}

/// Complete R3000 state preserved across an HLE exception or callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CpuContext {
    registers: [u32; REGISTER_COUNT],
    /// Multiply/divide high register.
    pub hi: u32,
    /// Multiply/divide low register.
    pub lo: u32,
    /// Coprocessor-zero status register.
    pub status: u32,
    /// Coprocessor-zero cause register.
    pub cause: u32,
    /// Coprocessor-zero exception program counter.
    pub epc: u32,
    /// Current instruction address.
    pub pc: u32,
}

impl CpuContext {
    /// Constructs zeroed CPU state at an entry point and stack.
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

    /// Reads a general-purpose register.
    #[must_use]
    pub fn register(&self, index: usize) -> Option<u32> {
        self.registers.get(index).copied()
    }

    /// Writes a general-purpose register while preserving register zero.
    pub fn set_register(&mut self, index: usize, value: u32) -> bool {
        let Some(register) = self.registers.get_mut(index) else {
            return false;
        };
        if index != 0 {
            *register = value;
        }
        true
    }

    /// Returns the complete register file.
    #[must_use]
    pub const fn registers(&self) -> &[u32; REGISTER_COUNT] {
        &self.registers
    }

    fn encode_frame(&self) -> [u8; EXCEPTION_FRAME_SIZE] {
        let mut bytes = [0; EXCEPTION_FRAME_SIZE];
        for (index, register) in self.registers.iter().copied().enumerate() {
            put_word(&mut bytes, index, register);
        }
        for (index, word) in [self.hi, self.lo, self.status, self.cause, self.epc, self.pc]
            .into_iter()
            .enumerate()
        {
            put_word(&mut bytes, REGISTER_COUNT + index, word);
        }
        bytes
    }

    fn decode_frame(bytes: &[u8; EXCEPTION_FRAME_SIZE]) -> Self {
        let mut registers = [0; REGISTER_COUNT];
        for (index, register) in registers.iter_mut().enumerate() {
            *register = word(bytes, index);
        }
        registers[0] = 0;
        Self {
            registers,
            hi: word(bytes, 32),
            lo: word(bytes, 33),
            status: word(bytes, 34),
            cause: word(bytes, 35),
            epc: word(bytes, 36),
            pc: word(bytes, 37),
        }
    }
}

/// R3000 exception codes accepted by the IOP BIOS boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ExceptionCode {
    /// Hardware interrupt.
    Interrupt = 0,
    /// Address error on load or instruction fetch.
    AddressLoad = 4,
    /// Address error on store.
    AddressStore = 5,
    /// System call.
    Syscall = 8,
    /// Breakpoint instruction.
    Breakpoint = 9,
    /// Reserved instruction.
    ReservedInstruction = 10,
    /// Arithmetic overflow.
    Overflow = 12,
}

/// Fully contextualized IOP import operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportCall {
    /// Eight-character import library name.
    pub library: String,
    /// Required packed major/minor version.
    pub version: u16,
    /// Function ordinal.
    pub ordinal: u16,
    /// Calling module identifier.
    pub module_id: u32,
    /// Original guest call-site PC.
    pub pc: u32,
}

/// Operation decoded at an HLE boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchCall {
    /// R3000 syscall-number dispatch.
    Syscall {
        /// Low 16 bits of the syscall number.
        number: u16,
        /// Calling module identifier.
        module_id: u32,
        /// Original guest call-site PC.
        pc: u32,
    },
    /// Named IOP import dispatch.
    Import(ImportCall),
}

impl DispatchCall {
    /// Produces the required explicit unknown-operation diagnostic.
    #[must_use]
    pub fn unknown(&self) -> BiosError {
        match self {
            Self::Syscall {
                number,
                module_id,
                pc,
            } => BiosError::UnknownOperation {
                library: "syscall".to_owned(),
                ordinal: *number,
                module_id: *module_id,
                pc: *pc,
            },
            Self::Import(call) => BiosError::UnknownOperation {
                library: call.library.clone(),
                ordinal: call.ordinal,
                module_id: call.module_id,
                pc: call.pc,
            },
        }
    }
}

/// Dynamically allocated guest import trampoline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Trampoline {
    /// Guest address trapped by the machine.
    pub address: u32,
    /// Operation associated with this slot.
    pub call: ImportCall,
}

/// BIOS-owned exception and import dispatch state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchBoundary {
    control: ControlBlock,
    trampolines: FixedTable<ImportCall, IMPORT_TRAMPOLINE_COUNT>,
    exception_active: bool,
}

impl DispatchBoundary {
    /// Constructs uninitialized dispatch state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            control: ControlBlock::new(),
            trampolines: FixedTable::new(0),
            exception_active: false,
        }
    }

    /// Writes all reserved guest structures and guard instructions.
    ///
    /// # Errors
    ///
    /// Returns [`BiosError`] if the guest does not expose the complete region.
    pub fn reset<M: GuestMemory>(&mut self, guest: &mut M) -> Result<(), BiosError> {
        self.trampolines.clear();
        self.exception_active = false;
        write_guest(guest, CONTROL_BLOCK_ADDRESS, &self.control.encode())?;
        write_guest(guest, EXCEPTION_FRAME_ADDRESS, &[0; EXCEPTION_FRAME_SIZE])?;
        for index in 0..CORE_ENTRY_COUNT {
            let index = u32::try_from(index).map_err(|_| KernelError::NoMemory)?;
            let address = EXCEPTION_ENTRY + index * CORE_ENTRY_STRIDE;
            write_guest(guest, address, &guard_stub(0x100 + index))?;
        }
        for index in 0..IMPORT_TRAMPOLINE_COUNT {
            let index_u32 = u32::try_from(index).map_err(|_| KernelError::NoMemory)?;
            let address = trampoline_address(index).ok_or(KernelError::NoMemory)?;
            write_guest(guest, address, &guard_stub(0x200 + index_u32))?;
        }
        Ok(())
    }

    /// Returns the fixed guest control block.
    #[must_use]
    pub const fn control(&self) -> ControlBlock {
        self.control
    }

    /// Saves a byte-exact exception frame and enters the HLE exception vector.
    ///
    /// # Errors
    ///
    /// Returns an explicit state error for a nested entry or a guest-memory
    /// diagnostic if the fixed frame cannot be written.
    pub fn enter_exception<M: GuestMemory>(
        &mut self,
        code: ExceptionCode,
        context: &mut CpuContext,
        guest: &mut M,
    ) -> Result<(), BiosError> {
        if self.exception_active {
            return Err(BiosError::InvalidState("nested HLE exception"));
        }
        let mut saved = context.clone();
        saved.epc = context.pc;
        saved.cause = (saved.cause & !CAUSE_EXCEPTION_MASK) | (u32::from(code as u8) << 2);
        if code == ExceptionCode::Interrupt {
            saved.cause |= CAUSE_INTERRUPT_PENDING;
        }
        write_guest(guest, EXCEPTION_FRAME_ADDRESS, &saved.encode_frame())?;
        context.epc = saved.epc;
        context.cause = saved.cause;
        context.status =
            (context.status & !STATUS_MODE_MASK) | ((context.status << 2) & STATUS_MODE_MASK);
        context.pc = EXCEPTION_ENTRY;
        self.exception_active = true;
        Ok(())
    }

    /// Saves an interrupt exception frame and enters the interrupt vector.
    ///
    /// # Errors
    ///
    /// Returns the same diagnostics as [`Self::enter_exception`].
    pub fn enter_interrupt<M: GuestMemory>(
        &mut self,
        context: &mut CpuContext,
        guest: &mut M,
    ) -> Result<(), BiosError> {
        self.enter_exception(ExceptionCode::Interrupt, context, guest)?;
        context.pc = INTERRUPT_ENTRY;
        Ok(())
    }

    /// Restores the complete saved CPU context.
    ///
    /// # Errors
    ///
    /// Returns an explicit state error without an active exception or a
    /// guest-memory diagnostic if the frame cannot be read.
    pub fn return_from_exception<M: GuestMemory>(
        &mut self,
        context: &mut CpuContext,
        guest: &M,
    ) -> Result<(), BiosError> {
        if !self.exception_active {
            return Err(BiosError::InvalidState("exception return without entry"));
        }
        let mut bytes = [0; EXCEPTION_FRAME_SIZE];
        read_guest(guest, EXCEPTION_FRAME_ADDRESS, &mut bytes)?;
        *context = CpuContext::decode_frame(&bytes);
        self.exception_active = false;
        Ok(())
    }

    /// Allocates and initializes one import trampoline.
    ///
    /// # Errors
    ///
    /// Returns a library/address error, table exhaustion, or guest-memory
    /// diagnostic. Failed writes do not consume a slot.
    pub fn allocate_import<M: GuestMemory>(
        &mut self,
        guest: &mut M,
        call: ImportCall,
    ) -> Result<Trampoline, BiosError> {
        validate_library_name(&call.library)?;
        let id = self.trampolines.next_id().ok_or(KernelError::NoMemory)?;
        let index = usize::try_from(id).map_err(|_| KernelError::NoMemory)?;
        let address = trampoline_address(index).ok_or(KernelError::NoMemory)?;
        write_guest(guest, address, &guard_stub(0x200 + id))?;
        self.trampolines.insert_at(id, call.clone())?;
        Ok(Trampoline { address, call })
    }

    /// Releases an import trampoline.
    ///
    /// # Errors
    ///
    /// Returns [`KernelError::IllegalEntry`] for a non-slot address.
    pub fn release_import(&mut self, address: u32) -> Result<ImportCall, KernelError> {
        let id = trampoline_id(address).ok_or(KernelError::IllegalEntry)?;
        self.trampolines.remove(id).ok_or(KernelError::IllegalEntry)
    }

    /// Resolves a syscall or bound import entry.
    ///
    /// # Errors
    ///
    /// Unknown entries produce a structured diagnostic containing a library,
    /// ordinal, module, and original PC.
    pub fn resolve(
        &self,
        entry: u32,
        syscall_number: u16,
        module_id: u32,
        caller_pc: u32,
    ) -> Result<DispatchCall, BiosError> {
        if entry == SYSCALL_ENTRY {
            return Ok(DispatchCall::Syscall {
                number: syscall_number,
                module_id,
                pc: caller_pc,
            });
        }
        if let Some(id) = trampoline_id(entry) {
            if let Some(call) = self.trampolines.get(id) {
                let mut call = call.clone();
                call.module_id = module_id;
                call.pc = caller_pc;
                return Ok(DispatchCall::Import(call));
            }
            return Err(BiosError::UnknownOperation {
                library: "unbound".to_owned(),
                ordinal: u16::try_from(id).unwrap_or(u16::MAX),
                module_id,
                pc: caller_pc,
            });
        }
        Err(BiosError::UnknownOperation {
            library: "entry".to_owned(),
            ordinal: u16::try_from((entry >> 2) & 0xffff).unwrap_or(u16::MAX),
            module_id,
            pc: caller_pc,
        })
    }
}

impl Default for DispatchBoundary {
    fn default() -> Self {
        Self::new()
    }
}

/// Guest callback selected by interrupt or `VBlank` dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CallbackRequest {
    /// Guest handler entry point.
    pub entry: u32,
    /// Handler argument placed in `a0`.
    pub argument: u32,
    /// HLE return trampoline placed in `ra`.
    pub return_address: u32,
    /// Interrupt source, or `VBlank` phase for `VBlank` callbacks.
    pub source: u32,
}

impl CallbackRequest {
    /// Installs callback registers in an already saved exception context.
    pub fn apply(self, context: &mut CpuContext) {
        context.set_register(A0, self.argument);
        context.set_register(RA, self.return_address);
        context.pc = self.entry;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Handler {
    entry: u32,
    argument: u32,
    mode: u32,
}

/// Fixed interrupt and `VBlank` handler registrations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandlerRegistry {
    exceptions: [Option<Handler>; EXCEPTION_HANDLER_COUNT],
    interrupts: [Option<Handler>; INTERRUPT_HANDLER_COUNT],
    vblank: [[Option<Handler>; VBLANK_HANDLER_COUNT]; 2],
}

impl HandlerRegistry {
    /// Creates an empty handler registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            exceptions: [None; EXCEPTION_HANDLER_COUNT],
            interrupts: [None; INTERRUPT_HANDLER_COUNT],
            vblank: [[None; VBLANK_HANDLER_COUNT]; 2],
        }
    }

    /// Removes every registration.
    pub fn reset(&mut self) {
        self.exceptions = [None; EXCEPTION_HANDLER_COUNT];
        self.interrupts = [None; INTERRUPT_HANDLER_COUNT];
        self.vblank = [[None; VBLANK_HANDLER_COUNT]; 2];
    }

    /// Registers one IOP exception handler.
    ///
    /// # Errors
    ///
    /// Returns BIOS-compatible exception, entry, or duplicate errors.
    pub fn register_exception(
        &mut self,
        exception: u32,
        entry: u32,
        guest_range: GuestRange,
    ) -> Result<(), KernelError> {
        let index = usize::try_from(exception)
            .ok()
            .filter(|index| *index < EXCEPTION_HANDLER_COUNT)
            .ok_or(KernelError::IllegalExceptionCode)?;
        guest_range
            .validate(entry, 4, 4)
            .map_err(|_| KernelError::IllegalEntry)?;
        if self.exceptions[index].is_some() {
            return Err(KernelError::ExceptionHandlerInUse);
        }
        self.exceptions[index] = Some(Handler {
            entry,
            argument: 0,
            mode: 0,
        });
        Ok(())
    }

    /// Releases one IOP exception handler.
    ///
    /// # Errors
    ///
    /// Returns a BIOS-compatible invalid or unused-slot result.
    pub fn release_exception(&mut self, exception: u32) -> Result<u32, KernelError> {
        let index = usize::try_from(exception)
            .ok()
            .filter(|index| *index < EXCEPTION_HANDLER_COUNT)
            .ok_or(KernelError::IllegalExceptionCode)?;
        let handler = self.exceptions[index]
            .take()
            .ok_or(KernelError::ExceptionHandlerNotInUse)?;
        Ok(handler.entry)
    }

    /// Selects one registered exception callback.
    ///
    /// # Errors
    ///
    /// Returns a BIOS-compatible invalid or unused-slot result.
    pub fn dispatch_exception(&self, exception: u32) -> Result<CallbackRequest, KernelError> {
        let index = usize::try_from(exception)
            .ok()
            .filter(|index| *index < EXCEPTION_HANDLER_COUNT)
            .ok_or(KernelError::IllegalExceptionCode)?;
        let handler = self.exceptions[index].ok_or(KernelError::ExceptionHandlerNotInUse)?;
        Ok(CallbackRequest {
            entry: handler.entry,
            argument: 0,
            return_address: RETURN_ENTRY,
            source: exception,
        })
    }

    /// Registers one IOP interrupt handler.
    ///
    /// # Errors
    ///
    /// Returns BIOS-compatible interrupt, mode, entry, or duplicate errors.
    pub fn register_interrupt(
        &mut self,
        interrupt: u32,
        mode: u32,
        entry: u32,
        argument: u32,
        guest_range: GuestRange,
    ) -> Result<(), KernelError> {
        let index = usize::try_from(interrupt)
            .ok()
            .filter(|index| *index < INTERRUPT_HANDLER_COUNT)
            .ok_or(KernelError::IllegalInterruptCode)?;
        if mode > 2 {
            return Err(KernelError::IllegalObject);
        }
        guest_range
            .validate(entry, 4, 4)
            .map_err(|_| KernelError::IllegalEntry)?;
        if self.interrupts[index].is_some() {
            return Err(KernelError::FoundHandler);
        }
        self.interrupts[index] = Some(Handler {
            entry,
            argument,
            mode,
        });
        Ok(())
    }

    /// Releases one interrupt handler and returns its guest entry.
    ///
    /// # Errors
    ///
    /// Returns a BIOS-compatible code for an invalid or empty slot.
    pub fn release_interrupt(&mut self, interrupt: u32) -> Result<u32, KernelError> {
        let index = usize::try_from(interrupt)
            .ok()
            .filter(|index| *index < INTERRUPT_HANDLER_COUNT)
            .ok_or(KernelError::IllegalInterruptCode)?;
        let handler = self.interrupts[index]
            .take()
            .ok_or(KernelError::HandlerNotFound)?;
        Ok(handler.entry)
    }

    /// Selects a registered interrupt callback.
    ///
    /// # Errors
    ///
    /// Returns a BIOS-compatible code for an invalid or empty slot.
    pub fn dispatch_interrupt(&self, interrupt: u32) -> Result<CallbackRequest, KernelError> {
        let index = usize::try_from(interrupt)
            .ok()
            .filter(|index| *index < INTERRUPT_HANDLER_COUNT)
            .ok_or(KernelError::IllegalInterruptCode)?;
        let handler = self.interrupts[index].ok_or(KernelError::HandlerNotFound)?;
        Ok(CallbackRequest {
            entry: handler.entry,
            argument: handler.argument,
            return_address: RETURN_ENTRY,
            source: interrupt,
        })
    }

    /// Registers one start/end `VBlank` handler at a deterministic priority.
    ///
    /// # Errors
    ///
    /// Returns a BIOS-compatible identifier, duplicate, or entry error.
    pub fn register_vblank(
        &mut self,
        phase: u32,
        priority: u32,
        entry: u32,
        argument: u32,
        guest_range: GuestRange,
    ) -> Result<(), KernelError> {
        let phase = vblank_phase(phase)?;
        let priority = usize::try_from(priority)
            .ok()
            .filter(|priority| *priority < VBLANK_HANDLER_COUNT)
            .ok_or(KernelError::IllegalId)?;
        guest_range
            .validate(entry, 4, 4)
            .map_err(|_| KernelError::IllegalEntry)?;
        if self.vblank[phase][priority].is_some() {
            return Err(KernelError::FoundHandler);
        }
        self.vblank[phase][priority] = Some(Handler {
            entry,
            argument,
            mode: 0,
        });
        Ok(())
    }

    /// Releases one `VBlank` handler.
    ///
    /// # Errors
    ///
    /// Returns a BIOS-compatible identifier or missing-handler error.
    pub fn release_vblank(&mut self, phase: u32, priority: u32) -> Result<u32, KernelError> {
        let phase = vblank_phase(phase)?;
        let priority = usize::try_from(priority)
            .ok()
            .filter(|priority| *priority < VBLANK_HANDLER_COUNT)
            .ok_or(KernelError::IllegalId)?;
        let handler = self.vblank[phase][priority]
            .take()
            .ok_or(KernelError::HandlerNotFound)?;
        Ok(handler.entry)
    }

    /// Returns `VBlank` callbacks in ascending priority order.
    ///
    /// # Errors
    ///
    /// Returns [`KernelError::IllegalId`] for a phase other than start/end.
    pub fn dispatch_vblank(&self, phase: u32) -> Result<Vec<CallbackRequest>, KernelError> {
        let phase_index = vblank_phase(phase)?;
        Ok(self.vblank[phase_index]
            .iter()
            .flatten()
            .map(|handler| CallbackRequest {
                entry: handler.entry,
                argument: handler.argument,
                return_address: RETURN_ENTRY,
                source: phase,
            })
            .collect())
    }
}

impl Default for HandlerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn vblank_phase(phase: u32) -> Result<usize, KernelError> {
    usize::try_from(phase)
        .ok()
        .filter(|phase| *phase < 2)
        .ok_or(KernelError::IllegalId)
}

fn validate_library_name(name: &str) -> Result<(), KernelError> {
    if name.is_empty() || name.len() > 8 || !name.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(KernelError::IllegalLibrary);
    }
    Ok(())
}

fn trampoline_address(index: usize) -> Option<u32> {
    let index = u32::try_from(index).ok()?;
    (index < IMPORT_TRAMPOLINE_COUNT_U32)
        .then(|| IMPORT_TRAMPOLINE_BASE + index * IMPORT_TRAMPOLINE_STRIDE)
}

fn trampoline_id(address: u32) -> Option<u32> {
    let offset = address.checked_sub(IMPORT_TRAMPOLINE_BASE)?;
    if offset % IMPORT_TRAMPOLINE_STRIDE != 0 {
        return None;
    }
    let id = offset / IMPORT_TRAMPOLINE_STRIDE;
    (id < IMPORT_TRAMPOLINE_COUNT_U32).then_some(id)
}

fn guard_stub(code: u32) -> [u8; IMPORT_TRAMPOLINE_STRIDE as usize] {
    let mut bytes = [0; IMPORT_TRAMPOLINE_STRIDE as usize];
    let instruction = ((code & 0x000f_ffff) << 6) | 0x0d;
    bytes[..4].copy_from_slice(&instruction.to_le_bytes());
    bytes[4..8].copy_from_slice(&0_u32.to_le_bytes());
    bytes[8..12].copy_from_slice(&CONTROL_MAGIC.to_le_bytes());
    bytes[12..16].copy_from_slice(&code.to_le_bytes());
    bytes
}

fn put_word(output: &mut [u8], index: usize, value: u32) {
    let offset = index * 4;
    output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn word(input: &[u8], index: usize) -> u32 {
    let offset = index * 4;
    u32::from_le_bytes(input[offset..offset + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GuestMemoryError;

    #[derive(Clone, Debug)]
    struct TestMemory(Vec<u8>);

    impl TestMemory {
        fn new() -> Self {
            Self(vec![0; 0x20_0000])
        }
    }

    impl GuestMemory for TestMemory {
        fn range(&self) -> GuestRange {
            GuestRange {
                start: 0,
                end: u32::try_from(self.0.len()).unwrap(),
            }
        }

        fn read(&self, address: u32, output: &mut [u8]) -> Result<(), GuestMemoryError> {
            let start = usize::try_from(address).unwrap();
            let end = start + output.len();
            output.copy_from_slice(
                self.0
                    .get(start..end)
                    .ok_or_else(|| GuestMemoryError::new("test read outside RAM"))?,
            );
            Ok(())
        }

        fn write(&mut self, address: u32, input: &[u8]) -> Result<(), GuestMemoryError> {
            let start = usize::try_from(address).unwrap();
            let end = start + input.len();
            self.0
                .get_mut(start..end)
                .ok_or_else(|| GuestMemoryError::new("test write outside RAM"))?
                .copy_from_slice(input);
            Ok(())
        }
    }

    #[test]
    fn control_block_and_guard_are_byte_exact() {
        let mut memory = TestMemory::new();
        let mut dispatch = DispatchBoundary::new();
        dispatch.reset(&mut memory).unwrap();
        assert_eq!(
            &memory.0[CONTROL_BLOCK_ADDRESS as usize..CONTROL_BLOCK_ADDRESS as usize + 4],
            b"UP2H"
        );
        assert_eq!(
            word(
                &memory.0[CONTROL_BLOCK_ADDRESS as usize..][..CONTROL_BLOCK_SIZE],
                4
            ),
            EXCEPTION_ENTRY
        );
        assert_eq!(
            word(&memory.0[EXCEPTION_ENTRY as usize..][..16], 0),
            (0x100 << 6) | 0x0d
        );
        assert_eq!(
            word(&memory.0[IMPORT_TRAMPOLINE_BASE as usize..][..16], 2),
            CONTROL_MAGIC
        );
    }

    #[test]
    fn exception_entry_and_return_restore_every_word() {
        let mut memory = TestMemory::new();
        let mut dispatch = DispatchBoundary::new();
        dispatch.reset(&mut memory).unwrap();
        let mut context = CpuContext::reset(0x1234, 0x1f_ff00);
        for index in 1..32 {
            context.set_register(index, u32::try_from(index).unwrap() * 0x0101_0101);
        }
        context.hi = 0x1122_3344;
        context.lo = 0x5566_7788;
        context.status = 0x0040_0103;
        context.cause = 0xabcd_0000;
        let original = context.clone();

        dispatch
            .enter_exception(ExceptionCode::Syscall, &mut context, &mut memory)
            .unwrap();
        assert_eq!(context.pc, EXCEPTION_ENTRY);
        assert_eq!(context.epc, original.pc);
        assert_eq!((context.cause >> 2) & 0x1f, 8);
        dispatch
            .return_from_exception(&mut context, &memory)
            .unwrap();
        let mut expected = original;
        expected.epc = expected.pc;
        expected.cause = (expected.cause & !CAUSE_EXCEPTION_MASK) | (8 << 2);
        assert_eq!(context, expected);
    }

    #[test]
    fn handlers_validate_and_dispatch_in_priority_order() {
        let range = GuestRange {
            start: 0x1000,
            end: 0x20_0000,
        };
        let mut handlers = HandlerRegistry::new();
        handlers.register_exception(8, 0x1800, range).unwrap();
        assert_eq!(handlers.dispatch_exception(8).unwrap().entry, 0x1800);
        assert_eq!(
            handlers.register_exception(8, 0x1804, range),
            Err(KernelError::ExceptionHandlerInUse)
        );
        handlers
            .register_interrupt(9, 0, 0x2000, 0x55, range)
            .unwrap();
        assert_eq!(
            handlers.register_interrupt(9, 0, 0x2004, 0, range),
            Err(KernelError::FoundHandler)
        );
        assert_eq!(handlers.dispatch_interrupt(9).unwrap().entry, 0x2000);
        handlers.register_vblank(0, 3, 0x3000, 3, range).unwrap();
        handlers.register_vblank(0, 1, 0x3010, 1, range).unwrap();
        let callbacks = handlers.dispatch_vblank(0).unwrap();
        assert_eq!(
            callbacks
                .iter()
                .map(|item| item.argument)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        assert_eq!(handlers.release_interrupt(9), Ok(0x2000));
        assert_eq!(handlers.release_exception(8), Ok(0x1800));
        assert_eq!(
            handlers.dispatch_interrupt(9),
            Err(KernelError::HandlerNotFound)
        );
    }

    #[test]
    fn import_slots_are_bound_and_unknown_calls_are_diagnostic() {
        let mut memory = TestMemory::new();
        let mut dispatch = DispatchBoundary::new();
        dispatch.reset(&mut memory).unwrap();
        let trampoline = dispatch
            .allocate_import(
                &mut memory,
                ImportCall {
                    library: "sysclib".to_owned(),
                    version: 0x0101,
                    ordinal: 12,
                    module_id: 3,
                    pc: 0,
                },
            )
            .unwrap();
        assert_eq!(
            dispatch.resolve(trampoline.address, 0, 7, 0x9876).unwrap(),
            DispatchCall::Import(ImportCall {
                library: "sysclib".to_owned(),
                version: 0x0101,
                ordinal: 12,
                module_id: 7,
                pc: 0x9876,
            })
        );
        dispatch.release_import(trampoline.address).unwrap();
        let error = dispatch
            .resolve(trampoline.address, 0, 7, 0x9876)
            .unwrap_err();
        assert_eq!(
            error,
            BiosError::UnknownOperation {
                library: "unbound".to_owned(),
                ordinal: 0,
                module_id: 7,
                pc: 0x9876,
            }
        );
        assert!(error.to_string().contains("module 7 at PC 0x00009876"));
    }

    #[test]
    fn kernel_results_are_byte_exact_signed_values() {
        assert_eq!(
            KernelError::IllegalInterruptCode.code().to_le_bytes(),
            (-101_i32).to_le_bytes()
        );
        assert_eq!(
            KernelError::UnknownModule.code().to_le_bytes(),
            (-202_i32).to_le_bytes()
        );
        assert_eq!(
            KernelError::NoMemory.code().to_le_bytes(),
            (-400_i32).to_le_bytes()
        );
        assert_eq!(
            KernelError::IllegalAddress.code().to_le_bytes(),
            (-429_i32).to_le_bytes()
        );
        assert_eq!(
            KernelError::IllegalSize.code().to_le_bytes(),
            (-432_i32).to_le_bytes()
        );
    }
}
