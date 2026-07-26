// SPDX-License-Identifier: LGPL-2.1-or-later
//! Bare-metal PS2 IOP machine composition without BIOS or SPU2 implementation.

#![allow(clippy::too_many_lines)]

use thiserror::Error;
use upse_clock::{ClockError, Deadline, Ticks};
use upse_iop_dma::{
    DICR1, DICR2, DMA1_CHANNEL_END, DMA1_CHANNEL_START, DMA2_CHANNEL_END, DMA2_CHANNEL_START,
    DMAC_ENABLE, DPCR1, DPCR2, DmaController, DmaError, DmaEvent, DmaObserver, Spu2DmaEndpoint,
    Spu2MmioEndpoint,
};
use upse_iop_irq::{I_MASK, I_STAT, InterruptController};
use upse_iop_memory::{IopMemory, MemoryError, MemoryRegion, OpenBusPolicy};
use upse_iop_timers::{
    IopTimers, RefreshClock, TIMER0_BASE, TIMER1_BASE, TIMER2_BASE, TIMER3_BASE, TIMER4_BASE,
    TIMER5_BASE, TimerError, TimingEvent, VideoStandard,
};
use upse_r3000::{Bus, BusFault, Cpu, CpuError, ResetProfile, StepOutcome};
use upse_scheduler::Scheduler;

const STATUS_BEV: u32 = 1 << 22;
const IOP_PROCESSOR_ID: u32 = 0x1f;
const IRQ_REGISTER_END: u32 = I_MASK + 2;

/// Architectural PS2 IOP power-on profile.
pub const IOP_RESET_PROFILE: ResetProfile = ResetProfile {
    pc: 0xbfc0_0000,
    exception_vector: 0x8000_0080,
    bootstrap_exception_vector: 0xbfc0_0180,
    status: STATUS_BEV,
    processor_id: IOP_PROCESSOR_ID,
};

/// Machine construction policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MachineConfig {
    /// Handling of otherwise unmapped physical addresses.
    pub open_bus: OpenBusPolicy,
    /// Refresh timing used by the `VBlank` source.
    pub video_standard: VideoStandard,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            open_bus: OpenBusPolicy::Strict,
            video_standard: VideoStandard::Ntsc,
        }
    }
}

/// Timestamped hardware event available to a later BIOS composition layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HardwareEvent {
    /// Emulated IOP timestamp.
    pub at: Deadline,
    /// Device event.
    pub kind: HardwareEventKind,
}

/// Device-specific contents of one observable hardware event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HardwareEventKind {
    /// Counter or refresh event.
    Timing(TimingEvent),
    /// Sound DMA lifecycle event.
    Dma(DmaEvent),
}

/// One machine step and its resulting R3000 event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MachineStep {
    /// R3000 outcome.
    pub cpu: StepOutcome,
    /// Emulated timestamp after device advancement.
    pub now: Deadline,
}

/// Bare-metal IOP machine failure.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum MachineError {
    /// Initial program could not be placed in RAM.
    #[error("IOP program load failure: {0}")]
    Program(#[from] MemoryError),
    /// Entry point is not aligned or does not select physical main RAM.
    #[error("invalid bare-metal IOP entry point {entry:#010x}")]
    InvalidEntry {
        /// Requested entry address.
        entry: u32,
    },
    /// Shared R3000 execution failed.
    #[error("IOP CPU failure: {0}")]
    Cpu(#[from] CpuError),
    /// Counter or refresh advancement failed.
    #[error("IOP timer failure: {0}")]
    Timer(#[from] TimerError),
    /// Sound DMA completion failed.
    #[error("IOP DMA failure: {0}")]
    Dma(#[from] DmaError),
    /// Machine timestamp overflowed.
    #[error("IOP machine clock overflow")]
    ClockOverflow,
}

impl From<ClockError> for MachineError {
    fn from(_: ClockError) -> Self {
        Self::ClockOverflow
    }
}

/// R3000, IOP devices, and an injected sound endpoint.
#[derive(Clone, Debug)]
pub struct IopMachine<E> {
    cpu: Cpu,
    memory: IopMemory,
    irq: InterruptController,
    timers: IopTimers,
    refresh: RefreshClock,
    dma: DmaController,
    scheduler: Scheduler,
    sound: E,
    now: Deadline,
    hardware_events: Vec<HardwareEvent>,
}

impl<E: Spu2DmaEndpoint + Spu2MmioEndpoint> IopMachine<E> {
    /// Constructs architectural power-on state.
    ///
    /// The reset vector remains in an HLE-only ROM range. Stepping this machine
    /// before a BIOS layer installs a reset trampoline therefore fails with an
    /// explicit firmware diagnostic; there is no firmware-image input path.
    #[must_use]
    pub fn new(sound: E, config: MachineConfig) -> Self {
        Self {
            cpu: Cpu::new(IOP_RESET_PROFILE),
            memory: IopMemory::new(config.open_bus),
            irq: InterruptController::new(),
            timers: IopTimers::new(),
            refresh: RefreshClock::new(config.video_standard),
            dma: DmaController::new(),
            scheduler: Scheduler::new(),
            sound,
            now: Deadline::ZERO,
            hardware_events: Vec::new(),
        }
    }

    /// Constructs a BIOS-free machine with an original bare-metal RAM image.
    ///
    /// # Errors
    ///
    /// Returns [`MachineError::InvalidEntry`] unless the aligned entry decodes
    /// to main RAM, or [`MachineError::Program`] if the image does not fit.
    pub fn from_bare_metal(
        load_address: u32,
        entry: u32,
        program: &[u8],
        sound: E,
        config: MachineConfig,
    ) -> Result<Self, MachineError> {
        if entry & 3 != 0 || !matches!(IopMemory::classify(entry), Ok(MemoryRegion::Ram { .. })) {
            return Err(MachineError::InvalidEntry { entry });
        }
        let mut machine = Self::new(sound, config);
        let physical = IopMemory::translate(load_address)?;
        machine.memory.load_ram(physical, program)?;
        machine.cpu = Cpu::new(ResetProfile {
            pc: entry,
            exception_vector: IOP_RESET_PROFILE.exception_vector,
            bootstrap_exception_vector: IOP_RESET_PROFILE.bootstrap_exception_vector,
            status: 0,
            processor_id: IOP_PROCESSOR_ID,
        });
        machine.cpu.set_register(29, 0x001f_fff0);
        Ok(machine)
    }

    /// Returns current emulated IOP time.
    #[must_use]
    pub const fn now(&self) -> Deadline {
        self.now
    }

    /// Returns the shared R3000 state.
    #[must_use]
    pub const fn cpu(&self) -> &Cpu {
        &self.cpu
    }

    /// Returns mutable R3000 state for BIOS context setup.
    #[must_use]
    pub const fn cpu_mut(&mut self) -> &mut Cpu {
        &mut self.cpu
    }

    /// Returns IOP memory.
    #[must_use]
    pub const fn memory(&self) -> &IopMemory {
        &self.memory
    }

    /// Returns mutable IOP memory for module loading.
    #[must_use]
    pub const fn memory_mut(&mut self) -> &mut IopMemory {
        &mut self.memory
    }

    /// Returns the interrupt controller.
    #[must_use]
    pub const fn interrupt_controller(&self) -> &InterruptController {
        &self.irq
    }

    /// Returns the timer component.
    #[must_use]
    pub const fn timers(&self) -> &IopTimers {
        &self.timers
    }

    /// Returns the DMA component.
    #[must_use]
    pub const fn dma(&self) -> &DmaController {
        &self.dma
    }

    /// Returns the injected sound endpoint.
    #[must_use]
    pub const fn sound(&self) -> &E {
        &self.sound
    }

    /// Returns the injected sound endpoint mutably.
    #[must_use]
    pub const fn sound_mut(&mut self) -> &mut E {
        &mut self.sound
    }

    /// Removes all timestamped hardware events accumulated since the last call.
    pub fn take_hardware_events(&mut self) -> Vec<HardwareEvent> {
        std::mem::take(&mut self.hardware_events)
    }

    /// Executes one R3000 boundary, then services same-cycle DMA, counters, and
    /// refresh in that stable order.
    ///
    /// # Errors
    ///
    /// Returns a structured CPU, DMA, timer, endpoint, or clock diagnostic.
    pub fn step(&mut self) -> Result<MachineStep, MachineError> {
        let outcome = {
            let mut bus = MachineBus {
                memory: &mut self.memory,
                irq: &mut self.irq,
                timers: &mut self.timers,
                dma: &mut self.dma,
                scheduler: &mut self.scheduler,
                sound: &mut self.sound,
                now: self.now,
                events: &mut self.hardware_events,
            };
            self.cpu.step(&mut bus)?
        };
        self.advance_devices(u64::from(outcome.cycles))?;
        Ok(MachineStep {
            cpu: outcome,
            now: self.now,
        })
    }

    /// Advances devices without executing an instruction.
    ///
    /// This boundary exists for BIOS scheduler idle periods and tests; it uses
    /// the same DMA-before-timer-before-refresh ordering as [`IopMachine::step`].
    ///
    /// # Errors
    ///
    /// Returns a structured DMA, timer, endpoint, or clock diagnostic.
    pub fn advance_devices(&mut self, cycles: u64) -> Result<(), MachineError> {
        self.now = self.now.checked_advance(Ticks::new(cycles))?;
        while self
            .scheduler
            .next_deadline()
            .is_some_and(|deadline| deadline <= self.now)
        {
            let Some(event) = self.scheduler.pop_due(self.now) else {
                break;
            };
            let mut observer = MachineDmaObserver {
                at: event.deadline,
                events: &mut self.hardware_events,
            };
            self.dma.complete(
                event,
                &mut self.memory,
                &mut self.sound,
                &mut self.irq,
                &mut observer,
            )?;
        }

        let mut timing = Vec::new();
        self.timers
            .advance_cpu(cycles, &mut self.irq, &mut timing)?;
        self.refresh.advance(cycles, &mut self.irq, &mut timing)?;
        for event in timing {
            match event {
                TimingEvent::VBlankStart => {
                    self.timers.set_gate(upse_iop_timers::TimerId::Timer1, true);
                    self.timers.set_gate(upse_iop_timers::TimerId::Timer3, true);
                }
                TimingEvent::VBlankEnd => {
                    self.timers
                        .set_gate(upse_iop_timers::TimerId::Timer1, false);
                    self.timers
                        .set_gate(upse_iop_timers::TimerId::Timer3, false);
                }
                TimingEvent::Counter { .. } => {}
            }
            self.hardware_events.push(HardwareEvent {
                at: self.now,
                kind: HardwareEventKind::Timing(event),
            });
        }
        Ok(())
    }
}

struct MachineDmaObserver<'a> {
    at: Deadline,
    events: &'a mut Vec<HardwareEvent>,
}

impl DmaObserver for MachineDmaObserver<'_> {
    fn observe(&mut self, event: DmaEvent) {
        self.events.push(HardwareEvent {
            at: self.at,
            kind: HardwareEventKind::Dma(event),
        });
    }
}

struct MachineBus<'a, E> {
    memory: &'a mut IopMemory,
    irq: &'a mut InterruptController,
    timers: &'a mut IopTimers,
    dma: &'a mut DmaController,
    scheduler: &'a mut Scheduler,
    sound: &'a mut E,
    now: Deadline,
    events: &'a mut Vec<HardwareEvent>,
}

impl<E: Spu2DmaEndpoint + Spu2MmioEndpoint> MachineBus<'_, E> {
    fn read_mmio_u16(&mut self, address: u32) -> Result<u16, BusFault> {
        match address {
            I_STAT..=IRQ_REGISTER_END => {
                let aligned = address & !3;
                let value = self.irq.read(aligned).map_err(bus_fault)?;
                Ok(half(value, address))
            }
            _ if is_timer_address(address) => self.timers.read_u16(address).map_err(bus_fault),
            _ if is_dma_address(address) => self.dma.read_u16(address).map_err(bus_fault),
            _ => Err(BusFault::new(format!(
                "unmodeled IOP MMIO read at {address:#010x}"
            ))),
        }
    }

    fn write_mmio_u16(&mut self, address: u32, value: u16) -> Result<(), BusFault> {
        match address {
            I_STAT..=IRQ_REGISTER_END => {
                let aligned = address & !3;
                let old = self.irq.read(aligned).map_err(bus_fault)?;
                self.irq
                    .write(aligned, merge_half(old, address, value))
                    .map_err(bus_fault)
            }
            _ if is_timer_address(address) => {
                self.timers.write_u16(address, value).map_err(bus_fault)
            }
            _ if is_dma_address(address) => {
                let mut observer = MachineDmaObserver {
                    at: self.now,
                    events: self.events,
                };
                self.dma
                    .write_u16(
                        address,
                        value,
                        self.now,
                        self.scheduler,
                        self.irq,
                        &mut observer,
                    )
                    .map_err(bus_fault)
            }
            _ => Err(BusFault::new(format!(
                "unmodeled IOP MMIO write at {address:#010x}"
            ))),
        }
    }

    fn read_spu2_u16(&mut self, address: u32) -> Result<u16, BusFault> {
        self.sound.read_register(address).map_err(bus_fault)
    }

    fn write_spu2_u16(&mut self, address: u32, value: u16) -> Result<(), BusFault> {
        self.sound.write_register(address, value).map_err(bus_fault)
    }
}

impl<E: Spu2DmaEndpoint + Spu2MmioEndpoint> Bus for MachineBus<'_, E> {
    fn read_u8(&mut self, address: u32) -> Result<u8, BusFault> {
        match IopMemory::classify(address).map_err(bus_fault)? {
            MemoryRegion::Mmio { physical } => {
                let value = self.read_mmio_u16(physical & !1)?;
                Ok(value.to_le_bytes()[usize::from((physical & 1) != 0)])
            }
            MemoryRegion::Spu2 { physical } => {
                let value = self.read_spu2_u16(physical & !1)?;
                Ok(value.to_le_bytes()[usize::from((physical & 1) != 0)])
            }
            _ => self.memory.read_u8(address).map_err(bus_fault),
        }
    }

    fn read_u16(&mut self, address: u32) -> Result<u16, BusFault> {
        match IopMemory::classify(address).map_err(bus_fault)? {
            MemoryRegion::Mmio { physical } => self.read_mmio_u16(physical),
            MemoryRegion::Spu2 { physical } => self.read_spu2_u16(physical),
            _ => self.memory.read_u16(address).map_err(bus_fault),
        }
    }

    fn read_u32(&mut self, address: u32) -> Result<u32, BusFault> {
        match IopMemory::classify(address).map_err(bus_fault)? {
            MemoryRegion::Mmio { physical } => match physical {
                I_STAT | I_MASK => self.irq.read(physical).map_err(bus_fault),
                _ if is_timer_address(physical) => {
                    self.timers.read_u32(physical).map_err(bus_fault)
                }
                _ if is_dma_address(physical) => self.dma.read_u32(physical).map_err(bus_fault),
                _ => Err(BusFault::new(format!(
                    "unmodeled IOP MMIO read at {physical:#010x}"
                ))),
            },
            MemoryRegion::Spu2 { physical } => {
                let low = self.read_spu2_u16(physical)?;
                let high = self.read_spu2_u16(physical + 2)?;
                Ok(u32::from(low) | (u32::from(high) << 16))
            }
            _ => self.memory.read_u32(address).map_err(bus_fault),
        }
    }

    fn write_u8(&mut self, address: u32, value: u8) -> Result<(), BusFault> {
        match IopMemory::classify(address).map_err(bus_fault)? {
            MemoryRegion::Mmio { physical } => {
                let aligned = physical & !1;
                let old = self.read_mmio_u16(aligned)?;
                self.write_mmio_u16(aligned, merge_byte(old, physical, value))
            }
            MemoryRegion::Spu2 { physical } => {
                let aligned = physical & !1;
                let old = self.read_spu2_u16(aligned)?;
                self.write_spu2_u16(aligned, merge_byte(old, physical, value))
            }
            _ => self.memory.write_u8(address, value).map_err(bus_fault),
        }
    }

    fn write_u16(&mut self, address: u32, value: u16) -> Result<(), BusFault> {
        match IopMemory::classify(address).map_err(bus_fault)? {
            MemoryRegion::Mmio { physical } => self.write_mmio_u16(physical, value),
            MemoryRegion::Spu2 { physical } => self.write_spu2_u16(physical, value),
            _ => self.memory.write_u16(address, value).map_err(bus_fault),
        }
    }

    fn write_u32(&mut self, address: u32, value: u32) -> Result<(), BusFault> {
        match IopMemory::classify(address).map_err(bus_fault)? {
            MemoryRegion::Mmio { physical } => match physical {
                I_STAT | I_MASK => self.irq.write(physical, value).map_err(bus_fault),
                _ if is_timer_address(physical) => {
                    self.timers.write_u32(physical, value).map_err(bus_fault)
                }
                _ if is_dma_address(physical) => {
                    let mut observer = MachineDmaObserver {
                        at: self.now,
                        events: self.events,
                    };
                    self.dma
                        .write_u32(
                            physical,
                            value,
                            self.now,
                            self.scheduler,
                            self.irq,
                            &mut observer,
                        )
                        .map_err(bus_fault)
                }
                _ => Err(BusFault::new(format!(
                    "unmodeled IOP MMIO write at {physical:#010x}"
                ))),
            },
            MemoryRegion::Spu2 { physical } => {
                let bytes = value.to_le_bytes();
                self.write_spu2_u16(physical, u16::from_le_bytes([bytes[0], bytes[1]]))?;
                self.write_spu2_u16(physical + 2, u16::from_le_bytes([bytes[2], bytes[3]]))
            }
            _ => self.memory.write_u32(address, value).map_err(bus_fault),
        }
    }

    fn interrupt_pending(&self) -> bool {
        self.irq.pending()
    }
}

fn is_timer_address(address: u32) -> bool {
    for base in [
        TIMER0_BASE,
        TIMER1_BASE,
        TIMER2_BASE,
        TIMER3_BASE,
        TIMER4_BASE,
        TIMER5_BASE,
    ] {
        if (base..=base + 0x0a).contains(&address) {
            return true;
        }
    }
    false
}

fn is_dma_address(address: u32) -> bool {
    (DMA1_CHANNEL_START..=DMA1_CHANNEL_END + 2).contains(&address)
        || (DMA2_CHANNEL_START..=DMA2_CHANNEL_END + 2).contains(&address)
        || (DPCR1..=DICR1 + 2).contains(&address)
        || (0x1f80_1560..=0x1f80_156a).contains(&address)
        || (DPCR2..=DMAC_ENABLE + 2).contains(&address)
        || address == DICR2
}

fn half(value: u32, address: u32) -> u16 {
    let bytes = value.to_le_bytes();
    if address & 2 == 0 {
        u16::from_le_bytes([bytes[0], bytes[1]])
    } else {
        u16::from_le_bytes([bytes[2], bytes[3]])
    }
}

fn merge_half(old: u32, address: u32, value: u16) -> u32 {
    if address & 2 == 0 {
        (old & 0xffff_0000) | u32::from(value)
    } else {
        (old & 0x0000_ffff) | (u32::from(value) << 16)
    }
}

fn merge_byte(old: u16, address: u32, value: u8) -> u16 {
    if address & 1 == 0 {
        (old & 0xff00) | u16::from(value)
    } else {
        (old & 0x00ff) | (u16::from(value) << 8)
    }
}

#[allow(clippy::needless_pass_by_value)]
fn bus_fault(error: impl ToString) -> BusFault {
    BusFault::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use upse_iop_dma::{DmaEvent, MockSpu2Endpoint, SoundDmaChannel, Spu2MmioEndpoint};
    use upse_iop_irq::InterruptSource;
    use upse_iop_timers::{CounterBoundary, TimerId, TimingEvent};
    use upse_r3000::{Exception, StepEvent};

    use super::{HardwareEventKind, IOP_RESET_PROFILE, IopMachine, MachineConfig, MachineError};

    const PROGRAM_START: u32 = 0x0000_1000;
    const TRACE_INTERRUPTS: u32 = 0x0000_3000;
    const TRACE_MAIN: u32 = 0x0000_3004;
    const TRACE_SPU2: u32 = 0x0000_3010;

    fn lui(rt: u32, immediate: u16) -> u32 {
        (0x0f << 26) | (rt << 16) | u32::from(immediate)
    }

    fn ori(rt: u32, rs: u32, immediate: u16) -> u32 {
        (0x0d << 26) | (rs << 21) | (rt << 16) | u32::from(immediate)
    }

    fn addiu(rt: u32, rs: u32, immediate: i16) -> u32 {
        (0x09 << 26)
            | (rs << 21)
            | (rt << 16)
            | u32::from(u16::from_ne_bytes(immediate.to_ne_bytes()))
    }

    fn lw(rt: u32, offset: u16, base: u32) -> u32 {
        (0x23 << 26) | (base << 21) | (rt << 16) | u32::from(offset)
    }

    fn lh(rt: u32, offset: u16, base: u32) -> u32 {
        (0x21 << 26) | (base << 21) | (rt << 16) | u32::from(offset)
    }

    fn sw(rt: u32, offset: u16, base: u32) -> u32 {
        (0x2b << 26) | (base << 21) | (rt << 16) | u32::from(offset)
    }

    fn sh(rt: u32, offset: u16, base: u32) -> u32 {
        (0x29 << 26) | (base << 21) | (rt << 16) | u32::from(offset)
    }

    fn generated_program() -> Vec<u8> {
        let mut image = vec![0_u8; 0x1800];
        let handler = [
            lui(26, 0x1f80),
            sw(0, 0x1070, 26),
            lw(27, u16::try_from(TRACE_INTERRUPTS).unwrap(), 0),
            0,
            addiu(27, 27, 1),
            sw(27, u16::try_from(TRACE_INTERRUPTS).unwrap(), 0),
            (0x10 << 26) | (26 << 16) | (14 << 11),
            (26 << 21) | 8,
            0x4200_0010,
        ];
        write_words(&mut image, 0x80, &handler);

        let words = [
            lui(8, 0x1f80),
            addiu(10, 0, 0x2000),
            lui(9, 0x1111),
            ori(9, 9, 0x0001),
            sw(9, 0, 10),
            lui(9, 0x2222),
            ori(9, 9, 0x0002),
            sw(9, 4, 10),
            lui(9, 0x3333),
            ori(9, 9, 0x0003),
            sw(9, 8, 10),
            lui(9, 0x4444),
            ori(9, 9, 0x0004),
            sw(9, 12, 10),
            lui(11, 0x1f90),
            addiu(9, 0, 0x1234),
            sh(9, 0, 11),
            lh(12, 0, 11),
            0,
            sw(12, u16::try_from(TRACE_SPU2).unwrap(), 0),
            addiu(9, 0, 0x48),
            sw(9, 0x1074, 8),
            addiu(9, 0, 120),
            sw(9, 0x1128, 8),
            addiu(9, 0, 0x18),
            sw(9, 0x1124, 8),
            lui(9, 0x0090),
            sw(9, 0x10f4, 8),
            lui(9, 0x076d),
            ori(9, 9, 0x4321),
            sw(9, 0x10f0, 8),
            addiu(9, 0, 0x2000),
            sw(9, 0x10c0, 8),
            addiu(9, 0, 4),
            sw(9, 0x10c4, 8),
            lui(9, 0x0100),
            ori(9, 9, 1),
            sw(9, 0x10c8, 8),
            addiu(9, 0, 0x1111),
            sw(9, u16::try_from(TRACE_MAIN).unwrap(), 0),
            0x0800_0000 | (((PROGRAM_START + 40 * 4) >> 2) & 0x03ff_ffff),
            0,
        ];
        write_words(&mut image, PROGRAM_START as usize, &words);
        image
    }

    fn write_words(image: &mut [u8], address: usize, words: &[u32]) {
        for (index, word) in words.iter().enumerate() {
            let start = address + index * 4;
            image[start..start + 4].copy_from_slice(&word.to_le_bytes());
        }
    }

    #[test]
    fn reset_profile_has_hle_only_vectors_and_no_firmware_fallback() {
        assert_eq!(IOP_RESET_PROFILE.pc, 0xbfc0_0000);
        assert_eq!(IOP_RESET_PROFILE.exception_vector, 0x8000_0080);
        assert_eq!(IOP_RESET_PROFILE.bootstrap_exception_vector, 0xbfc0_0180);
        assert_ne!(IOP_RESET_PROFILE.status & (1 << 22), 0);
        assert_eq!(IOP_RESET_PROFILE.processor_id, 0x1f);
        let mut machine = IopMachine::new(MockSpu2Endpoint::new(), MachineConfig::default());
        assert!(matches!(machine.step(), Err(MachineError::Cpu(_))));
    }

    #[test]
    fn generated_bare_metal_program_runs_cpu_timers_dma_irqs_and_sound_mmio() {
        let program = generated_program();
        let mut machine = IopMachine::from_bare_metal(
            0,
            PROGRAM_START,
            &program,
            MockSpu2Endpoint::new(),
            MachineConfig::default(),
        )
        .unwrap();
        machine.cpu_mut().cop0_mut().status = 0x0000_0401;
        let mut interrupt_entries = 0;
        for _ in 0..600 {
            let step = machine.step().unwrap();
            if step.cpu.event == StepEvent::Exception(Exception::Interrupt) {
                interrupt_entries += 1;
            }
        }

        assert_eq!(machine.memory().read_u32(TRACE_MAIN).unwrap(), 0x1111);
        assert_eq!(machine.memory().read_u32(TRACE_SPU2).unwrap(), 0x1234);
        assert!(machine.memory().read_u32(TRACE_INTERRUPTS).unwrap() >= 2);
        assert!(interrupt_entries >= 2);
        assert_eq!(
            machine.sound().written_words(SoundDmaChannel::Core0),
            &[0x1111_0001, 0x2222_0002, 0x3333_0003, 0x4444_0004]
        );
        assert_eq!(
            machine.sound_mut().read_register(0x1f90_0000).unwrap(),
            0x1234
        );

        let events = machine.take_hardware_events();
        assert!(events.iter().any(|event| matches!(
            event.kind,
            HardwareEventKind::Dma(DmaEvent::Completed {
                channel: SoundDmaChannel::Core0,
                ..
            })
        )));
        assert!(events.iter().any(|event| matches!(
            event.kind,
            HardwareEventKind::Timing(TimingEvent::Counter {
                timer: TimerId::Timer2,
                boundary: CounterBoundary::Target
            })
        )));
        assert_eq!(
            machine.interrupt_controller().mask(),
            InterruptSource::Dma.bit() | InterruptSource::Timer2.bit()
        );
    }

    #[test]
    fn invalid_program_memory_and_unmodeled_mmio_are_diagnostic() {
        assert!(matches!(
            IopMachine::from_bare_metal(
                0,
                0x4000_0000,
                &[0; 4],
                MockSpu2Endpoint::new(),
                MachineConfig::default()
            ),
            Err(MachineError::InvalidEntry { .. })
        ));
        assert!(matches!(
            IopMachine::from_bare_metal(
                0x001f_ffff,
                0,
                &[0; 8],
                MockSpu2Endpoint::new(),
                MachineConfig::default()
            ),
            Err(MachineError::Program(_))
        ));

        let mut program = vec![0_u8; 12];
        write_words(&mut program, 0, &[lui(8, 0x1f80), lw(9, 0x1460, 8), 0]);
        let mut machine = IopMachine::from_bare_metal(
            0,
            0,
            &program,
            MockSpu2Endpoint::new(),
            MachineConfig::default(),
        )
        .unwrap();
        machine.step().unwrap();
        let error = machine.step().unwrap_err().to_string();
        assert!(error.contains("unmodeled IOP MMIO read at 0x1f801460"));
    }
}
