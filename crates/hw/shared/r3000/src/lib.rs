// SPDX-License-Identifier: LGPL-2.1-or-later
//! A standalone MIPS-I interpreter for R3000-family processors.
//!
//! The consumer owns address translation and physical devices through [`Bus`].
//! The core models one architectural branch slot and one load-delay slot.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use thiserror::Error;

const CAUSE_BD: u32 = 1 << 31;
const CAUSE_IP2: u32 = 1 << 10;
const STATUS_BEV: u32 = 1 << 22;

/// The type of a failing bus transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessType {
    /// Instruction fetch.
    Instruction,
    /// Data read.
    Load,
    /// Data write.
    Store,
}

/// A machine-supplied bus failure that is not an architectural alignment trap.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct BusFault {
    message: String,
}

impl BusFault {
    /// Constructs a bus diagnostic.
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

/// Consumer-owned memory and interrupt interface.
pub trait Bus {
    /// Reads one byte from a guest virtual address.
    ///
    /// # Errors
    ///
    /// Returns [`BusFault`] when the machine cannot complete the access.
    fn read_u8(&mut self, address: u32) -> Result<u8, BusFault>;

    /// Reads a little-endian halfword from a guest virtual address.
    ///
    /// # Errors
    ///
    /// Returns [`BusFault`] when the machine cannot complete the access.
    fn read_u16(&mut self, address: u32) -> Result<u16, BusFault>;

    /// Reads a little-endian word from a guest virtual address.
    ///
    /// # Errors
    ///
    /// Returns [`BusFault`] when the machine cannot complete the access.
    fn read_u32(&mut self, address: u32) -> Result<u32, BusFault>;

    /// Writes one byte to a guest virtual address.
    ///
    /// # Errors
    ///
    /// Returns [`BusFault`] when the machine cannot complete the access.
    fn write_u8(&mut self, address: u32, value: u8) -> Result<(), BusFault>;

    /// Writes a little-endian halfword to a guest virtual address.
    ///
    /// # Errors
    ///
    /// Returns [`BusFault`] when the machine cannot complete the access.
    fn write_u16(&mut self, address: u32, value: u16) -> Result<(), BusFault>;

    /// Writes a little-endian word to a guest virtual address.
    ///
    /// # Errors
    ///
    /// Returns [`BusFault`] when the machine cannot complete the access.
    fn write_u32(&mut self, address: u32, value: u32) -> Result<(), BusFault>;

    /// Reports the machine's external interrupt request line.
    fn interrupt_pending(&self) -> bool;
}

/// Reset and exception-vector policy supplied by a machine profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResetProfile {
    /// Initial program counter.
    pub pc: u32,
    /// General exception vector when bootstrap vectors are disabled.
    pub exception_vector: u32,
    /// General exception vector when the COP0 `BEV` bit is set.
    pub bootstrap_exception_vector: u32,
    /// Initial COP0 status value.
    pub status: u32,
    /// Processor revision identifier returned by COP0 register 15.
    pub processor_id: u32,
}

impl Default for ResetProfile {
    fn default() -> Self {
        Self {
            pc: 0xbfc0_0000,
            exception_vector: 0x8000_0080,
            bootstrap_exception_vector: 0xbfc0_0180,
            status: STATUS_BEV,
            processor_id: 0x0000_0002,
        }
    }
}

/// Visibility of a completed load to the immediately following instruction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LoadDelayMode {
    /// Preserve the R3000A architectural load-delay slot.
    #[default]
    Architectural,
    /// Make the loaded value visible after the load instruction completes.
    ///
    /// This models the interlocked behavior assumed by some emulator-oriented
    /// PSF driver executables while leaving the standalone CPU hardware-accurate
    /// by default.
    Interlocked,
}

/// Implemented architectural exception classes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Exception {
    /// Enabled interrupt request.
    Interrupt,
    /// Misaligned instruction or data load address.
    AddressLoad,
    /// Misaligned data store address.
    AddressStore,
    /// `syscall` instruction.
    Syscall,
    /// `break` instruction.
    Break,
    /// Unsupported or invalid instruction encoding.
    ReservedInstruction,
    /// Access to an unusable coprocessor.
    CoprocessorUnusable,
    /// Signed add/subtract overflow.
    Overflow,
}

impl Exception {
    const fn code(self) -> u32 {
        match self {
            Self::Interrupt => 0,
            Self::AddressLoad => 4,
            Self::AddressStore => 5,
            Self::Syscall => 8,
            Self::Break => 9,
            Self::ReservedInstruction => 10,
            Self::CoprocessorUnusable => 11,
            Self::Overflow => 12,
        }
    }
}

/// COP0 state exposed for machine diagnostics and tested HLE setup.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cop0 {
    /// Virtual address associated with the latest address exception.
    pub bad_vaddr: u32,
    /// Status register.
    pub status: u32,
    /// Cause register.
    pub cause: u32,
    /// Exception program counter.
    pub epc: u32,
    /// Processor revision identifier.
    pub processor_id: u32,
}

impl Cop0 {
    fn read(self, index: usize) -> Option<u32> {
        match index {
            8 => Some(self.bad_vaddr),
            12 => Some(self.status),
            13 => Some(self.cause),
            14 => Some(self.epc),
            15 => Some(self.processor_id),
            _ => None,
        }
    }

    fn write(&mut self, index: usize, value: u32) -> bool {
        match index {
            8 => self.bad_vaddr = value,
            12 => self.status = value,
            13 => self.cause = (self.cause & !0x300) | (value & 0x300),
            14 => self.epc = value,
            _ => return false,
        }
        true
    }
}

/// Observable event produced by one interpreter step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepEvent {
    /// One instruction completed normally.
    Instruction,
    /// The CPU entered an exception vector.
    Exception(Exception),
}

/// Result of one bounded interpreter step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StepOutcome {
    /// Address at which the instruction was fetched or the exception recognized.
    pub pc: u32,
    /// Fetched instruction, or `None` for a pre-fetch interrupt/alignment trap.
    pub instruction: Option<u32>,
    /// Explicit nominal cycle charge.
    pub cycles: u32,
    /// Architectural event.
    pub event: StepEvent,
}

/// Host-visible interpreter failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("bus {access:?} failure at {address:#010x}, PC {pc:#010x}: {source}")]
pub struct CpuError {
    /// Current instruction address.
    pub pc: u32,
    /// Failing guest address.
    pub address: u32,
    /// Access type.
    pub access: AccessType,
    /// Machine bus diagnostic.
    pub source: BusFault,
}

/// Complete movable R3000 architectural state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cpu {
    registers: [u32; 32],
    hi: u32,
    lo: u32,
    pc: u32,
    next_pc: u32,
    delay_branch_pc: Option<u32>,
    pending_load: Option<(usize, u32)>,
    cop0: Cop0,
    profile: ResetProfile,
    load_delay_mode: LoadDelayMode,
}

impl Cpu {
    /// Constructs reset state for a machine profile.
    #[must_use]
    pub fn new(profile: ResetProfile) -> Self {
        Self::with_load_delay_mode(profile, LoadDelayMode::Architectural)
    }

    /// Constructs reset state with an explicit load-delay policy.
    #[must_use]
    pub fn with_load_delay_mode(profile: ResetProfile, load_delay_mode: LoadDelayMode) -> Self {
        Self {
            registers: [0; 32],
            hi: 0,
            lo: 0,
            pc: profile.pc,
            next_pc: profile.pc.wrapping_add(4),
            delay_branch_pc: None,
            pending_load: None,
            cop0: Cop0 {
                status: profile.status,
                processor_id: profile.processor_id,
                ..Cop0::default()
            },
            profile,
            load_delay_mode,
        }
    }

    /// Restores architectural reset state.
    pub fn reset(&mut self) {
        *self = Self::with_load_delay_mode(self.profile, self.load_delay_mode);
    }

    /// Returns a general-purpose register; register zero is always zero.
    #[must_use]
    pub fn register(&self, index: usize) -> Option<u32> {
        self.registers.get(index).copied()
    }

    /// Sets a general-purpose register for machine initialization.
    ///
    /// Writes to register zero are ignored. An out-of-range index returns false.
    pub fn set_register(&mut self, index: usize, value: u32) -> bool {
        if index >= self.registers.len() {
            return false;
        }
        self.write_register(index, value);
        true
    }

    /// Returns the current instruction address.
    #[must_use]
    pub const fn pc(&self) -> u32 {
        self.pc
    }

    /// Sets the current instruction address and clears pipeline delays.
    pub fn set_pc(&mut self, pc: u32) {
        self.pc = pc;
        self.next_pc = pc.wrapping_add(4);
        self.delay_branch_pc = None;
        self.pending_load = None;
    }

    /// Returns the HI multiply/divide register.
    #[must_use]
    pub const fn hi(&self) -> u32 {
        self.hi
    }

    /// Returns the LO multiply/divide register.
    #[must_use]
    pub const fn lo(&self) -> u32 {
        self.lo
    }

    /// Replaces the multiply/divide result registers for an HLE context switch.
    pub fn set_hi_lo(&mut self, hi: u32, lo: u32) {
        self.hi = hi;
        self.lo = lo;
    }

    /// Returns COP0 state.
    #[must_use]
    pub const fn cop0(&self) -> &Cop0 {
        &self.cop0
    }

    /// Returns mutable COP0 state for machine reset/HLE initialization.
    #[must_use]
    pub const fn cop0_mut(&mut self) -> &mut Cop0 {
        &mut self.cop0
    }

    /// Executes at most one instruction or recognizes one interrupt.
    ///
    /// # Errors
    ///
    /// Returns [`CpuError`] only for a machine bus failure. Architectural traps
    /// enter the configured exception vector and are returned in [`StepOutcome`].
    pub fn step<B: Bus>(&mut self, bus: &mut B) -> Result<StepOutcome, CpuError> {
        self.step_inner(bus, true)
    }

    /// Executes one instruction without sampling the external interrupt line.
    ///
    /// Machine profiles that route interrupts through HLE can use this entry
    /// point to avoid redundant COP0 interrupt checks on every instruction.
    /// Software interrupts and all instruction-raised exceptions remain
    /// architectural.
    ///
    /// # Errors
    ///
    /// Returns [`CpuError`] only for a machine bus failure. Architectural traps
    /// enter the configured exception vector and are returned in [`StepOutcome`].
    pub fn step_without_external_interrupts<B: Bus>(
        &mut self,
        bus: &mut B,
    ) -> Result<StepOutcome, CpuError> {
        self.step_inner(bus, false)
    }

    fn step_inner<B: Bus>(
        &mut self,
        bus: &mut B,
        sample_external_interrupt: bool,
    ) -> Result<StepOutcome, CpuError> {
        if sample_external_interrupt {
            self.update_interrupt_line(bus.interrupt_pending());
        }
        let current_pc = self.pc;
        let delay_branch_pc = self.delay_branch_pc;
        if sample_external_interrupt && self.interrupt_enabled() {
            self.commit_old_load(None);
            self.enter_exception(Exception::Interrupt, current_pc, delay_branch_pc, None);
            return Ok(StepOutcome {
                pc: current_pc,
                instruction: None,
                cycles: 1,
                event: StepEvent::Exception(Exception::Interrupt),
            });
        }
        if current_pc & 3 != 0 {
            self.commit_old_load(None);
            self.enter_exception(
                Exception::AddressLoad,
                current_pc,
                delay_branch_pc,
                Some(current_pc),
            );
            return Ok(StepOutcome {
                pc: current_pc,
                instruction: None,
                cycles: 1,
                event: StepEvent::Exception(Exception::AddressLoad),
            });
        }
        let instruction = bus.read_u32(current_pc).map_err(|source| CpuError {
            pc: current_pc,
            address: current_pc,
            access: AccessType::Instruction,
            source,
        })?;
        let execution = self.execute(bus, current_pc, instruction)?;
        if let Some(exception) = execution.exception {
            self.commit_old_load(None);
            self.enter_exception(exception, current_pc, delay_branch_pc, execution.bad_vaddr);
            return Ok(StepOutcome {
                pc: current_pc,
                instruction: Some(instruction),
                cycles: execution.cycles,
                event: StepEvent::Exception(exception),
            });
        }

        self.commit_old_load(execution.register_write);
        match self.load_delay_mode {
            LoadDelayMode::Architectural => self.pending_load = execution.delayed_write,
            LoadDelayMode::Interlocked => {
                if let Some((register, value)) = execution.delayed_write {
                    self.write_register(register, value);
                }
                self.pending_load = None;
            }
        }
        let sequential = self.next_pc;
        self.pc = sequential;
        self.next_pc = execution
            .branch_target
            .unwrap_or_else(|| sequential.wrapping_add(4));
        self.delay_branch_pc = execution.is_branch.then_some(current_pc);
        self.registers[0] = 0;
        Ok(StepOutcome {
            pc: current_pc,
            instruction: Some(instruction),
            cycles: execution.cycles,
            event: StepEvent::Instruction,
        })
    }

    fn execute<B: Bus>(
        &mut self,
        bus: &mut B,
        pc: u32,
        instruction: u32,
    ) -> Result<Execution, CpuError> {
        let opcode = instruction >> 26;
        let rs = ((instruction >> 21) & 31) as usize;
        let rt = ((instruction >> 16) & 31) as usize;
        let rd = ((instruction >> 11) & 31) as usize;
        let shift = (instruction >> 6) & 31;
        let function = instruction & 63;
        let immediate = instruction as u16;
        let signed_immediate = i16::from_ne_bytes(immediate.to_ne_bytes());
        let left = self.registers[rs];
        let right = self.registers[rt];
        let mut result = Execution::default();

        match opcode {
            0x00 => match function {
                0x00 => result.write(rd, right << shift),
                0x02 => result.write(rd, right >> shift),
                0x03 => result.write(rd, from_i32(to_i32(right) >> shift)),
                0x04 => result.write(rd, right << (left & 31)),
                0x06 => result.write(rd, right >> (left & 31)),
                0x07 => result.write(rd, from_i32(to_i32(right) >> (left & 31))),
                0x08 => result.branch(left),
                0x09 => {
                    result.write(rd, pc.wrapping_add(8));
                    result.branch(left);
                }
                0x0c => result.exception = Some(Exception::Syscall),
                0x0d => result.exception = Some(Exception::Break),
                0x10 => result.write(rd, self.hi),
                0x11 => self.hi = left,
                0x12 => result.write(rd, self.lo),
                0x13 => self.lo = left,
                0x18 => {
                    let product = i64::from(to_i32(left)) * i64::from(to_i32(right));
                    self.lo = product as u32;
                    self.hi = (product >> 32) as u32;
                    result.cycles = 6;
                }
                0x19 => {
                    let product = u64::from(left) * u64::from(right);
                    self.lo = product as u32;
                    self.hi = (product >> 32) as u32;
                    result.cycles = 6;
                }
                0x1a => {
                    signed_divide(left, right, &mut self.hi, &mut self.lo);
                    result.cycles = 10;
                }
                0x1b => {
                    if right == 0 {
                        self.hi = left;
                        self.lo = u32::MAX;
                    } else {
                        self.hi = left % right;
                        self.lo = left / right;
                    }
                    result.cycles = 10;
                }
                0x20 => match to_i32(left).checked_add(to_i32(right)) {
                    Some(value) => result.write(rd, from_i32(value)),
                    None => result.exception = Some(Exception::Overflow),
                },
                0x21 => result.write(rd, left.wrapping_add(right)),
                0x22 => match to_i32(left).checked_sub(to_i32(right)) {
                    Some(value) => result.write(rd, from_i32(value)),
                    None => result.exception = Some(Exception::Overflow),
                },
                0x23 => result.write(rd, left.wrapping_sub(right)),
                0x24 => result.write(rd, left & right),
                0x25 => result.write(rd, left | right),
                0x26 => result.write(rd, left ^ right),
                0x27 => result.write(rd, !(left | right)),
                0x2a => result.write(rd, u32::from(to_i32(left) < to_i32(right))),
                0x2b => result.write(rd, u32::from(left < right)),
                _ => result.exception = Some(Exception::ReservedInstruction),
            },
            0x01 => {
                let condition = match rt {
                    0x00 => to_i32(left) < 0,
                    0x01 => to_i32(left) >= 0,
                    0x10 => {
                        result.write(31, pc.wrapping_add(8));
                        to_i32(left) < 0
                    }
                    0x11 => {
                        result.write(31, pc.wrapping_add(8));
                        to_i32(left) >= 0
                    }
                    _ => {
                        result.exception = Some(Exception::ReservedInstruction);
                        false
                    }
                };
                if result.exception.is_none() {
                    result.conditional_branch(condition, pc, signed_immediate);
                }
            }
            0x02 => result.branch(jump_target(pc, instruction)),
            0x03 => {
                result.write(31, pc.wrapping_add(8));
                result.branch(jump_target(pc, instruction));
            }
            0x04 => result.conditional_branch(left == right, pc, signed_immediate),
            0x05 => result.conditional_branch(left != right, pc, signed_immediate),
            0x06 => result.conditional_branch(to_i32(left) <= 0, pc, signed_immediate),
            0x07 => result.conditional_branch(to_i32(left) > 0, pc, signed_immediate),
            0x08 => match to_i32(left).checked_add(i32::from(signed_immediate)) {
                Some(value) => result.write(rt, from_i32(value)),
                None => result.exception = Some(Exception::Overflow),
            },
            0x09 => result.write(rt, left.wrapping_add_signed(i32::from(signed_immediate))),
            0x0a => result.write(rt, u32::from(to_i32(left) < i32::from(signed_immediate))),
            0x0b => result.write(rt, u32::from(left < sign_extend(signed_immediate))),
            0x0c => result.write(rt, left & u32::from(immediate)),
            0x0d => result.write(rt, left | u32::from(immediate)),
            0x0e => result.write(rt, left ^ u32::from(immediate)),
            0x0f => result.write(rt, u32::from(immediate) << 16),
            0x10 => self.execute_cop0(&mut result, rs, rt, rd, function),
            0x11..=0x13 => result.exception = Some(Exception::CoprocessorUnusable),
            0x20 => {
                let address = left.wrapping_add_signed(i32::from(signed_immediate));
                let value = Self::load_u8(bus, pc, address)?;
                result.load(rt, sign_extend(i8::from_ne_bytes([value]).into()));
            }
            0x21 => {
                let address = left.wrapping_add_signed(i32::from(signed_immediate));
                if address & 1 != 0 {
                    result.address_exception(Exception::AddressLoad, address);
                } else {
                    let value = Self::load_u16(bus, pc, address)?;
                    result.load(rt, sign_extend(i16::from_ne_bytes(value.to_ne_bytes())));
                }
            }
            0x22 => {
                let address = left.wrapping_add_signed(i32::from(signed_immediate));
                let word = Self::load_u32(bus, pc, address & !3)?;
                let base = self.merge_base(rt);
                let value = match address & 3 {
                    0 => (base & 0x00ff_ffff) | (word << 24),
                    1 => (base & 0x0000_ffff) | (word << 16),
                    2 => (base & 0x0000_00ff) | (word << 8),
                    _ => word,
                };
                result.load(rt, value);
            }
            0x23 => {
                let address = left.wrapping_add_signed(i32::from(signed_immediate));
                if address & 3 != 0 {
                    result.address_exception(Exception::AddressLoad, address);
                } else {
                    result.load(rt, Self::load_u32(bus, pc, address)?);
                }
            }
            0x24 => {
                let address = left.wrapping_add_signed(i32::from(signed_immediate));
                result.load(rt, u32::from(Self::load_u8(bus, pc, address)?));
            }
            0x25 => {
                let address = left.wrapping_add_signed(i32::from(signed_immediate));
                if address & 1 != 0 {
                    result.address_exception(Exception::AddressLoad, address);
                } else {
                    result.load(rt, u32::from(Self::load_u16(bus, pc, address)?));
                }
            }
            0x26 => {
                let address = left.wrapping_add_signed(i32::from(signed_immediate));
                let word = Self::load_u32(bus, pc, address & !3)?;
                let base = self.merge_base(rt);
                let value = match address & 3 {
                    0 => word,
                    1 => (base & 0xff00_0000) | (word >> 8),
                    2 => (base & 0xffff_0000) | (word >> 16),
                    _ => (base & 0xffff_ff00) | (word >> 24),
                };
                result.load(rt, value);
            }
            0x28 => {
                let address = left.wrapping_add_signed(i32::from(signed_immediate));
                Self::store_u8(bus, pc, address, right as u8)?;
                result.cycles = 2;
            }
            0x29 => {
                let address = left.wrapping_add_signed(i32::from(signed_immediate));
                if address & 1 != 0 {
                    result.address_exception(Exception::AddressStore, address);
                } else {
                    Self::store_u16(bus, pc, address, right as u16)?;
                    result.cycles = 2;
                }
            }
            0x2a => {
                let address = left.wrapping_add_signed(i32::from(signed_immediate));
                let aligned = address & !3;
                let old = Self::load_u32(bus, pc, aligned)?;
                let value = match address & 3 {
                    0 => (old & 0xffff_ff00) | (right >> 24),
                    1 => (old & 0xffff_0000) | (right >> 16),
                    2 => (old & 0xff00_0000) | (right >> 8),
                    _ => right,
                };
                Self::store_u32(bus, pc, aligned, value)?;
                result.cycles = 2;
            }
            0x2b => {
                let address = left.wrapping_add_signed(i32::from(signed_immediate));
                if address & 3 != 0 {
                    result.address_exception(Exception::AddressStore, address);
                } else {
                    Self::store_u32(bus, pc, address, right)?;
                    result.cycles = 2;
                }
            }
            0x2e => {
                let address = left.wrapping_add_signed(i32::from(signed_immediate));
                let aligned = address & !3;
                let old = Self::load_u32(bus, pc, aligned)?;
                let value = match address & 3 {
                    0 => right,
                    1 => (old & 0x0000_00ff) | (right << 8),
                    2 => (old & 0x0000_ffff) | (right << 16),
                    _ => (old & 0x00ff_ffff) | (right << 24),
                };
                Self::store_u32(bus, pc, aligned, value)?;
                result.cycles = 2;
            }
            0x30..=0x33 | 0x38..=0x3b => {
                result.exception = Some(Exception::CoprocessorUnusable);
            }
            _ => result.exception = Some(Exception::ReservedInstruction),
        }
        Ok(result)
    }

    fn execute_cop0(
        &mut self,
        result: &mut Execution,
        rs: usize,
        rt: usize,
        rd: usize,
        function: u32,
    ) {
        match rs {
            0x00 => match self.cop0.read(rd) {
                Some(value) => result.load(rt, value),
                None => result.exception = Some(Exception::ReservedInstruction),
            },
            0x04 => {
                if !self.cop0.write(rd, self.registers[rt]) {
                    result.exception = Some(Exception::ReservedInstruction);
                }
            }
            0x10 if function == 0x10 => {
                self.cop0.status = (self.cop0.status & !0x0f) | ((self.cop0.status >> 2) & 0x0f);
            }
            _ => result.exception = Some(Exception::ReservedInstruction),
        }
    }

    fn merge_base(&self, register: usize) -> u32 {
        self.pending_load
            .filter(|(pending, _)| *pending == register)
            .map_or(self.registers[register], |(_, value)| value)
    }

    fn commit_old_load(&mut self, current_write: Option<(usize, u32)>) {
        if let Some((register, value)) = self.pending_load.take() {
            self.write_register(register, value);
        }
        if let Some((register, value)) = current_write {
            self.write_register(register, value);
        }
    }

    fn write_register(&mut self, index: usize, value: u32) {
        if index != 0 {
            self.registers[index] = value;
        }
    }

    fn update_interrupt_line(&mut self, pending: bool) {
        if pending {
            self.cop0.cause |= CAUSE_IP2;
        } else {
            self.cop0.cause &= !CAUSE_IP2;
        }
    }

    fn interrupt_enabled(&self) -> bool {
        self.cop0.status & 1 != 0 && self.cop0.status & self.cop0.cause & 0xff00 != 0
    }

    fn enter_exception(
        &mut self,
        exception: Exception,
        pc: u32,
        delay_branch_pc: Option<u32>,
        bad_vaddr: Option<u32>,
    ) {
        self.cop0.cause = (self.cop0.cause & !(0x7c | CAUSE_BD)) | (exception.code() << 2);
        if let Some(branch_pc) = delay_branch_pc {
            self.cop0.cause |= CAUSE_BD;
            self.cop0.epc = branch_pc;
        } else {
            self.cop0.epc = pc;
        }
        if let Some(address) = bad_vaddr {
            self.cop0.bad_vaddr = address;
        }
        self.cop0.status = (self.cop0.status & !0x3f) | ((self.cop0.status << 2) & 0x3f);
        let vector = if self.cop0.status & STATUS_BEV != 0 {
            self.profile.bootstrap_exception_vector
        } else {
            self.profile.exception_vector
        };
        self.pc = vector;
        self.next_pc = vector.wrapping_add(4);
        self.delay_branch_pc = None;
        self.pending_load = None;
    }

    fn load_u8<B: Bus>(bus: &mut B, pc: u32, address: u32) -> Result<u8, CpuError> {
        bus.read_u8(address).map_err(|source| CpuError {
            pc,
            address,
            access: AccessType::Load,
            source,
        })
    }

    fn load_u16<B: Bus>(bus: &mut B, pc: u32, address: u32) -> Result<u16, CpuError> {
        bus.read_u16(address).map_err(|source| CpuError {
            pc,
            address,
            access: AccessType::Load,
            source,
        })
    }

    fn load_u32<B: Bus>(bus: &mut B, pc: u32, address: u32) -> Result<u32, CpuError> {
        bus.read_u32(address).map_err(|source| CpuError {
            pc,
            address,
            access: AccessType::Load,
            source,
        })
    }

    fn store_u8<B: Bus>(bus: &mut B, pc: u32, address: u32, value: u8) -> Result<(), CpuError> {
        bus.write_u8(address, value).map_err(|source| CpuError {
            pc,
            address,
            access: AccessType::Store,
            source,
        })
    }

    fn store_u16<B: Bus>(bus: &mut B, pc: u32, address: u32, value: u16) -> Result<(), CpuError> {
        bus.write_u16(address, value).map_err(|source| CpuError {
            pc,
            address,
            access: AccessType::Store,
            source,
        })
    }

    fn store_u32<B: Bus>(bus: &mut B, pc: u32, address: u32, value: u32) -> Result<(), CpuError> {
        bus.write_u32(address, value).map_err(|source| CpuError {
            pc,
            address,
            access: AccessType::Store,
            source,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct Execution {
    register_write: Option<(usize, u32)>,
    delayed_write: Option<(usize, u32)>,
    branch_target: Option<u32>,
    is_branch: bool,
    exception: Option<Exception>,
    bad_vaddr: Option<u32>,
    cycles: u32,
}

impl Default for Execution {
    fn default() -> Self {
        Self {
            register_write: None,
            delayed_write: None,
            branch_target: None,
            is_branch: false,
            exception: None,
            bad_vaddr: None,
            cycles: 1,
        }
    }
}

impl Execution {
    fn write(&mut self, register: usize, value: u32) {
        self.register_write = Some((register, value));
    }

    fn load(&mut self, register: usize, value: u32) {
        self.delayed_write = Some((register, value));
        self.cycles = 2;
    }

    fn branch(&mut self, target: u32) {
        self.is_branch = true;
        self.branch_target = Some(target);
    }

    fn conditional_branch(&mut self, condition: bool, pc: u32, immediate: i16) {
        self.is_branch = true;
        self.branch_target = Some(if condition {
            pc.wrapping_add(4)
                .wrapping_add_signed(i32::from(immediate) << 2)
        } else {
            pc.wrapping_add(8)
        });
    }

    fn address_exception(&mut self, exception: Exception, address: u32) {
        self.exception = Some(exception);
        self.bad_vaddr = Some(address);
    }
}

fn jump_target(pc: u32, instruction: u32) -> u32 {
    (pc.wrapping_add(4) & 0xf000_0000) | ((instruction & 0x03ff_ffff) << 2)
}

fn sign_extend(value: i16) -> u32 {
    from_i32(i32::from(value))
}

fn to_i32(value: u32) -> i32 {
    i32::from_ne_bytes(value.to_ne_bytes())
}

fn from_i32(value: i32) -> u32 {
    u32::from_ne_bytes(value.to_ne_bytes())
}

fn signed_divide(numerator: u32, denominator: u32, hi: &mut u32, lo: &mut u32) {
    let numerator = to_i32(numerator);
    let denominator = to_i32(denominator);
    if denominator == 0 {
        *hi = from_i32(numerator);
        *lo = if numerator >= 0 { u32::MAX } else { 1 };
    } else if numerator == i32::MIN && denominator == -1 {
        *hi = 0;
        *lo = from_i32(i32::MIN);
    } else {
        *hi = from_i32(numerator % denominator);
        *lo = from_i32(numerator / denominator);
    }
}

/// One trace record available under the `trace` feature.
#[cfg(feature = "trace")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TraceRecord {
    /// Step result.
    pub outcome: StepOutcome,
    /// Register state after the step.
    pub registers: [u32; 32],
    /// HI after the step.
    pub hi: u32,
    /// LO after the step.
    pub lo: u32,
}

/// Consumer trace hook available under the `trace` feature.
#[cfg(feature = "trace")]
pub trait TraceSink {
    /// Observes one completed step.
    fn trace(&mut self, record: TraceRecord);
}

#[cfg(feature = "trace")]
impl Cpu {
    /// Executes one step and synchronously reports its post-state.
    ///
    /// # Errors
    ///
    /// Returns [`CpuError`] under the same conditions as [`Cpu::step`].
    pub fn step_traced<B: Bus, T: TraceSink>(
        &mut self,
        bus: &mut B,
        sink: &mut T,
    ) -> Result<StepOutcome, CpuError> {
        let outcome = self.step(bus)?;
        sink.trace(TraceRecord {
            outcome,
            registers: self.registers,
            hi: self.hi,
            lo: self.lo,
        });
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Bus, BusFault, CAUSE_BD, CAUSE_IP2, Cpu, Exception, LoadDelayMode, ResetProfile, StepEvent,
    };

    struct TestBus {
        memory: Vec<u8>,
        irq: bool,
    }

    impl TestBus {
        fn new(words: &[u32]) -> Self {
            let mut memory = vec![0_u8; 4096];
            for (index, word) in words.iter().enumerate() {
                memory[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
            }
            Self { memory, irq: false }
        }

        fn range(&self, address: u32, bytes: usize) -> Result<std::ops::Range<usize>, BusFault> {
            let start = usize::try_from(address).map_err(|_| BusFault::new("address"))?;
            let end = start
                .checked_add(bytes)
                .filter(|end| *end <= self.memory.len())
                .ok_or_else(|| BusFault::new("out of range"))?;
            Ok(start..end)
        }
    }

    impl Bus for TestBus {
        fn read_u8(&mut self, address: u32) -> Result<u8, BusFault> {
            Ok(self.memory[self.range(address, 1)?.start])
        }

        fn read_u16(&mut self, address: u32) -> Result<u16, BusFault> {
            let range = self.range(address, 2)?;
            Ok(u16::from_le_bytes(
                self.memory[range].try_into().expect("length"),
            ))
        }

        fn read_u32(&mut self, address: u32) -> Result<u32, BusFault> {
            let range = self.range(address, 4)?;
            Ok(u32::from_le_bytes(
                self.memory[range].try_into().expect("length"),
            ))
        }

        fn write_u8(&mut self, address: u32, value: u8) -> Result<(), BusFault> {
            let index = self.range(address, 1)?.start;
            self.memory[index] = value;
            Ok(())
        }

        fn write_u16(&mut self, address: u32, value: u16) -> Result<(), BusFault> {
            let range = self.range(address, 2)?;
            self.memory[range].copy_from_slice(&value.to_le_bytes());
            Ok(())
        }

        fn write_u32(&mut self, address: u32, value: u32) -> Result<(), BusFault> {
            let range = self.range(address, 4)?;
            self.memory[range].copy_from_slice(&value.to_le_bytes());
            Ok(())
        }

        fn interrupt_pending(&self) -> bool {
            self.irq
        }
    }

    fn profile() -> ResetProfile {
        ResetProfile {
            pc: 0,
            exception_vector: 0x100,
            bootstrap_exception_vector: 0x180,
            status: 0,
            processor_id: 2,
        }
    }

    const fn i(op: u32, rs: u32, rt: u32, imm: u16) -> u32 {
        (op << 26) | (rs << 21) | (rt << 16) | imm as u32
    }

    const fn r(rs: u32, rt: u32, rd: u32, shift: u32, function: u32) -> u32 {
        (rs << 21) | (rt << 16) | (rd << 11) | (shift << 6) | function
    }

    #[test]
    fn arithmetic_logic_shift_and_zero_register_table() {
        let words = [
            i(0x09, 0, 1, 7),
            i(0x09, 0, 2, 3),
            r(1, 2, 3, 0, 0x21),
            r(1, 2, 4, 0, 0x23),
            r(1, 2, 5, 0, 0x24),
            r(1, 2, 6, 0, 0x25),
            r(1, 2, 7, 0, 0x2a),
            r(0, 1, 8, 2, 0x00),
            i(0x0f, 0, 9, 0x8000),
            r(0, 9, 10, 1, 0x03),
            i(0x09, 0, 0, 99),
        ];
        let mut bus = TestBus::new(&words);
        let mut cpu = Cpu::new(profile());
        for _ in words {
            cpu.step(&mut bus).unwrap();
        }
        assert_eq!(cpu.register(0), Some(0));
        assert_eq!(cpu.register(3), Some(10));
        assert_eq!(cpu.register(4), Some(4));
        assert_eq!(cpu.register(5), Some(3));
        assert_eq!(cpu.register(6), Some(7));
        assert_eq!(cpu.register(7), Some(0));
        assert_eq!(cpu.register(8), Some(28));
        assert_eq!(cpu.register(10), Some(0xc000_0000));
    }

    #[test]
    fn branch_delay_slot_and_link_address_are_architectural() {
        let words = [
            i(0x04, 0, 0, 2),
            i(0x09, 0, 1, 11),
            i(0x09, 0, 1, 99),
            0x0c00_0005,
            0,
            r(31, 0, 2, 0, 0x21),
        ];
        let mut bus = TestBus::new(&words);
        let mut cpu = Cpu::new(profile());
        let mut pcs = Vec::new();
        for _ in 0..5 {
            pcs.push(cpu.step(&mut bus).unwrap().pc);
        }
        assert_eq!(pcs, [0, 4, 12, 16, 20]);
        assert_eq!(cpu.register(1), Some(11));
        assert_eq!(cpu.register(2), Some(20));
        assert_eq!(cpu.register(31), Some(20));
    }

    #[test]
    fn branch_in_delay_slot_preserves_both_pipeline_targets() {
        let mut words = vec![0; 9];
        words[0] = i(0x04, 0, 0, 3);
        words[1] = 0x0800_0008;
        words[4] = i(0x09, 0, 1, 11);
        words[8] = i(0x09, 0, 2, 22);
        let mut bus = TestBus::new(&words);
        let mut cpu = Cpu::new(profile());
        let pcs = std::array::from_fn::<_, 4, _>(|_| cpu.step(&mut bus).unwrap().pc);
        assert_eq!(pcs, [0, 4, 16, 32]);
        assert_eq!(cpu.register(1), Some(11));
        assert_eq!(cpu.register(2), Some(22));
    }

    #[test]
    fn load_delay_exposes_old_value_for_exactly_one_instruction() {
        let words = [
            i(0x23, 0, 1, 0x100),
            r(1, 0, 2, 0, 0x21),
            r(1, 0, 3, 0, 0x21),
        ];
        let mut bus = TestBus::new(&words);
        bus.write_u32(0x100, 0x1234_5678).unwrap();
        let mut cpu = Cpu::new(profile());
        for _ in 0..3 {
            cpu.step(&mut bus).unwrap();
        }
        assert_eq!(cpu.register(1), Some(0x1234_5678));
        assert_eq!(cpu.register(2), Some(0));
        assert_eq!(cpu.register(3), Some(0x1234_5678));
    }

    #[test]
    fn interlocked_loads_are_visible_to_the_following_instruction() {
        let words = [i(0x23, 0, 1, 0x100), r(1, 0, 2, 0, 0x21)];
        let mut bus = TestBus::new(&words);
        bus.write_u32(0x100, 0x1234_5678).unwrap();
        let mut cpu = Cpu::with_load_delay_mode(profile(), LoadDelayMode::Interlocked);
        cpu.step(&mut bus).unwrap();
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.register(1), Some(0x1234_5678));
        assert_eq!(cpu.register(2), Some(0x1234_5678));
        cpu.reset();
        assert_eq!(cpu.register(1), Some(0));
    }

    #[test]
    fn exceptions_in_delay_slots_set_bd_epc_and_bad_address() {
        let words = [i(0x04, 0, 0, 1), i(0x23, 0, 1, 1), 0];
        let mut bus = TestBus::new(&words);
        let mut cpu = Cpu::new(profile());
        cpu.step(&mut bus).unwrap();
        let outcome = cpu.step(&mut bus).unwrap();
        assert_eq!(outcome.event, StepEvent::Exception(Exception::AddressLoad));
        assert_eq!(cpu.cop0().epc, 0);
        assert_ne!(cpu.cop0().cause & CAUSE_BD, 0);
        assert_eq!(cpu.cop0().bad_vaddr, 1);
        assert_eq!(cpu.pc(), 0x100);
    }

    #[test]
    fn interrupts_follow_status_mask_and_enter_before_fetch() {
        let mut bus = TestBus::new(&[0]);
        bus.irq = true;
        let mut cpu = Cpu::new(profile());
        cpu.cop0_mut().status = 1 | CAUSE_IP2;
        let outcome = cpu.step(&mut bus).unwrap();
        assert_eq!(outcome.instruction, None);
        assert_eq!(outcome.event, StepEvent::Exception(Exception::Interrupt));
        assert_eq!(cpu.cop0().epc, 0);
    }

    #[test]
    fn multiply_divide_and_signed_overflow_are_explicit() {
        let words = [
            r(1, 2, 0, 0, 0x18),
            r(0, 0, 3, 0, 0x12),
            r(1, 0, 0, 0, 0x1a),
            r(0, 0, 4, 0, 0x12),
            r(5, 6, 7, 0, 0x20),
        ];
        let mut bus = TestBus::new(&words);
        let mut cpu = Cpu::new(profile());
        cpu.set_register(1, u32::MAX);
        cpu.set_register(2, 2);
        cpu.set_register(5, 0x7fff_ffff);
        cpu.set_register(6, 1);
        assert_eq!(cpu.step(&mut bus).unwrap().cycles, 6);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.register(3), Some(u32::MAX - 1));
        assert_eq!(cpu.step(&mut bus).unwrap().cycles, 10);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.register(4), Some(1));
        assert_eq!(
            cpu.step(&mut bus).unwrap().event,
            StepEvent::Exception(Exception::Overflow)
        );
    }

    #[test]
    fn unsupported_instruction_traps_without_panicking() {
        let mut bus = TestBus::new(&[0xffff_ffff]);
        let mut cpu = Cpu::new(profile());
        let outcome = cpu.step(&mut bus).unwrap();
        assert_eq!(
            outcome.event,
            StepEvent::Exception(Exception::ReservedInstruction)
        );
        assert_eq!(cpu.cop0().epc, 0);
    }

    #[test]
    fn immediate_cop0_and_system_instruction_groups() {
        let words = [
            i(0x0d, 0, 1, 0xf0f0),
            i(0x0f, 0, 2, 0x1234),
            i(0x0c, 2, 3, 0x00ff),
            i(0x0e, 1, 4, 0xffff),
            i(0x0a, 1, 5, 0xffff),
            i(0x0b, 1, 6, 0xffff),
            (0x10 << 26) | (0x04 << 21) | (1 << 16) | (12 << 11),
            (0x10 << 26) | (12 << 16) | (12 << 11),
            0,
            0x4200_0010,
            0x0000_000c,
        ];
        let mut bus = TestBus::new(&words);
        let mut cpu = Cpu::new(profile());
        for _ in 0..10 {
            cpu.step(&mut bus).unwrap();
        }
        assert_eq!(cpu.register(1), Some(0xf0f0));
        assert_eq!(cpu.register(2), Some(0x1234_0000));
        assert_eq!(cpu.register(3), Some(0));
        assert_eq!(cpu.register(4), Some(0x0000_0f0f));
        assert_eq!(cpu.register(5), Some(0));
        assert_eq!(cpu.register(6), Some(1));
        assert_eq!(cpu.register(12), Some(0xf0f0));
        assert_eq!(cpu.cop0().status & 0x0f, (0xf0f0 >> 2) & 0x0f);
        assert_eq!(
            cpu.step(&mut bus).unwrap().event,
            StepEvent::Exception(Exception::Syscall)
        );
    }

    #[test]
    fn aligned_signed_unsigned_loads_and_stores_round_trip() {
        let words = [
            i(0x28, 1, 2, 0),
            i(0x29, 1, 3, 2),
            i(0x2b, 1, 4, 4),
            i(0x20, 1, 5, 0),
            0,
            i(0x24, 1, 6, 0),
            0,
            i(0x21, 1, 7, 2),
            0,
            i(0x25, 1, 8, 2),
            0,
            i(0x23, 1, 9, 4),
            0,
        ];
        let mut bus = TestBus::new(&words);
        let mut cpu = Cpu::new(profile());
        cpu.set_register(1, 0x100);
        cpu.set_register(2, 0x80);
        cpu.set_register(3, 0x8001);
        cpu.set_register(4, 0x89ab_cdef);
        for _ in words {
            cpu.step(&mut bus).unwrap();
        }
        assert_eq!(cpu.register(5), Some(0xffff_ff80));
        assert_eq!(cpu.register(6), Some(0x80));
        assert_eq!(cpu.register(7), Some(0xffff_8001));
        assert_eq!(cpu.register(8), Some(0x8001));
        assert_eq!(cpu.register(9), Some(0x89ab_cdef));
    }

    #[test]
    fn unaligned_word_load_and_store_lanes_are_little_endian() {
        let load_expectations = [0x11bb_ccdd, 0x2211_ccdd, 0x3322_11dd, 0x4433_2211];
        let right_expectations = [0x4433_2211, 0xaa44_3322, 0xaabb_4433, 0xaabb_cc44];
        for offset in 0_u16..4 {
            let words = [i(0x22, 1, 2, offset), 0, i(0x26, 1, 3, offset), 0];
            let mut bus = TestBus::new(&words);
            bus.write_u32(0x100, 0x4433_2211).unwrap();
            let mut cpu = Cpu::new(profile());
            cpu.set_register(1, 0x100);
            cpu.set_register(2, 0xaabb_ccdd);
            cpu.set_register(3, 0xaabb_ccdd);
            for _ in words {
                cpu.step(&mut bus).unwrap();
            }
            assert_eq!(
                cpu.register(2),
                Some(load_expectations[usize::from(offset)])
            );
            assert_eq!(
                cpu.register(3),
                Some(right_expectations[usize::from(offset)])
            );
        }

        let stored_left = [0x4433_22aa, 0x4433_aabb, 0x44aa_bbcc, 0xaabb_ccdd];
        let stored_right = [0xaabb_ccdd, 0xbbcc_dd11, 0xccdd_2211, 0xdd33_2211];
        for offset in 0_u16..4 {
            let words = [i(0x2a, 1, 2, offset)];
            let mut bus = TestBus::new(&words);
            bus.write_u32(0x100, 0x4433_2211).unwrap();
            let mut cpu = Cpu::new(profile());
            cpu.set_register(1, 0x100);
            cpu.set_register(2, 0xaabb_ccdd);
            cpu.step(&mut bus).unwrap();
            assert_eq!(
                bus.read_u32(0x100).unwrap(),
                stored_left[usize::from(offset)]
            );

            let words = [i(0x2e, 1, 2, offset)];
            let mut bus = TestBus::new(&words);
            bus.write_u32(0x100, 0x4433_2211).unwrap();
            let mut cpu = Cpu::new(profile());
            cpu.set_register(1, 0x100);
            cpu.set_register(2, 0xaabb_ccdd);
            cpu.step(&mut bus).unwrap();
            assert_eq!(
                bus.read_u32(0x100).unwrap(),
                stored_right[usize::from(offset)]
            );
        }
    }

    #[test]
    fn malformed_memory_operations_and_coprocessors_trap() {
        let cases = [
            (i(0x21, 0, 1, 1), Exception::AddressLoad),
            (i(0x23, 0, 1, 2), Exception::AddressLoad),
            (i(0x29, 0, 1, 1), Exception::AddressStore),
            (i(0x2b, 0, 1, 2), Exception::AddressStore),
            (0x4400_0000, Exception::CoprocessorUnusable),
            (0xc000_0000, Exception::CoprocessorUnusable),
            (0x0000_000d, Exception::Break),
        ];
        for (instruction, exception) in cases {
            let mut bus = TestBus::new(&[instruction]);
            let mut cpu = Cpu::new(profile());
            assert_eq!(
                cpu.step(&mut bus).unwrap().event,
                StepEvent::Exception(exception)
            );
        }
    }

    #[test]
    fn generated_alu_program_matches_independent_reference_model() {
        let mut state = 0x9e37_79b9_u32;
        let mut words = Vec::new();
        let mut expected = [0_u32; 32];
        expected[1] = 0x8765_4321;
        expected[2] = 0x1234_5678;
        for index in 0..1000_u32 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let destination = 3 + (index % 29);
            let source = 1 + (state % 31);
            let other = 1 + ((state >> 8) % 31);
            let selector = (state >> 16) % 8;
            let (instruction, value) = match selector {
                0 => (
                    r(source, other, destination, 0, 0x21),
                    expected[source as usize].wrapping_add(expected[other as usize]),
                ),
                1 => (
                    r(source, other, destination, 0, 0x23),
                    expected[source as usize].wrapping_sub(expected[other as usize]),
                ),
                2 => (
                    r(source, other, destination, 0, 0x24),
                    expected[source as usize] & expected[other as usize],
                ),
                3 => (
                    r(source, other, destination, 0, 0x25),
                    expected[source as usize] | expected[other as usize],
                ),
                4 => (
                    r(source, other, destination, 0, 0x26),
                    expected[source as usize] ^ expected[other as usize],
                ),
                5 => (
                    r(source, other, destination, 0, 0x27),
                    !(expected[source as usize] | expected[other as usize]),
                ),
                6 => (
                    r(source, other, destination, 0, 0x2a),
                    u32::from(
                        super::to_i32(expected[source as usize])
                            < super::to_i32(expected[other as usize]),
                    ),
                ),
                _ => (
                    r(source, other, destination, 0, 0x2b),
                    u32::from(expected[source as usize] < expected[other as usize]),
                ),
            };
            words.push(instruction);
            if destination != 0 {
                expected[destination as usize] = value;
            }
        }
        let mut bus = TestBus::new(&words);
        let mut cpu = Cpu::new(profile());
        cpu.set_register(1, expected_seed(1));
        cpu.set_register(2, expected_seed(2));

        // Recompute from the same seeds because `expected` now contains final state.
        let mut reference = [0_u32; 32];
        reference[1] = expected_seed(1);
        reference[2] = expected_seed(2);
        for instruction in &words {
            let source = ((instruction >> 21) & 31) as usize;
            let other = ((instruction >> 16) & 31) as usize;
            let destination = ((instruction >> 11) & 31) as usize;
            let value = match instruction & 63 {
                0x21 => reference[source].wrapping_add(reference[other]),
                0x23 => reference[source].wrapping_sub(reference[other]),
                0x24 => reference[source] & reference[other],
                0x25 => reference[source] | reference[other],
                0x26 => reference[source] ^ reference[other],
                0x27 => !(reference[source] | reference[other]),
                0x2a => {
                    u32::from(super::to_i32(reference[source]) < super::to_i32(reference[other]))
                }
                0x2b => u32::from(reference[source] < reference[other]),
                _ => unreachable!(),
            };
            if destination != 0 {
                reference[destination] = value;
            }
            cpu.step(&mut bus).unwrap();
        }
        for (index, value) in reference.into_iter().enumerate() {
            assert_eq!(cpu.register(index), Some(value), "register {index}");
        }
    }

    const fn expected_seed(register: usize) -> u32 {
        match register {
            1 => 0x8765_4321,
            2 => 0x1234_5678,
            _ => 0,
        }
    }

    #[cfg(feature = "trace")]
    #[test]
    fn trace_hook_observes_post_state() {
        use super::{TraceRecord, TraceSink};

        #[derive(Default)]
        struct Sink(Option<TraceRecord>);
        impl TraceSink for Sink {
            fn trace(&mut self, record: TraceRecord) {
                self.0 = Some(record);
            }
        }

        let mut bus = TestBus::new(&[i(0x09, 0, 1, 42)]);
        let mut cpu = Cpu::new(profile());
        let mut sink = Sink::default();
        cpu.step_traced(&mut bus, &mut sink).unwrap();
        assert_eq!(sink.0.unwrap().registers[1], 42);
    }
}
