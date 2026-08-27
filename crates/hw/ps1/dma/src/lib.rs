// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS1 DMA registers with MDEC-input and sound transfers.

use thiserror::Error;
use upse_clock::{ClockError, Deadline, Ticks};
use upse_ps1_irq::{InterruptController, InterruptSource};
use upse_ps1_memory::Ps1Memory;
use upse_scheduler::{DueEvent, EventId, Scheduler, SchedulerError};

/// First DMA channel register.
pub const DMA_CHANNEL_START: u32 = 0x1f80_1080;
/// Last DMA channel register (channel 6 CHCR).
pub const DMA_CHANNEL_END: u32 = 0x1f80_10e8;
/// Last DMA channel halfword address (high half of channel 6 CHCR).
pub const DMA_CHANNEL_HALFWORD_END: u32 = DMA_CHANNEL_END + 2;
/// Last DMA global-control halfword address.
pub const DMA_CONTROL_END: u32 = 0x1f80_10f6;
/// Channel 0 memory address register.
pub const D0_MADR: u32 = 0x1f80_1080;
/// Channel 0 block control register.
pub const D0_BCR: u32 = 0x1f80_1084;
/// Channel 0 channel control register.
pub const D0_CHCR: u32 = 0x1f80_1088;
/// Channel 4 memory address register.
pub const D4_MADR: u32 = 0x1f80_10c0;
/// Channel 4 block control register.
pub const D4_BCR: u32 = 0x1f80_10c4;
/// Channel 4 channel control register.
pub const D4_CHCR: u32 = 0x1f80_10c8;
/// DMA priority control register.
pub const DPCR: u32 = 0x1f80_10f0;
/// DMA interrupt control register.
pub const DICR: u32 = 0x1f80_10f4;
/// Scheduler identity reserved for DMA channel 4 completion.
pub const CHANNEL4_EVENT: EventId = EventId::new(0x0004_0004);

const DPCR_RESET: u32 = 0x0765_4321;
const CHANNEL0_ENABLE: u32 = 1 << 3;
const CHANNEL4_ENABLE: u32 = 1 << 19;
const CHCR_DIRECTION_FROM_RAM: u32 = 1 << 0;
const CHCR_DECREMENT: u32 = 1 << 1;
const CHCR_SYNC_MASK: u32 = 3 << 9;
const CHCR_START: u32 = 1 << 24;
const CHCR_TRIGGER: u32 = 1 << 28;
const CHCR_WRITABLE: u32 =
    CHCR_DIRECTION_FROM_RAM | CHCR_DECREMENT | CHCR_SYNC_MASK | CHCR_START | CHCR_TRIGGER;
const DICR_FORCE: u32 = 1 << 15;
const DICR_CHANNEL_MASKS: u32 = 0x007f_0000;
const DICR_MASTER_ENABLE: u32 = 1 << 23;
const DICR_CHANNEL_FLAGS: u32 = 0x7f00_0000;
const DICR_MASTER_FLAG: u32 = 1 << 31;
const DICR_WRITABLE_CONTROL: u32 = 0x00ff_803f;
/// Channel 4 interrupt-enable bit in `DICR`.
pub const DICR_CHANNEL4_MASK: u32 = 1 << 20;
const DICR_CHANNEL4_FLAG: u32 = 1 << 28;
const DICR_CHANNEL0_FLAG: u32 = 1 << 24;
const RAM_ADDRESS_MASK: u32 = 0x00ff_fffc;
const CYCLES_PER_WORD: u64 = 4;
const MAX_RAM_WORDS: u64 = (2 * 1024 * 1024) / 4;
const CHANNEL_COUNT: usize = 7;
const CHANNEL_STRIDE: u32 = 0x10;
const CHANNEL_REGISTER_COUNT: usize = 3;
const CHANNEL4: usize = 4;
const CHANNEL0: usize = 0;

/// MDEC command port consumed by DMA channel 0.
pub trait MdecDmaEndpoint {
    /// Accepts one little-endian 32-bit word from main RAM.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError`] if the device cannot accept the word.
    fn write_word(&mut self, value: u32) -> Result<(), EndpointError>;
}

/// Sound-device port consumed by DMA channel 4.
pub trait SoundDmaEndpoint {
    /// Accepts one little-endian 32-bit word from main RAM.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError`] if the device cannot accept the word.
    fn write_word(&mut self, value: u32) -> Result<(), EndpointError>;

    /// Supplies one little-endian 32-bit word for main RAM.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError`] if the device cannot supply the word.
    fn read_word(&mut self) -> Result<u32, EndpointError>;
}

/// Typed interrupt output consumed by the DMA component.
pub trait InterruptSink {
    /// Latches or records one DMA interrupt request.
    fn request(&mut self, source: InterruptSource);
}

impl InterruptSink for InterruptController {
    fn request(&mut self, source: InterruptSource) {
        InterruptController::request(self, source);
    }
}

/// A device endpoint rejected or failed a transfer word.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct EndpointError {
    message: String,
}

impl EndpointError {
    /// Constructs an endpoint diagnostic.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns the endpoint diagnostic.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Invalid DMA programming or transfer failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DmaError {
    /// Address does not select a modeled DMA register.
    #[error("invalid PS1 DMA register address {address:#010x}")]
    InvalidRegister {
        /// Physical address supplied by the machine.
        address: u32,
    },
    /// Channel 4 linked-list or reserved synchronization mode is unsupported.
    #[error("unsupported DMA channel 4 synchronization mode {mode}")]
    UnsupportedSync {
        /// Two-bit synchronization field.
        mode: u8,
    },
    /// Guest word count exceeds the complete emulated RAM allocation.
    #[error("DMA transfer of {words} words exceeds emulated RAM")]
    TransferTooLarge {
        /// Guest-programmed transfer size.
        words: u64,
    },
    /// Transfer addresses leave physical main RAM.
    #[error(
        "DMA transfer range leaves RAM: base {base:#010x}, {words} words, decrement={decrement}"
    )]
    RamRange {
        /// Masked physical start address.
        base: u32,
        /// Transfer word count.
        words: u64,
        /// Whether addresses move downward.
        decrement: bool,
    },
    /// Scheduler delivered a different or obsolete event.
    #[error("stale DMA completion event")]
    StaleEvent,
    /// Device endpoint failed while transferring a word.
    #[error("DMA endpoint failure: {0}")]
    Endpoint(#[from] EndpointError),
    /// Channel 0 was programmed in a direction unsupported by MDEC input.
    #[error("MDEC input DMA direction is device-to-RAM")]
    MdecDirection,
    /// Channel 0 linked-list or reserved synchronization mode is unsupported.
    #[error("unsupported MDEC input DMA synchronization mode {mode}")]
    UnsupportedMdecSync {
        /// Two-bit synchronization field.
        mode: u8,
    },
    /// Scheduler insertion sequence was exhausted.
    #[error("DMA scheduler failure")]
    Scheduler,
    /// Completion-cycle arithmetic exceeded `u64`.
    #[error("DMA timing overflow")]
    ClockOverflow,
}

impl From<SchedulerError> for DmaError {
    fn from(_: SchedulerError) -> Self {
        Self::Scheduler
    }
}

impl From<ClockError> for DmaError {
    fn from(_: ClockError) -> Self {
        Self::ClockOverflow
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingTransfer {
    deadline: Deadline,
    words: u64,
}

/// Instance-owned PS1 DMA controller implementing MDEC input and sound channels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DmaController {
    madr: u32,
    bcr: u32,
    chcr: u32,
    register_only_channels: [[u32; CHANNEL_REGISTER_COUNT]; CHANNEL_COUNT],
    dpcr: u32,
    dicr_control: u32,
    dicr_flags: u32,
    pending: Option<PendingTransfer>,
}

impl Default for DmaController {
    fn default() -> Self {
        Self::new()
    }
}

impl DmaController {
    /// Constructs reset DMA registers with channel 4 disabled.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            madr: 0,
            bcr: 0,
            chcr: 0,
            register_only_channels: [[0; CHANNEL_REGISTER_COUNT]; CHANNEL_COUNT],
            dpcr: DPCR_RESET,
            dicr_control: 0,
            dicr_flags: 0,
            pending: None,
        }
    }

    /// Returns the scheduled channel 4 completion, if active.
    #[must_use]
    pub const fn completion_deadline(&self) -> Option<Deadline> {
        match self.pending {
            Some(pending) => Some(pending.deadline),
            None => None,
        }
    }

    /// Reads a modeled 32-bit DMA register.
    ///
    /// # Errors
    ///
    /// Returns [`DmaError::InvalidRegister`] for addresses outside the seven
    /// channel register triplets, `DPCR`, and `DICR`.
    pub fn read(&self, address: u32) -> Result<u32, DmaError> {
        if let Some((channel, register)) = channel_register(address) {
            return Ok(if channel == CHANNEL4 {
                [self.madr, self.bcr, self.chcr][register]
            } else {
                self.register_only_channels[channel][register]
            });
        }
        match address {
            DPCR => Ok(self.dpcr),
            DICR => Ok(self.dicr()),
            _ => Err(DmaError::InvalidRegister { address }),
        }
    }

    /// Writes a modeled DMA register and schedules newly active channel 4 work.
    ///
    /// Enabling `DPCR` after an already-started `CHCR` also starts the channel.
    /// `DICR` writes apply write-one-to-clear flag behavior and may raise the
    /// aggregate DMA interrupt on a low-to-high master transition.
    ///
    /// # Errors
    ///
    /// Returns [`DmaError`] for invalid registers, modes, sizes, scheduling, or
    /// timestamp overflow.
    pub fn write<S: InterruptSink>(
        &mut self,
        address: u32,
        value: u32,
        now: Deadline,
        scheduler: &mut Scheduler,
        sink: &mut S,
    ) -> Result<(), DmaError> {
        if let Some((channel, register)) = channel_register(address) {
            if channel == CHANNEL4 {
                match register {
                    0 => self.madr = value & RAM_ADDRESS_MASK,
                    1 => self.bcr = value,
                    2 => {
                        self.chcr = value & CHCR_WRITABLE;
                        self.reschedule(now, scheduler)?;
                    }
                    _ => unreachable!("validated DMA register index"),
                }
            } else if channel == CHANNEL0 {
                self.register_only_channels[channel][register] = match register {
                    0 => value & RAM_ADDRESS_MASK,
                    1 => value,
                    2 => value & CHCR_WRITABLE,
                    _ => unreachable!("validated DMA register index"),
                };
            } else {
                self.register_only_channels[channel][register] = match register {
                    0 => value & RAM_ADDRESS_MASK,
                    1 => value,
                    // Non-audio channels have no machine-visible endpoint in
                    // the PSF profile. Complete starts immediately so game
                    // initialization cannot block while polling CHCR.
                    2 => value & !(CHCR_START | CHCR_TRIGGER),
                    _ => unreachable!("validated DMA register index"),
                };
            }
            return Ok(());
        }
        match address {
            DPCR => {
                self.dpcr = value;
                self.reschedule(now, scheduler)?;
            }
            DICR => self.write_dicr(value, sink),
            _ => return Err(DmaError::InvalidRegister { address }),
        }
        Ok(())
    }

    /// Immediately services one active MDEC-input transfer.
    ///
    /// The audio-oriented machine has no video timing consumer, so channel 0
    /// completes at its next MMIO programming boundary. Sound channel 4 keeps
    /// its scheduled cycle timing.
    ///
    /// # Errors
    ///
    /// Returns [`DmaError`] for invalid direction, synchronization, RAM range,
    /// word count, or endpoint input.
    pub fn service_mdec_in<E: MdecDmaEndpoint, S: InterruptSink>(
        &mut self,
        memory: &Ps1Memory,
        endpoint: &mut E,
        sink: &mut S,
    ) -> Result<bool, DmaError> {
        let registers = &mut self.register_only_channels[CHANNEL0];
        let [madr, bcr, chcr] = *registers;
        if self.dpcr & CHANNEL0_ENABLE == 0 || chcr & CHCR_START == 0 {
            return Ok(false);
        }
        if chcr & CHCR_DIRECTION_FROM_RAM == 0 {
            return Err(DmaError::MdecDirection);
        }
        let sync = sync_mode(chcr);
        let active = match sync {
            0 => chcr & CHCR_TRIGGER != 0,
            1 => true,
            mode => return Err(DmaError::UnsupportedMdecSync { mode }),
        };
        if !active {
            return Ok(false);
        }
        let words = transfer_words(bcr, sync)?;
        let decrement = chcr & CHCR_DECREMENT != 0;
        validate_ram_range(madr, words, decrement, memory.ram().len())?;
        for index in 0..words {
            let address = transfer_address(madr, index, decrement)?;
            let offset = usize::try_from(address).map_err(|_| DmaError::RamRange {
                base: madr,
                words,
                decrement,
            })?;
            endpoint.write_word(read_ram_word(memory.ram(), offset))?;
        }
        let byte_count = words.checked_mul(4).ok_or(DmaError::ClockOverflow)?;
        let byte_count = u32::try_from(byte_count).map_err(|_| DmaError::ClockOverflow)?;
        registers[0] = if decrement {
            madr.wrapping_sub(byte_count)
        } else {
            madr.wrapping_add(byte_count)
        } & RAM_ADDRESS_MASK;
        registers[2] &= !(CHCR_START | CHCR_TRIGGER);
        self.finish_channel0(sink);
        Ok(true)
    }

    /// Completes the scheduler event and transfers exact words through the port.
    ///
    /// # Errors
    ///
    /// Returns [`DmaError::StaleEvent`] for another event/deadline,
    /// [`DmaError::RamRange`] if the programmed address range leaves RAM, or
    /// [`DmaError::Endpoint`] when the sound port fails.
    pub fn complete<E: SoundDmaEndpoint, S: InterruptSink>(
        &mut self,
        event: DueEvent,
        memory: &mut Ps1Memory,
        endpoint: &mut E,
        sink: &mut S,
    ) -> Result<(), DmaError> {
        let Some(pending) = self.pending else {
            return Err(DmaError::StaleEvent);
        };
        if event.id != CHANNEL4_EVENT || event.deadline != pending.deadline {
            return Err(DmaError::StaleEvent);
        }
        self.pending = None;
        let decrement = self.chcr & CHCR_DECREMENT != 0;
        let from_ram = self.chcr & CHCR_DIRECTION_FROM_RAM != 0;
        validate_ram_range(self.madr, pending.words, decrement, memory.ram().len())?;

        for index in 0..pending.words {
            let address = transfer_address(self.madr, index, decrement)?;
            let offset = usize::try_from(address).map_err(|_| DmaError::RamRange {
                base: self.madr,
                words: pending.words,
                decrement,
            })?;
            if from_ram {
                let value = read_ram_word(memory.ram(), offset);
                endpoint.write_word(value)?;
            } else {
                let value = endpoint.read_word()?;
                write_ram_word(memory.ram_mut(), offset, value);
            }
        }

        let byte_count = pending
            .words
            .checked_mul(4)
            .ok_or(DmaError::ClockOverflow)?;
        let byte_count = u32::try_from(byte_count).map_err(|_| DmaError::ClockOverflow)?;
        self.madr = if decrement {
            self.madr.wrapping_sub(byte_count)
        } else {
            self.madr.wrapping_add(byte_count)
        } & RAM_ADDRESS_MASK;
        self.chcr &= !(CHCR_START | CHCR_TRIGGER);
        self.finish_channel4(sink);
        Ok(())
    }

    fn dicr(&self) -> u32 {
        self.dicr_control
            | self.dicr_flags
            | if self.master_irq() {
                DICR_MASTER_FLAG
            } else {
                0
            }
    }

    fn master_irq(&self) -> bool {
        self.dicr_control & DICR_FORCE != 0
            || (self.dicr_control & DICR_MASTER_ENABLE != 0
                && ((self.dicr_control & DICR_CHANNEL_MASKS) << 8) & self.dicr_flags != 0)
    }

    fn write_dicr<S: InterruptSink>(&mut self, value: u32, sink: &mut S) {
        let was_asserted = self.master_irq();
        self.dicr_control = value & DICR_WRITABLE_CONTROL;
        self.dicr_flags &= !(value & DICR_CHANNEL_FLAGS);
        if !was_asserted && self.master_irq() {
            sink.request(InterruptSource::Dma);
        }
    }

    fn finish_channel4<S: InterruptSink>(&mut self, sink: &mut S) {
        let was_asserted = self.master_irq();
        self.dicr_flags |= DICR_CHANNEL4_FLAG;
        if !was_asserted && self.master_irq() {
            sink.request(InterruptSource::Dma);
        }
    }

    fn finish_channel0<S: InterruptSink>(&mut self, sink: &mut S) {
        let was_asserted = self.master_irq();
        self.dicr_flags |= DICR_CHANNEL0_FLAG;
        if !was_asserted && self.master_irq() {
            sink.request(InterruptSource::Dma);
        }
    }

    fn reschedule(&mut self, now: Deadline, scheduler: &mut Scheduler) -> Result<(), DmaError> {
        scheduler.cancel(CHANNEL4_EVENT);
        self.pending = None;
        if self.dpcr & CHANNEL4_ENABLE == 0 || self.chcr & CHCR_START == 0 {
            return Ok(());
        }
        let sync = sync_mode(self.chcr);
        let active = match sync {
            0 => self.chcr & CHCR_TRIGGER != 0,
            1 => true,
            mode => return Err(DmaError::UnsupportedSync { mode }),
        };
        if !active {
            return Ok(());
        }
        let words = transfer_words(self.bcr, sync)?;
        let cycles = words
            .checked_mul(CYCLES_PER_WORD)
            .ok_or(DmaError::ClockOverflow)?;
        let deadline = now.checked_advance(Ticks::new(cycles))?;
        scheduler.schedule(CHANNEL4_EVENT, deadline)?;
        self.pending = Some(PendingTransfer { deadline, words });
        Ok(())
    }
}

fn channel_register(address: u32) -> Option<(usize, usize)> {
    let offset = address.checked_sub(DMA_CHANNEL_START)?;
    let channel = usize::try_from(offset / CHANNEL_STRIDE).ok()?;
    if channel >= CHANNEL_COUNT {
        return None;
    }
    let register = usize::try_from((offset % CHANNEL_STRIDE) / 4).ok()?;
    if offset % 4 != 0 || register >= CHANNEL_REGISTER_COUNT {
        return None;
    }
    Some((channel, register))
}

fn sync_mode(chcr: u32) -> u8 {
    let bits = (chcr & CHCR_SYNC_MASK) >> 9;
    u8::try_from(bits).expect("two-bit DMA sync mode")
}

fn transfer_words(bcr: u32, sync: u8) -> Result<u64, DmaError> {
    let low = u64::from(bcr & 0xffff);
    let low = if low == 0 { 0x1_0000 } else { low };
    let words = if sync == 0 {
        low
    } else {
        let high = u64::from(bcr >> 16);
        let high = if high == 0 { 0x1_0000 } else { high };
        low.checked_mul(high)
            .ok_or(DmaError::TransferTooLarge { words: u64::MAX })?
    };
    if words > MAX_RAM_WORDS {
        return Err(DmaError::TransferTooLarge { words });
    }
    Ok(words)
}

fn validate_ram_range(
    base: u32,
    words: u64,
    decrement: bool,
    ram_len: usize,
) -> Result<(), DmaError> {
    let last = transfer_address(base, words - 1, decrement)?;
    let first = usize::try_from(base).map_err(|_| DmaError::RamRange {
        base,
        words,
        decrement,
    })?;
    let last = usize::try_from(last).map_err(|_| DmaError::RamRange {
        base,
        words,
        decrement,
    })?;
    if first.checked_add(4).is_none_or(|end| end > ram_len)
        || last.checked_add(4).is_none_or(|end| end > ram_len)
    {
        return Err(DmaError::RamRange {
            base,
            words,
            decrement,
        });
    }
    Ok(())
}

fn transfer_address(base: u32, index: u64, decrement: bool) -> Result<u32, DmaError> {
    let offset = index.checked_mul(4).ok_or(DmaError::ClockOverflow)?;
    let offset = u32::try_from(offset).map_err(|_| DmaError::ClockOverflow)?;
    if decrement {
        base.checked_sub(offset)
    } else {
        base.checked_add(offset)
    }
    .ok_or(DmaError::RamRange {
        base,
        words: index + 1,
        decrement,
    })
}

fn read_ram_word(ram: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        ram[offset..offset + 4]
            .try_into()
            .expect("validated DMA RAM word"),
    )
}

fn write_ram_word(ram: &mut [u8], offset: usize, value: u32) {
    ram[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use upse_clock::Deadline;
    use upse_ps1_irq::{InterruptController, InterruptSource};
    use upse_ps1_memory::{OpenBusPolicy, Ps1Memory};
    use upse_r3000::{Bus, BusFault, Cpu, ResetProfile};
    use upse_scheduler::Scheduler;

    use super::{
        CHANNEL0_ENABLE, CHANNEL4_ENABLE, CHCR_DECREMENT, CHCR_DIRECTION_FROM_RAM, CHCR_START,
        CHCR_TRIGGER, D0_BCR, D0_CHCR, D0_MADR, D4_BCR, D4_CHCR, D4_MADR, DICR, DICR_CHANNEL0_FLAG,
        DICR_CHANNEL4_FLAG, DICR_CHANNEL4_MASK, DICR_MASTER_ENABLE, DICR_MASTER_FLAG,
        DMA_CHANNEL_START, DPCR, DmaController, DmaError, EndpointError, InterruptSink,
        MdecDmaEndpoint, SoundDmaEndpoint,
    };

    #[derive(Default)]
    struct Endpoint {
        written: Vec<u32>,
        reads: VecDeque<u32>,
    }

    impl SoundDmaEndpoint for Endpoint {
        fn write_word(&mut self, value: u32) -> Result<(), EndpointError> {
            self.written.push(value);
            Ok(())
        }

        fn read_word(&mut self) -> Result<u32, EndpointError> {
            self.reads
                .pop_front()
                .ok_or_else(|| EndpointError::new("empty synthetic endpoint"))
        }
    }

    impl MdecDmaEndpoint for Endpoint {
        fn write_word(&mut self, value: u32) -> Result<(), EndpointError> {
            self.written.push(value);
            Ok(())
        }
    }

    #[derive(Default)]
    struct Log(Vec<InterruptSource>);

    impl InterruptSink for Log {
        fn request(&mut self, source: InterruptSource) {
            self.0.push(source);
        }
    }

    fn write(
        dma: &mut DmaController,
        address: u32,
        value: u32,
        scheduler: &mut Scheduler,
        sink: &mut impl InterruptSink,
    ) -> Result<(), DmaError> {
        dma.write(address, value, Deadline::ZERO, scheduler, sink)
    }

    fn enable(dma: &mut DmaController, scheduler: &mut Scheduler, sink: &mut impl InterruptSink) {
        write(
            dma,
            DPCR,
            super::DPCR_RESET | CHANNEL4_ENABLE,
            scheduler,
            sink,
        )
        .unwrap();
    }

    #[test]
    fn ram_to_sound_transfer_completes_at_four_cycles_per_word() {
        let mut dma = DmaController::new();
        let mut memory = Ps1Memory::new(OpenBusPolicy::Strict);
        memory.write_u32(0x100, 0x1122_3344).unwrap();
        memory.write_u32(0x104, 0xaabb_ccdd).unwrap();
        let mut endpoint = Endpoint::default();
        let mut scheduler = Scheduler::new();
        let mut irq = InterruptController::new();
        enable(&mut dma, &mut scheduler, &mut irq);
        write(
            &mut dma,
            DICR,
            DICR_MASTER_ENABLE | DICR_CHANNEL4_MASK,
            &mut scheduler,
            &mut irq,
        )
        .unwrap();
        write(&mut dma, D4_MADR, 0x100, &mut scheduler, &mut irq).unwrap();
        write(&mut dma, D4_BCR, 2, &mut scheduler, &mut irq).unwrap();
        write(
            &mut dma,
            D4_CHCR,
            CHCR_DIRECTION_FROM_RAM | CHCR_START | CHCR_TRIGGER,
            &mut scheduler,
            &mut irq,
        )
        .unwrap();
        assert_eq!(scheduler.next_deadline(), Some(Deadline::new(8)));
        assert_eq!(scheduler.pop_due(Deadline::new(7)), None);
        let event = scheduler.pop_due(Deadline::new(8)).unwrap();
        dma.complete(event, &mut memory, &mut endpoint, &mut irq)
            .unwrap();
        assert_eq!(endpoint.written, [0x1122_3344, 0xaabb_ccdd]);
        assert_eq!(dma.read(D4_MADR).unwrap(), 0x108);
        assert_eq!(dma.read(D4_CHCR).unwrap() & CHCR_START, 0);
        assert_ne!(dma.read(DICR).unwrap() & DICR_CHANNEL4_FLAG, 0);
        assert_ne!(dma.read(DICR).unwrap() & DICR_MASTER_FLAG, 0);
        assert_eq!(irq.status(), InterruptSource::Dma.bit());
    }

    #[test]
    fn ram_to_mdec_transfer_services_exact_words_and_completes() {
        let mut dma = DmaController::new();
        let mut memory = Ps1Memory::new(OpenBusPolicy::Strict);
        for index in 0..32_u32 {
            memory.write_u32(0x100 + index * 4, index).unwrap();
        }
        let mut endpoint = Endpoint::default();
        let mut scheduler = Scheduler::new();
        let mut irq = InterruptController::new();
        write(
            &mut dma,
            DPCR,
            super::DPCR_RESET | CHANNEL0_ENABLE,
            &mut scheduler,
            &mut irq,
        )
        .unwrap();
        write(&mut dma, D0_MADR, 0x100, &mut scheduler, &mut irq).unwrap();
        write(&mut dma, D0_BCR, 0x0001_0020, &mut scheduler, &mut irq).unwrap();
        write(
            &mut dma,
            D0_CHCR,
            CHCR_DIRECTION_FROM_RAM | (1 << 9) | CHCR_START,
            &mut scheduler,
            &mut irq,
        )
        .unwrap();

        assert!(
            dma.service_mdec_in(&memory, &mut endpoint, &mut irq)
                .unwrap()
        );
        assert_eq!(endpoint.written, (0..32).collect::<Vec<_>>());
        assert_eq!(dma.read(D0_MADR).unwrap(), 0x180);
        assert_eq!(dma.read(D0_CHCR).unwrap() & CHCR_START, 0);
        assert_ne!(dma.read(DICR).unwrap() & DICR_CHANNEL0_FLAG, 0);
    }

    #[test]
    fn non_audio_channel_registers_read_back_without_remaining_busy() {
        let mut dma = DmaController::new();
        let mut scheduler = Scheduler::new();
        let mut log = Log::default();
        let gpu_madr = DMA_CHANNEL_START + 2 * 0x10;
        let gpu_bcr = gpu_madr + 4;
        let gpu_chcr = gpu_madr + 8;

        write(&mut dma, gpu_madr, 0x8012_3457, &mut scheduler, &mut log).unwrap();
        write(&mut dma, gpu_bcr, 0x1234_5678, &mut scheduler, &mut log).unwrap();
        write(&mut dma, gpu_chcr, 0x1100_0401, &mut scheduler, &mut log).unwrap();

        assert_eq!(dma.read(gpu_madr).unwrap(), 0x0012_3454);
        assert_eq!(dma.read(gpu_bcr).unwrap(), 0x1234_5678);
        assert_eq!(dma.read(gpu_chcr).unwrap(), 0x0000_0401);
        assert!(scheduler.is_empty());
        assert!(log.0.is_empty());
    }

    #[test]
    fn sound_to_ram_decrement_transfer_preserves_word_order() {
        let mut dma = DmaController::new();
        let mut memory = Ps1Memory::new(OpenBusPolicy::Strict);
        let mut endpoint = Endpoint {
            written: Vec::new(),
            reads: VecDeque::from([0x0102_0304, 0xa0b0_c0d0]),
        };
        let mut scheduler = Scheduler::new();
        let mut log = Log::default();
        enable(&mut dma, &mut scheduler, &mut log);
        write(&mut dma, D4_MADR, 0x104, &mut scheduler, &mut log).unwrap();
        write(&mut dma, D4_BCR, 2, &mut scheduler, &mut log).unwrap();
        write(
            &mut dma,
            D4_CHCR,
            CHCR_DECREMENT | CHCR_START | CHCR_TRIGGER,
            &mut scheduler,
            &mut log,
        )
        .unwrap();
        let event = scheduler.pop_due(Deadline::new(8)).unwrap();
        dma.complete(event, &mut memory, &mut endpoint, &mut log)
            .unwrap();
        assert_eq!(memory.read_u32(0x104).unwrap(), 0x0102_0304);
        assert_eq!(memory.read_u32(0x100).unwrap(), 0xa0b0_c0d0);
    }

    #[test]
    fn disabled_manual_and_invalid_modes_do_not_run() {
        let mut dma = DmaController::new();
        let mut scheduler = Scheduler::new();
        let mut log = Log::default();
        write(&mut dma, D4_BCR, 1, &mut scheduler, &mut log).unwrap();
        write(
            &mut dma,
            D4_CHCR,
            CHCR_START | CHCR_TRIGGER,
            &mut scheduler,
            &mut log,
        )
        .unwrap();
        assert!(scheduler.is_empty());
        enable(&mut dma, &mut scheduler, &mut log);
        assert_eq!(scheduler.next_deadline(), Some(Deadline::new(4)));
        write(&mut dma, D4_CHCR, CHCR_START, &mut scheduler, &mut log).unwrap();
        assert!(scheduler.is_empty());
        assert_eq!(
            write(
                &mut dma,
                D4_CHCR,
                CHCR_START | (2 << 9),
                &mut scheduler,
                &mut log,
            ),
            Err(DmaError::UnsupportedSync { mode: 2 })
        );
    }

    #[test]
    fn checked_counts_and_ranges_cannot_escape_ram() {
        let mut dma = DmaController::new();
        let mut scheduler = Scheduler::new();
        let mut log = Log::default();
        enable(&mut dma, &mut scheduler, &mut log);
        write(&mut dma, D4_BCR, 0x0009_0000, &mut scheduler, &mut log).unwrap();
        assert!(matches!(
            write(
                &mut dma,
                D4_CHCR,
                CHCR_START | (1 << 9),
                &mut scheduler,
                &mut log,
            ),
            Err(DmaError::TransferTooLarge { .. })
        ));

        write(&mut dma, D4_BCR, 2, &mut scheduler, &mut log).unwrap();
        write(&mut dma, D4_MADR, 0, &mut scheduler, &mut log).unwrap();
        write(
            &mut dma,
            D4_CHCR,
            CHCR_DECREMENT | CHCR_START | CHCR_TRIGGER,
            &mut scheduler,
            &mut log,
        )
        .unwrap();
        let event = scheduler.pop_due(Deadline::new(8)).unwrap();
        let mut memory = Ps1Memory::new(OpenBusPolicy::Strict);
        let mut endpoint = Endpoint::default();
        assert!(matches!(
            dma.complete(event, &mut memory, &mut endpoint, &mut log),
            Err(DmaError::RamRange { .. })
        ));
        assert!(endpoint.written.is_empty());
    }

    #[test]
    fn interrupt_master_edge_and_write_one_ack_are_stable() {
        let mut dma = DmaController::new();
        let mut scheduler = Scheduler::new();
        let mut log = Log::default();
        write(&mut dma, DICR, DICR_CHANNEL4_MASK, &mut scheduler, &mut log).unwrap();
        dma.dicr_flags = DICR_CHANNEL4_FLAG;
        write(
            &mut dma,
            DICR,
            DICR_MASTER_ENABLE | DICR_CHANNEL4_MASK,
            &mut scheduler,
            &mut log,
        )
        .unwrap();
        assert_eq!(log.0, [InterruptSource::Dma]);
        assert_ne!(dma.read(DICR).unwrap() & DICR_MASTER_FLAG, 0);
        write(
            &mut dma,
            DICR,
            DICR_MASTER_ENABLE | DICR_CHANNEL4_MASK | DICR_CHANNEL4_FLAG,
            &mut scheduler,
            &mut log,
        )
        .unwrap();
        assert_eq!(dma.read(DICR).unwrap() & DICR_CHANNEL4_FLAG, 0);
        assert_eq!(dma.read(DICR).unwrap() & DICR_MASTER_FLAG, 0);
    }

    struct CpuDmaBus {
        words: Vec<u32>,
        dma: DmaController,
        memory: Ps1Memory,
        endpoint: Endpoint,
        scheduler: Scheduler,
        irq: InterruptController,
        now: u64,
    }

    impl CpuDmaBus {
        fn advance(&mut self, cycles: u32) {
            self.now += u64::from(cycles);
            while let Some(event) = self.scheduler.pop_due(Deadline::new(self.now)) {
                self.dma
                    .complete(event, &mut self.memory, &mut self.endpoint, &mut self.irq)
                    .unwrap();
            }
        }
    }

    impl Bus for CpuDmaBus {
        fn read_u8(&mut self, _address: u32) -> Result<u8, BusFault> {
            Err(BusFault::new("byte access is not used"))
        }

        fn read_u16(&mut self, _address: u32) -> Result<u16, BusFault> {
            Err(BusFault::new("halfword access is not used"))
        }

        fn read_u32(&mut self, address: u32) -> Result<u32, BusFault> {
            if address == upse_ps1_irq::I_STAT {
                return self
                    .irq
                    .read(address)
                    .map_err(|error| BusFault::new(error.to_string()));
            }
            let index =
                usize::try_from(address / 4).map_err(|error| BusFault::new(error.to_string()))?;
            self.words
                .get(index)
                .copied()
                .ok_or_else(|| BusFault::new("instruction fetch outside synthetic RAM"))
        }

        fn write_u8(&mut self, _address: u32, _value: u8) -> Result<(), BusFault> {
            Err(BusFault::new("byte access is not used"))
        }

        fn write_u16(&mut self, _address: u32, _value: u16) -> Result<(), BusFault> {
            Err(BusFault::new("halfword access is not used"))
        }

        fn write_u32(&mut self, address: u32, value: u32) -> Result<(), BusFault> {
            if address == upse_ps1_irq::I_STAT {
                return self
                    .irq
                    .write(address, value)
                    .map_err(|error| BusFault::new(error.to_string()));
            }
            self.dma
                .write(
                    address,
                    value,
                    Deadline::new(self.now),
                    &mut self.scheduler,
                    &mut self.irq,
                )
                .map_err(|error| BusFault::new(error.to_string()))
        }

        fn interrupt_pending(&self) -> bool {
            self.irq.pending()
        }
    }

    #[test]
    fn tiny_cpu_program_transfers_sound_words_and_observes_completion() {
        let mut bus = CpuDmaBus {
            words: vec![
                0x3c08_1f80, // lui t0,0x1f80
                0x3c09_076d, // lui t1,0x076d
                0x3529_4321, // ori t1,t1,0x4321
                0xad09_10f0, // sw t1,DPCR(t0)
                0x3c09_0090, // lui t1,0x0090 (DMA master + channel 4 IRQ)
                0xad09_10f4, // sw t1,DICR(t0)
                0x2409_0100, // addiu t1,zero,0x100
                0xad09_10c0, // sw t1,D4_MADR(t0)
                0x2409_0002, // addiu t1,zero,2
                0xad09_10c4, // sw t1,D4_BCR(t0)
                0x3c09_1100, // lui t1,0x1100
                0x3529_0001, // ori t1,t1,1 (RAM to device)
                0xad09_10c8, // sw t1,D4_CHCR(t0)
                0x8d0a_1070, // lw t2,I_STAT(t0)
                0x0000_0000, // load-delay slot
                0x314a_0008, // andi t2,t2,DMA
                0x1140_fffc, // beq t2,zero,poll
                0x0000_0000, // branch-delay slot
                0xad00_1070, // sw zero,I_STAT(t0)
                0x2402_0001, // addiu v0,zero,1
            ],
            dma: DmaController::new(),
            memory: Ps1Memory::new(OpenBusPolicy::Strict),
            endpoint: Endpoint::default(),
            scheduler: Scheduler::new(),
            irq: InterruptController::new(),
            now: 0,
        };
        bus.memory.write_u32(0x100, 0x7654_3210).unwrap();
        bus.memory.write_u32(0x104, 0xfedc_ba98).unwrap();
        let mut cpu = Cpu::new(ResetProfile {
            pc: 0,
            exception_vector: 0x80,
            bootstrap_exception_vector: 0x80,
            status: 0,
            processor_id: 2,
        });
        let mut trace = Vec::new();
        for _ in 0..64 {
            let outcome = cpu.step(&mut bus).unwrap();
            trace.push(outcome.pc);
            bus.advance(outcome.cycles);
            if cpu.register(2) == Some(1) {
                break;
            }
        }
        assert_eq!(cpu.register(2), Some(1));
        assert_eq!(bus.endpoint.written, [0x7654_3210, 0xfedc_ba98]);
        assert_eq!(bus.irq.status(), 0);
        assert!(
            trace
                .windows(7)
                .any(|window| window == [52, 56, 60, 64, 68, 72, 76])
        );
    }
}
