// SPDX-License-Identifier: LGPL-2.1-or-later
//! End-to-end PSF1 machine composition with explicit device routing.

#![allow(clippy::too_many_lines)]

use std::collections::VecDeque;

use thiserror::Error;
use upse_clock::{ClockError, Deadline, RateConverter, Ticks};
use upse_ps1_bios::{
    BiosError, BiosHle, BiosVector, CpuContext, GuestMemory, GuestMemoryError, HleAction,
};
use upse_ps1_dma::{
    D4_BCR, D4_CHCR, D4_MADR, DICR, DPCR, DmaController, DmaError,
    InterruptSink as DmaInterruptSink,
};
use upse_ps1_irq::{I_MASK, I_STAT, InterruptController, InterruptSource};
use upse_ps1_memory::{MemoryError, MemoryRegion, OpenBusPolicy, Ps1Memory};
use upse_ps1_spu::{
    InterruptSink as SpuInterruptSink, SAMPLE_RATE, SPU_BASE, SPU_END, Spu, SpuError,
};
use upse_ps1_timers::{
    CPU_HZ, ClockInput, InterruptSink as TimerInterruptSink, RootCounters, TIMER_BASE, TimerError,
    VBlankClock, VideoStandard,
};
use upse_psf::{Psf1LoadPlan, RefreshRate};
use upse_psx_exe::{ExecutableImage, ImageError};
use upse_r3000::{Bus, BusFault, Cpu, CpuError, ResetProfile, StepEvent};
use upse_scheduler::{Scheduler, SchedulerError};

const TIMER_END: u32 = TIMER_BASE + 0x28;
const AUDIO_CHUNK_FRAMES: usize = 256;

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
    /// One deferred event callback context was restored.
    CallbackReturn,
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
    #[error("PS1 BIOS HLE failure: {0}")]
    Bios(#[from] BiosError),
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
    irq: InterruptController,
    timers: RootCounters,
    refresh: VBlankClock,
    dma: DmaController,
    bios: BiosHle,
    spu: Spu,
    scheduler: Scheduler,
    now: Deadline,
    sample_clock: RateConverter,
    pending_audio: VecDeque<i16>,
    trace_events: bool,
    event_log: Vec<MachineEvent>,
}

/// Fully composed PSF1 machine with a reset snapshot.
#[derive(Clone, Debug)]
pub struct Ps1Machine {
    state: MachineState,
    reset: MachineState,
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
        let memory = Ps1Memory::from_image(&image, config.open_bus)?;
        let standard = match image.refresh {
            RefreshRate::Hz50 => VideoStandard::Pal,
            RefreshRate::Hz60 => VideoStandard::Ntsc,
        };
        let mut cpu = Cpu::new(ResetProfile {
            pc: image.pc,
            exception_vector: 0x8000_0080,
            bootstrap_exception_vector: 0xbfc0_0180,
            status: 0,
            processor_id: 2,
        });
        cpu.set_register(29, image.sp);
        let state = MachineState {
            cpu,
            memory,
            irq: InterruptController::new(),
            timers: RootCounters::new(),
            refresh: VBlankClock::new(standard),
            dma: DmaController::new(),
            bios: BiosHle::default(),
            spu: Spu::new(),
            scheduler: Scheduler::new(),
            now: Deadline::ZERO,
            sample_clock: RateConverter::new(CPU_HZ, u64::from(SAMPLE_RATE))?,
            pending_audio: VecDeque::new(),
            trace_events: config.trace_events,
            event_log: Vec::new(),
        };
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
        if self.state.cpu.pc() == BiosHle::callback_return_pc() {
            let mut context = cpu_context(&self.state.cpu);
            self.state.bios.return_from_callback(&mut context)?;
            apply_context(&mut self.state.cpu, &context);
            self.advance_devices(1)?;
            return Ok(MachineStep {
                cycles: 1,
                kind: MachineStepKind::CallbackReturn,
            });
        }
        if self.state.bios.interrupts_enabled()
            && let Some(callback) = self.state.bios.take_callback()
        {
            let mut context = cpu_context(&self.state.cpu);
            self.state.bios.enter_callback(&mut context, callback)?;
            apply_context(&mut self.state.cpu, &context);
        }
        if let Some(vector) = bios_vector(self.state.cpu.pc()) {
            return self.step_bios(vector);
        }

        let outcome = {
            let state = &mut self.state;
            let mut bus = MachineBus {
                memory: &mut state.memory,
                irq: &mut state.irq,
                timers: &mut state.timers,
                dma: &mut state.dma,
                spu: &mut state.spu,
                scheduler: &mut state.scheduler,
                now: state.now,
            };
            state.cpu.step(&mut bus)?
        };
        self.advance_devices(outcome.cycles)?;
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
                self.step()?;
            }
            *sample = self
                .state
                .pending_audio
                .pop_front()
                .ok_or(MachineError::ClockOverflow)?;
        }
        Ok(())
    }

    fn step_bios(&mut self, vector: BiosVector) -> Result<MachineStep, MachineError> {
        let mut context = cpu_context(&self.state.cpu);
        let outcome = {
            let mut memory = BiosMemory(&mut self.state.memory);
            self.state
                .bios
                .dispatch(vector, &mut context, &mut memory)?
        };
        apply_context(&mut self.state.cpu, &context);
        if outcome.action == HleAction::ReturnFromException {
            let epc = self.state.cpu.cop0().epc;
            let status = self.state.cpu.cop0().status;
            self.state.cpu.cop0_mut().status = (status & !0x0f) | ((status >> 2) & 0x0f);
            self.state.cpu.set_pc(epc);
        }
        self.advance_devices(outcome.cycles)?;
        Ok(MachineStep {
            cycles: outcome.cycles,
            kind: MachineStepKind::Bios(vector),
        })
    }

    fn advance_devices(&mut self, cycles: u32) -> Result<(), MachineError> {
        let ticks = Ticks::new(u64::from(cycles));
        self.state.now = self.state.now.checked_advance(ticks)?;
        {
            let mut sink = EventSink {
                irq: &mut self.state.irq,
                trace: self.state.trace_events,
                events: &mut self.state.event_log,
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
            };
            self.state.dma.complete(
                event,
                &mut self.state.memory,
                &mut self.state.spu,
                &mut sink,
            )?;
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
            };
            self.state.spu.drain_irq(&mut sink);
        }
        Ok(())
    }

    fn render_due_frames(&mut self, mut frames: u64) -> Result<(), MachineError> {
        let mut buffer = [0_i16; AUDIO_CHUNK_FRAMES * 2];
        while frames != 0 {
            let chunk = frames.min(u64::try_from(AUDIO_CHUNK_FRAMES).unwrap_or(256));
            let chunk = usize::try_from(chunk).map_err(|_| MachineError::ClockOverflow)?;
            let samples = chunk * 2;
            self.state.spu.render(chunk, &mut buffer[..samples])?;
            self.state
                .pending_audio
                .extend(buffer[..samples].iter().copied());
            frames -= u64::try_from(chunk).map_err(|_| MachineError::ClockOverflow)?;
        }
        Ok(())
    }
}

struct EventSink<'a> {
    irq: &'a mut InterruptController,
    trace: bool,
    events: &'a mut Vec<MachineEvent>,
}

impl EventSink<'_> {
    fn request(&mut self, source: InterruptSource) {
        self.irq.request(source);
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
    irq: &'a mut InterruptController,
    timers: &'a mut RootCounters,
    dma: &'a mut DmaController,
    spu: &'a mut Spu,
    scheduler: &'a mut Scheduler,
    now: Deadline,
}

impl MachineBus<'_> {
    fn physical_region(address: u32) -> Result<MemoryRegion, BusFault> {
        Ps1Memory::classify(address).map_err(bus_fault)
    }

    fn read_mmio_u16(&mut self, address: u32) -> Result<u16, BusFault> {
        match address {
            I_STAT | I_MASK => self.irq.read(address).map(low_half).map_err(bus_fault),
            TIMER_BASE..=TIMER_END => self.timers.read(address).map(low_half).map_err(bus_fault),
            D4_MADR | D4_BCR | D4_CHCR | DPCR | DICR => {
                self.dma.read(address).map(low_half).map_err(bus_fault)
            }
            SPU_BASE..=SPU_END => self.spu.read_register(address).map_err(bus_fault),
            _ => Err(BusFault::new(format!(
                "unmodeled PS1 MMIO read at {address:#010x}"
            ))),
        }
    }

    fn write_mmio_u16(&mut self, address: u32, value: u16) -> Result<(), BusFault> {
        match address {
            I_STAT | I_MASK => self.irq.write(address, u32::from(value)).map_err(bus_fault),
            TIMER_BASE..=TIMER_END => self
                .timers
                .write(address, u32::from(value))
                .map_err(bus_fault),
            D4_MADR | D4_BCR | D4_CHCR | DPCR | DICR => {
                let old = self.dma.read(address).map_err(bus_fault)?;
                let merged = (old & 0xffff_0000) | u32::from(value);
                self.dma
                    .write(address, merged, self.now, self.scheduler, self.irq)
                    .map_err(bus_fault)
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
                let aligned = physical & !1;
                let value = self.read_mmio_u16(aligned)?;
                Ok(value.to_le_bytes()[usize::from((physical & 1).to_le_bytes()[0])])
            }
            _ => self.memory.read_u8(address).map_err(bus_fault),
        }
    }

    fn read_u16(&mut self, address: u32) -> Result<u16, BusFault> {
        match Self::physical_region(address)? {
            MemoryRegion::Mmio { physical } => self.read_mmio_u16(physical),
            _ => self.memory.read_u16(address).map_err(bus_fault),
        }
    }

    fn read_u32(&mut self, address: u32) -> Result<u32, BusFault> {
        match Self::physical_region(address)? {
            MemoryRegion::Mmio { physical } => match physical {
                I_STAT | I_MASK => self.irq.read(physical).map_err(bus_fault),
                TIMER_BASE..=TIMER_END => self.timers.read(physical).map_err(bus_fault),
                D4_MADR | D4_BCR | D4_CHCR | DPCR | DICR => {
                    self.dma.read(physical).map_err(bus_fault)
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
            _ => self.memory.read_u32(address).map_err(bus_fault),
        }
    }

    fn write_u8(&mut self, address: u32, value: u8) -> Result<(), BusFault> {
        match Self::physical_region(address)? {
            MemoryRegion::Mmio { physical } => {
                let aligned = physical & !1;
                let mut bytes = self.read_mmio_u16(aligned)?.to_le_bytes();
                bytes[usize::from((physical & 1).to_le_bytes()[0])] = value;
                self.write_mmio_u16(aligned, u16::from_le_bytes(bytes))
            }
            _ => self.memory.write_u8(address, value).map_err(bus_fault),
        }
    }

    fn write_u16(&mut self, address: u32, value: u16) -> Result<(), BusFault> {
        match Self::physical_region(address)? {
            MemoryRegion::Mmio { physical } => self.write_mmio_u16(physical, value),
            _ => self.memory.write_u16(address, value).map_err(bus_fault),
        }
    }

    fn write_u32(&mut self, address: u32, value: u32) -> Result<(), BusFault> {
        match Self::physical_region(address)? {
            MemoryRegion::Mmio { physical } => match physical {
                I_STAT | I_MASK => self.irq.write(physical, value).map_err(bus_fault),
                TIMER_BASE..=TIMER_END => self.timers.write(physical, value).map_err(bus_fault),
                D4_MADR | D4_BCR | D4_CHCR | DPCR | DICR => self
                    .dma
                    .write(physical, value, self.now, self.scheduler, self.irq)
                    .map_err(bus_fault),
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
            _ => self.memory.write_u32(address, value).map_err(bus_fault),
        }
    }

    fn interrupt_pending(&self) -> bool {
        self.irq.pending()
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

    use super::{MachineConfig, MachineEvent, Ps1Machine, VideoStandard};

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
        machine.advance_devices(4).unwrap();
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
