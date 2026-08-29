// SPDX-License-Identifier: LGPL-2.1-or-later
//! End-to-end PSF1 machine composition with explicit device routing.

#![allow(clippy::too_many_lines)]

use std::collections::VecDeque;

use thiserror::Error;
use upse_clock::{ClockError, Deadline, RateConverter, Ticks};
use upse_ps1_bios::{
    BiosError, BiosHle, BiosVector, CpuContext, GuestMemory, GuestMemoryError, HleAction,
};
use upse_ps1_cdrom::{CDROM_BASE, CDROM_END, CdRom};
use upse_ps1_dma::{
    DICR, DMA_CHANNEL_END, DMA_CHANNEL_HALFWORD_END, DMA_CHANNEL_START, DMA_CONTROL_END, DPCR,
    DmaController, DmaError, InterruptSink as DmaInterruptSink,
};
use upse_ps1_gpu::{GP0, GP1, Gpu};
use upse_ps1_irq::{I_MASK, I_STAT, InterruptController, InterruptSource};
use upse_ps1_mdec::{MDEC_CONTROL_STATUS, MDEC_DATA, Mdec};
use upse_ps1_memory::{
    MEMORY_CONTROL_END, MEMORY_CONTROL_START, MemoryError, MemoryRegion, OpenBusPolicy, Ps1Memory,
};
use upse_ps1_spu::{
    InterruptSink as SpuInterruptSink, SAMPLE_RATE, SPU_BASE, SPU_END, Spu, SpuError,
};
use upse_ps1_timers::{
    CPU_HZ, ClockInput, InterruptSink as TimerInterruptSink, RootCounters, TIMER_BASE, TimerError,
    VBlankClock, VideoStandard,
};
use upse_psf::{Psf1LoadPlan, RefreshRate};
use upse_psx_exe::{ExecutableImage, ImageError};
use upse_r3000::{
    Bus, BusFault, Cpu, CpuError, DelaySlotBranchMode, Exception, LoadDelayMode, ResetProfile,
    StepEvent, WordAlignmentMode,
};
use upse_scheduler::{Scheduler, SchedulerError};

const TIMER_END: u32 = TIMER_BASE + 0x28;
const AUDIO_CHUNK_FRAMES: usize = 256;
const DEVICE_SYNC_CYCLES: u64 = 256;
const IDLE_ADVANCE_CYCLES: u32 = 256;
const EVENT_SPEC_INTERRUPTED: u32 = 0x0000_0002;
const EVENT_SPEC_COMMAND_COMPLETE: u32 = 0x0000_0020;
const EVENT_SPEC_INTERRUPT: u32 = 0x0000_1000;
const EVENT_CLASS_SPU: u32 = 0xf000_0009;
const AKAO_DRIVER_EVENT_RETURN: u32 = 0x8005_b0d0;
const AKAO_DRIVER_EVENT_HANDLE: u32 = 0xf100_0001;
const AKAO_DRIVER_ARGUMENT_PRELOAD: u32 = 0x8009_80bc;
const AKAO_DRIVER_SEQUENCE_CALL: u32 = 0x8009_80c0;
const AKAO_DRIVER_SEQUENCE_DELAY: u32 = 0x8009_80c4;

/// Machine construction and diagnostic policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MachineConfig {
    /// Unmapped memory handling outside modeled devices.
    pub open_bus: OpenBusPolicy,
    /// Retain an explicit device-order trace for tests and diagnostics.
    pub trace_events: bool,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            open_bus: OpenBusPolicy::Strict,
            trace_events: false,
        }
    }
}

/// Same-cycle device event recorded by optional tracing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MachineEvent {
    /// A device requested an interrupt source.
    Interrupt(InterruptSource),
    /// Scheduled sound DMA completed before audio at the same cycle.
    DmaComplete,
    /// Native SPU frames were generated.
    AudioFrames(u64),
}

/// Kind of execution performed by one machine step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MachineStepKind {
    /// One R3000 architectural event.
    Cpu(StepEvent),
    /// One BIOS HLE table call.
    Bios(BiosVector),
    /// One HLE kernel syscall.
    Syscall(u32),
    /// One deferred event callback context was restored.
    CallbackReturn,
    /// One comparator callback resumed or completed a libc HLE routine.
    LibcCallback,
    /// A null indirect call returned without modifying guest low memory.
    NullCall,
    /// Guest execution is halted while devices continue advancing.
    Halt,
    /// One default BIOS hardware interrupt handler returned from exception.
    Interrupt(InterruptSource),
    /// A side-effect-free guest idle loop advanced emulated time.
    Idle,
}

/// Observable result of one CPU/HLE boundary plus device advancement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MachineStep {
    /// Emulated CPU cycles consumed.
    pub cycles: u32,
    /// Execution path taken.
    pub kind: MachineStepKind,
}

/// End-to-end PSF1 machine failure.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum MachineError {
    /// PS-X EXE plan could not become an executable image.
    #[error("PSF1 executable image failure: {0}")]
    Image(#[from] ImageError),
    /// Initial RAM construction failed.
    #[error("PS1 memory initialization failure: {0}")]
    Memory(#[from] MemoryError),
    /// R3000 step failed on the composed bus.
    #[error("PS1 CPU failure: {0}")]
    Cpu(#[from] CpuError),
    /// BIOS HLE dispatch failed.
    #[error(
        "PS1 BIOS HLE {vector:?} call {function:#04x} (ra={ra:#010x}, a0={a0:#010x}, a1={a1:#010x}, a2={a2:#010x}, a3={a3:#010x}) failed: {source}"
    )]
    Bios {
        /// BIOS call-table vector.
        vector: BiosVector,
        /// Function number from `t1`.
        function: u8,
        /// Guest return address.
        ra: u32,
        /// Guest argument zero.
        a0: u32,
        /// Guest argument one.
        a1: u32,
        /// Guest argument two.
        a2: u32,
        /// Guest argument three.
        a3: u32,
        /// Structured HLE failure.
        source: BiosError,
    },
    /// Deferred BIOS callback state transition failed.
    #[error("PS1 BIOS HLE state failure: {0}")]
    BiosState(#[from] BiosError),
    /// The HLE callback sentinel had no matching saved CPU pipeline state.
    #[error("PS1 BIOS HLE callback CPU state is missing")]
    CallbackCpuState,
    /// Kernel syscall HLE dispatch failed.
    #[error("PS1 BIOS HLE syscall {number} at {pc:#010x} failed: {source}")]
    Syscall {
        /// Syscall number from `a0`.
        number: u32,
        /// Guest syscall instruction address.
        pc: u32,
        /// Structured HLE failure.
        source: BiosError,
    },
    /// Timer clock arithmetic failed.
    #[error("PS1 timer failure: {0}")]
    Timer(#[from] TimerError),
    /// DMA scheduling or transfer failed.
    #[error("PS1 DMA failure: {0}")]
    Dma(#[from] DmaError),
    /// SPU register or rendering failed.
    #[error("PS1 SPU failure: {0}")]
    Spu(#[from] SpuError),
    /// Machine time or sample conversion overflowed.
    #[error("PS1 machine clock overflow")]
    ClockOverflow,
    /// Interleaved output length does not match the requested frame count.
    #[error("machine output has {actual} samples, expected {expected}")]
    OutputSize {
        /// Required scalar sample count.
        expected: usize,
        /// Supplied scalar sample count.
        actual: usize,
    },
}

impl From<ClockError> for MachineError {
    fn from(_: ClockError) -> Self {
        Self::ClockOverflow
    }
}

impl From<SchedulerError> for MachineError {
    fn from(_: SchedulerError) -> Self {
        Self::Dma(DmaError::Scheduler)
    }
}

#[derive(Clone, Debug)]
struct MachineState {
    cpu: Cpu,
    memory: Ps1Memory,
    cdrom: CdRom,
    gpu: Gpu,
    mdec: Mdec,
    irq: InterruptController,
    timers: RootCounters,
    refresh: VBlankClock,
    dma: DmaController,
    bios: BiosHle,
    spu: Spu,
    scheduler: Scheduler,
    now: Deadline,
    sample_clock: RateConverter,
    deferred_device_ticks: u64,
    pending_audio: VecDeque<i16>,
    audio_buffer: [i16; AUDIO_CHUNK_FRAMES * 2],
    discard_audio_frames: u64,
    callback_cpu: Option<Cpu>,
    libc_cpu: Option<Cpu>,
    interrupt_cpu: Option<Cpu>,
    halted: bool,
    trace_events: bool,
    event_log: Vec<MachineEvent>,
}

/// Fully composed PSF1 machine with a reset snapshot.
#[derive(Clone, Debug)]
pub struct Ps1Machine {
    state: Box<MachineState>,
    reset: Box<MachineState>,
}

impl Ps1Machine {
    /// Applies a PSF1 load plan and constructs reset CPU/device state.
    ///
    /// # Errors
    ///
    /// Returns [`MachineError`] when executable mapping, memory construction,
    /// or clock initialization fails.
    pub fn from_plan(plan: &Psf1LoadPlan, config: MachineConfig) -> Result<Self, MachineError> {
        let image = ExecutableImage::from_plan(plan)?;
        let mut memory = Ps1Memory::from_image(&image, config.open_bus)?;
        let mut bios = BiosHle::default();
        bios.initialize_boot_memory(&mut BiosMemory(&mut memory))?;
        let standard = match image.refresh {
            RefreshRate::Hz50 => VideoStandard::Pal,
            RefreshRate::Hz60 => VideoStandard::Ntsc,
        };
        // PSF rip drivers are emulator-facing executables. Some intentionally
        // place consumers directly after loads and therefore require the
        // interlocked behavior historically supplied by PSF players.
        let mut cpu = Cpu::with_load_delay_mode(
            ResetProfile {
                pc: image.pc,
                exception_vector: 0x8000_0080,
                bootstrap_exception_vector: 0xbfc0_0180,
                status: 0,
                processor_id: 2,
            },
            LoadDelayMode::Interlocked,
        );
        // Some emulator-facing PSF rip drivers put their loop-exit jump in a
        // conditional branch's delay slot. The sequence is undefined on MIPS-I,
        // but requires the outer taken branch to retain control of the loop.
        cpu.set_delay_slot_branch_mode(DelaySlotBranchMode::SuppressWhenOuterTaken);
        // Some emulator-facing PSF rip drivers rely on word accesses being
        // silently aligned during startup.
        cpu.set_word_alignment_mode(WordAlignmentMode::AlignDown);
        cpu.set_register(29, image.sp);
        let state = Box::new(MachineState {
            cpu,
            memory,
            cdrom: CdRom::new(),
            gpu: Gpu::new(),
            mdec: Mdec::new(),
            irq: InterruptController::new(),
            timers: RootCounters::new(),
            refresh: VBlankClock::new(standard),
            dma: DmaController::new(),
            bios,
            spu: Spu::new(),
            scheduler: Scheduler::new(),
            now: Deadline::ZERO,
            sample_clock: RateConverter::new(CPU_HZ, u64::from(SAMPLE_RATE))?,
            deferred_device_ticks: 0,
            pending_audio: VecDeque::new(),
            audio_buffer: [0; AUDIO_CHUNK_FRAMES * 2],
            discard_audio_frames: 0,
            callback_cpu: None,
            libc_cpu: None,
            interrupt_cpu: None,
            halted: false,
            trace_events: config.trace_events,
            event_log: Vec::new(),
        });
        Ok(Self {
            reset: state.clone(),
            state,
        })
    }

    /// Restores the complete post-load snapshot without reparsing the module.
    pub fn reset(&mut self) {
        self.state = self.reset.clone();
    }

    /// Returns current emulated CPU time.
    #[must_use]
    pub const fn now(&self) -> Deadline {
        self.state.now
    }

    /// Returns the current program counter for diagnostics.
    #[must_use]
    pub fn pc(&self) -> u32 {
        self.state.cpu.pc()
    }

    /// Returns the selected refresh standard.
    #[must_use]
    pub const fn video_standard(&self) -> VideoStandard {
        self.state.refresh.standard()
    }

    /// Removes and returns the optional device-order trace.
    pub fn take_event_log(&mut self) -> Vec<MachineEvent> {
        std::mem::take(&mut self.state.event_log)
    }

    /// Executes one CPU, HLE, or callback-return boundary and advances devices.
    ///
    /// # Errors
    ///
    /// Returns [`MachineError`] for CPU, bus, HLE, device, or clock failure.
    pub fn step(&mut self) -> Result<MachineStep, MachineError> {
        self.step_inner(true)
    }

    fn step_inner(&mut self, synchronize_devices: bool) -> Result<MachineStep, MachineError> {
        if self.state.halted {
            self.advance_devices(IDLE_ADVANCE_CYCLES, true)?;
            return Ok(MachineStep {
                cycles: IDLE_ADVANCE_CYCLES,
                kind: MachineStepKind::Halt,
            });
        }
        if self.state.cpu.pc() == BiosHle::callback_return_pc() {
            if self.state.bios.libc_callback_active() {
                let mut context = cpu_context(&self.state.cpu);
                let outcome = {
                    let mut memory = BiosMemory(&mut self.state.memory);
                    self.state
                        .bios
                        .resume_libc_callback(&mut context, &mut memory)?
                };
                self.state.cpu = self
                    .state
                    .libc_cpu
                    .clone()
                    .ok_or(MachineError::CallbackCpuState)?;
                apply_context(&mut self.state.cpu, &context);
                if outcome.action != HleAction::Call {
                    self.state.libc_cpu = None;
                }
                self.advance_devices(outcome.cycles, synchronize_devices)?;
                return Ok(MachineStep {
                    cycles: outcome.cycles,
                    kind: MachineStepKind::LibcCallback,
                });
            }
            let mut context = cpu_context(&self.state.cpu);
            self.state.bios.return_from_callback(&mut context)?;
            self.state.cpu = self
                .state
                .callback_cpu
                .take()
                .ok_or(MachineError::CallbackCpuState)?;
            self.advance_devices(1, synchronize_devices)?;
            return Ok(MachineStep {
                cycles: 1,
                kind: MachineStepKind::CallbackReturn,
            });
        }
        if self.state.interrupt_cpu.is_none()
            && self.state.callback_cpu.is_none()
            && self.state.libc_cpu.is_none()
            && self.state.bios.interrupts_enabled()
            && self.state.irq.pending()
        {
            if let Some(source) = self.handle_default_interrupt()? {
                self.advance_devices(12, synchronize_devices)?;
                return Ok(MachineStep {
                    cycles: 12,
                    kind: MachineStepKind::Interrupt(source),
                });
            }
            if self.state.bios.interrupt_hook().is_some() {
                self.enter_interrupt_hook()?;
            }
        }
        if self.state.bios.interrupts_enabled()
            && self.state.interrupt_cpu.is_none()
            && !self.state.bios.callback_active()
            && let Some(callback) = self.state.bios.take_callback()
        {
            let saved_cpu = self.state.cpu.clone();
            let mut context = cpu_context(&self.state.cpu);
            self.state.bios.enter_callback(&mut context, callback)?;
            self.state.callback_cpu = Some(saved_cpu);
            apply_context(&mut self.state.cpu, &context);
        }
        if self.state.cpu.pc() == 0
            && self.state.memory.read_u32(0)? == 0
            && self.state.memory.read_u32(4)? == 0
        {
            let return_pc = self.state.cpu.register(31).unwrap_or(0);
            self.state.cpu.set_pc(return_pc);
            self.advance_devices(2, synchronize_devices)?;
            return Ok(MachineStep {
                cycles: 2,
                kind: MachineStepKind::NullCall,
            });
        }
        if let Some(vector) = bios_vector(self.state.cpu.pc()) {
            return self.step_bios(vector);
        }
        let prefetched_instruction = if synchronize_devices {
            None
        } else {
            let (idle, instruction) = self.probe_idle_loop()?;
            if idle {
                self.advance_devices(IDLE_ADVANCE_CYCLES, true)?;
                return Ok(MachineStep {
                    cycles: IDLE_ADVANCE_CYCLES,
                    kind: MachineStepKind::Idle,
                });
            }
            instruction
        };

        let outcome = {
            let state = &mut *self.state;
            let prefetched_instruction =
                prefetched_instruction.map(|instruction| (state.cpu.pc(), instruction));
            let mut bus = MachineBus {
                memory: &mut state.memory,
                cdrom: &mut state.cdrom,
                gpu: &mut state.gpu,
                mdec: &mut state.mdec,
                irq: &mut state.irq,
                timers: &mut state.timers,
                dma: &mut state.dma,
                spu: &mut state.spu,
                scheduler: &mut state.scheduler,
                now: state.now,
                prefetched_instruction,
            };
            state.cpu.step_without_external_interrupts(&mut bus)?
        };
        if outcome.event == StepEvent::Exception(Exception::Syscall) {
            return self.step_syscall(outcome.cycles);
        }
        self.advance_devices(outcome.cycles, synchronize_devices)?;
        Ok(MachineStep {
            cycles: outcome.cycles,
            kind: MachineStepKind::Cpu(outcome.event),
        })
    }

    /// Runs the machine until exactly `frames` interleaved integer frames exist.
    ///
    /// # Errors
    ///
    /// Returns [`MachineError::OutputSize`] for a mismatched buffer or propagates
    /// execution/device failures.
    pub fn render(&mut self, frames: usize, output: &mut [i16]) -> Result<(), MachineError> {
        let expected = frames.checked_mul(2).ok_or(MachineError::OutputSize {
            expected: usize::MAX,
            actual: output.len(),
        })?;
        if output.len() != expected {
            return Err(MachineError::OutputSize {
                expected,
                actual: output.len(),
            });
        }
        for sample in output {
            while self.state.pending_audio.is_empty() {
                self.step_inner(false)?;
            }
            *sample = self
                .state
                .pending_audio
                .pop_front()
                .ok_or(MachineError::ClockOverflow)?;
        }
        Ok(())
    }

    /// Advances exactly `frames` native-rate frames without retaining output.
    ///
    /// Sound synthesis and all device side effects remain active so subsequent
    /// rendered samples are identical to rendering and discarding the interval.
    ///
    /// # Errors
    ///
    /// Propagates CPU, HLE, device, clock, and SPU failures.
    pub fn advance(&mut self, frames: usize) -> Result<(), MachineError> {
        let mut remaining = u64::try_from(frames).map_err(|_| MachineError::ClockOverflow)?;
        let queued = u64::try_from(self.state.pending_audio.len() / 2)
            .map_err(|_| MachineError::ClockOverflow)?;
        let queued = queued.min(remaining);
        let queued_samples =
            usize::try_from(queued.checked_mul(2).ok_or(MachineError::ClockOverflow)?)
                .map_err(|_| MachineError::ClockOverflow)?;
        self.state.pending_audio.drain(..queued_samples);
        remaining -= queued;
        if remaining == 0 {
            return Ok(());
        }

        debug_assert_eq!(self.state.discard_audio_frames, 0);
        self.state.discard_audio_frames = remaining;
        while self.state.discard_audio_frames != 0 {
            if let Err(error) = self.step_inner(false) {
                self.state.discard_audio_frames = 0;
                return Err(error);
            }
        }
        Ok(())
    }

    fn enter_interrupt_hook(&mut self) -> Result<(), MachineError> {
        let saved_cpu = self.state.cpu.clone();
        let mut context = cpu_context(&self.state.cpu);
        let prepared = {
            let mut memory = BiosMemory(&mut self.state.memory);
            self.state
                .bios
                .prepare_interrupt_hook(&mut context, &mut memory)?
        };
        if prepared {
            self.state.interrupt_cpu = Some(saved_cpu);
            apply_context(&mut self.state.cpu, &context);
        }
        Ok(())
    }

    fn probe_idle_loop(&self) -> Result<(bool, Option<u32>), MachineError> {
        let pc = self.state.cpu.pc();
        if pc & 3 != 0 {
            return Ok((false, None));
        }
        let instruction = self.state.memory.read_u32(pc)?;
        let branch_to_self = instruction == 0x1000_ffff;
        let jump_to_self = instruction >> 26 == 2
            && ((pc.wrapping_add(4) & 0xf000_0000) | ((instruction & 0x03ff_ffff) << 2)) == pc;
        if !branch_to_self && !jump_to_self {
            return Ok((false, Some(instruction)));
        }
        Ok((
            self.state.memory.read_u32(pc.wrapping_add(4))? == 0,
            Some(instruction),
        ))
    }

    fn handle_default_interrupt(&mut self) -> Result<Option<InterruptSource>, MachineError> {
        let pending = self.state.irq.status() & self.state.irq.mask();
        for (source, counter) in [
            (InterruptSource::VBlank, 3),
            (InterruptSource::Timer2, 2),
            (InterruptSource::Timer1, 1),
            (InterruptSource::Timer0, 0),
        ] {
            if pending & source.bit() == 0 {
                continue;
            }
            self.state.bios.signal_event(
                0xf200_0000 | u32::try_from(counter).unwrap_or(0),
                EVENT_SPEC_INTERRUPTED,
            )?;
            if self.state.bios.clear_root_counter(counter) == Some(true) {
                self.state.irq.acknowledge(!source.bit());
                return Ok(Some(source));
            }
        }
        Ok(None)
    }

    fn step_bios(&mut self, vector: BiosVector) -> Result<MachineStep, MachineError> {
        let saved_cpu = self.state.cpu.clone();
        let mut context = cpu_context(&self.state.cpu);
        let function = context.register(9).unwrap_or(0).to_le_bytes()[0];
        let arguments: [u32; 4] =
            std::array::from_fn(|index| context.register(4 + index).unwrap_or(0));
        let outcome = {
            let mut memory = BiosMemory(&mut self.state.memory);
            self.state
                .bios
                .dispatch(vector, &mut context, &mut memory)
                .map_err(|source| MachineError::Bios {
                    vector,
                    function,
                    ra: context.register(31).unwrap_or(0),
                    a0: arguments[0],
                    a1: arguments[1],
                    a2: arguments[2],
                    a3: arguments[3],
                    source,
                })?
        };
        self.apply_driver_bios_return_quirks(vector, function, &mut context);
        if outcome.action == HleAction::Call {
            self.state.libc_cpu = Some(saved_cpu);
            apply_context(&mut self.state.cpu, &context);
        } else if outcome.action == HleAction::Halt {
            self.state.halted = true;
            apply_context(&mut self.state.cpu, &context);
        } else if outcome.action == HleAction::ReturnFromException {
            if let Some(saved_cpu) = self.state.interrupt_cpu.take() {
                self.state.cpu = saved_cpu;
            } else {
                apply_context(&mut self.state.cpu, &context);
                let epc = self.restore_exception_status();
                self.state.cpu.set_pc(epc);
            }
        } else {
            apply_context(&mut self.state.cpu, &context);
        }
        self.advance_devices(outcome.cycles, true)?;
        Ok(MachineStep {
            cycles: outcome.cycles,
            kind: MachineStepKind::Bios(vector),
        })
    }

    fn apply_driver_bios_return_quirks(
        &self,
        vector: BiosVector,
        function: u8,
        context: &mut CpuContext,
    ) {
        // Work around CaitSith2's crappy Chocobo Racing rip:
        // The PSF driver from that rip schedules the first sequence-bank
        // pointer into a call delay slot before initializing its timer event.
        // The event setup leaves its descriptor in a0, which the driver then
        // mistakes for that pointer.
        if vector == BiosVector::B0
            && function == 0x0c
            && context.register(31) == Some(AKAO_DRIVER_EVENT_RETURN)
            && context.register(4) == Some(AKAO_DRIVER_EVENT_HANDLE)
            && let Some(argument) = self.akao_driver_sequence_argument()
        {
            context.set_register(4, argument);
        }
    }

    fn akao_driver_sequence_argument(&self) -> Option<u32> {
        let preload = self
            .state
            .memory
            .read_u32(AKAO_DRIVER_ARGUMENT_PRELOAD)
            .ok()?;
        let sequence_call = self.state.memory.read_u32(AKAO_DRIVER_SEQUENCE_CALL).ok()?;
        let sequence_delay = self
            .state
            .memory
            .read_u32(AKAO_DRIVER_SEQUENCE_DELAY)
            .ok()?;
        if preload & 0xffff_0000 == 0x3c04_0000
            && sequence_call == 0x0220_f809
            && sequence_delay == 0x2405_0001
        {
            Some((preload & 0xffff) << 16)
        } else {
            None
        }
    }

    fn step_syscall(&mut self, cpu_cycles: u32) -> Result<MachineStep, MachineError> {
        let pc = self.state.cpu.cop0().epc;
        let mut context = cpu_context(&self.state.cpu);
        context.pc = pc;
        let number = context.register(4).unwrap_or(0);
        let outcome = self
            .state
            .bios
            .dispatch_syscall(number, &mut context)
            .map_err(|source| MachineError::Syscall { number, pc, source })?;
        apply_context(&mut self.state.cpu, &context);
        self.restore_exception_status();
        let cycles = cpu_cycles
            .checked_add(outcome.cycles)
            .ok_or(MachineError::ClockOverflow)?;
        self.advance_devices(cycles, true)?;
        Ok(MachineStep {
            cycles,
            kind: MachineStepKind::Syscall(number),
        })
    }

    fn restore_exception_status(&mut self) -> u32 {
        let epc = self.state.cpu.cop0().epc;
        let status = self.state.cpu.cop0().status;
        self.state.cpu.cop0_mut().status = (status & !0x0f) | ((status >> 2) & 0x0f);
        epc
    }

    fn advance_devices(&mut self, cycles: u32, synchronize: bool) -> Result<(), MachineError> {
        let ticks = Ticks::new(u64::from(cycles));
        self.state.now = self.state.now.checked_advance(ticks)?;
        self.state.deferred_device_ticks = self
            .state
            .deferred_device_ticks
            .checked_add(u64::from(cycles))
            .ok_or(MachineError::ClockOverflow)?;
        let scheduler_due = !self.state.scheduler.is_empty()
            && self
                .state
                .scheduler
                .next_deadline()
                .is_some_and(|deadline| deadline <= self.state.now);
        if !synchronize && !scheduler_due && self.state.deferred_device_ticks < DEVICE_SYNC_CYCLES {
            return Ok(());
        }
        let ticks = Ticks::new(std::mem::take(&mut self.state.deferred_device_ticks));
        let mut interrupt_requests = Vec::new();
        {
            let mut sink = EventSink {
                irq: &mut self.state.irq,
                trace: self.state.trace_events,
                events: &mut self.state.event_log,
                requests: &mut interrupt_requests,
            };
            self.state
                .timers
                .advance(ClockInput::System, ticks, &mut sink)?;
            self.state.refresh.advance(ticks, &mut sink)?;
        }
        while let Some(event) = self.state.scheduler.pop_due(self.state.now) {
            if self.state.trace_events {
                self.state.event_log.push(MachineEvent::DmaComplete);
            }
            let mut sink = EventSink {
                irq: &mut self.state.irq,
                trace: self.state.trace_events,
                events: &mut self.state.event_log,
                requests: &mut interrupt_requests,
            };
            self.state.dma.complete(
                event,
                &mut self.state.memory,
                &mut self.state.spu,
                &mut sink,
            )?;
            if self.state.bios.interrupt_hook().is_none() {
                self.state
                    .bios
                    .signal_event(EVENT_CLASS_SPU, EVENT_SPEC_COMMAND_COMPLETE)?;
            }
        }
        let due_frames = self.state.sample_clock.advance(ticks)?.get();
        if due_frames != 0 {
            self.render_due_frames(due_frames)?;
            if self.state.trace_events {
                self.state
                    .event_log
                    .push(MachineEvent::AudioFrames(due_frames));
            }
        }
        {
            let mut sink = EventSink {
                irq: &mut self.state.irq,
                trace: self.state.trace_events,
                events: &mut self.state.event_log,
                requests: &mut interrupt_requests,
            };
            self.state.spu.drain_irq(&mut sink);
        }
        if self.state.bios.interrupt_hook().is_none() {
            for source in interrupt_requests {
                for &(class, spec) in bios_interrupt_events(source) {
                    self.state.bios.signal_event(class, spec)?;
                }
            }
        }
        Ok(())
    }

    fn render_due_frames(&mut self, mut frames: u64) -> Result<(), MachineError> {
        while frames != 0 {
            let chunk = frames.min(u64::try_from(AUDIO_CHUNK_FRAMES).unwrap_or(256));
            let chunk = usize::try_from(chunk).map_err(|_| MachineError::ClockOverflow)?;
            let samples = chunk * 2;
            self.state
                .spu
                .render(chunk, &mut self.state.audio_buffer[..samples])?;
            let discarded = usize::try_from(
                frames
                    .min(self.state.discard_audio_frames)
                    .min(u64::try_from(chunk).unwrap_or(u64::MAX)),
            )
            .map_err(|_| MachineError::ClockOverflow)?;
            self.state.discard_audio_frames -=
                u64::try_from(discarded).map_err(|_| MachineError::ClockOverflow)?;
            self.state.pending_audio.extend(
                self.state.audio_buffer[discarded * 2..samples]
                    .iter()
                    .copied(),
            );
            frames -= u64::try_from(chunk).map_err(|_| MachineError::ClockOverflow)?;
        }
        Ok(())
    }
}

struct EventSink<'a> {
    irq: &'a mut InterruptController,
    trace: bool,
    events: &'a mut Vec<MachineEvent>,
    requests: &'a mut Vec<InterruptSource>,
}

impl EventSink<'_> {
    fn request(&mut self, source: InterruptSource) {
        self.irq.request(source);
        self.requests.push(source);
        if self.trace {
            self.events.push(MachineEvent::Interrupt(source));
        }
    }
}

impl TimerInterruptSink for EventSink<'_> {
    fn request(&mut self, source: InterruptSource) {
        self.request(source);
    }
}

impl DmaInterruptSink for EventSink<'_> {
    fn request(&mut self, source: InterruptSource) {
        self.request(source);
    }
}

impl SpuInterruptSink for EventSink<'_> {
    fn request(&mut self, source: InterruptSource) {
        self.request(source);
    }
}

struct BiosMemory<'a>(&'a mut Ps1Memory);

impl GuestMemory for BiosMemory<'_> {
    fn read_u8(&mut self, address: u32) -> Result<u8, GuestMemoryError> {
        self.0
            .read_u8(address)
            .map_err(|error| GuestMemoryError::new(error.to_string()))
    }

    fn write_u8(&mut self, address: u32, value: u8) -> Result<(), GuestMemoryError> {
        self.0
            .write_u8(address, value)
            .map_err(|error| GuestMemoryError::new(error.to_string()))
    }
}

struct MachineBus<'a> {
    memory: &'a mut Ps1Memory,
    cdrom: &'a mut CdRom,
    gpu: &'a mut Gpu,
    mdec: &'a mut Mdec,
    irq: &'a mut InterruptController,
    timers: &'a mut RootCounters,
    dma: &'a mut DmaController,
    spu: &'a mut Spu,
    scheduler: &'a mut Scheduler,
    now: Deadline,
    prefetched_instruction: Option<(u32, u32)>,
}

impl MachineBus<'_> {
    fn physical_region(address: u32) -> Result<MemoryRegion, BusFault> {
        Ps1Memory::classify(address).map_err(bus_fault)
    }

    fn write_dma(&mut self, address: u32, value: u32) -> Result<(), BusFault> {
        self.dma
            .write(address, value, self.now, self.scheduler, self.irq)
            .map_err(bus_fault)?;
        self.dma
            .service_mdec_in(self.memory, self.mdec, self.irq)
            .map_err(bus_fault)?;
        Ok(())
    }

    fn read_mmio_u16(&mut self, address: u32) -> Result<u16, BusFault> {
        match address {
            MEMORY_CONTROL_START..=MEMORY_CONTROL_END => {
                let word = self.memory.read_control(address & !3).map_err(bus_fault)?;
                let bytes = word.to_le_bytes();
                if address & 2 == 0 {
                    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
                } else {
                    Ok(u16::from_le_bytes([bytes[2], bytes[3]]))
                }
            }
            I_STAT | I_MASK => self.irq.read(address).map(low_half).map_err(bus_fault),
            TIMER_BASE..=TIMER_END => self.timers.read(address).map(low_half).map_err(bus_fault),
            DMA_CHANNEL_START..=DMA_CHANNEL_HALFWORD_END | DPCR..=DMA_CONTROL_END => self
                .dma
                .read(address & !3)
                .map(|value| {
                    let bytes = value.to_le_bytes();
                    if address & 2 == 0 {
                        u16::from_le_bytes([bytes[0], bytes[1]])
                    } else {
                        u16::from_le_bytes([bytes[2], bytes[3]])
                    }
                })
                .map_err(bus_fault),
            SPU_BASE..=SPU_END => self.spu.read_register(address).map_err(bus_fault),
            _ => Err(BusFault::new(format!(
                "unmodeled PS1 MMIO read at {address:#010x}"
            ))),
        }
    }

    fn write_mmio_u16(&mut self, address: u32, value: u16) -> Result<(), BusFault> {
        match address {
            MEMORY_CONTROL_START..=MEMORY_CONTROL_END => {
                let aligned = address & !3;
                let mut bytes = self
                    .memory
                    .read_control(aligned)
                    .map_err(bus_fault)?
                    .to_le_bytes();
                let value = value.to_le_bytes();
                if address & 2 == 0 {
                    bytes[..2].copy_from_slice(&value);
                } else {
                    bytes[2..].copy_from_slice(&value);
                }
                self.memory
                    .write_control(aligned, u32::from_le_bytes(bytes))
                    .map_err(bus_fault)
            }
            I_STAT | I_MASK => self.irq.write(address, u32::from(value)).map_err(bus_fault),
            TIMER_BASE..=TIMER_END => self
                .timers
                .write(address, u32::from(value))
                .map_err(bus_fault),
            DMA_CHANNEL_START..=DMA_CHANNEL_HALFWORD_END | DPCR..=DMA_CONTROL_END => {
                let aligned = address & !3;
                let old = self.dma.read(aligned).map_err(bus_fault)?;
                let merged = if address & 2 == 0 {
                    (old & 0xffff_0000) | u32::from(value)
                } else {
                    (old & 0x0000_ffff) | (u32::from(value) << 16)
                };
                self.write_dma(aligned, merged)
            }
            SPU_BASE..=SPU_END => self.spu.write_register(address, value).map_err(bus_fault),
            _ => Err(BusFault::new(format!(
                "unmodeled PS1 MMIO write at {address:#010x}"
            ))),
        }
    }
}

impl Bus for MachineBus<'_> {
    fn read_u8(&mut self, address: u32) -> Result<u8, BusFault> {
        match Self::physical_region(address)? {
            MemoryRegion::Mmio { physical } => {
                if (CDROM_BASE..=CDROM_END).contains(&physical) {
                    return self.cdrom.read_register(physical).map_err(bus_fault);
                }
                let aligned = physical & !1;
                let value = self.read_mmio_u16(aligned)?;
                Ok(value.to_le_bytes()[usize::from((physical & 1).to_le_bytes()[0])])
            }
            _ => self.memory.read_u8(address).map_err(bus_fault),
        }
    }

    fn read_u16(&mut self, address: u32) -> Result<u16, BusFault> {
        let region = Self::physical_region(address)?;
        match region {
            MemoryRegion::Mmio { physical } => self.read_mmio_u16(physical),
            _ => self
                .memory
                .read_decoded_u16(address, region)
                .map_err(bus_fault),
        }
    }

    fn read_u32(&mut self, address: u32) -> Result<u32, BusFault> {
        if let Some((prefetched_address, instruction)) = self.prefetched_instruction
            && prefetched_address == address
        {
            self.prefetched_instruction = None;
            return Ok(instruction);
        }
        let region = Self::physical_region(address)?;
        match region {
            MemoryRegion::Mmio { physical } => match physical {
                MEMORY_CONTROL_START..=MEMORY_CONTROL_END => {
                    self.memory.read_control(physical).map_err(bus_fault)
                }
                I_STAT | I_MASK => self.irq.read(physical).map_err(bus_fault),
                TIMER_BASE..=TIMER_END => self.timers.read(physical).map_err(bus_fault),
                DMA_CHANNEL_START..=DMA_CHANNEL_END | DPCR | DICR => {
                    self.dma.read(physical).map_err(bus_fault)
                }
                GP0 | GP1 => self.gpu.read_register(physical).map_err(bus_fault),
                MDEC_DATA | MDEC_CONTROL_STATUS => {
                    self.mdec.read_register(physical).map_err(bus_fault)
                }
                SPU_BASE..=SPU_END => {
                    let low = self.spu.read_register(physical).map_err(bus_fault)?;
                    let high = self.spu.read_register(physical + 2).map_err(bus_fault)?;
                    Ok(u32::from(low) | (u32::from(high) << 16))
                }
                _ => Err(BusFault::new(format!(
                    "unmodeled PS1 MMIO read at {physical:#010x}"
                ))),
            },
            _ => self
                .memory
                .read_decoded_u32(address, region)
                .map_err(bus_fault),
        }
    }

    fn write_u8(&mut self, address: u32, value: u8) -> Result<(), BusFault> {
        match Self::physical_region(address)? {
            MemoryRegion::Mmio { physical } => {
                if (CDROM_BASE..=CDROM_END).contains(&physical) {
                    return self
                        .cdrom
                        .write_register(physical, value)
                        .map_err(bus_fault);
                }
                let aligned = physical & !1;
                let mut bytes = self.read_mmio_u16(aligned)?.to_le_bytes();
                bytes[usize::from((physical & 1).to_le_bytes()[0])] = value;
                self.write_mmio_u16(aligned, u16::from_le_bytes(bytes))
            }
            _ => self.memory.write_u8(address, value).map_err(bus_fault),
        }
    }

    fn write_u16(&mut self, address: u32, value: u16) -> Result<(), BusFault> {
        let region = Self::physical_region(address)?;
        match region {
            MemoryRegion::Mmio { physical } => self.write_mmio_u16(physical, value),
            _ => self
                .memory
                .write_decoded_u16(address, region, value)
                .map_err(bus_fault),
        }
    }

    fn write_u32(&mut self, address: u32, value: u32) -> Result<(), BusFault> {
        let region = Self::physical_region(address)?;
        match region {
            MemoryRegion::Mmio { physical } => match physical {
                MEMORY_CONTROL_START..=MEMORY_CONTROL_END => self
                    .memory
                    .write_control(physical, value)
                    .map_err(bus_fault),
                I_STAT | I_MASK => self.irq.write(physical, value).map_err(bus_fault),
                TIMER_BASE..=TIMER_END => self.timers.write(physical, value).map_err(bus_fault),
                DMA_CHANNEL_START..=DMA_CHANNEL_END | DPCR | DICR => {
                    self.write_dma(physical, value)
                }
                GP0 | GP1 => self.gpu.write_register(physical, value).map_err(bus_fault),
                MDEC_DATA | MDEC_CONTROL_STATUS => {
                    self.mdec.write_register(physical, value).map_err(bus_fault)
                }
                SPU_BASE..=SPU_END => {
                    let bytes = value.to_le_bytes();
                    self.spu
                        .write_register(physical, u16::from_le_bytes([bytes[0], bytes[1]]))
                        .map_err(bus_fault)?;
                    self.spu
                        .write_register(physical + 2, u16::from_le_bytes([bytes[2], bytes[3]]))
                        .map_err(bus_fault)
                }
                _ => Err(BusFault::new(format!(
                    "unmodeled PS1 MMIO write at {physical:#010x}"
                ))),
            },
            _ => self
                .memory
                .write_decoded_u32(address, region, value)
                .map_err(bus_fault),
        }
    }

    fn interrupt_pending(&self) -> bool {
        // The firmware exception vector is deliberately absent in an HLE-only
        // machine. Device IRQs are translated into kernel events after each
        // boundary, so exposing the raw line here would enter uninitialized
        // low RAM instead of the BIOS interrupt dispatcher.
        false
    }
}

fn bios_interrupt_events(source: InterruptSource) -> &'static [(u32, u32)] {
    match source {
        InterruptSource::VBlank => &[
            (0xf200_0003, EVENT_SPEC_INTERRUPTED),
            (0xf000_0001, EVENT_SPEC_INTERRUPT),
        ],
        InterruptSource::Gpu => &[(0xf000_0002, EVENT_SPEC_INTERRUPT)],
        InterruptSource::CdRom => &[(0xf000_0003, EVENT_SPEC_INTERRUPT)],
        InterruptSource::Dma => &[(0xf000_0004, EVENT_SPEC_INTERRUPT)],
        InterruptSource::Timer0 => &[
            (0xf200_0000, EVENT_SPEC_INTERRUPTED),
            (0xf000_0005, EVENT_SPEC_INTERRUPT),
        ],
        InterruptSource::Timer1 => &[
            (0xf200_0001, EVENT_SPEC_INTERRUPTED),
            (0xf000_0006, EVENT_SPEC_INTERRUPT),
        ],
        InterruptSource::Timer2 => &[
            (0xf200_0002, EVENT_SPEC_INTERRUPTED),
            (0xf000_0006, EVENT_SPEC_INTERRUPT),
        ],
        InterruptSource::Controller => &[(0xf000_0008, EVENT_SPEC_INTERRUPT)],
        InterruptSource::Sio => &[(0xf000_000b, EVENT_SPEC_INTERRUPT)],
        InterruptSource::Spu => &[(EVENT_CLASS_SPU, EVENT_SPEC_INTERRUPT)],
        InterruptSource::LightPen => &[(0xf000_000a, EVENT_SPEC_INTERRUPT)],
    }
}

fn bios_vector(pc: u32) -> Option<BiosVector> {
    match pc & 0x1fff_ffff {
        0x0000_00a0 => Some(BiosVector::A0),
        0x0000_00b0 => Some(BiosVector::B0),
        0x0000_00c0 => Some(BiosVector::C0),
        _ => None,
    }
}

fn cpu_context(cpu: &Cpu) -> CpuContext {
    let mut context = CpuContext::reset(cpu.pc(), cpu.register(29).unwrap_or(0));
    for index in 0..32 {
        context.set_register(index, cpu.register(index).unwrap_or(0));
    }
    context.hi = cpu.hi();
    context.lo = cpu.lo();
    context
}

fn apply_context(cpu: &mut Cpu, context: &CpuContext) {
    for (index, &value) in context.registers().iter().enumerate() {
        cpu.set_register(index, value);
    }
    cpu.set_pc(context.pc);
}

#[allow(clippy::needless_pass_by_value)]
fn bus_fault(error: impl ToString) -> BusFault {
    BusFault::new(error.to_string())
}

fn low_half(value: u32) -> u16 {
    let bytes = value.to_le_bytes();
    u16::from_le_bytes([bytes[0], bytes[1]])
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};

    use upse_clock::{Deadline, Ticks};
    use upse_ps1_dma::{D4_BCR, D4_CHCR, D4_MADR, DICR, DICR_CHANNEL4_MASK, DPCR};
    use upse_ps1_irq::InterruptSource;
    use upse_ps1_memory::OpenBusPolicy;
    use upse_ps1_timers::{ClockInput, TimerId, TimerRegister};
    use upse_psf::{
        DependencyLimits, LoadPlan, MemoryResolver, ParseLimits, PsfBuilder, PsfVersion, load_plan,
    };
    use upse_scheduler::Scheduler;

    use super::{
        IDLE_ADVANCE_CYCLES, MachineConfig, MachineEvent, MachineStepKind, Ps1Machine, StepEvent,
        VideoStandard,
    };

    #[test]
    fn large_snapshots_are_heap_backed() {
        assert_eq!(
            std::mem::size_of::<Ps1Machine>(),
            2 * std::mem::size_of::<usize>()
        );
    }

    fn instruction_lui(rt: u32, immediate: u16) -> u32 {
        (0x0f << 26) | (rt << 16) | u32::from(immediate)
    }

    fn instruction_ori(rt: u32, rs: u32, immediate: u16) -> u32 {
        (0x0d << 26) | (rs << 21) | (rt << 16) | u32::from(immediate)
    }

    fn instruction_addiu(rt: u32, rs: u32, immediate: i16) -> u32 {
        (0x09 << 26)
            | (rs << 21)
            | (rt << 16)
            | u32::from(u16::from_ne_bytes(immediate.to_ne_bytes()))
    }

    fn instruction_sh(rt: u32, offset: u16, base: u32) -> u32 {
        (0x29 << 26) | (base << 21) | (rt << 16) | u32::from(offset)
    }

    fn instruction_sw(rt: u32, offset: u16, base: u32) -> u32 {
        (0x2b << 26) | (base << 21) | (rt << 16) | u32::from(offset)
    }

    fn instruction_lw(rt: u32, offset: u16, base: u32) -> u32 {
        (0x23 << 26) | (base << 21) | (rt << 16) | u32::from(offset)
    }

    fn synthetic_plan() -> upse_psf::Psf1LoadPlan {
        let mut words = Vec::new();
        words.push(instruction_lui(8, 0x1f80));
        let halfword_writes = [
            (0x3fff_i16, 0x1c00_u16),
            (0x3fff, 0x1c02),
            (0x1000, 0x1c04),
            (0, 0x1c06),
            (0x00ff, 0x1c08),
            (0x1f00, 0x1c0a),
            (0, 0x1c0e),
            (0x3fff, 0x1d80),
            (0x3fff, 0x1d82),
            (0, 0x1da6),
        ];
        for (value, offset) in halfword_writes {
            words.push(instruction_addiu(9, 0, value));
            words.push(instruction_sh(9, offset, 8));
        }
        words.extend([
            instruction_lui(9, 0x076d),
            instruction_ori(9, 9, 0x4321),
            instruction_sw(9, 0x10f0, 8),
            instruction_lui(9, 0x0090),
            instruction_sw(9, 0x10f4, 8),
            instruction_lui(9, 0x0001),
            instruction_ori(9, 9, 0x1000),
            instruction_sw(9, 0x10c0, 8),
            instruction_addiu(9, 0, 4),
            instruction_sw(9, 0x10c4, 8),
            instruction_lui(9, 0x1100),
            instruction_ori(9, 9, 1),
            instruction_sw(9, 0x10c8, 8),
        ]);
        let poll_index = words.len();
        words.extend([
            instruction_lw(9, 0x10c8, 8),
            0,
            0, // replaced with srl t1,t1,24 below
            instruction_ori(9, 9, 0),
        ]);
        words[poll_index + 2] = (9 << 16) | (9 << 11) | (24 << 6) | 2;
        words[poll_index + 3] = (0x0c << 26) | (9 << 21) | (9 << 16) | 1;
        let branch_index = words.len();
        words.push(0);
        words.push(0);
        words.extend([
            instruction_addiu(9, 0, -32_768),
            instruction_sh(9, 0x1daa, 8),
            instruction_addiu(9, 0, 1),
            instruction_sh(9, 0x1d88, 8),
        ]);
        let loop_index = words.len();
        let loop_address = 0x8001_0000_u32 + u32::try_from(loop_index * 4).unwrap();
        words.push(0x0800_0000 | ((loop_address >> 2) & 0x03ff_ffff));
        words.push(0);
        let displacement =
            i32::try_from(poll_index).unwrap() - i32::try_from(branch_index).unwrap() - 1;
        let immediate = u16::from_ne_bytes(i16::try_from(displacement).unwrap().to_ne_bytes());
        words[branch_index] = (0x05 << 26) | (9 << 21) | u32::from(immediate);

        let mut text = vec![0_u8; 0x1010];
        for (index, word) in words.into_iter().enumerate() {
            text[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        text[0x1000] = 0;
        text[0x1001] = 3;
        text[0x1002..0x1010].fill(0x11);
        let mut exe = vec![0_u8; 0x800 + text.len()];
        exe[..8].copy_from_slice(b"PS-X EXE");
        exe[0x10..0x14].copy_from_slice(&0x8001_0000_u32.to_le_bytes());
        exe[0x18..0x1c].copy_from_slice(&0x8001_0000_u32.to_le_bytes());
        exe[0x1c..0x20].copy_from_slice(&u32::try_from(text.len()).unwrap().to_le_bytes());
        exe[0x30..0x34].copy_from_slice(&0x801f_ff00_u32.to_le_bytes());
        exe[0x4c..0x51].copy_from_slice(b"Japan");
        exe[0x800..].copy_from_slice(&text);
        let root = PsfBuilder::new(PsfVersion::Psf1).program(exe).build();
        let plan = load_plan(
            "synthetic.psf",
            &root,
            &mut MemoryResolver::new(),
            ParseLimits::default(),
            DependencyLimits::default(),
        )
        .unwrap();
        match plan {
            LoadPlan::Psf1(plan) => plan,
            LoadPlan::Psf2(_) => unreachable!(),
        }
    }

    #[test]
    fn generated_psf1_boots_through_cpu_dma_and_spu_to_audible_golden() {
        let plan = synthetic_plan();
        let mut machine = Ps1Machine::from_plan(&plan, MachineConfig::default()).unwrap();
        assert_eq!(machine.video_standard(), VideoStandard::Ntsc);
        let mut output = [0_i16; 32];
        machine.render(16, &mut output).unwrap();
        assert!(output.iter().any(|&sample| sample != 0));
        let golden = output;
        machine.reset();
        let mut reset = [0_i16; 32];
        machine.render(16, &mut reset).unwrap();
        assert_eq!(reset, golden);
    }

    #[test]
    fn mdec_reset_and_status_are_routed_through_the_cpu_bus() {
        let plan = synthetic_plan();
        let mut machine = Ps1Machine::from_plan(&plan, MachineConfig::default()).unwrap();
        let program = 0x8001_8000;
        let instructions = [
            instruction_lui(8, 0x1f80),
            instruction_lui(9, 0x8000),
            instruction_sw(9, 0x1824, 8),
            instruction_lw(10, 0x1824, 8),
        ];
        for (index, instruction) in instructions.into_iter().enumerate() {
            machine
                .state
                .memory
                .write_u32(program + u32::try_from(index * 4).unwrap(), instruction)
                .unwrap();
        }
        machine.state.cpu.set_pc(program);

        for _ in 0..instructions.len() {
            machine.step().unwrap();
        }

        assert_eq!(machine.state.cpu.register(10), Some(0x8004_ffff));
    }

    #[test]
    fn null_indirect_calls_return_without_claiming_low_ram() {
        let plan = synthetic_plan();
        let mut machine = Ps1Machine::from_plan(&plan, MachineConfig::default()).unwrap();
        assert_eq!(machine.state.memory.read_u32(0).unwrap(), 0);
        assert_eq!(machine.state.memory.read_u32(4).unwrap(), 0);

        machine.state.cpu.set_register(31, 0x8001_0100);
        machine.state.cpu.set_pc(0);
        let outcome = machine.step().unwrap();
        assert_eq!(outcome.kind, MachineStepKind::NullCall);
        assert_eq!(outcome.cycles, 2);
        assert_eq!(machine.pc(), 0x8001_0100);

        machine
            .state
            .memory
            .write_u32(0, instruction_addiu(2, 0, 7))
            .unwrap();
        machine.state.cpu.set_pc(0);
        let outcome = machine.step().unwrap();
        assert_eq!(outcome.kind, MachineStepKind::Cpu(StepEvent::Instruction));
        assert_eq!(machine.state.cpu.register(2), Some(7));
    }

    #[test]
    fn kernel_syscalls_return_from_the_exception_boundary() {
        let plan = synthetic_plan();
        let mut machine = Ps1Machine::from_plan(&plan, MachineConfig::default()).unwrap();
        let wrapper = 0x8001_0000;
        machine
            .state
            .memory
            .write_u32(wrapper, instruction_addiu(4, 0, 1))
            .unwrap();
        machine
            .state
            .memory
            .write_u32(wrapper + 4, 0x0000_000c)
            .unwrap();
        machine.state.cpu.set_register(31, 0x8001_0100);
        machine.state.cpu.cop0_mut().status = 0x0000_0401;
        machine.state.cpu.set_pc(wrapper);

        machine.step().unwrap();
        let outcome = machine.step().unwrap();
        assert_eq!(outcome.kind, MachineStepKind::Syscall(1));
        assert_eq!(outcome.cycles, 7);
        assert_eq!(machine.pc(), 0x8001_0100);
        assert_eq!(machine.state.cpu.cop0().status & 0x0f, 1);
        assert!(!machine.state.bios.interrupts_enabled());

        machine
            .state
            .memory
            .write_u32(wrapper, instruction_addiu(4, 0, 2))
            .unwrap();
        machine.state.cpu.set_register(31, 0x8001_0200);
        machine.state.cpu.set_pc(wrapper);
        machine.step().unwrap();
        let outcome = machine.step().unwrap();
        assert_eq!(outcome.kind, MachineStepKind::Syscall(2));
        assert_eq!(machine.pc(), 0x8001_0200);
        assert!(machine.state.bios.interrupts_enabled());
    }

    #[test]
    fn akao_driver_recovers_sequence_argument_after_event_initialization() {
        let plan = synthetic_plan();
        let mut machine = Ps1Machine::from_plan(&plan, MachineConfig::default()).unwrap();
        machine
            .state
            .memory
            .write_u32(super::AKAO_DRIVER_ARGUMENT_PRELOAD, 0x3c04_800a)
            .unwrap();
        machine
            .state
            .memory
            .write_u32(super::AKAO_DRIVER_SEQUENCE_CALL, 0x0220_f809)
            .unwrap();
        machine
            .state
            .memory
            .write_u32(super::AKAO_DRIVER_SEQUENCE_DELAY, 0x2405_0001)
            .unwrap();
        let mut context = super::CpuContext::reset(0, 0);
        context.set_register(4, super::AKAO_DRIVER_EVENT_HANDLE);
        context.set_register(31, super::AKAO_DRIVER_EVENT_RETURN);

        machine.apply_driver_bios_return_quirks(super::BiosVector::B0, 0x0c, &mut context);

        assert_eq!(context.register(4), Some(0x800a_0000));

        context.set_register(4, super::AKAO_DRIVER_EVENT_HANDLE);
        context.set_register(31, super::AKAO_DRIVER_EVENT_RETURN + 4);
        machine.apply_driver_bios_return_quirks(super::BiosVector::B0, 0x0c, &mut context);
        assert_eq!(context.register(4), Some(super::AKAO_DRIVER_EVENT_HANDLE));
    }

    #[test]
    fn libc_comparator_callbacks_execute_as_guest_code_and_resume_hle() {
        let plan = synthetic_plan();
        let mut machine = Ps1Machine::from_plan(&plan, MachineConfig::default()).unwrap();
        let callback = 0x8001_0200;
        let caller = 0x8001_0300;
        let array = 0x8001_0400;
        let callback_words = [
            (0x24 << 26) | (4 << 21) | (2 << 16),
            (0x24 << 26) | (5 << 21) | (3 << 16),
            (2 << 21) | (3 << 16) | (2 << 11) | 0x23,
            (31 << 21) | 8,
            0,
        ];
        for (index, word) in callback_words.into_iter().enumerate() {
            machine
                .state
                .memory
                .write_u32(callback + u32::try_from(index * 4).unwrap(), word)
                .unwrap();
        }
        for (offset, value) in [4, 1, 3, 2].into_iter().enumerate() {
            machine
                .state
                .memory
                .write_u8(array + u32::try_from(offset).unwrap(), value)
                .unwrap();
        }
        machine.state.cpu.set_register(4, array);
        machine.state.cpu.set_register(5, 4);
        machine.state.cpu.set_register(6, 1);
        machine.state.cpu.set_register(7, callback);
        machine.state.cpu.set_register(9, 0x31);
        machine.state.cpu.set_register(31, caller);
        machine.state.cpu.set_pc(0x0000_00a0);

        assert_eq!(
            machine.step().unwrap().kind,
            MachineStepKind::Bios(super::BiosVector::A0)
        );
        assert_eq!(machine.pc(), callback);
        let mut resumed = 0;
        for _ in 0..100 {
            if machine.pc() == caller {
                break;
            }
            if machine.step().unwrap().kind == MachineStepKind::LibcCallback {
                resumed += 1;
            }
        }
        assert_eq!(machine.pc(), caller);
        assert!(resumed > 0);
        let sorted: Vec<u8> = (0..4)
            .map(|offset| machine.state.memory.read_u8(array + offset).unwrap())
            .collect();
        assert_eq!(sorted, [1, 2, 3, 4]);
    }

    #[test]
    fn libc_exit_halts_the_cpu_but_keeps_machine_time_advancing() {
        let plan = synthetic_plan();
        let mut machine = Ps1Machine::from_plan(&plan, MachineConfig::default()).unwrap();
        machine.state.cpu.set_register(9, 0x3a);
        machine.state.cpu.set_pc(0x0000_00a0);

        assert_eq!(
            machine.step().unwrap().kind,
            MachineStepKind::Bios(super::BiosVector::A0)
        );
        assert!(machine.state.halted);
        let before = machine.now();
        let outcome = machine.step().unwrap();
        assert_eq!(outcome.kind, MachineStepKind::Halt);
        assert_eq!(outcome.cycles, IDLE_ADVANCE_CYCLES);
        assert!(machine.now() > before);
        assert_eq!(machine.pc(), 0x0000_00a0);
    }

    #[test]
    fn default_timer_handler_acknowledges_before_guest_interrupt_hooks() {
        let plan = synthetic_plan();
        let mut machine = Ps1Machine::from_plan(&plan, MachineConfig::default()).unwrap();
        machine.state.irq.set_mask(InterruptSource::Timer2.bit());
        machine.state.irq.request(InterruptSource::Timer2);
        let pc = machine.pc();

        let outcome = machine.step().unwrap();

        assert_eq!(
            outcome.kind,
            MachineStepKind::Interrupt(InterruptSource::Timer2)
        );
        assert_eq!(machine.state.irq.status(), 0);
        assert_eq!(machine.pc(), pc);
    }

    #[test]
    fn render_fast_forwards_canonical_idle_loops_without_changing_single_step() {
        let plan = synthetic_plan();
        let mut machine = Ps1Machine::from_plan(&plan, MachineConfig::default()).unwrap();
        let idle = 0x8001_0000;
        machine.state.memory.write_u32(idle, 0x1000_ffff).unwrap();
        machine.state.memory.write_u32(idle + 4, 0).unwrap();
        machine.state.cpu.set_pc(idle);

        let outcome = machine.step_inner(false).unwrap();
        assert_eq!(outcome.kind, MachineStepKind::Idle);
        assert_eq!(outcome.cycles, IDLE_ADVANCE_CYCLES);
        assert_eq!(machine.pc(), idle);
        assert_eq!(machine.now(), Deadline::new(u64::from(IDLE_ADVANCE_CYCLES)));

        let outcome = machine.step().unwrap();
        assert_eq!(outcome.kind, MachineStepKind::Cpu(StepEvent::Instruction));
        assert_eq!(outcome.cycles, 1);
        assert_eq!(machine.pc(), idle + 4);
    }

    #[test]
    fn same_cycle_timer_dma_and_irq_order_is_trace_stable() {
        let plan = synthetic_plan();
        let mut machine = Ps1Machine::from_plan(
            &plan,
            MachineConfig {
                open_bus: OpenBusPolicy::Strict,
                trace_events: true,
            },
        )
        .unwrap();
        machine
            .state
            .timers
            .write_register(TimerId::Timer0, TimerRegister::Target, 4);
        machine.state.timers.write_register(
            TimerId::Timer0,
            TimerRegister::Mode,
            (1 << 3) | (1 << 4) | (1 << 6),
        );
        machine.state.memory.write_u32(0x100, 0x1122_3344).unwrap();
        let now = Deadline::ZERO;
        let mut scheduler = Scheduler::new();
        machine
            .state
            .dma
            .write(
                DPCR,
                0x076d_4321,
                now,
                &mut scheduler,
                &mut machine.state.irq,
            )
            .unwrap();
        machine
            .state
            .dma
            .write(
                DICR,
                (1 << 23) | DICR_CHANNEL4_MASK,
                now,
                &mut scheduler,
                &mut machine.state.irq,
            )
            .unwrap();
        machine
            .state
            .dma
            .write(D4_MADR, 0x100, now, &mut scheduler, &mut machine.state.irq)
            .unwrap();
        machine
            .state
            .dma
            .write(D4_BCR, 1, now, &mut scheduler, &mut machine.state.irq)
            .unwrap();
        machine
            .state
            .dma
            .write(
                D4_CHCR,
                0x1100_0001,
                now,
                &mut scheduler,
                &mut machine.state.irq,
            )
            .unwrap();
        machine.state.scheduler = scheduler;
        machine.advance_devices(4, true).unwrap();
        assert_eq!(
            machine.take_event_log(),
            [
                MachineEvent::Interrupt(InterruptSource::Timer0),
                MachineEvent::DmaComplete,
                MachineEvent::Interrupt(InterruptSource::Dma),
            ]
        );
        assert_eq!(machine.state.timers.now(), Deadline::new(4));
        assert_eq!(
            machine
                .state
                .timers
                .advance(ClockInput::System, Ticks::ZERO, &mut machine.state.irq),
            Ok(())
        );
    }

    #[test]
    fn two_instances_are_isolated_interleaved_and_on_separate_threads() {
        let plan = synthetic_plan();
        let mut first = Ps1Machine::from_plan(&plan, MachineConfig::default()).unwrap();
        let mut second = Ps1Machine::from_plan(&plan, MachineConfig::default()).unwrap();
        let mut first_output = [0_i16; 64];
        let mut second_output = [0_i16; 64];
        for (left, right) in first_output
            .chunks_exact_mut(8)
            .zip(second_output.chunks_exact_mut(8))
        {
            first.render(4, left).unwrap();
            second.render(4, right).unwrap();
        }
        assert_eq!(first_output, second_output);

        let expected = first_output;
        let left_plan = plan.clone();
        let right_plan = plan;
        let left = thread::spawn(move || {
            let mut machine = Ps1Machine::from_plan(&left_plan, MachineConfig::default()).unwrap();
            let mut output = [0_i16; 64];
            machine.render(32, &mut output).unwrap();
            output
        });
        let right = thread::spawn(move || {
            let mut machine = Ps1Machine::from_plan(&right_plan, MachineConfig::default()).unwrap();
            let mut output = [0_i16; 64];
            machine.render(32, &mut output).unwrap();
            output
        });
        assert_eq!(left.join().unwrap(), expected);
        assert_eq!(right.join().unwrap(), expected);
    }

    #[test]
    #[ignore = "explicit release-mode real-time performance gate"]
    fn release_fixture_renders_faster_than_real_time() {
        let plan = synthetic_plan();
        let mut machine = Ps1Machine::from_plan(&plan, MachineConfig::default()).unwrap();
        let mut output = vec![0_i16; 44_100 * 2];
        let start = Instant::now();
        machine.render(44_100, &mut output).unwrap();
        let elapsed = start.elapsed();
        assert!(output.iter().any(|&sample| sample != 0));
        assert!(
            elapsed < Duration::from_secs(1),
            "one emulated second took {elapsed:?}"
        );
    }
}
