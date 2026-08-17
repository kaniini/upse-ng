// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS2 IOP DMA register banks with SPU2 channels 4 and 7.

#![allow(clippy::cast_possible_truncation)]

use std::collections::VecDeque;

use thiserror::Error;
use upse_clock::{ClockError, Deadline, Ticks};
use upse_iop_irq::{InterruptSink, InterruptSource};
use upse_iop_memory::IopMemory;
use upse_scheduler::{DueEvent, EventId, Scheduler, SchedulerError};

/// First register of DMA channel 0.
pub const DMA1_CHANNEL_START: u32 = 0x1f80_1080;
/// Last channel-register word in the first DMA bank.
pub const DMA1_CHANNEL_END: u32 = 0x1f80_10ec;
/// First register of DMA channel 7.
pub const DMA2_CHANNEL_START: u32 = 0x1f80_1500;
/// Last channel-register word in the second DMA bank.
pub const DMA2_CHANNEL_END: u32 = 0x1f80_155c;
/// DMA priority control for channels 0 through 6.
pub const DPCR1: u32 = 0x1f80_10f0;
/// DMA interrupt control for channels 0 through 6.
pub const DICR1: u32 = 0x1f80_10f4;
/// DMA priority control for channels 7 through 12.
pub const DPCR2: u32 = 0x1f80_1570;
/// DMA interrupt control for channels 7 through 12.
pub const DICR2: u32 = 0x1f80_1574;
/// Second DMA controller enable register.
pub const DMAC_ENABLE: u32 = 0x1f80_1578;
/// Channel 4 memory address register.
pub const D4_MADR: u32 = 0x1f80_10c0;
/// Channel 4 block control register.
pub const D4_BCR: u32 = 0x1f80_10c4;
/// Channel 4 channel control register.
pub const D4_CHCR: u32 = 0x1f80_10c8;
/// Channel 7 memory address register.
pub const D7_MADR: u32 = 0x1f80_1500;
/// Channel 7 block control register.
pub const D7_BCR: u32 = 0x1f80_1504;
/// Channel 7 channel control register.
pub const D7_CHCR: u32 = 0x1f80_1508;
/// Scheduler identity for channel 4 completion.
pub const CHANNEL4_EVENT: EventId = EventId::new(0x0204_0004);
/// Scheduler identity for channel 7 completion.
pub const CHANNEL7_EVENT: EventId = EventId::new(0x0207_0007);

const CHANNEL_COUNT: usize = 13;
const CHANNEL4: usize = 4;
const CHANNEL7: usize = 7;
const RAM_ADDRESS_MASK: u32 = 0x00ff_fffc;
const CHCR_DIRECTION_FROM_RAM: u32 = 1 << 0;
const CHCR_DECREMENT: u32 = 1 << 1;
const CHCR_SYNC_MASK: u32 = 3 << 9;
const CHCR_START: u32 = 1 << 24;
const DPCR1_RESET: u32 = 0x0765_4321;
const DPCR_ENABLE: u32 = 1 << 3;
const DICR_FORCE: u32 = 1 << 15;
const DICR_MASTER_ENABLE: u32 = 1 << 23;
const DICR_MASTER_FLAG: u32 = 1 << 31;
// Sound DMA payloads become visible atomically at the next scheduler boundary.
const COMPLETION_CYCLES: u64 = 0;
const MAX_RAM_WORDS: u64 = (2 * 1024 * 1024) / 4;

/// SPU2 core selected by an IOP DMA channel.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum SoundDmaChannel {
    /// Channel 4, feeding SPU2 core 0.
    Core0 = 0,
    /// Channel 7, feeding SPU2 core 1.
    Core1 = 1,
}

impl SoundDmaChannel {
    const ALL: [Self; 2] = [Self::Core0, Self::Core1];

    const fn core(self) -> usize {
        self as usize
    }

    const fn channel(self) -> usize {
        match self {
            Self::Core0 => CHANNEL4,
            Self::Core1 => CHANNEL7,
        }
    }

    const fn event(self) -> EventId {
        match self {
            Self::Core0 => CHANNEL4_EVENT,
            Self::Core1 => CHANNEL7_EVENT,
        }
    }
}

/// Direction of one sound DMA transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaDirection {
    /// IOP RAM to SPU2.
    FromRam,
    /// SPU2 to IOP RAM.
    ToRam,
}

/// Hardware DMA event exposed to machine and BIOS layers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaEvent {
    /// A validated transfer was scheduled.
    Started {
        /// SPU2 core and DMA channel.
        channel: SoundDmaChannel,
        /// Transfer direction.
        direction: DmaDirection,
        /// Number of 32-bit words.
        words: u64,
        /// Exact completion timestamp.
        deadline: Deadline,
    },
    /// All words reached their destination.
    Completed {
        /// SPU2 core and DMA channel.
        channel: SoundDmaChannel,
        /// Transfer direction.
        direction: DmaDirection,
        /// Number of 32-bit words.
        words: u64,
    },
}

/// Observer used by machine and BIOS composition layers.
pub trait DmaObserver {
    /// Observes one DMA lifecycle event.
    fn observe(&mut self, event: DmaEvent);
}

impl DmaObserver for Vec<DmaEvent> {
    fn observe(&mut self, event: DmaEvent) {
        self.push(event);
    }
}

/// Observer which intentionally discards events.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NoopObserver;

impl DmaObserver for NoopObserver {
    fn observe(&mut self, _event: DmaEvent) {}
}

/// SPU2 transfer endpoint consumed independently by both sound channels.
pub trait Spu2DmaEndpoint {
    /// Accepts one little-endian word from IOP RAM.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError`] when the selected core cannot accept data.
    fn write_word(&mut self, channel: SoundDmaChannel, value: u32) -> Result<(), EndpointError>;

    /// Supplies one little-endian word for IOP RAM.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError`] when the selected core cannot supply data.
    fn read_word(&mut self, channel: SoundDmaChannel) -> Result<u32, EndpointError>;
}

/// SPU2 register endpoint used by the IOP machine's sound MMIO router.
pub trait Spu2MmioEndpoint {
    /// Reads one aligned 16-bit SPU2 register.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError`] when the register is not implemented.
    fn read_register(&mut self, address: u32) -> Result<u16, EndpointError>;

    /// Writes one aligned 16-bit SPU2 register.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError`] when the register is not implemented.
    fn write_register(&mut self, address: u32, value: u16) -> Result<(), EndpointError>;
}

/// Deterministic standalone SPU2 endpoint for DMA and machine tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MockSpu2Endpoint {
    written: [Vec<u32>; 2],
    readable: [VecDeque<u32>; 2],
    registers: Vec<u16>,
}

impl Default for MockSpu2Endpoint {
    fn default() -> Self {
        Self {
            written: [Vec::new(), Vec::new()],
            readable: [VecDeque::new(), VecDeque::new()],
            registers: vec![0; 0x400],
        }
    }
}

impl MockSpu2Endpoint {
    /// Constructs an empty endpoint.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues words returned by a later SPU2-to-IOP transfer.
    pub fn queue_read_words(
        &mut self,
        channel: SoundDmaChannel,
        words: impl IntoIterator<Item = u32>,
    ) {
        self.readable[channel.core()].extend(words);
    }

    /// Returns words received by an IOP-to-SPU2 transfer.
    #[must_use]
    pub fn written_words(&self, channel: SoundDmaChannel) -> &[u32] {
        &self.written[channel.core()]
    }
}

impl Spu2DmaEndpoint for MockSpu2Endpoint {
    fn write_word(&mut self, channel: SoundDmaChannel, value: u32) -> Result<(), EndpointError> {
        self.written[channel.core()].push(value);
        Ok(())
    }

    fn read_word(&mut self, channel: SoundDmaChannel) -> Result<u32, EndpointError> {
        self.readable[channel.core()].pop_front().ok_or_else(|| {
            EndpointError::new(format!("SPU2 core {} DMA read underflow", channel.core()))
        })
    }
}

impl Spu2MmioEndpoint for MockSpu2Endpoint {
    fn read_register(&mut self, address: u32) -> Result<u16, EndpointError> {
        let index = sound_register_index(address)?;
        Ok(self.registers[index])
    }

    fn write_register(&mut self, address: u32, value: u16) -> Result<(), EndpointError> {
        let index = sound_register_index(address)?;
        self.registers[index] = value;
        Ok(())
    }
}

/// A sound endpoint rejected or failed one transfer word.
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

    /// Returns the diagnostic text.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Invalid DMA programming or transfer failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DmaError {
    /// Address does not select a modeled DMA register.
    #[error("invalid IOP DMA register address {address:#010x}")]
    InvalidRegister {
        /// Physical address supplied by the machine.
        address: u32,
    },
    /// Linked-list or reserved synchronization mode is unsupported for sound.
    #[error("unsupported IOP sound DMA channel {channel} synchronization mode {mode}")]
    UnsupportedSync {
        /// Numeric DMA channel.
        channel: usize,
        /// Two-bit synchronization field.
        mode: u8,
    },
    /// Guest word count exceeds the complete IOP RAM allocation.
    #[error("IOP DMA transfer of {words} words exceeds emulated RAM")]
    TransferTooLarge {
        /// Guest-programmed transfer size.
        words: u64,
    },
    /// Transfer addresses leave physical main RAM.
    #[error(
        "IOP DMA range leaves RAM: channel {channel}, base {base:#010x}, {words} words, decrement={decrement}"
    )]
    RamRange {
        /// Numeric DMA channel.
        channel: usize,
        /// Masked physical start address.
        base: u32,
        /// Transfer word count.
        words: u64,
        /// Whether addresses move downward.
        decrement: bool,
    },
    /// Scheduler delivered a different or obsolete completion event.
    #[error("stale IOP sound DMA completion event {event:#010x}")]
    StaleEvent {
        /// Numeric scheduler event identifier.
        event: u32,
    },
    /// Sound endpoint failed while transferring a word.
    #[error("SPU2 DMA endpoint failure: {0}")]
    Endpoint(#[from] EndpointError),
    /// Scheduler insertion sequence was exhausted.
    #[error("IOP DMA scheduler failure")]
    Scheduler,
    /// Completion-cycle arithmetic exceeded `u64`.
    #[error("IOP DMA timing overflow")]
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ChannelRegisters {
    madr: u32,
    bcr: u32,
    chcr: u32,
    tadr: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingTransfer {
    deadline: Deadline,
    address: u32,
    words: u64,
    direction: DmaDirection,
    decrement: bool,
}

/// Instance-owned IOP DMA controller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DmaController {
    channels: [ChannelRegisters; CHANNEL_COUNT],
    dpcr1: u32,
    dpcr2: u32,
    dicr1_control: u32,
    dicr1_flags: u32,
    dicr2_control: u32,
    dicr2_flags: u32,
    extended: [u32; 3],
    dmac_enable: u32,
    pending: [Option<PendingTransfer>; 2],
}

impl Default for DmaController {
    fn default() -> Self {
        Self::new()
    }
}

impl DmaController {
    /// Constructs reset DMA state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            channels: [ChannelRegisters {
                madr: 0,
                bcr: 0,
                chcr: 0,
                tadr: 0,
            }; CHANNEL_COUNT],
            dpcr1: DPCR1_RESET,
            dpcr2: 0,
            dicr1_control: 0,
            dicr1_flags: 0,
            dicr2_control: 0,
            dicr2_flags: 0,
            extended: [0; 3],
            dmac_enable: 0,
            pending: [None; 2],
        }
    }

    /// Returns the completion deadline for one sound channel.
    #[must_use]
    pub const fn completion_deadline(&self, channel: SoundDmaChannel) -> Option<Deadline> {
        match self.pending[channel.core()] {
            Some(pending) => Some(pending.deadline),
            None => None,
        }
    }

    /// Reads one aligned 32-bit DMA register.
    ///
    /// # Errors
    ///
    /// Returns [`DmaError::InvalidRegister`] outside modeled DMA blocks.
    pub fn read_u32(&self, address: u32) -> Result<u32, DmaError> {
        if let Some((channel, register, half)) = decode_channel_register(address) {
            if half != 0 {
                return Err(DmaError::InvalidRegister { address });
            }
            return Ok(channel_value(self.channels[channel], register));
        }
        match address {
            DPCR1 => Ok(self.dpcr1),
            DICR1 => Ok(self.dicr1()),
            0x1f80_1560 | 0x1f80_1564 | 0x1f80_1568 => {
                Ok(self.extended[((address - 0x1f80_1560) / 4) as usize])
            }
            DPCR2 => Ok(self.dpcr2),
            DICR2 => Ok(self.dicr2()),
            DMAC_ENABLE => Ok(self.dmac_enable),
            _ => Err(DmaError::InvalidRegister { address }),
        }
    }

    /// Reads one DMA-register halfword.
    ///
    /// # Errors
    ///
    /// Returns [`DmaError::InvalidRegister`] outside modeled DMA blocks.
    pub fn read_u16(&self, address: u32) -> Result<u16, DmaError> {
        let base = address & !3;
        let value = self.read_u32(base)?;
        Ok(if address & 2 == 0 {
            value as u16
        } else {
            (value >> 16) as u16
        })
    }

    /// Writes one aligned 32-bit DMA register and schedules newly active sound
    /// transfers.
    ///
    /// # Errors
    ///
    /// Returns a structured register, size, mode, scheduler, or clock error.
    pub fn write_u32<S: InterruptSink, O: DmaObserver>(
        &mut self,
        address: u32,
        value: u32,
        now: Deadline,
        scheduler: &mut Scheduler,
        sink: &mut S,
        observer: &mut O,
    ) -> Result<(), DmaError> {
        if let Some((channel, register, half)) = decode_channel_register(address) {
            if half != 0 {
                return Err(DmaError::InvalidRegister { address });
            }
            set_channel_value(&mut self.channels[channel], register, value);
            if register == 0 {
                self.channels[channel].madr &= RAM_ADDRESS_MASK;
            }
            if let Some(sound) = sound_channel(channel) {
                if register == 2 {
                    scheduler.cancel(sound.event());
                    self.pending[sound.core()] = None;
                }
                self.schedule_if_ready(sound, now, scheduler, observer)?;
            } else if register == 2 {
                self.channels[channel].chcr &= !CHCR_START;
            }
            return Ok(());
        }
        match address {
            DPCR1 => {
                self.dpcr1 = value;
                self.schedule_all(now, scheduler, observer)?;
            }
            DICR1 => self.write_dicr1(value, u32::MAX, sink),
            0x1f80_1560 | 0x1f80_1564 | 0x1f80_1568 => {
                self.extended[((address - 0x1f80_1560) / 4) as usize] = value;
            }
            DPCR2 => {
                self.dpcr2 = value;
                self.schedule_all(now, scheduler, observer)?;
            }
            DICR2 => self.write_dicr2(value, u32::MAX, sink),
            DMAC_ENABLE => self.dmac_enable = value,
            _ => return Err(DmaError::InvalidRegister { address }),
        }
        Ok(())
    }

    /// Writes one DMA-register halfword.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`DmaController::write_u32`].
    pub fn write_u16<S: InterruptSink, O: DmaObserver>(
        &mut self,
        address: u32,
        value: u16,
        now: Deadline,
        scheduler: &mut Scheduler,
        sink: &mut S,
        observer: &mut O,
    ) -> Result<(), DmaError> {
        let base = address & !3;
        if base == DICR1 {
            let shifted = if address & 2 == 0 {
                u32::from(value)
            } else {
                u32::from(value) << 16
            };
            let mask = if address & 2 == 0 {
                0x0000_ffff
            } else {
                0xffff_0000
            };
            self.write_dicr1(shifted, mask, sink);
            return Ok(());
        }
        if base == DICR2 {
            let shifted = if address & 2 == 0 {
                u32::from(value)
            } else {
                u32::from(value) << 16
            };
            let mask = if address & 2 == 0 {
                0x0000_ffff
            } else {
                0xffff_0000
            };
            self.write_dicr2(shifted, mask, sink);
            return Ok(());
        }
        let old = self.read_u32(base)?;
        let merged = if address & 2 == 0 {
            (old & 0xffff_0000) | u32::from(value)
        } else {
            (old & 0x0000_ffff) | (u32::from(value) << 16)
        };
        self.write_u32(base, merged, now, scheduler, sink, observer)
    }

    /// Completes one due sound transfer.
    ///
    /// The complete RAM range is validated before the endpoint or memory is
    /// touched.
    ///
    /// # Errors
    ///
    /// Returns a stale-event, RAM-range, endpoint, or timing diagnostic.
    pub fn complete<E: Spu2DmaEndpoint, S: InterruptSink, O: DmaObserver>(
        &mut self,
        event: DueEvent,
        memory: &mut IopMemory,
        endpoint: &mut E,
        sink: &mut S,
        observer: &mut O,
    ) -> Result<(), DmaError> {
        let channel = if event.id == CHANNEL4_EVENT {
            SoundDmaChannel::Core0
        } else if event.id == CHANNEL7_EVENT {
            SoundDmaChannel::Core1
        } else {
            return Err(DmaError::StaleEvent {
                event: event.id.get(),
            });
        };
        let Some(pending) = self.pending[channel.core()] else {
            return Err(DmaError::StaleEvent {
                event: event.id.get(),
            });
        };
        if pending.deadline != event.deadline {
            return Err(DmaError::StaleEvent {
                event: event.id.get(),
            });
        }
        transfer(channel, pending, memory, endpoint)?;
        self.pending[channel.core()] = None;
        let registers = &mut self.channels[channel.channel()];
        let byte_count = u32::try_from(pending.words * 4).map_err(|_| DmaError::ClockOverflow)?;
        registers.madr = if pending.decrement {
            registers.madr.wrapping_sub(byte_count)
        } else {
            registers.madr.wrapping_add(byte_count)
        } & RAM_ADDRESS_MASK;
        registers.chcr &= !CHCR_START;
        observer.observe(DmaEvent::Completed {
            channel,
            direction: pending.direction,
            words: pending.words,
        });
        self.finish_channel(channel, sink);
        Ok(())
    }

    fn schedule_all<O: DmaObserver>(
        &mut self,
        now: Deadline,
        scheduler: &mut Scheduler,
        observer: &mut O,
    ) -> Result<(), DmaError> {
        for channel in SoundDmaChannel::ALL {
            self.schedule_if_ready(channel, now, scheduler, observer)?;
        }
        Ok(())
    }

    fn schedule_if_ready<O: DmaObserver>(
        &mut self,
        channel: SoundDmaChannel,
        now: Deadline,
        scheduler: &mut Scheduler,
        observer: &mut O,
    ) -> Result<(), DmaError> {
        if self.pending[channel.core()].is_some() || !self.channel_enabled(channel) {
            return Ok(());
        }
        let registers = self.channels[channel.channel()];
        if registers.chcr & CHCR_START == 0 {
            return Ok(());
        }
        let sync = ((registers.chcr & CHCR_SYNC_MASK) >> 9) as u8;
        let words = transfer_words(registers.bcr, sync).ok_or(DmaError::UnsupportedSync {
            channel: channel.channel(),
            mode: sync,
        })?;
        if words > MAX_RAM_WORDS {
            return Err(DmaError::TransferTooLarge { words });
        }
        let direction = if registers.chcr & CHCR_DIRECTION_FROM_RAM != 0 {
            DmaDirection::FromRam
        } else {
            DmaDirection::ToRam
        };
        let deadline = now.checked_advance(Ticks::new(COMPLETION_CYCLES))?;
        let pending = PendingTransfer {
            deadline,
            address: registers.madr,
            words,
            direction,
            decrement: registers.chcr & CHCR_DECREMENT != 0,
        };
        scheduler.schedule(channel.event(), deadline)?;
        self.pending[channel.core()] = Some(pending);
        observer.observe(DmaEvent::Started {
            channel,
            direction,
            words,
            deadline,
        });
        Ok(())
    }

    fn channel_enabled(&self, channel: SoundDmaChannel) -> bool {
        let channel = channel.channel();
        if channel < 7 {
            self.dpcr1 & (DPCR_ENABLE << (channel * 4)) != 0
        } else {
            self.dpcr2 & (DPCR_ENABLE << ((channel - 7) * 4)) != 0
        }
    }

    fn dicr1(&self) -> u32 {
        self.dicr1_control
            | self.dicr1_flags
            | if self.master_irq() {
                DICR_MASTER_FLAG
            } else {
                0
            }
    }

    fn dicr2(&self) -> u32 {
        self.dicr2_control
            | self.dicr2_flags
            | if self.master_irq() {
                DICR_MASTER_FLAG
            } else {
                0
            }
    }

    fn master_irq(&self) -> bool {
        self.dicr1_control & DICR_FORCE != 0
            || (self.dicr1_control & DICR_MASTER_ENABLE != 0
                && (enabled_flags(self.dicr1_control, self.dicr1_flags, 7) != 0
                    || enabled_flags(self.dicr2_control, self.dicr2_flags, 6) != 0))
    }

    fn write_dicr1<S: InterruptSink>(&mut self, value: u32, write_mask: u32, sink: &mut S) {
        let was_asserted = self.master_irq();
        let control_mask = write_mask & 0x00ff_ffff;
        self.dicr1_control =
            (self.dicr1_control & !control_mask) | (value & control_mask & 0x00ff_ffff);
        self.dicr1_flags &= !(value & write_mask & 0x7f00_0000);
        if !was_asserted && self.master_irq() {
            sink.request(InterruptSource::Dma);
        }
    }

    fn write_dicr2<S: InterruptSink>(&mut self, value: u32, write_mask: u32, sink: &mut S) {
        let was_asserted = self.master_irq();
        let control_mask = write_mask & 0x003f_ffff;
        self.dicr2_control =
            (self.dicr2_control & !control_mask) | (value & control_mask & 0x003f_ffff);
        self.dicr2_flags &= !(value & write_mask & 0x3f00_0000);
        if !was_asserted && self.master_irq() {
            sink.request(InterruptSource::Dma);
        }
    }

    fn finish_channel<S: InterruptSink>(&mut self, channel: SoundDmaChannel, sink: &mut S) {
        let was_asserted = self.master_irq();
        match channel {
            SoundDmaChannel::Core0 => self.dicr1_flags |= 1 << (24 + CHANNEL4),
            SoundDmaChannel::Core1 => self.dicr2_flags |= 1 << 24,
        }
        if !was_asserted && self.master_irq() {
            sink.request(InterruptSource::Dma);
        }
    }
}

fn sound_channel(channel: usize) -> Option<SoundDmaChannel> {
    match channel {
        CHANNEL4 => Some(SoundDmaChannel::Core0),
        CHANNEL7 => Some(SoundDmaChannel::Core1),
        _ => None,
    }
}

fn channel_value(channel: ChannelRegisters, register: usize) -> u32 {
    match register {
        0 => channel.madr,
        1 => channel.bcr,
        2 => channel.chcr,
        3 => channel.tadr,
        _ => unreachable!("validated DMA register"),
    }
}

fn set_channel_value(channel: &mut ChannelRegisters, register: usize, value: u32) {
    match register {
        0 => channel.madr = value,
        1 => channel.bcr = value,
        2 => channel.chcr = value,
        3 => channel.tadr = value,
        _ => unreachable!("validated DMA register"),
    }
}

fn decode_channel_register(address: u32) -> Option<(usize, usize, u8)> {
    let (channel, offset) = if (DMA1_CHANNEL_START..=DMA1_CHANNEL_END + 2).contains(&address) {
        let offset = address - DMA1_CHANNEL_START;
        (usize::try_from(offset / 0x10).ok()?, offset % 0x10)
    } else if (DMA2_CHANNEL_START..=DMA2_CHANNEL_END + 2).contains(&address) {
        let offset = address - DMA2_CHANNEL_START;
        (7 + usize::try_from(offset / 0x10).ok()?, offset % 0x10)
    } else {
        return None;
    };
    if channel >= CHANNEL_COUNT || !matches!(offset, 0 | 2 | 4 | 6 | 8 | 10 | 12 | 14) {
        return None;
    }
    Some((
        channel,
        usize::try_from(offset / 4).ok()?,
        ((offset & 2) / 2) as u8,
    ))
}

fn transfer_words(bcr: u32, sync: u8) -> Option<u64> {
    let block_words = u64::from(bcr & 0xffff);
    match sync {
        0 => Some(if block_words == 0 {
            0x1_0000
        } else {
            block_words
        }),
        1 => {
            let blocks = u64::from(bcr >> 16);
            let block_words = if block_words == 0 {
                0x1_0000
            } else {
                block_words
            };
            let blocks = if blocks == 0 { 0x1_0000 } else { blocks };
            block_words.checked_mul(blocks)
        }
        _ => None,
    }
}

fn enabled_flags(control: u32, flags: u32, channels: u32) -> u32 {
    let mask = (1_u32 << channels) - 1;
    ((control >> 16) & mask) & ((flags >> 24) & mask)
}

fn transfer<E: Spu2DmaEndpoint>(
    channel: SoundDmaChannel,
    pending: PendingTransfer,
    memory: &mut IopMemory,
    endpoint: &mut E,
) -> Result<(), DmaError> {
    validate_ram_range(
        channel.channel(),
        pending.address,
        pending.words,
        pending.decrement,
        memory.ram().len(),
    )?;
    for index in 0..pending.words {
        let address = transfer_address(pending.address, index, pending.decrement);
        let offset = usize::try_from(address).map_err(|_| DmaError::RamRange {
            channel: channel.channel(),
            base: pending.address,
            words: pending.words,
            decrement: pending.decrement,
        })?;
        match pending.direction {
            DmaDirection::FromRam => {
                endpoint.write_word(channel, read_ram_word(memory.ram(), offset))?;
            }
            DmaDirection::ToRam => {
                let value = endpoint.read_word(channel)?;
                write_ram_word(memory.ram_mut(), offset, value);
            }
        }
    }
    Ok(())
}

fn validate_ram_range(
    channel: usize,
    base: u32,
    words: u64,
    decrement: bool,
    ram_len: usize,
) -> Result<(), DmaError> {
    if words == 0 {
        return Ok(());
    }
    let byte_span = words
        .checked_sub(1)
        .and_then(|value| value.checked_mul(4))
        .ok_or(DmaError::ClockOverflow)?;
    let valid = if decrement {
        u64::from(base) >= byte_span
    } else {
        u64::from(base)
            .checked_add(byte_span)
            .and_then(|last| last.checked_add(4))
            .is_some_and(|end| end <= ram_len as u64)
    };
    if !valid {
        return Err(DmaError::RamRange {
            channel,
            base,
            words,
            decrement,
        });
    }
    Ok(())
}

fn transfer_address(base: u32, index: u64, decrement: bool) -> u32 {
    let byte_offset = (index * 4) as u32;
    if decrement {
        base - byte_offset
    } else {
        base + byte_offset
    }
}

fn read_ram_word(ram: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        ram[offset..offset + 4]
            .try_into()
            .expect("validated DMA range"),
    )
}

fn write_ram_word(ram: &mut [u8], offset: usize, value: u32) {
    ram[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn sound_register_index(address: u32) -> Result<usize, EndpointError> {
    if address & 1 != 0 || !(0x1f90_0000..=0x1f90_07fe).contains(&address) {
        return Err(EndpointError::new(format!(
            "invalid SPU2 register address {address:#010x}"
        )));
    }
    usize::try_from((address - 0x1f90_0000) / 2)
        .map_err(|_| EndpointError::new(format!("SPU2 register address too wide: {address:#010x}")))
}

#[cfg(test)]
mod tests {
    use upse_clock::Deadline;
    use upse_iop_irq::{InterruptController, InterruptSource};
    use upse_iop_memory::{IopMemory, OpenBusPolicy};
    use upse_scheduler::{DueEvent, EventId, Scheduler};

    use super::{
        CHANNEL4_EVENT, CHANNEL7_EVENT, D4_BCR, D4_CHCR, D4_MADR, D7_BCR, D7_CHCR, D7_MADR, DICR1,
        DICR2, DPCR1, DPCR2, DmaController, DmaDirection, DmaError, DmaEvent, MockSpu2Endpoint,
        SoundDmaChannel, Spu2MmioEndpoint,
    };

    #[allow(clippy::too_many_arguments)]
    fn enable_and_start(
        dma: &mut DmaController,
        scheduler: &mut Scheduler,
        irq: &mut InterruptController,
        events: &mut Vec<DmaEvent>,
        channel: SoundDmaChannel,
        address: u32,
        words: u16,
        from_ram: bool,
    ) {
        let (madr, bcr, chcr, dpcr, enable) = match channel {
            SoundDmaChannel::Core0 => (D4_MADR, D4_BCR, D4_CHCR, DPCR1, 1 << 19),
            SoundDmaChannel::Core1 => (D7_MADR, D7_BCR, D7_CHCR, DPCR2, 1 << 3),
        };
        dma.write_u32(
            dpcr,
            dma.read_u32(dpcr).unwrap() | enable,
            Deadline::ZERO,
            scheduler,
            irq,
            events,
        )
        .unwrap();
        dma.write_u32(madr, address, Deadline::ZERO, scheduler, irq, events)
            .unwrap();
        dma.write_u16(bcr, 0x10, Deadline::ZERO, scheduler, irq, events)
            .unwrap();
        dma.write_u16(bcr + 2, words / 16, Deadline::ZERO, scheduler, irq, events)
            .unwrap();
        dma.write_u32(
            chcr,
            (1 << 24) | (1 << 9) | u32::from(from_ram),
            Deadline::ZERO,
            scheduler,
            irq,
            events,
        )
        .unwrap();
    }

    #[test]
    fn both_sound_channels_transfer_exact_patterns_and_complete_in_fifo_order() {
        let mut dma = DmaController::new();
        let mut scheduler = Scheduler::new();
        let mut irq = InterruptController::new();
        let mut events = Vec::new();
        let mut memory = IopMemory::new(OpenBusPolicy::Strict);
        let mut endpoint = MockSpu2Endpoint::new();
        for index in 0..16_u32 {
            memory.write_u32(index * 4, 0x1000_0000 | index).unwrap();
            endpoint.queue_read_words(SoundDmaChannel::Core1, [0x2000_0000 | index]);
        }
        enable_and_start(
            &mut dma,
            &mut scheduler,
            &mut irq,
            &mut events,
            SoundDmaChannel::Core0,
            0,
            16,
            true,
        );
        enable_and_start(
            &mut dma,
            &mut scheduler,
            &mut irq,
            &mut events,
            SoundDmaChannel::Core1,
            0x100,
            16,
            false,
        );
        let deadline = dma.completion_deadline(SoundDmaChannel::Core0).unwrap();
        assert_eq!(deadline, Deadline::ZERO);
        assert_eq!(
            deadline,
            dma.completion_deadline(SoundDmaChannel::Core1).unwrap()
        );
        while let Some(event) = scheduler.pop_due(deadline) {
            dma.complete(event, &mut memory, &mut endpoint, &mut irq, &mut events)
                .unwrap();
        }
        assert_eq!(
            endpoint.written_words(SoundDmaChannel::Core0)[0],
            0x1000_0000
        );
        assert_eq!(
            endpoint.written_words(SoundDmaChannel::Core0)[15],
            0x1000_000f
        );
        assert_eq!(memory.read_u32(0x100).unwrap(), 0x2000_0000);
        assert_eq!(memory.read_u32(0x13c).unwrap(), 0x2000_000f);
        assert!(matches!(
            events[0],
            DmaEvent::Started {
                channel: SoundDmaChannel::Core0,
                ..
            }
        ));
        assert!(matches!(
            events[1],
            DmaEvent::Started {
                channel: SoundDmaChannel::Core1,
                ..
            }
        ));
        assert!(matches!(
            events[2],
            DmaEvent::Completed {
                channel: SoundDmaChannel::Core0,
                ..
            }
        ));
        assert!(matches!(
            events[3],
            DmaEvent::Completed {
                channel: SoundDmaChannel::Core1,
                ..
            }
        ));
    }

    #[test]
    fn completion_flags_and_halfword_acknowledgements_raise_aggregate_irq() {
        let mut dma = DmaController::new();
        let mut scheduler = Scheduler::new();
        let mut irq = InterruptController::new();
        let mut events = Vec::new();
        dma.write_u32(
            DICR1,
            (1 << 23) | (1 << 20),
            Deadline::ZERO,
            &mut scheduler,
            &mut irq,
            &mut events,
        )
        .unwrap();
        dma.write_u16(
            DICR2 + 2,
            1,
            Deadline::ZERO,
            &mut scheduler,
            &mut irq,
            &mut events,
        )
        .unwrap();
        let mut memory = IopMemory::new(OpenBusPolicy::Strict);
        let mut endpoint = MockSpu2Endpoint::new();
        for channel in [SoundDmaChannel::Core0, SoundDmaChannel::Core1] {
            enable_and_start(
                &mut dma,
                &mut scheduler,
                &mut irq,
                &mut events,
                channel,
                0,
                16,
                true,
            );
        }
        let deadline = dma.completion_deadline(SoundDmaChannel::Core0).unwrap();
        while let Some(event) = scheduler.pop_due(deadline) {
            dma.complete(event, &mut memory, &mut endpoint, &mut irq, &mut events)
                .unwrap();
        }
        assert_eq!(irq.status(), InterruptSource::Dma.bit());
        assert_ne!(dma.read_u32(DICR1).unwrap() & (1 << 28), 0);
        assert_ne!(dma.read_u32(DICR2).unwrap() & (1 << 24), 0);
        dma.write_u16(
            DICR1 + 2,
            1 << 12,
            deadline,
            &mut scheduler,
            &mut irq,
            &mut events,
        )
        .unwrap();
        assert_eq!(dma.read_u32(DICR1).unwrap() & (1 << 28), 0);
    }

    #[test]
    fn invalid_ranges_fail_before_endpoint_or_ram_mutation() {
        let mut dma = DmaController::new();
        let mut scheduler = Scheduler::new();
        let mut irq = InterruptController::new();
        let mut events = Vec::new();
        enable_and_start(
            &mut dma,
            &mut scheduler,
            &mut irq,
            &mut events,
            SoundDmaChannel::Core0,
            0x001f_fff0,
            16,
            true,
        );
        let mut memory = IopMemory::new(OpenBusPolicy::Strict);
        let before = memory.ram().to_vec();
        let mut endpoint = MockSpu2Endpoint::new();
        let deadline = dma.completion_deadline(SoundDmaChannel::Core0).unwrap();
        let event = scheduler.pop_due(deadline).unwrap();
        assert!(matches!(
            dma.complete(event, &mut memory, &mut endpoint, &mut irq, &mut events),
            Err(DmaError::RamRange { channel: 4, .. })
        ));
        assert!(endpoint.written_words(SoundDmaChannel::Core0).is_empty());
        assert_eq!(memory.ram(), before);
    }

    #[test]
    fn unsupported_modes_sizes_registers_and_stale_events_are_diagnostic() {
        let mut dma = DmaController::new();
        let mut scheduler = Scheduler::new();
        let mut irq = InterruptController::new();
        let mut events = Vec::new();
        dma.write_u32(
            DPCR1,
            dma.read_u32(DPCR1).unwrap() | (1 << 19),
            Deadline::ZERO,
            &mut scheduler,
            &mut irq,
            &mut events,
        )
        .unwrap();
        assert!(matches!(
            dma.write_u32(
                D4_CHCR,
                (1 << 24) | (2 << 9),
                Deadline::ZERO,
                &mut scheduler,
                &mut irq,
                &mut events
            ),
            Err(DmaError::UnsupportedSync {
                channel: 4,
                mode: 2
            })
        ));
        assert_eq!(
            dma.read_u32(0x1f80_14fc),
            Err(DmaError::InvalidRegister {
                address: 0x1f80_14fc
            })
        );
        let mut memory = IopMemory::new(OpenBusPolicy::Strict);
        let mut endpoint = MockSpu2Endpoint::new();
        assert_eq!(
            dma.complete(
                DueEvent {
                    id: EventId::new(99),
                    deadline: Deadline::ZERO
                },
                &mut memory,
                &mut endpoint,
                &mut irq,
                &mut events
            ),
            Err(DmaError::StaleEvent { event: 99 })
        );
        assert_eq!(CHANNEL4_EVENT.get(), 0x0204_0004);
        assert_eq!(CHANNEL7_EVENT.get(), 0x0207_0007);
        assert_eq!(DmaDirection::FromRam, DmaDirection::FromRam);
    }

    #[test]
    fn mock_endpoint_routes_the_complete_spu2_register_window() {
        let mut endpoint = MockSpu2Endpoint::new();
        endpoint.write_register(0x1f90_0000, 0x1234).unwrap();
        endpoint.write_register(0x1f90_07fe, 0xabcd).unwrap();
        assert_eq!(endpoint.read_register(0x1f90_0000).unwrap(), 0x1234);
        assert_eq!(endpoint.read_register(0x1f90_07fe).unwrap(), 0xabcd);
        assert!(endpoint.read_register(0x1f90_0001).is_err());
        assert!(endpoint.read_register(0x1f90_0800).is_err());
    }
}
