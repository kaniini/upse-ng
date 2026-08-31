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
const V1: usize = 3;
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
const RESIDENT_FILE_DESCRIPTOR: u32 = 3;
const RAM_SIZE_ADDRESS: u32 = 0x0000_0060;
const STRTOK_BUFFER_ADDRESS: u32 = 0x0000_c000;
const STRTOK_BUFFER_SIZE: u32 = 256;
const KERNEL_MEMORY_ADDRESS: u32 = 0xa000_e000;
const KERNEL_MEMORY_SIZE: u32 = 0x2000;
const A0_TABLE_ADDRESS: u32 = 0x0000_0200;
const B0_TABLE_ADDRESS: u32 = 0x0000_0500;
const C0_TABLE_ADDRESS: u32 = 0x0000_0900;
const B0_STUB_ADDRESS: u32 = 0x0000_1000;
const C0_STUB_ADDRESS: u32 = 0x0000_1c00;
// Games inspect and patch these two routines by fixed offsets. Keep their
// writable synthetic bodies separate from the generic HLE jump stubs and from
// user RAM, which begins at 0x10000.
const C0_EXCEPTION_STUB_ADDRESS: u32 = 0x0000_4000;
const B0_CHANGE_CLEAR_PAD_STUB_ADDRESS: u32 = 0x0000_5000;
const A0_STUB_ADDRESS: u32 = 0x0000_7000;
const A0_GET_CONF_STUB_ADDRESS: u32 = 0x0000_a000;
const KERNEL_CONFIG_ADDRESS: u32 = 0x0000_b000;
const BIOS_PATCH_SCRATCH_ADDRESS: u32 = 0x0000_df80;
const A0_TABLE_ENTRIES: u32 = 0xc0;
const BIOS_TABLE_ENTRIES: u32 = 256;
const BIOS_STUB_BYTES: u32 = 12;

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
    /// Enter a guest callback owned by a suspended HLE routine.
    Call,
    /// Stop guest CPU execution while emulated devices continue advancing.
    Halt,
    /// Keep the guest at the BIOS vector until a kernel event is delivered.
    Wait,
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
    /// Maximum guest comparisons performed by one libc search or sort call.
    pub libc_callback_calls: u32,
}

impl Default for HleLimits {
    fn default() -> Self {
        Self {
            memory_operation_bytes: 2 * 1024 * 1024,
            pending_callbacks: 64,
            callback_depth: 16,
            libc_callback_calls: 1_000_000,
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
    /// A libc search or sort exceeded the configured guest-comparison bound.
    #[error("PS1 HLE libc callback operation exceeds limit {limit}")]
    LibcCallbackLimit {
        /// Configured maximum comparator calls.
        limit: u32,
    },
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
struct HeapBlock {
    address: u32,
    size: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Heap {
    base: u32,
    end: u32,
    blocks: Vec<HeapBlock>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResidentFile {
    position: u32,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CardState {
    initialized: bool,
    started: bool,
    pad_enabled: bool,
    backup_unit_initialized: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct KernelHandlerState {
    timer_and_vblank: Option<u32>,
    syscall: Option<u32>,
    default_interrupt: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct BiosTableState {
    a0: bool,
    b0: bool,
    c0: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ParsedInteger {
    value: u32,
    end: u32,
    bytes_read: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LibcOperationKind {
    Qsort {
        base: u32,
        elements: u32,
        width: u32,
        outer: u32,
        inner: u32,
    },
    Lsearch {
        key: u32,
        base: u32,
        elements: u32,
        width: u32,
        index: u32,
    },
    Bsearch {
        key: u32,
        base: u32,
        width: u32,
        low: u32,
        high: u32,
        middle: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LibcOperation {
    saved_context: CpuContext,
    callback: u32,
    calls: u32,
    kind: LibcOperationKind,
}

/// Instance-owned HLE kernel state with bounded resources.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BiosHle {
    limits: HleLimits,
    events: [EventSlot; EVENT_SLOTS],
    pending_callbacks: VecDeque<CallbackRequest>,
    callback_stack: Vec<CpuContext>,
    heap: Option<Heap>,
    kernel_heap: Heap,
    kernel_handlers: KernelHandlerState,
    resident_file: Option<ResidentFile>,
    card: CardState,
    tables: BiosTableState,
    strtok_next: Option<u32>,
    libc_operation: Option<LibcOperation>,
    random_state: u32,
    interrupts_enabled: bool,
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
            kernel_heap: Heap {
                base: KERNEL_MEMORY_ADDRESS,
                end: KERNEL_MEMORY_ADDRESS + KERNEL_MEMORY_SIZE,
                blocks: Vec::new(),
            },
            kernel_handlers: KernelHandlerState::default(),
            resident_file: None,
            card: CardState::default(),
            tables: BiosTableState::default(),
            strtok_next: None,
            libc_operation: None,
            random_state: 1,
            interrupts_enabled: true,
            interrupt_hook: None,
            interrupt_queues: [0; INTERRUPT_PRIORITIES],
            clear_root_counter: [true; 4],
        }
    }

    /// Installs the fixed guest-visible A0 jump table used by software that
    /// inspects or patches service entry points directly.
    ///
    /// # Errors
    ///
    /// Returns [`BiosError`] when guest RAM rejects the synthetic table.
    pub fn initialize_boot_memory<M: GuestMemory>(
        &mut self,
        memory: &mut M,
    ) -> Result<(), BiosError> {
        if self.tables.a0 {
            return Ok(());
        }
        for function in 0..A0_TABLE_ENTRIES {
            let table_offset = function.checked_mul(4).ok_or(BiosError::AddressOverflow)?;
            let stub_offset = function
                .checked_mul(BIOS_STUB_BYTES)
                .ok_or(BiosError::AddressOverflow)?;
            let stub = A0_STUB_ADDRESS
                .checked_add(stub_offset)
                .ok_or(BiosError::AddressOverflow)?;
            write_word(memory, A0_TABLE_ADDRESS, table_offset, stub)?;
            write_hle_stub(memory, stub, function, 0x0000_00a0)?;
        }

        write_word(memory, A0_TABLE_ADDRESS, 0x9d * 4, A0_GET_CONF_STUB_ADDRESS)?;
        // GetConf conventionally begins by loading the address of the kernel
        // config. Some games decode these two immediates to find that storage.
        let config_reference = KERNEL_CONFIG_ADDRESS + 8;
        let config_upper = (config_reference + 0x8000) >> 16;
        write_word(
            memory,
            A0_GET_CONF_STUB_ADDRESS,
            0,
            0x3c02_0000 | config_upper,
        )?;
        write_word(
            memory,
            A0_GET_CONF_STUB_ADDRESS,
            4,
            0x2442_0000 | (config_reference & 0xffff),
        )?;
        write_word(memory, A0_GET_CONF_STUB_ADDRESS, 8, 0x2409_009d)?;
        write_word(memory, A0_GET_CONF_STUB_ADDRESS, 12, 0x0800_0028)?;
        write_word(memory, A0_GET_CONF_STUB_ADDRESS, 16, 0)?;

        self.tables.a0 = true;
        Ok(())
    }

    /// Dispatches the function number in `t1` and returns to `ra`.
    ///
    /// # Errors
    ///
    /// Returns [`BiosError`] for unknown calls, invalid resources, guest memory
    /// failures, arithmetic overflow, or configured-bound violations.
    #[allow(clippy::too_many_lines)]
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
            (BiosVector::A0, 0x00) | (BiosVector::B0, 0x32) => self.open(context, memory)?,
            (BiosVector::A0, 0x01) | (BiosVector::B0, 0x33) => self.lseek(context)?,
            (BiosVector::A0, 0x02) | (BiosVector::B0, 0x34) => self.read(context),
            (BiosVector::A0, 0x03) | (BiosVector::B0, 0x35) => self.write(context, memory)?,
            (BiosVector::A0, 0x04) | (BiosVector::B0, 0x36) => self.close(context),
            (BiosVector::A0, 0x05) | (BiosVector::B0, 0x37) => Self::ioctl(context),
            (BiosVector::A0, 0x06) | (BiosVector::B0, 0x38) => {
                action = HleAction::Halt;
                10
            }
            (BiosVector::A0, 0x07) | (BiosVector::B0, 0x39) => Self::isatty(context),
            (BiosVector::A0, 0x08) | (BiosVector::B0, 0x3a) => Self::getc(context),
            (BiosVector::A0, 0x09) | (BiosVector::B0, 0x3b) => Self::putc(context),
            (BiosVector::A0, 0x0a) => Self::to_digit(context),
            (BiosVector::A0, 0x0c) => self.strtoul(context, memory)?,
            (BiosVector::A0, 0x0d) => self.strtol(context, memory)?,
            (BiosVector::A0, 0x0e | 0x0f) => Self::absolute_value(context),
            (BiosVector::A0, 0x10 | 0x11) => self.atoi(context, memory)?,
            (BiosVector::A0, 0x12) => self.atob(context, memory)?,
            (BiosVector::A0, 0x13) => self.setjmp(context, memory)?,
            (BiosVector::A0, 0x14) => self.longjmp(context, memory)?,
            (BiosVector::A0, 0x15) => self.strcat(context, memory)?,
            (BiosVector::A0, 0x16) => self.strncat(context, memory)?,
            (BiosVector::A0, 0x17) => self.string_compare(context, memory, None)?,
            (BiosVector::A0, 0x18) => {
                self.string_compare(context, memory, Some(context.argument(2)))?
            }
            (BiosVector::A0, 0x19) => self.string_copy(context, memory, None)?,
            (BiosVector::A0, 0x1a) => {
                self.string_copy(context, memory, Some(context.argument(2)))?
            }
            (BiosVector::A0, 0x1b) => self.string_length(context, memory)?,
            (BiosVector::A0, 0x1c | 0x1e) => self.string_character(context, memory, false)?,
            (BiosVector::A0, 0x1d | 0x1f) => self.string_character(context, memory, true)?,
            (BiosVector::A0, 0x20) => self.string_pbrk(context, memory)?,
            (BiosVector::A0, 0x21) => self.string_span(context, memory, true)?,
            (BiosVector::A0, 0x22) => self.string_span(context, memory, false)?,
            (BiosVector::A0, 0x23) => self.string_token(context, memory)?,
            (BiosVector::A0, 0x24) => self.string_string(context, memory)?,
            (BiosVector::A0, 0x25) => Self::change_case(context, true),
            (BiosVector::A0, 0x26) => Self::change_case(context, false),
            (BiosVector::A0, 0x27) => self.bcopy(context, memory)?,
            (BiosVector::A0, 0x28) => self.bzero(context, memory)?,
            (BiosVector::A0, 0x29 | 0x2d) => self.memory_compare(context, memory)?,
            (BiosVector::A0, 0x55 | 0x70) => self.initialize_backup_unit(),
            (BiosVector::A0, 0x72) => 10,
            (BiosVector::A0, 0x2a) => self.memory_copy(context, memory, false)?,
            (BiosVector::A0, 0x2b) => self.memory_set(context, memory)?,
            (BiosVector::A0, 0x2c) => self.memory_copy(context, memory, true)?,
            (BiosVector::A0, 0x2e) => self.memory_character(context, memory)?,
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
            (BiosVector::A0, 0x31) => {
                if self.start_qsort(context)? {
                    action = HleAction::Call;
                }
                12
            }
            (BiosVector::A0, 0x33) => self.malloc(context)?,
            (BiosVector::A0, 0x34) => self.free(context),
            (BiosVector::A0, 0x35) => {
                if self.start_lsearch(context, memory)? {
                    action = HleAction::Call;
                }
                12
            }
            (BiosVector::A0, 0x36) => {
                if self.start_bsearch(context, memory)? {
                    action = HleAction::Call;
                }
                12
            }
            (BiosVector::A0, 0x37) => self.calloc(context, memory)?,
            (BiosVector::A0, 0x38) => self.realloc(context, memory)?,
            (BiosVector::A0, 0x39) => self.initialize_heap(context)?,
            (BiosVector::A0, 0x3a) => {
                action = HleAction::Halt;
                6
            }
            (BiosVector::A0, 0x3b) | (BiosVector::B0, 0x3c) => Self::getchar(context),
            (BiosVector::A0, 0x3c) | (BiosVector::B0, 0x3d) => Self::putchar(context),
            (BiosVector::A0, 0x3d) | (BiosVector::B0, 0x3e) => Self::gets(context, memory)?,
            (BiosVector::A0, 0x3e) | (BiosVector::B0, 0x3f) => self.puts(context, memory)?,
            (BiosVector::A0, 0x3f) => self.printf(context, memory)?,
            (BiosVector::A0, 0x44) => 12,
            (BiosVector::A0, 0x9f) => Self::set_memory_size(context, memory)?,
            (BiosVector::B0, 0x00) => self.allocate_kernel_memory(context)?,
            (BiosVector::B0, 0x01) => self.free_kernel_memory(context),
            (BiosVector::B0, 0x07) => self.deliver_event(context)?,
            (BiosVector::B0, 0x08) => self.open_event(context),
            (BiosVector::B0, 0x09) => self.close_event(context),
            (BiosVector::B0, 0x0a) => {
                let (cycles, ready) = self.wait_event(context);
                if !ready {
                    action = HleAction::Wait;
                }
                cycles
            }
            (BiosVector::B0, 0x0b) => self.test_event(context),
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
            (BiosVector::B0, 0x4a) => self.initialize_card(context),
            (BiosVector::B0, 0x4b) => self.start_card(),
            (BiosVector::B0, 0x4c) => self.stop_card(),
            (BiosVector::B0, 0x56) => self.get_function_table(context, memory, BiosVector::C0)?,
            (BiosVector::B0, 0x57) => self.get_function_table(context, memory, BiosVector::B0)?,
            (BiosVector::B0, 0x5b) => 6,
            (BiosVector::C0, 0x00) => self.initialize_timer_and_vblank_handlers(context),
            (BiosVector::C0, 0x01) => self.initialize_syscall_handler(context),
            (BiosVector::C0, 0x02) => self.enqueue_interrupt(context)?,
            (BiosVector::C0, 0x03) => self.dequeue_interrupt(context)?,
            (BiosVector::C0, 0x08) => self.initialize_kernel_heap(context)?,
            (BiosVector::C0, 0x0a) => self.change_clear_counter(context)?,
            (BiosVector::C0, 0x0c) => self.initialize_default_interrupt_handler(context),
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
                let was_enabled = self.interrupts_enabled;
                self.interrupts_enabled = false;
                context.return_value(u32::from(was_enabled));
                6
            }
            2 => {
                self.interrupts_enabled = true;
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
        self.interrupts_enabled
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
        !self.callback_stack.is_empty() || self.libc_operation.is_some()
    }

    /// Reports whether a guest comparator is servicing a libc HLE routine.
    #[must_use]
    pub const fn libc_callback_active(&self) -> bool {
        self.libc_operation.is_some()
    }

    /// Consumes a guest comparator result and either schedules the next
    /// comparison or completes the suspended libc operation.
    ///
    /// # Errors
    ///
    /// Returns [`BiosError`] for an unmatched callback, an operation bound,
    /// address arithmetic, or guest-memory failure.
    pub fn resume_libc_callback<M: GuestMemory>(
        &mut self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<HleOutcome, BiosError> {
        let result = i32::from_ne_bytes(context.registers[V0].to_ne_bytes());
        let Some(mut operation) = self.libc_operation.take() else {
            return Err(BiosError::CallbackStackEmpty);
        };
        let mut completed_result = None;
        match &mut operation.kind {
            LibcOperationKind::Qsort {
                base,
                elements,
                width,
                outer,
                inner,
            } => {
                if result > 0 {
                    let left = element_address(*base, inner.saturating_sub(1), *width)?;
                    let right = element_address(*base, *inner, *width)?;
                    for offset in 0..*width {
                        let left_value = read_byte(memory, left, offset)?;
                        let right_value = read_byte(memory, right, offset)?;
                        write_byte(memory, left, offset, right_value)?;
                        write_byte(memory, right, offset, left_value)?;
                    }
                    *inner = inner.saturating_sub(1);
                }
                if result <= 0 || *inner == 0 {
                    *outer = outer.saturating_add(1);
                    *inner = *outer;
                }
                if *outer >= *elements {
                    completed_result = Some(None);
                }
            }
            LibcOperationKind::Lsearch {
                key: _,
                base,
                elements,
                width,
                index,
            } => {
                if result == 0 {
                    completed_result = Some(Some(element_address(*base, *index, *width)?));
                } else {
                    *index = index.saturating_add(1);
                    if *index >= *elements {
                        completed_result = Some(Some(0));
                    }
                }
            }
            LibcOperationKind::Bsearch {
                key: _,
                base,
                width,
                low,
                high,
                middle,
            } => {
                if result == 0 {
                    completed_result = Some(Some(element_address(*base, *middle, *width)?));
                } else {
                    if result < 0 {
                        *high = *middle;
                    } else {
                        *low = middle.saturating_add(1);
                    }
                    if *low >= *high {
                        completed_result = Some(Some(0));
                    } else {
                        *middle = *low + (*high - *low) / 2;
                    }
                }
            }
        }

        if let Some(return_value) = completed_result {
            *context = operation.saved_context;
            if let Some(return_value) = return_value {
                context.return_value(return_value);
            }
            context.return_to_caller();
            return Ok(HleOutcome {
                cycles: 10,
                action: HleAction::Return,
            });
        }

        self.prepare_libc_comparison(&mut operation, context)?;
        self.libc_operation = Some(operation);
        Ok(HleOutcome {
            cycles: 10,
            action: HleAction::Call,
        })
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

    fn bcopy<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let source = context.argument(0);
        let destination = context.argument(1);
        let size = context.argument(2);
        self.check_memory_size(size)?;
        for offset in 0..size {
            let value = read_byte(memory, source, offset)?;
            write_byte(memory, destination, offset, value)?;
        }
        context.return_value(source);
        Ok(memory_cycles(size))
    }

    fn bzero<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let destination = context.argument(0);
        let size = context.argument(1);
        self.check_memory_size(size)?;
        for offset in 0..size {
            write_byte(memory, destination, offset, 0)?;
        }
        context.return_value(destination);
        Ok(memory_cycles(size))
    }

    fn set_memory_size<M: GuestMemory>(
        context: &CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        for (offset, byte) in context.argument(0).to_le_bytes().into_iter().enumerate() {
            write_byte(
                memory,
                RAM_SIZE_ADDRESS,
                u32::try_from(offset).map_err(|_| BiosError::AddressOverflow)?,
                byte,
            )?;
        }
        Ok(12)
    }

    fn start_qsort(&mut self, context: &mut CpuContext) -> Result<bool, BiosError> {
        let base = context.argument(0);
        let elements = context.argument(1);
        let width = context.argument(2);
        self.check_array(base, elements, width)?;
        if elements < 2 || width == 0 {
            return Ok(false);
        }
        let callback = context.argument(3);
        self.start_libc_operation(
            context,
            callback,
            LibcOperationKind::Qsort {
                base,
                elements,
                width,
                outer: 1,
                inner: 1,
            },
        )?;
        Ok(true)
    }

    fn start_lsearch<M: GuestMemory>(
        &mut self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<bool, BiosError> {
        let key = context.argument(0);
        let base = context.argument(1);
        let elements = context.argument(2);
        let width = context.argument(3);
        self.check_array(base, elements, width)?;
        if elements == 0 || width == 0 {
            context.return_value(0);
            return Ok(false);
        }
        let callback = read_word(memory, context.registers[SP], 16)?;
        self.start_libc_operation(
            context,
            callback,
            LibcOperationKind::Lsearch {
                key,
                base,
                elements,
                width,
                index: 0,
            },
        )?;
        Ok(true)
    }

    fn start_bsearch<M: GuestMemory>(
        &mut self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<bool, BiosError> {
        let key = context.argument(0);
        let base = context.argument(1);
        let elements = context.argument(2);
        let width = context.argument(3);
        self.check_array(base, elements, width)?;
        if elements == 0 || width == 0 {
            context.return_value(0);
            return Ok(false);
        }
        let callback = read_word(memory, context.registers[SP], 16)?;
        self.start_libc_operation(
            context,
            callback,
            LibcOperationKind::Bsearch {
                key,
                base,
                width,
                low: 0,
                high: elements,
                middle: elements / 2,
            },
        )?;
        Ok(true)
    }

    fn start_libc_operation(
        &mut self,
        context: &mut CpuContext,
        callback: u32,
        kind: LibcOperationKind,
    ) -> Result<(), BiosError> {
        if self.libc_operation.is_some() {
            return Err(BiosError::CallbackDepth);
        }
        let mut operation = LibcOperation {
            saved_context: context.clone(),
            callback,
            calls: 0,
            kind,
        };
        self.prepare_libc_comparison(&mut operation, context)?;
        self.libc_operation = Some(operation);
        Ok(())
    }

    fn prepare_libc_comparison(
        &self,
        operation: &mut LibcOperation,
        context: &mut CpuContext,
    ) -> Result<(), BiosError> {
        if operation.calls >= self.limits.libc_callback_calls {
            return Err(BiosError::LibcCallbackLimit {
                limit: self.limits.libc_callback_calls,
            });
        }
        operation.calls += 1;
        let (left, right) = match operation.kind {
            LibcOperationKind::Qsort {
                base, width, inner, ..
            } => (
                element_address(base, inner.saturating_sub(1), width)?,
                element_address(base, inner, width)?,
            ),
            LibcOperationKind::Lsearch {
                key,
                base,
                width,
                index,
                ..
            } => (key, element_address(base, index, width)?),
            LibcOperationKind::Bsearch {
                key,
                base,
                width,
                middle,
                ..
            } => (key, element_address(base, middle, width)?),
        };
        *context = operation.saved_context.clone();
        context.pc = operation.callback;
        context.registers[A0] = left;
        context.registers[A0 + 1] = right;
        context.registers[RA] = CALLBACK_RETURN_PC;
        Ok(())
    }

    fn check_array(&self, base: u32, elements: u32, width: u32) -> Result<(), BiosError> {
        let size = elements
            .checked_mul(width)
            .ok_or(BiosError::AddressOverflow)?;
        self.check_memory_size(size)?;
        base.checked_add(size).ok_or(BiosError::AddressOverflow)?;
        Ok(())
    }

    fn open<M: GuestMemory>(
        &mut self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let path = self.read_tty_string(memory, context.argument(0), TTY_STRING_BYTES)?;
        if path.starts_with(b"sim:") {
            // PSF executables are RAM snapshots: data that the original Psy-Q
            // development build read from its `sim:` device is already mapped
            // at the read destination by the PSF load plan.
            self.resident_file = Some(ResidentFile { position: 0 });
            context.return_value(RESIDENT_FILE_DESCRIPTOR);
        } else {
            context.return_value(u32::MAX);
        }
        Ok(memory_cycles(
            u32::try_from(path.len())
                .unwrap_or(u32::MAX)
                .saturating_add(1),
        ))
    }

    fn lseek(&mut self, context: &mut CpuContext) -> Result<u32, BiosError> {
        if context.argument(0) != RESIDENT_FILE_DESCRIPTOR {
            context.return_value(u32::MAX);
            return Ok(6);
        }
        let Some(file) = self.resident_file.as_mut() else {
            context.return_value(u32::MAX);
            return Ok(6);
        };
        let offset = i32::from_ne_bytes(context.argument(1).to_ne_bytes());
        let base = match context.argument(2) {
            0 => 0,
            1 => file.position,
            _ => {
                context.return_value(u32::MAX);
                return Ok(6);
            }
        };
        let position = i64::from(base) + i64::from(offset);
        if !(0..=i64::from(u32::MAX)).contains(&position) {
            context.return_value(u32::MAX);
            return Ok(6);
        }
        file.position = u32::try_from(position).map_err(|_| BiosError::AddressOverflow)?;
        context.return_value(file.position);
        Ok(8)
    }

    fn read(&mut self, context: &mut CpuContext) -> u32 {
        let size = context.argument(2);
        if context.argument(0) != RESIDENT_FILE_DESCRIPTOR {
            context.return_value(u32::MAX);
            return 6;
        }
        let Some(file) = self.resident_file.as_mut() else {
            context.return_value(u32::MAX);
            return 6;
        };
        file.position = file.position.saturating_add(size);
        context.return_value(size);
        memory_cycles(size)
    }

    fn write<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let descriptor = context.argument(0);
        let source = context.argument(1);
        let size = context.argument(2);
        if !matches!(descriptor, 1 | 2) {
            context.return_value(u32::MAX);
            return Ok(6);
        }
        self.check_memory_size(size)?;
        let mut output = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        for offset in 0..size {
            output.push(read_byte(memory, source, offset)?);
        }
        if !output.is_empty() {
            eprintln!("[PS1 BIOS TTY] {}", String::from_utf8_lossy(&output));
        }
        context.return_value(size);
        Ok(memory_cycles(size))
    }

    fn ioctl(context: &mut CpuContext) -> u32 {
        context.return_value(u32::MAX);
        6
    }

    fn isatty(context: &mut CpuContext) -> u32 {
        context.return_value(u32::from(matches!(context.argument(0), 0..=2)));
        4
    }

    fn getc(context: &mut CpuContext) -> u32 {
        context.return_value(u32::MAX);
        8
    }

    fn putc(context: &mut CpuContext) -> u32 {
        if !matches!(context.argument(1), 1 | 2) {
            context.return_value(u32::MAX);
            return 6;
        }
        Self::putchar(context)
    }

    fn close(&mut self, context: &mut CpuContext) -> u32 {
        if context.argument(0) == RESIDENT_FILE_DESCRIPTOR && self.resident_file.take().is_some() {
            context.return_value(0);
        } else {
            context.return_value(u32::MAX);
        }
        6
    }

    fn initialize_card(&mut self, context: &CpuContext) -> u32 {
        self.card = CardState {
            initialized: true,
            started: false,
            pad_enabled: context.argument(0) != 0,
            backup_unit_initialized: false,
        };
        40
    }

    fn start_card(&mut self) -> u32 {
        if self.card.initialized {
            self.card.started = true;
        }
        16
    }

    fn stop_card(&mut self) -> u32 {
        self.card.started = false;
        12
    }

    fn initialize_backup_unit(&mut self) -> u32 {
        if self.card.initialized {
            self.card.backup_unit_initialized = true;
        }
        24
    }

    fn get_function_table<M: GuestMemory>(
        &mut self,
        context: &mut CpuContext,
        memory: &mut M,
        vector: BiosVector,
    ) -> Result<u32, BiosError> {
        let (table_address, stub_address, installed, vector_address) = match vector {
            BiosVector::B0 => (
                B0_TABLE_ADDRESS,
                B0_STUB_ADDRESS,
                self.tables.b0,
                0x0000_00b0,
            ),
            BiosVector::C0 => (
                C0_TABLE_ADDRESS,
                C0_STUB_ADDRESS,
                self.tables.c0,
                0x0000_00c0,
            ),
            BiosVector::A0 => unreachable!("the BIOS does not expose its A0 table"),
        };
        if !installed {
            self.check_memory_size(BIOS_TABLE_ENTRIES * (4 + BIOS_STUB_BYTES))?;
            for function in 0..BIOS_TABLE_ENTRIES {
                let table_offset = function.checked_mul(4).ok_or(BiosError::AddressOverflow)?;
                let stub_offset = function
                    .checked_mul(BIOS_STUB_BYTES)
                    .ok_or(BiosError::AddressOverflow)?;
                let stub = stub_address
                    .checked_add(stub_offset)
                    .ok_or(BiosError::AddressOverflow)?;
                write_word(memory, table_address, table_offset, stub)?;
                write_hle_stub(memory, stub, function, vector_address)?;
            }
            match vector {
                BiosVector::B0 => {
                    write_word(
                        memory,
                        table_address,
                        0x5b * 4,
                        B0_CHANGE_CLEAR_PAD_STUB_ADDRESS,
                    )?;
                    write_hle_stub(
                        memory,
                        B0_CHANGE_CLEAR_PAD_STUB_ADDRESS,
                        0x5b,
                        vector_address,
                    )?;
                }
                BiosVector::C0 => {
                    write_word(memory, table_address, 0x06 * 4, C0_EXCEPTION_STUB_ADDRESS)?;
                    write_hle_stub(memory, C0_EXCEPTION_STUB_ADDRESS, 0x06, vector_address)?;
                    write_word(memory, C0_EXCEPTION_STUB_ADDRESS, 0x70, 0)?;
                    write_word(
                        memory,
                        C0_EXCEPTION_STUB_ADDRESS,
                        0x74,
                        BIOS_PATCH_SCRATCH_ADDRESS - 0x28,
                    )?;
                }
                BiosVector::A0 => unreachable!("the BIOS does not expose its A0 table"),
            }
            match vector {
                BiosVector::B0 => self.tables.b0 = true,
                BiosVector::C0 => self.tables.c0 = true,
                BiosVector::A0 => unreachable!("the BIOS does not expose its A0 table"),
            }
        }
        context.return_value(table_address);
        Ok(if installed { 8 } else { 4096 })
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
        let parsed = self.parse_integer(memory, source, context.argument(2), false, false)?;
        if context.argument(1) != 0 {
            write_word(memory, context.argument(1), 0, parsed.end)?;
        }
        context.return_value(parsed.value);
        Ok(memory_cycles(parsed.bytes_read).saturating_add(8))
    }

    fn strtol<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let source = context.argument(0);
        if source == 0 {
            context.return_value(0);
            return Ok(6);
        }
        let parsed = self.parse_integer(memory, source, context.argument(2), true, false)?;
        if context.argument(1) != 0 {
            write_word(memory, context.argument(1), 0, parsed.end)?;
        }
        context.return_value(parsed.value);
        Ok(memory_cycles(parsed.bytes_read).saturating_add(8))
    }

    fn atoi<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let source = context.argument(0);
        if source == 0 {
            context.return_value(0);
            return Ok(6);
        }
        let parsed = self.parse_integer(memory, source, 10, true, true)?;
        context.return_value(parsed.value);
        Ok(memory_cycles(parsed.bytes_read).saturating_add(8))
    }

    fn atob<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let source = context.argument(0);
        if source == 0 {
            context.return_value(0);
            return Ok(6);
        }
        let parsed = self.parse_integer(memory, source, 10, true, false)?;
        if context.argument(1) != 0 {
            write_word(memory, context.argument(1), 0, parsed.value)?;
        }
        context.return_value(parsed.end);
        Ok(memory_cycles(parsed.bytes_read).saturating_add(10))
    }

    fn parse_integer<M: GuestMemory>(
        &self,
        memory: &mut M,
        source: u32,
        requested_base: u32,
        signed: bool,
        zero_octal: bool,
    ) -> Result<ParsedInteger, BiosError> {
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

        let negative = signed && self.read_string_byte(memory, source, offset)? == b'-';
        if negative {
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
        } else if (zero_octal && first == b'0') || matches!(first, b'o' | b'O') {
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

        let value = if negative {
            0_u32.wrapping_sub(result)
        } else {
            result
        };
        Ok(ParsedInteger {
            value,
            end: source
                .checked_add(offset)
                .ok_or(BiosError::AddressOverflow)?,
            bytes_read: offset.saturating_add(1),
        })
    }

    fn to_digit(context: &mut CpuContext) -> u32 {
        let byte = context.argument(0).to_le_bytes()[0];
        context.return_value(ascii_digit(byte).unwrap_or(9_999_999));
        6
    }

    fn absolute_value(context: &mut CpuContext) -> u32 {
        let value = i32::from_ne_bytes(context.argument(0).to_ne_bytes());
        context.return_value(u32::from_ne_bytes(value.wrapping_abs().to_ne_bytes()));
        4
    }

    fn change_case(context: &mut CpuContext, uppercase: bool) -> u32 {
        let mut byte = context.argument(0).to_le_bytes()[0];
        if uppercase && byte.is_ascii_lowercase() {
            byte = byte.to_ascii_uppercase();
        } else if !uppercase && byte.is_ascii_uppercase() {
            byte = byte.to_ascii_lowercase();
        }
        context.return_value(u32::from(byte));
        4
    }

    fn strcat<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        self.string_concat(context, memory, None)
    }

    fn strncat<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        self.string_concat(context, memory, Some(context.argument(2)))
    }

    fn string_concat<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
        maximum: Option<u32>,
    ) -> Result<u32, BiosError> {
        let destination = context.argument(0);
        let source = context.argument(1);
        if destination == 0 || source == 0 {
            context.return_value(0);
            return Ok(6);
        }

        let mut destination_length = 0_u32;
        while self.read_string_byte(memory, destination, destination_length)? != 0 {
            destination_length = destination_length
                .checked_add(1)
                .ok_or(BiosError::AddressOverflow)?;
        }

        let mut source_offset = 0_u32;
        loop {
            if maximum.is_some_and(|maximum| source_offset >= maximum) {
                let size = destination_length
                    .checked_add(source_offset)
                    .and_then(|value| value.checked_add(1))
                    .ok_or(BiosError::AddressOverflow)?;
                self.check_memory_size(size)?;
                write_byte(memory, destination, destination_length + source_offset, 0)?;
                context.return_value(destination);
                return Ok(memory_cycles(size));
            }
            let size = destination_length
                .checked_add(source_offset)
                .and_then(|value| value.checked_add(1))
                .ok_or(BiosError::AddressOverflow)?;
            self.check_memory_size(size)?;
            let value = read_byte(memory, source, source_offset)?;
            write_byte(
                memory,
                destination,
                destination_length + source_offset,
                value,
            )?;
            if value == 0 {
                context.return_value(destination);
                return Ok(memory_cycles(size));
            }
            source_offset = source_offset
                .checked_add(1)
                .ok_or(BiosError::AddressOverflow)?;
        }
    }

    fn string_compare<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
        maximum: Option<u32>,
    ) -> Result<u32, BiosError> {
        let left = context.argument(0);
        let right = context.argument(1);
        let null_result = match (left == 0, right == 0) {
            (true, true) => Some(0_i32),
            (true, false) => Some(-1),
            (false, true) => Some(1),
            (false, false) => None,
        };
        if let Some(result) = null_result {
            context.return_value(u32::from_ne_bytes(result.to_ne_bytes()));
            return Ok(6);
        }

        let mut offset = 0_u32;
        let result = loop {
            if maximum.is_some_and(|maximum| offset >= maximum) {
                break 0_i32;
            }
            let left_byte = self.read_string_byte(memory, left, offset)?;
            let right_byte = self.read_string_byte(memory, right, offset)?;
            if left_byte != right_byte {
                break i32::from(i8::from_ne_bytes([left_byte]))
                    - i32::from(i8::from_ne_bytes([right_byte]));
            }
            if left_byte == 0 {
                break 0_i32;
            }
            offset = offset.checked_add(1).ok_or(BiosError::AddressOverflow)?;
        };
        context.return_value(u32::from_ne_bytes(result.to_ne_bytes()));
        Ok(memory_cycles(offset.saturating_add(1)))
    }

    fn string_copy<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
        maximum: Option<u32>,
    ) -> Result<u32, BiosError> {
        let destination = context.argument(0);
        let source = context.argument(1);
        if destination == 0 || source == 0 {
            context.return_value(0);
            return Ok(6);
        }
        let limit = maximum.unwrap_or(self.limits.memory_operation_bytes);
        self.check_memory_size(limit)?;
        let mut terminated = false;
        for offset in 0..limit {
            let value = if terminated {
                0
            } else {
                let value = self.read_string_byte(memory, source, offset)?;
                terminated = value == 0;
                value
            };
            write_byte(memory, destination, offset, value)?;
            if maximum.is_none() && terminated {
                context.return_value(destination);
                return Ok(memory_cycles(offset.saturating_add(1)));
            }
        }
        if maximum.is_none() {
            return Err(BiosError::MemoryOperationLimit { size: limit, limit });
        }
        context.return_value(destination);
        Ok(memory_cycles(limit))
    }

    fn string_length<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let source = context.argument(0);
        if source == 0 {
            context.return_value(0);
            return Ok(4);
        }
        let mut length = 0_u32;
        while self.read_string_byte(memory, source, length)? != 0 {
            length = length.checked_add(1).ok_or(BiosError::AddressOverflow)?;
        }
        context.return_value(length);
        Ok(memory_cycles(length.saturating_add(1)))
    }

    fn string_character<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
        reverse: bool,
    ) -> Result<u32, BiosError> {
        let source = context.argument(0);
        if source == 0 {
            context.return_value(0);
            return Ok(4);
        }
        let target = context.argument(1).to_le_bytes()[0];
        let mut offset = 0_u32;
        let mut found = None;
        loop {
            let value = self.read_string_byte(memory, source, offset)?;
            if value == target {
                found = Some(
                    source
                        .checked_add(offset)
                        .ok_or(BiosError::AddressOverflow)?,
                );
                if !reverse {
                    break;
                }
            }
            if value == 0 {
                break;
            }
            offset = offset.checked_add(1).ok_or(BiosError::AddressOverflow)?;
        }
        context.return_value(found.unwrap_or(0));
        Ok(memory_cycles(offset.saturating_add(1)))
    }

    fn string_pbrk<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let source = context.argument(0);
        let list = context.argument(1);
        if source == 0 || list == 0 {
            context.return_value(0);
            return Ok(4);
        }
        let mut offset = 0_u32;
        loop {
            let value = self.read_string_byte(memory, source, offset)?;
            if value == 0 {
                context.return_value(0);
                break;
            }
            if self.string_contains(memory, list, value)? {
                context.return_value(
                    source
                        .checked_add(offset)
                        .ok_or(BiosError::AddressOverflow)?,
                );
                break;
            }
            offset = offset.checked_add(1).ok_or(BiosError::AddressOverflow)?;
        }
        Ok(memory_cycles(offset.saturating_add(1)))
    }

    fn string_span<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
        accept: bool,
    ) -> Result<u32, BiosError> {
        let source = context.argument(0);
        let list = context.argument(1);
        if source == 0 {
            context.return_value(0);
            return Ok(4);
        }
        let mut offset = 0_u32;
        loop {
            let value = self.read_string_byte(memory, source, offset)?;
            if value == 0 || self.string_contains(memory, list, value)? != accept {
                break;
            }
            offset = offset.checked_add(1).ok_or(BiosError::AddressOverflow)?;
        }
        context.return_value(offset);
        Ok(memory_cycles(offset.saturating_add(1)))
    }

    fn string_token<M: GuestMemory>(
        &mut self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let source = context.argument(0);
        let delimiters = context.argument(1);
        if delimiters == 0 {
            context.return_value(0);
            return Ok(4);
        }
        if source != 0 {
            let input = self.read_tty_string(memory, source, STRTOK_BUFFER_SIZE)?;
            for (offset, value) in input.iter().copied().chain(std::iter::once(0)).enumerate() {
                write_byte(
                    memory,
                    STRTOK_BUFFER_ADDRESS,
                    u32::try_from(offset).map_err(|_| BiosError::AddressOverflow)?,
                    value,
                )?;
            }
            self.strtok_next = Some(STRTOK_BUFFER_ADDRESS);
        }
        let Some(mut cursor) = self.strtok_next else {
            context.return_value(0);
            return Ok(6);
        };
        let mut scanned = 0_u32;
        loop {
            let value = self.read_string_byte(memory, cursor, 0)?;
            if value == 0 {
                self.strtok_next = None;
                context.return_value(0);
                return Ok(memory_cycles(scanned.saturating_add(1)));
            }
            if !self.string_contains(memory, delimiters, value)? {
                break;
            }
            cursor = cursor.checked_add(1).ok_or(BiosError::AddressOverflow)?;
            scanned = scanned.checked_add(1).ok_or(BiosError::AddressOverflow)?;
        }
        let token = cursor;
        loop {
            let value = self.read_string_byte(memory, cursor, 0)?;
            if value == 0 {
                self.strtok_next = None;
                break;
            }
            if self.string_contains(memory, delimiters, value)? {
                write_byte(memory, cursor, 0, 0)?;
                self.strtok_next = Some(cursor.checked_add(1).ok_or(BiosError::AddressOverflow)?);
                break;
            }
            cursor = cursor.checked_add(1).ok_or(BiosError::AddressOverflow)?;
            scanned = scanned.checked_add(1).ok_or(BiosError::AddressOverflow)?;
            self.check_memory_size(scanned)?;
        }
        context.return_value(token);
        Ok(memory_cycles(scanned.saturating_add(1)))
    }

    fn string_string<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let haystack = context.argument(0);
        let needle_address = context.argument(1);
        if haystack == 0 || needle_address == 0 {
            context.return_value(0);
            return Ok(4);
        }
        let needle =
            self.read_tty_string(memory, needle_address, self.limits.memory_operation_bytes)?;
        if needle.is_empty() {
            context.return_value(haystack);
            return Ok(4);
        }
        let mut offset = 0_u32;
        'search: loop {
            if self.read_string_byte(memory, haystack, offset)? == 0 {
                context.return_value(0);
                break;
            }
            for (needle_offset, expected) in needle.iter().copied().enumerate() {
                let needle_offset =
                    u32::try_from(needle_offset).map_err(|_| BiosError::AddressOverflow)?;
                let candidate_offset = offset
                    .checked_add(needle_offset)
                    .ok_or(BiosError::AddressOverflow)?;
                if self.read_string_byte(memory, haystack, candidate_offset)? != expected {
                    offset = offset.checked_add(1).ok_or(BiosError::AddressOverflow)?;
                    continue 'search;
                }
            }
            context.return_value(
                haystack
                    .checked_add(offset)
                    .ok_or(BiosError::AddressOverflow)?,
            );
            break;
        }
        Ok(memory_cycles(offset.saturating_add(1)))
    }

    fn string_contains<M: GuestMemory>(
        &self,
        memory: &mut M,
        list: u32,
        target: u8,
    ) -> Result<bool, BiosError> {
        if list == 0 {
            return Ok(false);
        }
        let mut offset = 0_u32;
        loop {
            let value = self.read_string_byte(memory, list, offset)?;
            if value == 0 {
                return Ok(false);
            }
            if value == target {
                return Ok(true);
            }
            offset = offset.checked_add(1).ok_or(BiosError::AddressOverflow)?;
        }
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

    fn getchar(context: &mut CpuContext) -> u32 {
        context.return_value(u32::MAX);
        8
    }

    fn putchar(context: &mut CpuContext) -> u32 {
        let value = context.argument(0).to_le_bytes()[0];
        if value == b'\n' {
            eprintln!("[PS1 BIOS TTY]");
        } else if value.is_ascii_graphic() || value == b' ' || value == b'\t' {
            eprintln!("[PS1 BIOS TTY] {}", char::from(value));
        } else {
            eprintln!("[PS1 BIOS TTY] <{value:02x}>");
        }
        context.return_value(u32::from(value));
        8
    }

    fn gets<M: GuestMemory>(context: &mut CpuContext, memory: &mut M) -> Result<u32, BiosError> {
        let destination = context.argument(0);
        if destination != 0 {
            write_byte(memory, destination, 0, 0)?;
        }
        context.return_value(destination);
        Ok(8)
    }

    fn puts<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let source = context.argument(0);
        let output = if source == 0 {
            b"<NULL>".to_vec()
        } else {
            self.read_tty_string(memory, source, TTY_STRING_BYTES)?
        };
        if !output.is_empty() {
            eprintln!("[PS1 BIOS TTY] {}", String::from_utf8_lossy(&output));
        }
        context.return_value(u32::try_from(output.len()).unwrap_or(u32::MAX));
        Ok(memory_cycles(
            u32::try_from(output.len()).unwrap_or(u32::MAX),
        ))
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

    fn longjmp<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        const BUFFER_SIZE: u32 = 48;

        self.check_memory_size(BUFFER_SIZE)?;
        let buffer = context.argument(0);
        let value = context.argument(1);
        let mut saved = [0_u32; 12];
        for (index, word) in saved.iter_mut().enumerate() {
            let offset = u32::try_from(index)
                .map_err(|_| BiosError::AddressOverflow)?
                .checked_mul(4)
                .ok_or(BiosError::AddressOverflow)?;
            *word = read_word(memory, buffer, offset)?;
        }
        context.registers[RA] = saved[0];
        context.registers[SP] = saved[1];
        context.registers[FP] = saved[2];
        context.registers[S0..S0 + 8].copy_from_slice(&saved[3..11]);
        context.registers[GP] = saved[11];
        context.return_value(value);
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

    fn memory_character<M: GuestMemory>(
        &self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let source = context.argument(0);
        let target = context.argument(1).to_le_bytes()[0];
        let size = context.argument(2);
        self.check_memory_size(size)?;
        let mut result = 0_u32;
        for offset in 0..size {
            if read_byte(memory, source, offset)? == target {
                result = source
                    .checked_add(offset)
                    .ok_or(BiosError::AddressOverflow)?;
                break;
            }
        }
        context.return_value(result);
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
        self.heap = Some(Heap {
            base,
            end,
            blocks: Vec::new(),
        });
        context.return_value(base);
        Ok(20)
    }

    fn malloc(&mut self, context: &mut CpuContext) -> Result<u32, BiosError> {
        let size = context.argument(0);
        let address = self.allocate(size)?;
        context.return_value(address);
        Ok(12)
    }

    fn allocate_kernel_memory(&mut self, context: &mut CpuContext) -> Result<u32, BiosError> {
        match allocate_from_heap(&mut self.kernel_heap, context.argument(0)) {
            Ok(address) => context.return_value(address),
            Err(BiosError::OutOfMemory { .. }) => context.return_value(u32::MAX),
            Err(error) => return Err(error),
        }
        Ok(12)
    }

    fn free_kernel_memory(&mut self, context: &CpuContext) -> u32 {
        free_from_heap(&mut self.kernel_heap, context.argument(0));
        6
    }

    fn initialize_kernel_heap(&mut self, context: &CpuContext) -> Result<u32, BiosError> {
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
        self.kernel_heap = Heap {
            base,
            end,
            blocks: Vec::new(),
        };
        Ok(20)
    }

    fn initialize_timer_and_vblank_handlers(&mut self, context: &CpuContext) -> u32 {
        self.kernel_handlers.timer_and_vblank = Some(context.argument(0));
        12
    }

    fn initialize_syscall_handler(&mut self, context: &CpuContext) -> u32 {
        self.kernel_handlers.syscall = Some(context.argument(0));
        12
    }

    fn initialize_default_interrupt_handler(&mut self, context: &CpuContext) -> u32 {
        self.kernel_handlers.default_interrupt = Some(context.argument(0));
        12
    }

    fn allocate(&mut self, size: u32) -> Result<u32, BiosError> {
        let heap = self.heap.as_mut().ok_or(BiosError::HeapUnavailable)?;
        allocate_from_heap(heap, size)
    }

    fn free(&mut self, context: &mut CpuContext) -> u32 {
        let address = context.argument(0);
        if let Some(heap) = self.heap.as_mut() {
            free_from_heap(heap, address);
        }
        6
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

    fn realloc<M: GuestMemory>(
        &mut self,
        context: &mut CpuContext,
        memory: &mut M,
    ) -> Result<u32, BiosError> {
        let old_address = context.argument(0);
        let new_size = context.argument(1);
        self.check_memory_size(new_size)?;
        if old_address == 0 {
            let address = self.allocate(new_size)?;
            context.return_value(address);
            return Ok(12);
        }
        if new_size == 0 {
            self.free(context);
            context.return_value(0);
            return Ok(8);
        }

        let new_allocation_size = align_up(new_size.max(1), 8)?;
        let (index, old_size, following_address) = {
            let heap = self.heap.as_ref().ok_or(BiosError::HeapUnavailable)?;
            let Some(index) = heap
                .blocks
                .iter()
                .position(|block| block.address == old_address)
            else {
                context.return_value(0);
                return Ok(8);
            };
            let following_address = heap
                .blocks
                .get(index + 1)
                .map_or(heap.end, |block| block.address);
            (index, heap.blocks[index].size, following_address)
        };
        if old_address
            .checked_add(new_allocation_size)
            .ok_or(BiosError::AddressOverflow)?
            <= following_address
        {
            let heap = self.heap.as_mut().ok_or(BiosError::HeapUnavailable)?;
            heap.blocks[index].size = new_allocation_size;
            context.return_value(old_address);
            return Ok(10);
        }

        let new_address = self.allocate(new_size)?;
        let copied = old_size.min(new_size);
        for offset in 0..copied {
            let value = read_byte(memory, old_address, offset)?;
            write_byte(memory, new_address, offset, value)?;
        }
        if let Some(heap) = self.heap.as_mut()
            && let Some(index) = heap
                .blocks
                .iter()
                .position(|block| block.address == old_address)
        {
            heap.blocks.remove(index);
        }
        context.return_value(new_address);
        Ok(memory_cycles(copied).saturating_add(16))
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
        let delivered = u32::from(delivered);
        context.return_value(delivered);
        // The retail routine also leaves the readiness result in v1. The
        // shared Crash Bandicoot 2 and 3 PSF driver relies on this undocumented
        // clobber after calling SpuIsTransferCompleted.
        context.set_register(V1, delivered);
        8
    }

    fn wait_event(&mut self, context: &mut CpuContext) -> (u32, bool) {
        let handle = context.argument(0);
        let delivered = self.event_mut(handle).is_some_and(|event| {
            let delivered = event.enabled && event.state == EventState::Delivered;
            if delivered {
                event.state = EventState::Idle;
            }
            delivered
        });
        if delivered {
            context.return_value(1);
        }
        (8, delivered)
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

fn write_word<M: GuestMemory>(
    memory: &mut M,
    base: u32,
    offset: u32,
    value: u32,
) -> Result<(), BiosError> {
    for (index, byte) in value.to_le_bytes().into_iter().enumerate() {
        let byte_offset = u32::try_from(index).map_err(|_| BiosError::AddressOverflow)?;
        write_byte(memory, base, offset + byte_offset, byte)?;
    }
    Ok(())
}

fn write_hle_stub<M: GuestMemory>(
    memory: &mut M,
    address: u32,
    function: u32,
    vector_address: u32,
) -> Result<(), BiosError> {
    write_word(memory, address, 0, 0x2409_0000 | function)?;
    write_word(
        memory,
        address,
        4,
        0x0800_0000 | ((vector_address >> 2) & 0x03ff_ffff),
    )?;
    write_word(memory, address, 8, 0)
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

fn element_address(base: u32, index: u32, width: u32) -> Result<u32, BiosError> {
    base.checked_add(index.checked_mul(width).ok_or(BiosError::AddressOverflow)?)
        .ok_or(BiosError::AddressOverflow)
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

fn allocate_from_heap(heap: &mut Heap, size: u32) -> Result<u32, BiosError> {
    let allocation_size = align_up(size.max(1), 8)?;
    let mut address = heap.base;
    let mut insertion = heap.blocks.len();
    for (index, block) in heap.blocks.iter().copied().enumerate() {
        let candidate_end = address
            .checked_add(allocation_size)
            .ok_or(BiosError::AddressOverflow)?;
        if candidate_end <= block.address {
            insertion = index;
            break;
        }
        address = block
            .address
            .checked_add(block.size)
            .ok_or(BiosError::AddressOverflow)?;
    }
    let allocation_end = address
        .checked_add(allocation_size)
        .ok_or(BiosError::AddressOverflow)?;
    if allocation_end > heap.end {
        return Err(BiosError::OutOfMemory { size });
    }
    heap.blocks.insert(
        insertion,
        HeapBlock {
            address,
            size: allocation_size,
        },
    );
    Ok(address)
}

fn free_from_heap(heap: &mut Heap, address: u32) {
    if let Some(index) = heap
        .blocks
        .iter()
        .position(|block| block.address == address)
    {
        heap.blocks.remove(index);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        A0, A0_GET_CONF_STUB_ADDRESS, A0_STUB_ADDRESS, A0_TABLE_ADDRESS,
        B0_CHANGE_CLEAR_PAD_STUB_ADDRESS, B0_STUB_ADDRESS, B0_TABLE_ADDRESS,
        BIOS_PATCH_SCRATCH_ADDRESS, BIOS_STUB_BYTES, BiosError, BiosHle, BiosVector,
        C0_EXCEPTION_STUB_ADDRESS, C0_STUB_ADDRESS, C0_TABLE_ADDRESS, CpuContext, FP, GP,
        GuestMemory, GuestMemoryError, HleAction, HleLimits, KERNEL_CONFIG_ADDRESS,
        KERNEL_MEMORY_ADDRESS, KERNEL_MEMORY_SIZE, RA, RESIDENT_FILE_DESCRIPTOR, S0, SP,
        STRTOK_BUFFER_ADDRESS, T1, V0, V1,
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
    fn resident_sim_file_reads_preserve_preloaded_psf_memory() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 128]);
        memory.0[8..20].copy_from_slice(b"sim:\\AF2.CD\0");
        memory.0[64..68].copy_from_slice(&[1, 2, 3, 4]);

        let (opened, _) = call(&mut bios, BiosVector::B0, 0x32, [8, 1, 0, 0], &mut memory).unwrap();
        assert_eq!(opened.register(V0), Some(RESIDENT_FILE_DESCRIPTOR));
        let (seeked, _) = call(
            &mut bios,
            BiosVector::B0,
            0x33,
            [RESIDENT_FILE_DESCRIPTOR, 32, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(seeked.register(V0), Some(32));
        let (read, _) = call(
            &mut bios,
            BiosVector::B0,
            0x34,
            [RESIDENT_FILE_DESCRIPTOR, 64, 4, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(read.register(V0), Some(4));
        assert_eq!(&memory.0[64..68], &[1, 2, 3, 4]);
        let (closed, _) = call(
            &mut bios,
            BiosVector::B0,
            0x36,
            [RESIDENT_FILE_DESCRIPTOR, 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(closed.register(V0), Some(0));
    }

    #[test]
    fn card_initialization_tracks_lifecycle_without_media() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 16]);
        call(&mut bios, BiosVector::B0, 0x4a, [0, 0, 0, 0], &mut memory).unwrap();
        assert!(bios.card.initialized);
        assert!(!bios.card.started);
        assert!(!bios.card.pad_enabled);

        call(&mut bios, BiosVector::B0, 0x4b, [0; 4], &mut memory).unwrap();
        call(&mut bios, BiosVector::A0, 0x70, [0; 4], &mut memory).unwrap();
        assert!(bios.card.started);
        assert!(bios.card.backup_unit_initialized);

        call(&mut bios, BiosVector::B0, 0x4c, [0; 4], &mut memory).unwrap();
        assert!(!bios.card.started);
    }

    #[test]
    fn exposed_b0_and_c0_tables_contain_callable_hle_stubs() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 0x10000]);
        for (function, table, stubs, vector) in [
            (0x57, B0_TABLE_ADDRESS, B0_STUB_ADDRESS, 0x0000_00b0),
            (0x56, C0_TABLE_ADDRESS, C0_STUB_ADDRESS, 0x0000_00c0),
        ] {
            let (context, _) =
                call(&mut bios, BiosVector::B0, function, [0; 4], &mut memory).unwrap();
            assert_eq!(context.register(V0), Some(table));
            let entry = 0x4a_u32;
            let table_offset = usize::try_from(table + entry * 4).unwrap();
            let stub = stubs + entry * BIOS_STUB_BYTES;
            assert_eq!(
                u32::from_le_bytes(memory.0[table_offset..table_offset + 4].try_into().unwrap()),
                stub
            );
            let stub = usize::try_from(stub).unwrap();
            assert_eq!(
                u32::from_le_bytes(memory.0[stub..stub + 4].try_into().unwrap()),
                0x2409_004a
            );
            assert_eq!(
                u32::from_le_bytes(memory.0[stub + 4..stub + 8].try_into().unwrap()),
                0x0800_0000 | (vector >> 2)
            );
        }

        let b0_patch_entry = usize::try_from(B0_TABLE_ADDRESS + 0x5b * 4).unwrap();
        let b0_patch_address = u32::from_le_bytes(
            memory.0[b0_patch_entry..b0_patch_entry + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(b0_patch_address, B0_CHANGE_CLEAR_PAD_STUB_ADDRESS);
        assert!(b0_patch_address + 0x1988 < 0x10000);

        let c0_exception_entry = usize::try_from(C0_TABLE_ADDRESS + 0x06 * 4).unwrap();
        assert_eq!(
            u32::from_le_bytes(
                memory.0[c0_exception_entry..c0_exception_entry + 4]
                    .try_into()
                    .unwrap()
            ),
            C0_EXCEPTION_STUB_ADDRESS
        );
        let upper = usize::try_from(C0_EXCEPTION_STUB_ADDRESS + 0x70).unwrap();
        let lower = usize::try_from(C0_EXCEPTION_STUB_ADDRESS + 0x74).unwrap();
        let upper = u32::from_le_bytes(memory.0[upper..upper + 4].try_into().unwrap()) & 0xffff;
        let lower = u32::from_le_bytes(memory.0[lower..lower + 4].try_into().unwrap()) & 0xffff;
        assert_eq!((upper << 16) + lower + 0x28, BIOS_PATCH_SCRATCH_ADDRESS);
    }

    #[test]
    fn boot_memory_exposes_fixed_a0_table_and_introspectable_getconf() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 0xc000]);
        memory.0[..8].copy_from_slice(b"low ram!");
        bios.initialize_boot_memory(&mut memory).unwrap();

        assert_eq!(&memory.0[..8], b"low ram!");

        let generic_entry = usize::try_from(A0_TABLE_ADDRESS + 0x44 * 4).unwrap();
        assert_eq!(
            u32::from_le_bytes(
                memory.0[generic_entry..generic_entry + 4]
                    .try_into()
                    .unwrap()
            ),
            A0_STUB_ADDRESS + 0x44 * BIOS_STUB_BYTES
        );
        let getconf_entry = usize::try_from(A0_TABLE_ADDRESS + 0x9d * 4).unwrap();
        assert_eq!(
            u32::from_le_bytes(
                memory.0[getconf_entry..getconf_entry + 4]
                    .try_into()
                    .unwrap()
            ),
            A0_GET_CONF_STUB_ADDRESS
        );

        let stub = usize::try_from(A0_GET_CONF_STUB_ADDRESS).unwrap();
        let upper = u32::from_le_bytes(memory.0[stub..stub + 4].try_into().unwrap()) & 0xffff;
        let lower = i16::from_le_bytes(memory.0[stub + 4..stub + 6].try_into().unwrap());
        let decoded = (upper << 16)
            .wrapping_add(u32::from_ne_bytes(i32::from(lower).to_ne_bytes()))
            .wrapping_sub(8);
        assert_eq!(decoded, KERNEL_CONFIG_ADDRESS);
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
    fn signed_integer_and_character_conversions_match_the_bios_contract() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 160]);
        memory.0[16..24].copy_from_slice(b" -0x2a!\0");
        let (signed, _) =
            call(&mut bios, BiosVector::A0, 0x0d, [16, 8, 10, 0], &mut memory).unwrap();
        assert_eq!(
            signed.register(V0),
            Some(u32::from_ne_bytes((-42_i32).to_ne_bytes()))
        );
        assert_eq!(u32::from_le_bytes(memory.0[8..12].try_into().unwrap()), 22);

        memory.0[32..36].copy_from_slice(b"077\0");
        let (decimal, _) =
            call(&mut bios, BiosVector::A0, 0x10, [32, 0, 0, 0], &mut memory).unwrap();
        assert_eq!(decimal.register(V0), Some(63));

        memory.0[48..53].copy_from_slice(b"123z\0");
        let (converted, _) =
            call(&mut bios, BiosVector::A0, 0x12, [48, 80, 0, 0], &mut memory).unwrap();
        assert_eq!(converted.register(V0), Some(51));
        assert_eq!(
            u32::from_le_bytes(memory.0[80..84].try_into().unwrap()),
            123
        );

        let (digit, _) = call(
            &mut bios,
            BiosVector::A0,
            0x0a,
            [u32::from(b'Z'), 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(digit.register(V0), Some(35));
        let (upper, _) = call(
            &mut bios,
            BiosVector::A0,
            0x25,
            [u32::from(b'q'), 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(upper.register(V0), Some(u32::from(b'Q')));
        let (absolute, _) = call(
            &mut bios,
            BiosVector::A0,
            0x0e,
            [u32::from_ne_bytes((-19_i32).to_ne_bytes()), 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(absolute.register(V0), Some(19));
    }

    #[test]
    fn longjmp_restores_the_saved_context_and_exact_value() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 128]);
        let mut saved = CpuContext::reset(0x00a0, 0x1111_2222);
        saved.set_register(A0, 64);
        saved.set_register(T1, 0x13);
        saved.set_register(RA, 0x3333_4444);
        saved.set_register(FP, 0x5555_6666);
        saved.set_register(GP, 0x7777_8888);
        saved.set_register(S0 + 3, 0x9999_aaaa);
        bios.dispatch(BiosVector::A0, &mut saved, &mut memory)
            .unwrap();

        let mut restored = CpuContext::reset(0x00a0, 0xbbbb_cccc);
        restored.set_register(A0, 64);
        restored.set_register(A0 + 1, 7);
        restored.set_register(T1, 0x14);
        restored.set_register(RA, 0xdddd_eeee);
        bios.dispatch(BiosVector::A0, &mut restored, &mut memory)
            .unwrap();
        assert_eq!(restored.pc, 0x3333_4444);
        assert_eq!(restored.register(V0), Some(7));
        assert_eq!(restored.register(SP), Some(0x1111_2222));
        assert_eq!(restored.register(FP), Some(0x5555_6666));
        assert_eq!(restored.register(GP), Some(0x7777_8888));
        assert_eq!(restored.register(S0 + 3), Some(0x9999_aaaa));
    }

    #[test]
    fn strcat_appends_the_terminator_and_returns_the_destination() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 64]);
        memory.0[8..14].copy_from_slice(b"hello\0");
        memory.0[32..39].copy_from_slice(b" world\0");
        let (context, _) =
            call(&mut bios, BiosVector::A0, 0x15, [8, 32, 0, 0], &mut memory).unwrap();
        assert_eq!(&memory.0[8..20], b"hello world\0");
        assert_eq!(context.register(V0), Some(8));

        let (context, _) =
            call(&mut bios, BiosVector::A0, 0x15, [0, 32, 0, 0], &mut memory).unwrap();
        assert_eq!(context.register(V0), Some(0));
    }

    #[test]
    fn bounded_string_copy_compare_search_and_span_are_complete() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0xaa; 1024]);
        memory.0[32..38].copy_from_slice(b"alpha\0");
        memory.0[64..69].copy_from_slice(b"bet!\0");
        call(&mut bios, BiosVector::A0, 0x16, [32, 64, 3, 0], &mut memory).unwrap();
        assert_eq!(&memory.0[32..41], b"alphabet\0");

        memory.0[96..102].copy_from_slice(b"alpha\0");
        let (equal, _) =
            call(&mut bios, BiosVector::A0, 0x18, [32, 96, 5, 0], &mut memory).unwrap();
        assert_eq!(equal.register(V0), Some(0));
        let (ordered, _) =
            call(&mut bios, BiosVector::A0, 0x17, [32, 96, 0, 0], &mut memory).unwrap();
        assert!(i32::from_ne_bytes(ordered.register(V0).unwrap().to_ne_bytes()) > 0);

        call(
            &mut bios,
            BiosVector::A0,
            0x1a,
            [128, 96, 8, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(&memory.0[128..136], b"alpha\0\0\0");
        let (length, _) =
            call(&mut bios, BiosVector::A0, 0x1b, [32, 0, 0, 0], &mut memory).unwrap();
        assert_eq!(length.register(V0), Some(8));

        let (first, _) = call(
            &mut bios,
            BiosVector::A0,
            0x1c,
            [32, u32::from(b'a'), 0, 0],
            &mut memory,
        )
        .unwrap();
        let (last, _) = call(
            &mut bios,
            BiosVector::A0,
            0x1f,
            [32, u32::from(b'a'), 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(first.register(V0), Some(32));
        assert_eq!(last.register(V0), Some(36));

        memory.0[160..164].copy_from_slice(b"xyz\0");
        let (pbrk, _) = call(
            &mut bios,
            BiosVector::A0,
            0x20,
            [32, 160, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(pbrk.register(V0), Some(0));
        memory.0[176..180].copy_from_slice(b"alp\0");
        let (span, _) = call(
            &mut bios,
            BiosVector::A0,
            0x21,
            [32, 176, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(span.register(V0), Some(3));
        let (complement, _) = call(
            &mut bios,
            BiosVector::A0,
            0x22,
            [32, 160, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(complement.register(V0), Some(8));

        memory.0[192..196].copy_from_slice(b"bet\0");
        let (substring, _) = call(
            &mut bios,
            BiosVector::A0,
            0x24,
            [32, 192, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(substring.register(V0), Some(37));
    }

    #[test]
    fn strtok_uses_bounded_guest_visible_storage() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 0xc200]);
        memory.0[64..79].copy_from_slice(b"  one,two,,end\0");
        memory.0[32..35].copy_from_slice(b" ,\0");
        let mut tokens = Vec::new();
        let mut source = 64;
        loop {
            let (context, _) = call(
                &mut bios,
                BiosVector::A0,
                0x23,
                [source, 32, 0, 0],
                &mut memory,
            )
            .unwrap();
            source = 0;
            let address = context.register(V0).unwrap();
            if address == 0 {
                break;
            }
            let start = usize::try_from(address).unwrap();
            let end = memory.0[start..]
                .iter()
                .position(|byte| *byte == 0)
                .map(|length| start + length)
                .unwrap();
            tokens.push(memory.0[start..end].to_vec());
        }
        assert_eq!(tokens, [b"one".to_vec(), b"two".to_vec(), b"end".to_vec()]);
        assert_eq!(STRTOK_BUFFER_ADDRESS, 0xc000);
        assert_eq!(&memory.0[64..79], b"  one,two,,end\0");
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
        call(&mut bios, BiosVector::A0, 0x39, [3, 100, 0, 0], &mut memory).unwrap();
        let (first, _) = call(&mut bios, BiosVector::A0, 0x33, [9, 0, 0, 0], &mut memory).unwrap();
        assert_eq!(first.register(V0), Some(8));
        let (second, _) = call(&mut bios, BiosVector::A0, 0x37, [2, 4, 0, 0], &mut memory).unwrap();
        assert_eq!(second.register(V0), Some(24));
        assert_eq!(&memory.0[24..32], &[0; 8]);

        call(&mut bios, BiosVector::A0, 0x30, [7, 0, 0, 0], &mut memory).unwrap();
        let (random, _) = call(&mut bios, BiosVector::A0, 0x2f, [0; 4], &mut memory).unwrap();
        assert_eq!(random.register(V0), Some(19_564));
    }

    #[test]
    fn kernel_memory_allocator_is_preinitialized_independent_and_reuses_freed_blocks() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 16]);
        let (first, _) = call(
            &mut bios,
            BiosVector::B0,
            0x00,
            [0x20, 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        let (second, _) = call(&mut bios, BiosVector::B0, 0x00, [1, 0, 0, 0], &mut memory).unwrap();
        assert_eq!(first.register(V0), Some(KERNEL_MEMORY_ADDRESS));
        assert_eq!(second.register(V0), Some(KERNEL_MEMORY_ADDRESS + 0x20));

        call(
            &mut bios,
            BiosVector::B0,
            0x01,
            [KERNEL_MEMORY_ADDRESS, 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        let (reused, _) = call(&mut bios, BiosVector::B0, 0x00, [8, 0, 0, 0], &mut memory).unwrap();
        assert_eq!(reused.register(V0), Some(KERNEL_MEMORY_ADDRESS));

        let (exhausted, _) = call(
            &mut BiosHle::default(),
            BiosVector::B0,
            0x00,
            [KERNEL_MEMORY_SIZE + 1, 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(exhausted.register(V0), Some(u32::MAX));

        call(
            &mut bios,
            BiosVector::C0,
            0x08,
            [0xa000_e800, 0x1000, 0, 0],
            &mut memory,
        )
        .unwrap();
        let (reinitialized, _) =
            call(&mut bios, BiosVector::B0, 0x00, [16, 0, 0, 0], &mut memory).unwrap();
        assert_eq!(reinitialized.register(V0), Some(0xa000_e800));
    }

    #[test]
    fn legacy_memory_aliases_and_realloc_are_functional() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0xaa; 512]);
        memory.0[16..24].copy_from_slice(b"abcdefgh");
        let (copied, _) =
            call(&mut bios, BiosVector::A0, 0x27, [16, 32, 8, 0], &mut memory).unwrap();
        assert_eq!(copied.register(V0), Some(16));
        assert_eq!(&memory.0[32..40], b"abcdefgh");
        call(&mut bios, BiosVector::A0, 0x28, [34, 3, 0, 0], &mut memory).unwrap();
        assert_eq!(&memory.0[32..40], b"ab\0\0\0fgh");
        let (found, _) = call(
            &mut bios,
            BiosVector::A0,
            0x2e,
            [32, u32::from(b'f'), 8, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(found.register(V0), Some(37));

        call(
            &mut bios,
            BiosVector::A0,
            0x39,
            [0x100, 0x100, 0, 0],
            &mut memory,
        )
        .unwrap();
        let (first, _) = call(&mut bios, BiosVector::A0, 0x33, [16, 0, 0, 0], &mut memory).unwrap();
        let (second, _) =
            call(&mut bios, BiosVector::A0, 0x33, [16, 0, 0, 0], &mut memory).unwrap();
        let (third, _) = call(&mut bios, BiosVector::A0, 0x33, [16, 0, 0, 0], &mut memory).unwrap();
        assert_eq!(first.register(V0), Some(0x100));
        assert_eq!(second.register(V0), Some(0x110));
        assert_eq!(third.register(V0), Some(0x120));
        memory.0[0x110..0x120].copy_from_slice(b"preserved bytes!");
        let (moved, _) = call(
            &mut bios,
            BiosVector::A0,
            0x38,
            [0x110, 24, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(moved.register(V0), Some(0x130));
        assert_eq!(&memory.0[0x130..0x140], b"preserved bytes!");
        call(
            &mut bios,
            BiosVector::A0,
            0x34,
            [0x100, 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        let (reused, _) = call(&mut bios, BiosVector::A0, 0x33, [8, 0, 0, 0], &mut memory).unwrap();
        assert_eq!(reused.register(V0), Some(0x100));
    }

    fn finish_byte_comparisons(
        bios: &mut BiosHle,
        memory: &mut Memory,
        mut context: CpuContext,
        mut outcome: super::HleOutcome,
    ) -> CpuContext {
        while outcome.action == HleAction::Call {
            let left = usize::try_from(context.register(A0).unwrap()).unwrap();
            let right = usize::try_from(context.register(A0 + 1).unwrap()).unwrap();
            let result = i32::from(memory.0[left]) - i32::from(memory.0[right]);
            context.set_register(V0, u32::from_ne_bytes(result.to_ne_bytes()));
            outcome = bios.resume_libc_callback(&mut context, memory).unwrap();
        }
        context
    }

    #[test]
    fn callback_backed_sort_and_search_resume_to_the_original_caller() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 0x8020]);
        memory.0[0x100..0x105].copy_from_slice(&[4, 1, 3, 2, 5]);
        let (context, outcome) = call(
            &mut bios,
            BiosVector::A0,
            0x31,
            [0x100, 5, 1, 0x3000],
            &mut memory,
        )
        .unwrap();
        assert_eq!(context.pc, 0x3000);
        assert_eq!(context.register(RA), Some(BiosHle::callback_return_pc()));
        let context = finish_byte_comparisons(&mut bios, &mut memory, context, outcome);
        assert_eq!(&memory.0[0x100..0x105], &[1, 2, 3, 4, 5]);
        assert_eq!(context.pc, 0x2000);

        memory.0[0x80] = 3;
        memory.0[0x8010..0x8014].copy_from_slice(&0x3000_u32.to_le_bytes());
        let (context, outcome) = call(
            &mut bios,
            BiosVector::A0,
            0x35,
            [0x80, 0x100, 5, 1],
            &mut memory,
        )
        .unwrap();
        let context = finish_byte_comparisons(&mut bios, &mut memory, context, outcome);
        assert_eq!(context.register(V0), Some(0x102));
        assert_eq!(context.pc, 0x2000);

        memory.0[0x80] = 6;
        let (context, outcome) = call(
            &mut bios,
            BiosVector::A0,
            0x36,
            [0x80, 0x100, 5, 1],
            &mut memory,
        )
        .unwrap();
        let context = finish_byte_comparisons(&mut bios, &mut memory, context, outcome);
        assert_eq!(context.register(V0), Some(0));
        assert_eq!(context.pc, 0x2000);
    }

    #[test]
    fn libc_aliases_and_tty_fallbacks_dispatch_without_firmware() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 0x8020]);
        memory.0[32..38].copy_from_slice(b"hello\0");
        let (copied, _) =
            call(&mut bios, BiosVector::A0, 0x19, [64, 32, 0, 0], &mut memory).unwrap();
        assert_eq!(copied.register(V0), Some(64));
        assert_eq!(&memory.0[64..70], b"hello\0");
        for function in [0x1c, 0x1e] {
            let (found, _) = call(
                &mut bios,
                BiosVector::A0,
                function,
                [64, u32::from(b'e'), 0, 0],
                &mut memory,
            )
            .unwrap();
            assert_eq!(found.register(V0), Some(65));
        }
        let (lower, _) = call(
            &mut bios,
            BiosVector::A0,
            0x26,
            [u32::from(b'K'), 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(lower.register(V0), Some(u32::from(b'k')));
        let (compared, _) =
            call(&mut bios, BiosVector::A0, 0x29, [32, 64, 6, 0], &mut memory).unwrap();
        assert_eq!(compared.register(V0), Some(0));

        let (input, _) = call(&mut bios, BiosVector::A0, 0x3b, [0; 4], &mut memory).unwrap();
        assert_eq!(input.register(V0), Some(u32::MAX));
        let (line, _) = call(&mut bios, BiosVector::A0, 0x3d, [96, 0, 0, 0], &mut memory).unwrap();
        assert_eq!(line.register(V0), Some(96));
        assert_eq!(memory.0[96], 0);
        let (output, _) =
            call(&mut bios, BiosVector::B0, 0x3f, [32, 0, 0, 0], &mut memory).unwrap();
        assert_eq!(output.register(V0), Some(5));

        let (written, _) =
            call(&mut bios, BiosVector::A0, 0x03, [1, 32, 5, 0], &mut memory).unwrap();
        assert_eq!(written.register(V0), Some(5));
        let (terminal, _) =
            call(&mut bios, BiosVector::A0, 0x07, [2, 0, 0, 0], &mut memory).unwrap();
        assert_eq!(terminal.register(V0), Some(1));
        let (character, _) = call(
            &mut bios,
            BiosVector::B0,
            0x3b,
            [u32::from(b'!'), 1, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(character.register(V0), Some(u32::from(b'!')));
        let (halted, outcome) = call(&mut bios, BiosVector::A0, 0x3a, [0; 4], &mut memory).unwrap();
        assert_eq!(outcome.action, HleAction::Halt);
        assert_eq!(halted.pc, 0x1000);
    }

    #[test]
    fn set_mem_records_the_guest_visible_ram_size() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 128]);
        let (context, outcome) =
            call(&mut bios, BiosVector::A0, 0x9f, [2, 0, 0, 0], &mut memory).unwrap();

        assert_eq!(
            u32::from_le_bytes(memory.0[0x60..0x64].try_into().unwrap()),
            2
        );
        assert_eq!(context.pc, 0x2000);
        assert_eq!(outcome.cycles, 12);
        assert_eq!(outcome.action, HleAction::Return);
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
    fn test_event_mirrors_the_readiness_result_into_v1() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 16]);
        let (opened, _) = call(
            &mut bios,
            BiosVector::B0,
            0x08,
            [0xf000_0009, 0x20, 0x2000, 0],
            &mut memory,
        )
        .unwrap();
        let handle = opened.register(V0).unwrap();
        call(
            &mut bios,
            BiosVector::B0,
            0x0c,
            [handle, 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        call(
            &mut bios,
            BiosVector::B0,
            0x07,
            [0xf000_0009, 0x20, 0, 0],
            &mut memory,
        )
        .unwrap();

        let (ready, _) = call(
            &mut bios,
            BiosVector::B0,
            0x0b,
            [handle, 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(ready.register(V0), Some(1));
        assert_eq!(ready.register(V1), Some(1));

        let (idle, _) = call(
            &mut bios,
            BiosVector::B0,
            0x0b,
            [handle, 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(idle.register(V0), Some(0));
        assert_eq!(idle.register(V1), Some(0));
    }

    #[test]
    fn wait_event_stays_in_the_bios_until_a_ready_event_is_delivered() {
        let mut bios = BiosHle::default();
        let mut memory = Memory(vec![0; 16]);
        let (opened, _) = call(
            &mut bios,
            BiosVector::B0,
            0x08,
            [0x1234, 0x20, 0x2000, 0],
            &mut memory,
        )
        .unwrap();
        let handle = opened.register(V0).unwrap();
        call(
            &mut bios,
            BiosVector::B0,
            0x0c,
            [handle, 0, 0, 0],
            &mut memory,
        )
        .unwrap();

        let (waiting, outcome) = call(
            &mut bios,
            BiosVector::B0,
            0x0a,
            [handle, 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(outcome.action, HleAction::Wait);
        assert_eq!(waiting.pc, 0x1000);

        assert_eq!(bios.signal_event(0x1234, 0x20).unwrap(), 1);
        let (resumed, outcome) = call(
            &mut bios,
            BiosVector::B0,
            0x0a,
            [handle, 0, 0, 0],
            &mut memory,
        )
        .unwrap();
        assert_eq!(outcome.action, HleAction::Return);
        assert_eq!(resumed.pc, 0x2000);
        assert_eq!(resumed.register(V0), Some(1));
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
        context.set_register(V0, 0xfeed_beef);
        bios.dispatch_syscall(2, &mut context).unwrap();
        assert!(bios.interrupts_enabled());
        assert_eq!(context.register(V0), Some(0xfeed_beef));
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

        for (function, priority) in [(0x00, 1), (0x01, 0), (0x0c, 3)] {
            context.pc = 0x00c0;
            context.set_register(RA, 0x500);
            context.set_register(T1, function);
            context.set_register(A0, priority);
            bios.dispatch(BiosVector::C0, &mut context, &mut memory)
                .unwrap();
        }
        assert_eq!(bios.kernel_handlers.timer_and_vblank, Some(1));
        assert_eq!(bios.kernel_handlers.syscall, Some(0));
        assert_eq!(bios.kernel_handlers.default_interrupt, Some(3));

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
        for broken_cop1_function in [0x0b, 0x32] {
            assert!(matches!(
                call(
                    &mut bios,
                    BiosVector::A0,
                    broken_cop1_function,
                    [0; 4],
                    &mut memory
                ),
                Err(BiosError::UnsupportedCall { .. })
            ));
        }
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
