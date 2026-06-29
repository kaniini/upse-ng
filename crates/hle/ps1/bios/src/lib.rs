// SPDX-License-Identifier: LGPL-2.1-or-later
//! Independently implemented PS1 BIOS/kernel high-level emulation.
//!
//! This crate has no firmware-image input API. It operates only on explicit
//! guest registers and a consumer-provided memory contract.

use std::collections::VecDeque;
use std::fmt::Write as _;

use thiserror::Error;

const REGISTER_COUNT: usize = 32;
const V0: usize = 2;
const A0: usize = 4;
const S0: usize = 16;
const GP: usize = 28;
const T1: usize = 9;
const SP: usize = 29;
const FP: usize = 30;
const RA: usize = 31;
const EVENT_SLOTS: usize = 32;
const INTERRUPT_PRIORITIES: usize = 8;
const CALLBACK_RETURN_PC: u32 = 0xffff_ff00;
const TTY_FORMAT_BYTES: u32 = 4096;
const TTY_STRING_BYTES: u32 = 4096;

/// BIOS call-table vector intercepted by the machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BiosVector {
    /// A-function table at address 0xA0.
    A0,
    /// B-function table at address 0xB0.
    B0,
    /// C-function table at address 0xC0.
    C0,
}

/// Guest memory operation type for diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryOperation {
    /// Guest byte read.
    Read,
    /// Guest byte write.
    Write,
}

/// Failure returned by a machine-owned guest memory implementation.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct GuestMemoryError {
    message: String,
}

impl GuestMemoryError {
    /// Constructs a guest memory diagnostic.
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

/// Narrow guest-memory interface consumed by BIOS services.
pub trait GuestMemory {
    /// Reads one byte.
    ///
    /// # Errors
    ///
    /// Returns [`GuestMemoryError`] when the guest address is not readable.
    fn read_u8(&mut self, address: u32) -> Result<u8, GuestMemoryError>;

    /// Writes one byte.
    ///
    /// # Errors
    ///
    /// Returns [`GuestMemoryError`] when the guest address is not writable.
    fn write_u8(&mut self, address: u32, value: u8) -> Result<(), GuestMemoryError>;
}

/// Complete register context needed at an HLE boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CpuContext {
    registers: [u32; REGISTER_COUNT],
    /// Guest program counter.
    pub pc: u32,
    /// HI multiply/divide register.
    pub hi: u32,
    /// LO multiply/divide register.
    pub lo: u32,
}

impl CpuContext {
    /// Constructs zeroed registers at an executable entry point and stack.
    #[must_use]
    pub const fn reset(entry: u32, stack: u32) -> Self {
        let mut registers = [0; REGISTER_COUNT];
        registers[SP] = stack;
        Self {
            registers,
            pc: entry,
            hi: 0,
            lo: 0,
        }
    }

    /// Reads a general-purpose register.
    #[must_use]
    pub fn register(&self, index: usize) -> Option<u32> {
        self.registers.get(index).copied()
    }

    /// Writes a general-purpose register; register zero remains zero.
    pub fn set_register(&mut self, index: usize, value: u32) -> bool {
        let Some(register) = self.registers.get_mut(index) else {
            return false;
        };
        if index != 0 {
            *register = value;
        }
        true
    }

    /// Returns all registers for machine-state transfer.
    #[must_use]
    pub const fn registers(&self) -> &[u32; REGISTER_COUNT] {
        &self.registers
    }

    fn argument(&self, index: usize) -> u32 {
        self.registers[A0 + index]
    }

    fn return_value(&mut self, value: u32) {
        self.registers[V0] = value;
    }

    fn return_to_caller(&mut self) {
        self.pc = self.registers[RA];
    }
}

/// Machine action requested after one HLE dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HleAction {
    /// Resume at the guest return address.
    Return,
    /// Resume after restoring the machine's exception frame.
    ReturnFromException,
}

/// Deterministic result and time charge for an HLE call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HleOutcome {
    /// Nominal PS1 CPU cycles charged by the service.
    pub cycles: u32,
    /// Required machine continuation.
    pub action: HleAction,
}

/// Deferred guest callback requested by an event service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CallbackRequest {
    /// Guest callback entry address.
    pub address: u32,
}

/// Configurable HLE resource bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HleLimits {
    /// Maximum bytes touched by one memory service.
    pub memory_operation_bytes: u32,
    /// Maximum queued, not-yet-entered callbacks.
    pub pending_callbacks: usize,
    /// Maximum nested callback contexts.
    pub callback_depth: usize,
}

impl Default for HleLimits {
    fn default() -> Self {
        Self {
            memory_operation_bytes: 2 * 1024 * 1024,
            pending_callbacks: 64,
            callback_depth: 16,
        }
    }
}

/// Structured BIOS HLE failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BiosError {
    /// Call-table entry is not part of the promised HLE service matrix.
    #[error("unsupported PS1 BIOS {vector:?} call {function:#04x} at PC {pc:#010x}")]
    UnsupportedCall {
        /// BIOS vector.
        vector: BiosVector,
        /// Function number from `t1`.
        function: u8,
        /// Guest call-site PC.
        pc: u32,
    },
    /// Syscall number is not implemented.
    #[error("unsupported PS1 kernel syscall {number} at PC {pc:#010x}")]
    UnsupportedSyscall {
        /// Guest syscall number.
        number: u32,
        /// Guest call-site PC.
        pc: u32,
    },
    /// Pending callback queue reached its configured bound.
    #[error("PS1 HLE callback queue is full")]
    CallbackCapacity,
    /// Nested callback contexts reached their configured bound.
    #[error("PS1 HLE callback stack is full")]
    CallbackDepth,
    /// Callback return sentinel was observed without a saved context.
    #[error("PS1 HLE callback stack is empty")]
    CallbackStackEmpty,
    /// Guest memory service length exceeds its configured bound.
    #[error("PS1 HLE memory operation of {size} bytes exceeds limit {limit}")]
    MemoryOperationLimit {
        /// Guest-requested byte count.
        size: u32,
        /// Configured maximum byte count.
        limit: u32,
    },
    /// Guest address arithmetic overflowed.
    #[error("PS1 HLE guest address arithmetic overflow")]
    AddressOverflow,
    /// Machine-owned guest memory rejected an access.
    #[error("PS1 HLE guest memory {operation:?} failed at {address:#010x}: {source}")]
    GuestMemory {
        /// Failing guest address.
        address: u32,
        /// Access type.
        operation: MemoryOperation,
        /// Machine diagnostic.
        source: GuestMemoryError,
    },
    /// Heap was not initialized by the guest.
    #[error("PS1 HLE heap is not initialized")]
    HeapUnavailable,
    /// Bump allocator cannot satisfy the request.
    #[error("PS1 HLE heap cannot allocate {size} bytes")]
    OutOfMemory {
        /// Guest-requested bytes.
        size: u32,
    },
    /// Guest attempted to consume unmodeled ROM contents.
    #[error("PS1 BIOS ROM read is unavailable in HLE-only mode at {address:#010x}")]
    RomRead {
        /// Physical ROM address.
        address: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventState {
    Idle,
    Delivered,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Event {
    class: u32,
    spec: u32,
    mode: u32,
    callback: u32,
    enabled: bool,
    state: EventState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EventSlot {
    event: Option<Event>,
}

impl EventSlot {
    const EMPTY: Self = Self { event: None };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Heap {
    end: u32,
    next: u32,
}

/// Instance-owned HLE kernel state with bounded resources.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BiosHle {
    limits: HleLimits,
    events: [EventSlot; EVENT_SLOTS],
    pending_callbacks: VecDeque<CallbackRequest>,
    callback_stack: Vec<CpuContext>,
    heap: Option<Heap>,
    random_state: u32,
    critical_depth: u32,
    interrupt_hook: Option<u32>,
    interrupt_queues: [u32; INTERRUPT_PRIORITIES],
    clear_root_counter: [bool; 4],
}

impl Default for BiosHle {
    fn default() -> Self {
        Self::new(HleLimits::default())
    }
}

impl BiosHle {
    /// Constructs clean HLE state with explicit bounds and no firmware input.
    #[must_use]
    pub fn new(limits: HleLimits) -> Self {
        Self {
            limits,
            events: [EventSlot::EMPTY; EVENT_SLOTS],
            pending_callbacks: VecDeque::new(),
            callback_stack: Vec::new(),
            heap: None,
            random_state: 1,
            critical_depth: 0,
            interrupt_hook: None,
            interrupt_queues: [0; INTERRUPT_PRIORITIES],
            clear_root_counter: [true; 4],
        }
    }

    /// Dispatches the function number in `t1` and returns to `ra`.
    ///
    /// # Errors
    ///
    /// Returns [`BiosError`] for unknown calls, invalid resources, guest memory
    /// failures, arithmetic overflow, or configured-bound violations.
    pub fn dispatch<M: GuestMemory>(
        &mut self,
        vector: BiosVector,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<HleOutcome, BiosError> {
        let function = context.registers[T1].to_le_bytes()[0];
        let pc = context.pc;
        let mut action = HleAction::Return;
        let cycles = match (vector, function) {
            (BiosVector::A0, 0x0c) => self.strtoul(context, memory)?,
            (BiosVector::A0, 0x13) => self.setjmp(context, memory)?,
            (BiosVector::A0, 0x72) => 10,
            (BiosVector::A0, 0x2a) => self.memory_copy(context, memory, false)?,
            (BiosVector::A0, 0x2b) => self.memory_set(context, memory)?,
            (BiosVector::A0, 0x2c) => self.memory_copy(context, memory, true)?,
            (BiosVector::A0, 0x2d) => self.memory_compare(context, memory)?,
            (BiosVector::A0, 0x2f) => {
                self.random_state = self
                    .random_state
                    .wrapping_mul(1_103_515_245)
                    .wrapping_add(12_345);
                context.return_value((self.random_state >> 16) & 0x7fff);
                8
            }
            (BiosVector::A0, 0x30) => {
                self.random_state = context.argument(0);
                context.return_value(0);
                6
            }
            (BiosVector::A0, 0x33) => self.malloc(context)?,
            (BiosVector::A0, 0x34) => {
                context.return_value(0);
                4
            }
            (BiosVector::A0, 0x37) => self.calloc(context, memory)?,
            (BiosVector::A0, 0x3f) => self.printf(context, memory)?,
            (BiosVector::B0, 0x07) => self.deliver_event(context)?,
            (BiosVector::B0, 0x08) => self.open_event(context),
            (BiosVector::B0, 0x09) => self.close_event(context),
            (BiosVector::B0, 0x0a | 0x0b) => self.test_event(context),
            (BiosVector::B0, 0x0c) => self.enable_event(context, true),
            (BiosVector::B0, 0x0d) => self.enable_event(context, false),
            (BiosVector::B0, 0x17) => {
                context.return_value(1);
                action = HleAction::ReturnFromException;
                12
            }
            (BiosVector::B0, 0x18) => {
                self.interrupt_hook = None;
                context.return_value(1);
                8
            }
            (BiosVector::B0, 0x19) => {
                self.interrupt_hook = Some(context.argument(0));
                context.return_value(1);
                8
            }
            (BiosVector::B0, 0x20) => self.undeliver_event(context),
            (BiosVector::B0, 0x5b) => 6,
            (BiosVector::C0, 0x02) => self.enqueue_interrupt(context)?,
            (BiosVector::C0, 0x03) => self.dequeue_interrupt(context)?,
            (BiosVector::C0, 0x08) => self.initialize_heap(context)?,
            (BiosVector::C0, 0x0a) => self.change_clear_counter(context)?,
            _ => {
                return Err(BiosError::UnsupportedCall {
                    vector,
                    function,
                    pc,
                });
            }
        };
        if action == HleAction::Return {
            context.return_to_caller();
        }
        Ok(HleOutcome { cycles, action })
    }

    /// Dispatches PS1 kernel syscalls used for critical-section control.
    ///
    /// # Errors
    ///
    /// Returns [`BiosError::UnsupportedSyscall`] for numbers other than 1 and 2.
    pub fn dispatch_syscall(
        &mut self,
        number: u32,
        context: &mut CpuContext,
    ) -> Result<HleOutcome, BiosError> {
        let cycles = match number {
            1 => {
                let was_enabled = self.critical_depth == 0;
                self.critical_depth = self.critical_depth.saturating_add(1);
                context.return_value(u32::from(was_enabled));
                6
            }
            2 => {
                if self.critical_depth != 0 {
                    self.critical_depth -= 1;
                }
                context.return_value(1);
                6
            }
            _ => {
                return Err(BiosError::UnsupportedSyscall {
                    number,
                    pc: context.pc,
                });
            }
        };
        context.return_to_caller();
        Ok(HleOutcome {
            cycles,
            action: HleAction::Return,
        })
    }

    /// Reports whether HLE critical sections permit interrupt dispatch.
    #[must_use]
    pub const fn interrupts_enabled(&self) -> bool {
        self.critical_depth == 0
    }

    /// Returns the installed exception-entry hook address.
    #[must_use]
    pub const fn interrupt_hook(&self) -> Option<u32> {
        self.interrupt_hook
    }

    /// Reports whether the default handler acknowledges a root-counter or
    /// vertical-blank interrupt and returns immediately from the exception.
    #[must_use]
    pub fn clear_root_counter(&self, index: usize) -> Option<bool> {
        self.clear_root_counter.get(index).copied()
    }

    /// Applies the installed `HookEntryInt` jump-buffer context and returns
    /// whether a hook was present.
    ///
    /// # Errors
    ///
    /// Returns [`BiosError`] when the guest hook structure cannot be read.
    pub fn prepare_interrupt_hook<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<bool, BiosError> {
        let Some(buffer) = self.interrupt_hook else {
            return Ok(false);
        };
        let mut saved = [0_u32; 12];
        for (index, word) in saved.iter_mut().enumerate() {
            let offset = u32::try_from(index)
                .map_err(|_| BiosError::AddressOverflow)?
                .checked_mul(4)
                .ok_or(BiosError::AddressOverflow)?;
            *word = read_word(memory, buffer, offset)?;
        }
        context.pc = saved[0];
        context.registers[RA] = saved[0];
        context.registers[SP] = saved[1];
        context.registers[FP] = saved[2];
        context.registers[S0..S0 + 8].copy_from_slice(&saved[3..11]);
        context.registers[GP] = saved[11];
        context.return_value(1);
        Ok(true)
    }

    /// Removes the oldest deferred event callback.
    pub fn take_callback(&mut self) -> Option<CallbackRequest> {
        self.pending_callbacks.pop_front()
    }

    /// Reports whether guest code is currently executing in callback context.
    #[must_use]
    pub fn callback_active(&self) -> bool {
        !self.callback_stack.is_empty()
    }

    /// Delivers one kernel event raised by an emulated hardware source.
    ///
    /// Callback-mode events are queued for entry at the next safe machine
    /// boundary. Ready-mode events remain latched until `TestEvent` consumes
    /// them.
    ///
    /// # Errors
    ///
    /// Returns [`BiosError::CallbackCapacity`] when delivery would exceed the
    /// configured pending-callback bound.
    pub fn signal_event(&mut self, class: u32, spec: u32) -> Result<u32, BiosError> {
        let mut delivered = 0_u32;
        for slot in &mut self.events {
            let Some(event) = slot.event.as_mut() else {
                continue;
            };
            if !event.enabled || event.class != class || event.spec != spec {
                continue;
            }
            delivered = delivered.saturating_add(1);
            if event.mode == 0x1000 && event.callback != 0 {
                if self.pending_callbacks.len() >= self.limits.pending_callbacks {
                    return Err(BiosError::CallbackCapacity);
                }
                self.pending_callbacks.push_back(CallbackRequest {
                    address: event.callback,
                });
            } else if event.mode == 0x2000 {
                event.state = EventState::Delivered;
            }
        }
        Ok(delivered)
    }

    /// Saves a context and enters a zero-argument guest callback with `ra`
    /// prepared.
    ///
    /// # Errors
    ///
    /// Returns [`BiosError::CallbackDepth`] at the configured nesting bound.
    pub fn enter_callback(
        &mut self,
        context: &mut CpuContext,
        callback: CallbackRequest,
    ) -> Result<(), BiosError> {
        if self.callback_stack.len() >= self.limits.callback_depth {
            return Err(BiosError::CallbackDepth);
        }
        self.callback_stack.push(context.clone());
        context.pc = callback.address;
        context.registers[RA] = CALLBACK_RETURN_PC;
        Ok(())
    }

    /// Restores the most recently suspended callback context.
    ///
    /// # Errors
    ///
    /// Returns [`BiosError::CallbackStackEmpty`] without a matching entry.
    pub fn return_from_callback(&mut self, context: &mut CpuContext) -> Result<(), BiosError> {
        let Some(saved) = self.callback_stack.pop() else {
            return Err(BiosError::CallbackStackEmpty);
        };
        *context = saved;
        Ok(())
    }

    /// Returns the reserved callback-return sentinel recognized by the machine.
    #[must_use]
    pub const fn callback_return_pc() -> u32 {
        CALLBACK_RETURN_PC
    }

    /// Rejects all direct firmware-ROM reads explicitly.
    ///
    /// # Errors
    ///
    /// Always returns [`BiosError::RomRead`].
    pub const fn reject_rom_read(address: u32) -> Result<u32, BiosError> {
        Err(BiosError::RomRead { address })
    }

    fn memory_copy<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
        overlap_safe: bool,
    ) -> Result<u32, BiosError> {
        let destination = context.argument(0);
        let source = context.argument(1);
        let size = context.argument(2);
        self.check_memory_size(size)?;
        let backwards = overlap_safe
            && destination > source
            && destination < source.checked_add(size).ok_or(BiosError::AddressOverflow)?;
        if backwards {
            for offset in (0..size).rev() {
                let value = read_byte(memory, source, offset)?;
                write_byte(memory, destination, offset, value)?;
            }
        } else {
            for offset in 0..size {
                let value = read_byte(memory, source, offset)?;
                write_byte(memory, destination, offset, value)?;
            }
        }
        context.return_value(destination);
        Ok(memory_cycles(size))
    }

    fn strtoul<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let source = context.argument(0);
        if source == 0 {
            context.return_value(0);
            return Ok(6);
        }

        let end_pointer = context.argument(1);
        let requested_base = context.argument(2);
        let mut base = if (2..=36).contains(&requested_base) {
            requested_base
        } else {
            10
        };
        let mut offset = 0_u32;
        while matches!(
            self.read_string_byte(memory, source, offset)?,
            0x09..=0x0d | 0x20
        ) {
            offset = offset.checked_add(1).ok_or(BiosError::AddressOverflow)?;
        }

        let first = self.read_string_byte(memory, source, offset)?;
        let second = if first == b'0' {
            Some(self.read_string_byte(
                memory,
                source,
                offset.checked_add(1).ok_or(BiosError::AddressOverflow)?,
            )?)
        } else {
            None
        };
        if matches!(second, Some(b'b' | b'B')) {
            base = 2;
            offset = offset.checked_add(2).ok_or(BiosError::AddressOverflow)?;
        } else if matches!(second, Some(b'x' | b'X')) {
            base = 16;
            offset = offset.checked_add(2).ok_or(BiosError::AddressOverflow)?;
        } else if matches!(first, b'o' | b'O') {
            base = 8;
            offset = offset.checked_add(1).ok_or(BiosError::AddressOverflow)?;
        }

        let mut result = 0_u32;
        loop {
            let byte = self.read_string_byte(memory, source, offset)?;
            let Some(digit) = ascii_digit(byte) else {
                break;
            };
            if digit >= base {
                break;
            }
            result = result.wrapping_mul(base).wrapping_add(digit);
            offset = offset.checked_add(1).ok_or(BiosError::AddressOverflow)?;
        }

        if end_pointer != 0 {
            let end = source
                .checked_add(offset)
                .ok_or(BiosError::AddressOverflow)?;
            for (byte_offset, byte) in end.to_le_bytes().into_iter().enumerate() {
                write_byte(
                    memory,
                    end_pointer,
                    u32::try_from(byte_offset).map_err(|_| BiosError::AddressOverflow)?,
                    byte,
                )?;
            }
        }
        context.return_value(result);
        Ok(memory_cycles(offset.saturating_add(1)).saturating_add(8))
    }

    fn read_string_byte<M: GuestMemory>(
        &self,
        memory: &mut M,
        source: u32,
        offset: u32,
    ) -> Result<u8, BiosError> {
        if offset >= self.limits.memory_operation_bytes {
            return Err(BiosError::MemoryOperationLimit {
                size: offset.saturating_add(1),
                limit: self.limits.memory_operation_bytes,
            });
        }
        read_byte(memory, source, offset)
    }

    fn printf<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let format = self.read_tty_string(memory, context.argument(0), TTY_FORMAT_BYTES)?;
        let mut output = String::new();
        let mut index = 0_usize;
        let mut argument = 0_usize;
        while index < format.len() {
            if format[index] != b'%' {
                output.push(char::from(format[index]));
                index += 1;
                continue;
            }
            index += 1;
            if format.get(index) == Some(&b'%') {
                output.push('%');
                index += 1;
                continue;
            }
            while format
                .get(index)
                .is_some_and(|byte| b"-+ #0".contains(byte))
            {
                index += 1;
            }
            while format.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            if format.get(index) == Some(&b'.') {
                index += 1;
                while format.get(index).is_some_and(u8::is_ascii_digit) {
                    index += 1;
                }
            }
            while format
                .get(index)
                .is_some_and(|byte| matches!(byte, b'h' | b'l'))
            {
                index += 1;
            }
            let Some(&conversion) = format.get(index) else {
                output.push('%');
                break;
            };
            index += 1;
            let value = printf_argument(context, memory, argument)?;
            argument += 1;
            match conversion {
                b'c' => output.push(char::from(value.to_le_bytes()[0])),
                b'd' | b'i' => {
                    output.push_str(&i32::from_ne_bytes(value.to_ne_bytes()).to_string());
                }
                b'o' => {
                    let _ = write!(output, "{value:o}");
                }
                b'p' => {
                    let _ = write!(output, "0x{value:08x}");
                }
                b's' => {
                    if value == 0 {
                        output.push_str("(null)");
                    } else {
                        let string = self.read_tty_string(memory, value, TTY_STRING_BYTES)?;
                        output.push_str(&String::from_utf8_lossy(&string));
                    }
                }
                b'u' => output.push_str(&value.to_string()),
                b'x' => {
                    let _ = write!(output, "{value:x}");
                }
                b'X' => {
                    let _ = write!(output, "{value:X}");
                }
                other => {
                    output.push('%');
                    output.push(char::from(other));
                }
            }
        }
        if output.ends_with('\n') {
            eprint!("[PS1 BIOS TTY] {output}");
        } else {
            eprintln!("[PS1 BIOS TTY] {output}");
        }
        let length = u32::try_from(output.len()).unwrap_or(u32::MAX);
        context.return_value(length);
        Ok(12_u32.saturating_add(length.saturating_mul(2)))
    }

    fn read_tty_string<M: GuestMemory>(
        &self,
        memory: &mut M,
        source: u32,
        limit: u32,
    ) -> Result<Vec<u8>, BiosError> {
        let limit = limit.min(self.limits.memory_operation_bytes);
        let mut output = Vec::new();
        for offset in 0..limit {
            let byte = read_byte(memory, source, offset)?;
            if byte == 0 {
                return Ok(output);
            }
            output.push(byte);
        }
        Err(BiosError::MemoryOperationLimit { size: limit, limit })
    }

    fn setjmp<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        const BUFFER_SIZE: u32 = 48;

        self.check_memory_size(BUFFER_SIZE)?;
        let buffer = context.argument(0);
        let saved = [
            context.registers[RA],
            context.registers[SP],
            context.registers[FP],
            context.registers[S0],
            context.registers[S0 + 1],
            context.registers[S0 + 2],
            context.registers[S0 + 3],
            context.registers[S0 + 4],
            context.registers[S0 + 5],
            context.registers[S0 + 6],
            context.registers[S0 + 7],
            context.registers[GP],
        ];
        for (word_index, value) in saved.into_iter().enumerate() {
            let word_offset = u32::try_from(word_index)
                .map_err(|_| BiosError::AddressOverflow)?
                .checked_mul(4)
                .ok_or(BiosError::AddressOverflow)?;
            for (byte_index, byte) in value.to_le_bytes().into_iter().enumerate() {
                let byte_offset =
                    u32::try_from(byte_index).map_err(|_| BiosError::AddressOverflow)?;
                write_byte(memory, buffer, word_offset + byte_offset, byte)?;
            }
        }
        context.return_value(0);
        Ok(memory_cycles(BUFFER_SIZE))
    }

    fn memory_set<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let destination = context.argument(0);
        let value = context.argument(1).to_le_bytes()[0];
        let size = context.argument(2);
        self.check_memory_size(size)?;
        for offset in 0..size {
            write_byte(memory, destination, offset, value)?;
        }
        context.return_value(destination);
        Ok(memory_cycles(size))
    }

    fn memory_compare<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let left = context.argument(0);
        let right = context.argument(1);
        let size = context.argument(2);
        self.check_memory_size(size)?;
        let mut result = 0_i32;
        for offset in 0..size {
            let left = read_byte(memory, left, offset)?;
            let right = read_byte(memory, right, offset)?;
            if left != right {
                result = i32::from(left) - i32::from(right);
                break;
            }
        }
        context.return_value(u32::from_ne_bytes(result.to_ne_bytes()));
        Ok(memory_cycles(size))
    }

    fn check_memory_size(&self, size: u32) -> Result<(), BiosError> {
        if size > self.limits.memory_operation_bytes {
            return Err(BiosError::MemoryOperationLimit {
                size,
                limit: self.limits.memory_operation_bytes,
            });
        }
        Ok(())
    }

    fn initialize_heap(&mut self, context: &mut CpuContext) -> Result<u32, BiosError> {
        let base = align_up(context.argument(0), 8)?;
        let end = context
            .argument(0)
            .checked_add(context.argument(1))
            .ok_or(BiosError::AddressOverflow)?;
        if base > end {
            return Err(BiosError::OutOfMemory {
                size: context.argument(1),
            });
        }
        self.heap = Some(Heap { end, next: base });
        context.return_value(base);
        Ok(20)
    }

    fn malloc(&mut self, context: &mut CpuContext) -> Result<u32, BiosError> {
        let size = context.argument(0);
        let heap = self.heap.as_mut().ok_or(BiosError::HeapUnavailable)?;
        let start = heap.next;
        let next = align_up(
            start.checked_add(size).ok_or(BiosError::AddressOverflow)?,
            8,
        )?;
        if next > heap.end {
            return Err(BiosError::OutOfMemory { size });
        }
        heap.next = next;
        context.return_value(start);
        Ok(12)
    }

    fn calloc<M: GuestMemory>(
        &mut self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let count = context.argument(0);
        let element_size = context.argument(1);
        let size = count
            .checked_mul(element_size)
            .ok_or(BiosError::AddressOverflow)?;
        self.check_memory_size(size)?;
        context.registers[A0] = size;
        self.malloc(context)?;
        let destination = context.registers[V0];
        for offset in 0..size {
            write_byte(memory, destination, offset, 0)?;
        }
        context.return_value(destination);
        Ok(memory_cycles(size).saturating_add(12))
    }

    fn open_event(&mut self, context: &mut CpuContext) -> u32 {
        let Some((index, slot)) = self
            .events
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.event.is_none())
        else {
            context.return_value(u32::MAX);
            return 8;
        };
        slot.event = Some(Event {
            class: context.argument(0),
            spec: context.argument(1),
            mode: context.argument(2),
            callback: context.argument(3),
            enabled: false,
            state: EventState::Idle,
        });
        let handle = event_handle(index);
        context.return_value(handle);
        18
    }

    fn close_event(&mut self, context: &mut CpuContext) -> u32 {
        let handle = context.argument(0);
        if let Some(slot) = self.event_slot_mut(handle) {
            slot.event = None;
        }
        context.return_value(1);
        10
    }

    fn enable_event(&mut self, context: &mut CpuContext, enabled: bool) -> u32 {
        let handle = context.argument(0);
        if let Some(event) = self.event_mut(handle) {
            event.enabled = enabled;
        }
        context.return_value(1);
        8
    }

    fn test_event(&mut self, context: &mut CpuContext) -> u32 {
        let handle = context.argument(0);
        let delivered = self.event_mut(handle).is_some_and(|event| {
            let delivered = event.enabled && event.state == EventState::Delivered;
            if delivered {
                event.state = EventState::Idle;
            }
            delivered
        });
        context.return_value(u32::from(delivered));
        8
    }

    fn deliver_event(&mut self, context: &mut CpuContext) -> Result<u32, BiosError> {
        let class = context.argument(0);
        let spec = context.argument(1);
        let delivered = self.signal_event(class, spec)?;
        context.return_value(delivered);
        Ok(12_u32.saturating_add(delivered.saturating_mul(4)))
    }

    fn undeliver_event(&mut self, context: &mut CpuContext) -> u32 {
        let class = context.argument(0);
        let spec = context.argument(1);
        let mut changed = 0_u32;
        for slot in &mut self.events {
            let Some(event) = slot.event.as_mut() else {
                continue;
            };
            if event.class == class && event.spec == spec && event.mode == 0x2000 {
                event.state = EventState::Idle;
                changed = changed.saturating_add(1);
            }
        }
        context.return_value(changed);
        10
    }

    fn event_slot_mut(&mut self, handle: u32) -> Option<&mut EventSlot> {
        if handle & 0xffff_0000 != 0xf100_0000 {
            return None;
        }
        let bytes = handle.to_le_bytes();
        let index = usize::from(u16::from_le_bytes([bytes[0], bytes[1]]));
        self.events
            .get_mut(index)
            .filter(|slot| slot.event.is_some())
    }

    fn event_mut(&mut self, handle: u32) -> Option<&mut Event> {
        self.event_slot_mut(handle)?.event.as_mut()
    }

    fn enqueue_interrupt(&mut self, context: &mut CpuContext) -> Result<u32, BiosError> {
        let priority =
            usize::try_from(context.argument(0)).map_err(|_| BiosError::AddressOverflow)?;
        let Some(queue) = self.interrupt_queues.get_mut(priority) else {
            context.return_value(u32::MAX);
            return Ok(8);
        };
        *queue = context.argument(1);
        context.return_value(0);
        Ok(10)
    }

    fn dequeue_interrupt(&mut self, context: &mut CpuContext) -> Result<u32, BiosError> {
        let priority =
            usize::try_from(context.argument(0)).map_err(|_| BiosError::AddressOverflow)?;
        let Some(queue) = self.interrupt_queues.get_mut(priority) else {
            context.return_value(u32::MAX);
            return Ok(8);
        };
        let matched = *queue == context.argument(1);
        if matched {
            *queue = 0;
        }
        context.return_value(if matched { 0 } else { u32::MAX });
        Ok(10)
    }

    fn change_clear_counter(&mut self, context: &mut CpuContext) -> Result<u32, BiosError> {
        let index = usize::try_from(context.argument(0)).map_err(|_| BiosError::AddressOverflow)?;
        let Some(clear) = self.clear_root_counter.get_mut(index) else {
            context.return_value(u32::MAX);
            return Ok(6);
        };
        let previous = *clear;
        *clear = context.argument(1) != 0;
        context.return_value(u32::from(previous));
        Ok(6)
    }
}

fn event_handle(index: usize) -> u32 {
    0xf100_0000 | u32::try_from(index).unwrap_or(u32::MAX)
}

fn read_byte<M: GuestMemory>(memory: &mut M, base: u32, offset: u32) -> Result<u8, BiosError> {
    let address = base.checked_add(offset).ok_or(BiosError::AddressOverflow)?;
    memory
        .read_u8(address)
        .map_err(|source| BiosError::GuestMemory {
            address,
            operation: MemoryOperation::Read,
            source,
        })
}

fn read_word<M: GuestMemory>(memory: &mut M, base: u32, offset: u32) -> Result<u32, BiosError> {
    let mut bytes = [0_u8; 4];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let byte_offset = u32::try_from(index).map_err(|_| BiosError::AddressOverflow)?;
        *byte = read_byte(memory, base, offset + byte_offset)?;
    }
    Ok(u32::from_le_bytes(bytes))
}

fn printf_argument<M: GuestMemory>(
    context: &CpuContext,
    memory: &mut M,
    index: usize,
) -> Result<u32, BiosError> {
    if index <= 2 {
        return Ok(context.registers[A0 + 1 + index]);
    }
    let stack_index = u32::try_from(index - 3).map_err(|_| BiosError::AddressOverflow)?;
    let offset = 16_u32
        .checked_add(
            stack_index
                .checked_mul(4)
                .ok_or(BiosError::AddressOverflow)?,
        )
        .ok_or(BiosError::AddressOverflow)?;
    read_word(memory, context.registers[SP], offset)
}

fn write_byte<M: GuestMemory>(
    memory: &mut M,
    base: u32,
    offset: u32,
    value: u8,
) -> Result<(), BiosError> {
    let address = base.checked_add(offset).ok_or(BiosError::AddressOverflow)?;
    memory
        .write_u8(address, value)
        .map_err(|source| BiosError::GuestMemory {
            address,
            operation: MemoryOperation::Write,
            source,
        })
}

fn memory_cycles(size: u32) -> u32 {
    8_u32.saturating_add(size.saturating_mul(2))
}

fn ascii_digit(byte: u8) -> Option<u32> {
    match byte {
        b'0'..=b'9' => Some(u32::from(byte - b'0')),
        b'A'..=b'Z' => Some(u32::from(byte - b'A') + 10),
        b'a'..=b'z' => Some(u32::from(byte - b'a') + 10),
        _ => None,
    }
}

fn align_up(value: u32, alignment: u32) -> Result<u32, BiosError> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or(BiosError::AddressOverflow)
}

#[cfg(test)]
mod tests {
    use super::{
        A0, BiosError, BiosHle, BiosVector, CpuContext, FP, GP, GuestMemory, GuestMemoryError,
        HleAction, HleLimits, RA, S0, SP, T1, V0,
    };

    struct Memory(Vec<u8>);

    impl GuestMemory for Memory {
        fn read_u8(&mut self, address: u32) -> Result<u8, GuestMemoryError> {
            let index =
                usize::try_from(address).map_err(|_| GuestMemoryError::new("address width"))?;
            self.0
                .get(index)
                .copied()
                .ok_or_else(|| GuestMemoryError::new("outside synthetic RAM"))
        }

        fn write_u8(&mut self, address: u32, value: u8) -> Result<(), GuestMemoryError> {
            let index =
                usize::try_from(address).map_err(|_| GuestMemoryError::new("address width"))?;
            let byte = self
                .0
                .get_mut(index)
                .ok_or_else(|| GuestMemoryError::new("outside synthetic RAM"))?;
            *byte = value;
            Ok(())
        }
    }

    fn call(
        bios: &mut BiosHle,
        vector: BiosVector,
        function: u8,
        arguments: [u32; 4],
        memory: &mut Memory,
    ) -> Result<(CpuContext, super::HleOutcome), BiosError> {
        let mut context = CpuContext::reset(0x1000, 0x8000);
        context.set_register(RA, 0x2000);
        context.set_register(T1, u32::from(function));
        for (index, argument) in arguments.into_iter().enumerate() {
            context.set_register(A0 + index, argument);
        }
        let outcome = bios.dispatch(vector, &mut context, memory)?;
        Ok((context, outcome))
    }

    #[test]
    fn memory_services_are_overlap_safe_bounded_and_fault_explicitly() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 64]);
        memory.0[0..8].copy_from_slice(b"abcdefgh");
        let (context, outcome) =
            call(&mut bios, BiosVector::A0, 0x2c, [2, 0, 8, 0], &mut memory).unwrap();
        assert_eq!(&memory.0[..10], b"ababcdefgh");
        assert_eq!(context.register(V0), Some(2));
        assert_eq!(context.pc, 0x2000);
        assert_eq!(outcome.cycles, 24);

        call(
            &mut bios,
            BiosVector::A0,
            0x2b,
            [16, 0x5a, 4, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(&memory.0[16..20], &[0x5a; 4]);
        let (context, _) =
            call(&mut bios, BiosVector::A0, 0x2d, [16, 17, 3, 0], &mut memory).unwrap();
        assert_eq!(context.register(V0), Some(0));

        let limits = HleLimits {
            memory_operation_bytes: 3,
            ..HleLimits::default()
        };
        let mut bounded = BiosHle::new(limits);
        assert!(matches!(
            call(
                &mut bounded,
                BiosVector::A0,
                0x2a,
                [0, 0, 4, 0],
                &mut memory
            ),
            Err(BiosError::MemoryOperationLimit { .. })
        ));
        assert!(matches!(
            call(&mut bios, BiosVector::A0, 0x2a, [63, 0, 2, 0], &mut memory),
            Err(BiosError::GuestMemory { .. })
        ));
    }

    #[test]
    fn setjmp_saves_the_documented_abi_register_layout() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 128]);
        let mut context = CpuContext::reset(0x00a0, 0x0102_0304);
        context.set_register(A0, 64);
        context.set_register(T1, 0x13);
        context.set_register(RA, 0xaabb_ccdd);
        context.set_register(FP, 0x0506_0708);
        context.set_register(GP, 0x090a_0b0c);
        for index in 0..8 {
            context.set_register(S0 + index, 0x1000 + u32::try_from(index).unwrap());
        }

        let outcome = bios
            .dispatch(BiosVector::A0, &mut context, &mut memory)
            .unwrap();
        let words: Vec<u32> = memory.0[64..112]
            .chunks_exact(4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
            .collect();

        assert_eq!(
            words,
            vec![
                0xaabb_ccdd,
                0x0102_0304,
                0x0506_0708,
                0x1000,
                0x1001,
                0x1002,
                0x1003,
                0x1004,
                0x1005,
                0x1006,
                0x1007,
                0x090a_0b0c,
            ]
        );
        assert_eq!(context.register(V0), Some(0));
        assert_eq!(context.register(SP), Some(0x0102_0304));
        assert_eq!(context.pc, 0xaabb_ccdd);
        assert_eq!(outcome.cycles, 104);
        assert_eq!(outcome.action, HleAction::Return);
    }

    #[test]
    fn strtoul_parses_bios_prefixes_and_writes_the_guest_end_pointer() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 128]);
        memory.0[32..40].copy_from_slice(b" \t0x1fZ\0");
        let (context, _) =
            call(&mut bios, BiosVector::A0, 0x0c, [32, 8, 10, 0], &mut memory).unwrap();
        assert_eq!(context.register(V0), Some(31));
        assert_eq!(u32::from_le_bytes(memory.0[8..12].try_into().unwrap()), 38);

        memory.0[48..53].copy_from_slice(b"o77!\0");
        let (context, _) = call(
            &mut bios,
            BiosVector::A0,
            0x0c,
            [48, 12, 36, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(context.register(V0), Some(63));
        assert_eq!(u32::from_le_bytes(memory.0[12..16].try_into().unwrap()), 51);

        memory.0[16..20].copy_from_slice(&0xfeed_beef_u32.to_le_bytes());
        let (context, _) =
            call(&mut bios, BiosVector::A0, 0x0c, [0, 16, 10, 0], &mut memory).unwrap();
        assert_eq!(context.register(V0), Some(0));
        assert_eq!(
            u32::from_le_bytes(memory.0[16..20].try_into().unwrap()),
            0xfeed_beef
        );

        let mut bounded = BiosHle::new(HleLimits {
            memory_operation_bytes: 3,
            ..HleLimits::default()
        });
        memory.0[64..69].copy_from_slice(b"1234\0");
        assert!(matches!(
            call(
                &mut bounded,
                BiosVector::A0,
                0x0c,
                [64, 0, 10, 0],
                &mut memory
            ),
            Err(BiosError::MemoryOperationLimit { size: 4, limit: 3 })
        ));
    }

    #[test]
    fn printf_formats_register_and_stack_arguments_for_the_tty() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 0x8020]);
        memory.0[32..45].copy_from_slice(b"%s %x %d %u\n\0");
        memory.0[64..70].copy_from_slice(b"hello\0");
        memory.0[0x8010..0x8014].copy_from_slice(&9_u32.to_le_bytes());
        let (context, outcome) = call(
            &mut bios,
            BiosVector::A0,
            0x3f,
            [32, 64, 0x2a, u32::from_ne_bytes((-7_i32).to_ne_bytes())],
            &mut memory,
        )
        .unwrap();
        assert_eq!(context.register(V0), Some(14));
        assert_eq!(outcome.cycles, 40);
    }

    #[test]
    fn heap_random_and_zeroed_allocation_have_deterministic_results() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0xaa; 256]);
        call(&mut bios, BiosVector::C0, 0x08, [3, 100, 0, 0], &mut memory).unwrap();
        let (first, _) = call(&mut bios, BiosVector::A0, 0x33, [9, 0, 0, 0], &mut memory).unwrap();
        assert_eq!(first.register(V0), Some(8));
        let (second, _) = call(&mut bios, BiosVector::A0, 0x37, [2, 4, 0, 0], &mut memory).unwrap();
        assert_eq!(second.register(V0), Some(24));
        assert_eq!(&memory.0[24..32], &[0; 8]);

        call(&mut bios, BiosVector::A0, 0x30, [7, 0, 0, 0], &mut memory).unwrap();
        let (random, _) = call(&mut bios, BiosVector::A0, 0x2f, [0; 4], &mut memory).unwrap();
        assert_eq!(random.register(V0), Some(19_564));
    }

    fn open_event(bios: &mut BiosHle, memory: &mut Memory, class: u32, callback: u32) -> u32 {
        let (context, _) = call(
            bios,
            BiosVector::B0,
            0x08,
            [class, 2, 0x1000, callback],
            memory,
        )
        .unwrap();
        context.register(V0).unwrap()
    }

    #[test]
    fn events_queue_callbacks_and_preserve_complete_contexts() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 16]);
        let first = open_event(&mut bios, &mut memory, 1, 0x3000);
        let second = open_event(&mut bios, &mut memory, 1, 0x4000);
        assert_eq!(first, 0xf100_0000);
        assert_eq!(second, 0xf100_0001);
        for handle in [first, second] {
            call(
                &mut bios,
                BiosVector::B0,
                0x0c,
                [handle, 0, 0, 0],
                &mut memory,
            )
            .unwrap();
        }
        let (delivered, _) =
            call(&mut bios, BiosVector::B0, 0x07, [1, 2, 0, 0], &mut memory).unwrap();
        assert_eq!(delivered.register(V0), Some(2));

        let mut context = CpuContext::reset(0x5000, 0x9000);
        context.set_register(A0, 0xaa55_aa55);
        context.set_register(5, 0x55aa);
        let callback1 = bios.take_callback().unwrap();
        bios.enter_callback(&mut context, callback1).unwrap();
        assert!(bios.callback_active());
        assert_eq!(context.pc, 0x3000);
        assert_eq!(context.register(A0), Some(0xaa55_aa55));
        assert_eq!(context.register(RA), Some(BiosHle::callback_return_pc()));
        bios.return_from_callback(&mut context).unwrap();
        assert!(!bios.callback_active());
        assert_eq!(context.pc, 0x5000);

        let callback2 = bios.take_callback().unwrap();
        bios.enter_callback(&mut context, callback2).unwrap();
        assert_eq!(context.pc, 0x4000);
        bios.return_from_callback(&mut context).unwrap();
        assert_eq!(context.pc, 0x5000);
        assert_eq!(context.register(5), Some(0x55aa));
        assert_eq!(
            bios.return_from_callback(&mut context),
            Err(BiosError::CallbackStackEmpty)
        );

        call(
            &mut bios,
            BiosVector::B0,
            0x09,
            [first, 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        let (result, _) = call(
            &mut bios,
            BiosVector::B0,
            0x0c,
            [first, 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(result.register(V0), Some(1));
        let (result, _) = call(&mut bios, BiosVector::B0, 0x09, [0, 0, 0, 0], &mut memory).unwrap();
        assert_eq!(result.register(V0), Some(1));
    }

    #[test]
    fn critical_sections_hooks_interrupt_queues_and_exception_return_are_explicit() {
        let mut bios = BiosHle::default();
        let mut context = CpuContext::reset(0x100, 0x200);
        context.set_register(RA, 0x300);
        bios.dispatch_syscall(1, &mut context).unwrap();
        assert!(!bios.interrupts_enabled());
        assert_eq!(context.register(V0), Some(1));
        context.pc = 0x100;
        bios.dispatch_syscall(1, &mut context).unwrap();
        assert_eq!(context.register(V0), Some(0));
        bios.dispatch_syscall(2, &mut context).unwrap();
        assert!(!bios.interrupts_enabled());
        bios.dispatch_syscall(2, &mut context).unwrap();
        assert!(bios.interrupts_enabled());

        let mut memory = Memory(vec![0; 8]);
        context.pc = 0x00b0;
        context.set_register(RA, 0x400);
        context.set_register(T1, 0x5b);
        context.set_register(A0, 0);
        context.set_register(V0, 0xfeed_beef);
        bios.dispatch(BiosVector::B0, &mut context, &mut memory)
            .unwrap();
        assert_eq!(context.register(V0), Some(0xfeed_beef));
        assert_eq!(context.pc, 0x400);

        context.pc = 0x00a0;
        context.set_register(RA, 0x500);
        context.set_register(T1, 0x72);
        bios.dispatch(BiosVector::A0, &mut context, &mut memory)
            .unwrap();
        assert_eq!(context.register(V0), Some(0xfeed_beef));
        assert_eq!(context.pc, 0x500);

        call(
            &mut bios,
            BiosVector::B0,
            0x19,
            [0x1234, 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(bios.interrupt_hook(), Some(0x1234));
        let (_, outcome) = call(&mut bios, BiosVector::B0, 0x17, [0; 4], &mut memory).unwrap();
        assert_eq!(outcome.action, HleAction::ReturnFromException);

        let (result, _) = call(
            &mut bios,
            BiosVector::C0,
            0x02,
            [2, 0x7000, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(result.register(V0), Some(0));
        let (result, _) = call(
            &mut bios,
            BiosVector::C0,
            0x03,
            [2, 0x7000, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(result.register(V0), Some(0));
    }

    #[test]
    fn unknown_calls_syscalls_and_rom_reads_never_succeed_silently() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 4]);
        assert!(matches!(
            call(&mut bios, BiosVector::A0, 0xff, [0; 4], &mut memory),
            Err(BiosError::UnsupportedCall { .. })
        ));
        let mut context = CpuContext::reset(0x1234, 0);
        assert_eq!(
            bios.dispatch_syscall(99, &mut context),
            Err(BiosError::UnsupportedSyscall {
                number: 99,
                pc: 0x1234
            })
        );
        assert_eq!(
            BiosHle::reject_rom_read(0x1fc0_0100),
            Err(BiosError::RomRead {
                address: 0x1fc0_0100
            })
        );
    }
}
