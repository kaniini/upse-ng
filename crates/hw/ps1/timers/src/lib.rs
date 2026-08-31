// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS1 root counters and drift-free `VBlank` production.

use thiserror::Error;
use upse_clock::{ClockError, Deadline, Ticks};
use upse_ps1_irq::{InterruptController, InterruptSource};

/// Nominal PS1 CPU clock used by PSF1 timing.
pub const CPU_HZ: u64 = 33_868_800;
/// First root-counter register address.
pub const TIMER_BASE: u32 = 0x1f80_1100;

const COUNTER_STRIDE: u32 = 0x10;
const MODE_SYNC_ENABLE: u16 = 1 << 0;
const MODE_RESET_TARGET: u16 = 1 << 3;
const MODE_IRQ_TARGET: u16 = 1 << 4;
const MODE_IRQ_OVERFLOW: u16 = 1 << 5;
const MODE_IRQ_REPEAT: u16 = 1 << 6;
const MODE_IRQ_TOGGLE: u16 = 1 << 7;
const MODE_CLOCK_MASK: u16 = 3 << 8;
const MODE_IRQ_REQUEST: u16 = 1 << 10;
const MODE_REACHED_TARGET: u16 = 1 << 11;
const MODE_REACHED_OVERFLOW: u16 = 1 << 12;
const MODE_WRITABLE: u16 = 0x03ff;

/// Typed interrupt output consumed by the timer component.
pub trait InterruptSink {
    /// Latches or records one timer/refresh interrupt request.
    fn request(&mut self, source: InterruptSource);
}

impl InterruptSink for InterruptController {
    fn request(&mut self, source: InterruptSource) {
        InterruptController::request(self, source);
    }
}

/// Root-counter identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerId {
    /// Root counter 0.
    Timer0,
    /// Root counter 1.
    Timer1,
    /// Root counter 2.
    Timer2,
}

impl TimerId {
    const ALL: [Self; 3] = [Self::Timer0, Self::Timer1, Self::Timer2];

    const fn index(self) -> usize {
        match self {
            Self::Timer0 => 0,
            Self::Timer1 => 1,
            Self::Timer2 => 2,
        }
    }

    const fn interrupt(self) -> InterruptSource {
        match self {
            Self::Timer0 => InterruptSource::Timer0,
            Self::Timer1 => InterruptSource::Timer1,
            Self::Timer2 => InterruptSource::Timer2,
        }
    }
}

/// Counter register within one timer block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerRegister {
    /// Current 16-bit counter.
    Counter,
    /// Mode/configuration and edge flags.
    Mode,
    /// Target compare value.
    Target,
}

/// Input clock domain supplied to the root counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockInput {
    /// CPU/system clock ticks.
    System,
    /// GPU dot-clock ticks for counter 0.
    DotClock,
    /// Horizontal-blank rising edges for counter 1.
    HBlank,
}

/// PAL or NTSC refresh selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoStandard {
    /// 60 Hz refresh.
    Ntsc,
    /// 50 Hz refresh.
    Pal,
}

impl VideoStandard {
    /// Returns the integer refresh frequency.
    #[must_use]
    pub const fn refresh_hz(self) -> u64 {
        match self {
            Self::Ntsc => 60,
            Self::Pal => 50,
        }
    }

    /// Returns the exact number of PS1 CPU clocks per `VBlank` event.
    #[must_use]
    pub const fn cycles_per_vblank(self) -> u64 {
        CPU_HZ / self.refresh_hz()
    }

    /// Returns the number of scanlines in one video frame.
    #[must_use]
    pub const fn scanlines_per_frame(self) -> u64 {
        match self {
            Self::Ntsc => 263,
            Self::Pal => 314,
        }
    }

    /// Returns the horizontal-blank edge rate used by root counter 1.
    #[must_use]
    pub const fn hblank_hz(self) -> u64 {
        self.refresh_hz() * self.scanlines_per_frame()
    }
}

/// Invalid register, timer, or clock arithmetic.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TimerError {
    /// Address does not select a modeled root-counter register.
    #[error("invalid PS1 timer register address {address:#010x}")]
    InvalidRegister {
        /// Physical address supplied by the machine.
        address: u32,
    },
    /// Emulated time exceeded the public timestamp range.
    #[error("PS1 timer clock overflow")]
    ClockOverflow,
}

impl From<ClockError> for TimerError {
    fn from(_: ClockError) -> Self {
        Self::ClockOverflow
    }
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Counter {
    value: u16,
    target: u16,
    mode: u16,
    divider_remainder: u8,
    irq_fired: bool,
    irq_request_high: bool,
    pulse_pending: bool,
    blank: bool,
    sync_started: bool,
}

impl Default for Counter {
    fn default() -> Self {
        Self {
            value: 0,
            target: 0,
            mode: 0,
            divider_remainder: 0,
            irq_fired: false,
            irq_request_high: true,
            pulse_pending: false,
            blank: false,
            sync_started: false,
        }
    }
}

impl Counter {
    fn read_mode(&mut self) -> u16 {
        let mut value = self.mode;
        if self.irq_request_high {
            value |= MODE_IRQ_REQUEST;
        }
        if self.mode & MODE_REACHED_TARGET != 0 {
            value |= MODE_REACHED_TARGET;
        }
        if self.mode & MODE_REACHED_OVERFLOW != 0 {
            value |= MODE_REACHED_OVERFLOW;
        }
        self.mode &= !(MODE_REACHED_TARGET | MODE_REACHED_OVERFLOW);
        value
    }

    fn write_mode(&mut self, value: u16) {
        self.value = 0;
        self.mode = value & MODE_WRITABLE;
        self.divider_remainder = 0;
        self.irq_fired = false;
        self.irq_request_high = true;
        self.pulse_pending = false;
        self.sync_started = false;
    }

    fn clock_bits(self) -> u16 {
        (self.mode & MODE_CLOCK_MASK) >> 8
    }

    fn sync_mode(self) -> u16 {
        (self.mode >> 1) & 3
    }

    fn paused(self, id: TimerId) -> bool {
        if self.mode & MODE_SYNC_ENABLE == 0 {
            return false;
        }
        if id == TimerId::Timer2 {
            return matches!(self.sync_mode(), 0 | 3);
        }
        match self.sync_mode() {
            0 => self.blank,
            1 => false,
            2 => !self.blank,
            3 => !self.sync_started,
            _ => unreachable!("two-bit synchronization mode"),
        }
    }

    fn set_blank(&mut self, id: TimerId, asserted: bool) {
        let rising = asserted && !self.blank;
        self.blank = asserted;
        if id == TimerId::Timer2 || self.mode & MODE_SYNC_ENABLE == 0 || !rising {
            return;
        }
        match self.sync_mode() {
            1 | 2 => self.value = 0,
            3 => self.sync_started = true,
            _ => {}
        }
    }

    fn settle_pulse(&mut self) {
        if self.pulse_pending {
            self.irq_request_high = true;
            self.pulse_pending = false;
        }
    }

    fn tick<S: InterruptSink>(&mut self, id: TimerId, mut ticks: u64, sink: &mut S) {
        if ticks == 0 {
            return;
        }
        self.settle_pulse();
        if self.paused(id) {
            return;
        }
        while ticks != 0 {
            let value = u64::from(self.value);
            let target = u64::from(self.target);
            let overflow_distance = 0x1_0000 - value;
            let target_distance = if value < target {
                target - value
            } else {
                0x1_0000 - value + target
            };
            let distance = overflow_distance.min(target_distance);
            if ticks < distance {
                self.value =
                    u16::try_from(value + ticks).expect("counter step remains below 65536");
                break;
            }

            let next = (value + distance) & 0xffff;
            self.value = u16::try_from(next).expect("counter value is masked to 16 bits");
            ticks -= distance;
            let hit_target = distance == target_distance;
            let hit_overflow = distance == overflow_distance;
            if hit_target {
                self.mode |= MODE_REACHED_TARGET;
                if self.mode & MODE_RESET_TARGET != 0 {
                    self.value = 0;
                }
            }
            if hit_overflow {
                self.mode |= MODE_REACHED_OVERFLOW;
            }
            let irq = (hit_target && self.mode & MODE_IRQ_TARGET != 0)
                || (hit_overflow && self.mode & MODE_IRQ_OVERFLOW != 0);
            if irq {
                self.fire_irq(id, sink);
            }
            if ticks != 0 {
                self.settle_pulse();
            }
        }
    }

    fn fire_irq<S: InterruptSink>(&mut self, id: TimerId, sink: &mut S) {
        if self.mode & MODE_IRQ_REPEAT == 0 && self.irq_fired {
            return;
        }
        self.irq_fired = true;
        if self.mode & MODE_IRQ_TOGGLE != 0 {
            self.irq_request_high = !self.irq_request_high;
            if !self.irq_request_high {
                sink.request(id.interrupt());
            }
        } else {
            self.irq_request_high = false;
            self.pulse_pending = true;
            sink.request(id.interrupt());
        }
    }
}

/// Three independent PS1 root counters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RootCounters {
    counters: [Counter; 3],
    now: Deadline,
}

impl RootCounters {
    /// Constructs reset root counters.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counters: [
                Counter {
                    value: 0,
                    target: 0,
                    mode: 0,
                    divider_remainder: 0,
                    irq_fired: false,
                    irq_request_high: true,
                    pulse_pending: false,
                    blank: false,
                    sync_started: false,
                },
                Counter {
                    value: 0,
                    target: 0,
                    mode: 0,
                    divider_remainder: 0,
                    irq_fired: false,
                    irq_request_high: true,
                    pulse_pending: false,
                    blank: false,
                    sync_started: false,
                },
                Counter {
                    value: 0,
                    target: 0,
                    mode: 0,
                    divider_remainder: 0,
                    irq_fired: false,
                    irq_request_high: true,
                    pulse_pending: false,
                    blank: false,
                    sync_started: false,
                },
            ],
            now: Deadline::ZERO,
        }
    }

    /// Returns elapsed system-clock time.
    #[must_use]
    pub const fn now(&self) -> Deadline {
        self.now
    }

    /// Reads a typed counter register. Reading mode clears its reached flags.
    #[must_use]
    pub fn read_register(&mut self, id: TimerId, register: TimerRegister) -> u16 {
        let counter = &mut self.counters[id.index()];
        match register {
            TimerRegister::Counter => counter.value,
            TimerRegister::Mode => counter.read_mode(),
            TimerRegister::Target => counter.target,
        }
    }

    /// Writes a typed counter register.
    pub fn write_register(&mut self, id: TimerId, register: TimerRegister, value: u16) {
        let counter = &mut self.counters[id.index()];
        match register {
            TimerRegister::Counter => counter.value = value,
            TimerRegister::Mode => counter.write_mode(value),
            TimerRegister::Target => counter.target = value,
        }
    }

    /// Reads a physical counter register.
    ///
    /// # Errors
    ///
    /// Returns [`TimerError::InvalidRegister`] for unmodeled addresses.
    pub fn read(&mut self, address: u32) -> Result<u32, TimerError> {
        let (id, register) = decode_register(address)?;
        Ok(u32::from(self.read_register(id, register)))
    }

    /// Writes a physical counter register.
    ///
    /// # Errors
    ///
    /// Returns [`TimerError::InvalidRegister`] for unmodeled addresses.
    pub fn write(&mut self, address: u32, value: u32) -> Result<(), TimerError> {
        let (id, register) = decode_register(address)?;
        let bytes = value.to_le_bytes();
        self.write_register(id, register, u16::from_le_bytes([bytes[0], bytes[1]]));
        Ok(())
    }

    /// Advances one typed clock domain.
    ///
    /// Timer 0 accepts dot-clock input, timer 1 accepts `HBlank` edges, and all
    /// counters accept system clocks according to their mode selection.
    ///
    /// # Errors
    ///
    /// Returns [`TimerError::ClockOverflow`] if system time exceeds `u64`.
    pub fn advance<S: InterruptSink>(
        &mut self,
        input: ClockInput,
        ticks: Ticks,
        sink: &mut S,
    ) -> Result<(), TimerError> {
        match input {
            ClockInput::System => self.advance_system(ticks, sink),
            ClockInput::DotClock => {
                let counter = &mut self.counters[TimerId::Timer0.index()];
                if counter.clock_bits() & 1 != 0 {
                    counter.tick(TimerId::Timer0, ticks.get(), sink);
                }
                Ok(())
            }
            ClockInput::HBlank => {
                let counter = &mut self.counters[TimerId::Timer1.index()];
                if counter.clock_bits() & 1 != 0 {
                    counter.tick(TimerId::Timer1, ticks.get(), sink);
                }
                Ok(())
            }
        }
    }

    /// Changes horizontal-blank state for timer 0 synchronization.
    pub fn set_hblank(&mut self, asserted: bool) {
        self.counters[TimerId::Timer0.index()].set_blank(TimerId::Timer0, asserted);
    }

    /// Changes vertical-blank state for timer 1 synchronization.
    pub fn set_vblank(&mut self, asserted: bool) {
        self.counters[TimerId::Timer1.index()].set_blank(TimerId::Timer1, asserted);
    }

    fn advance_system<S: InterruptSink>(
        &mut self,
        ticks: Ticks,
        sink: &mut S,
    ) -> Result<(), TimerError> {
        self.now = self.now.checked_advance(ticks)?;
        for id in TimerId::ALL {
            let counter = &mut self.counters[id.index()];
            let clock_bits = counter.clock_bits();
            match id {
                TimerId::Timer0 | TimerId::Timer1 if clock_bits & 1 != 0 => {}
                TimerId::Timer2 if clock_bits & 2 != 0 => {
                    let total = u64::from(counter.divider_remainder) + ticks.get();
                    counter.divider_remainder =
                        u8::try_from(total % 8).expect("divider remainder is below eight");
                    counter.tick(id, total / 8, sink);
                }
                _ => counter.tick(id, ticks.get(), sink),
            }
        }
        Ok(())
    }
}

/// Drift-free periodic `VBlank` event producer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VBlankClock {
    standard: VideoStandard,
    now: Deadline,
    next: Deadline,
}

impl VBlankClock {
    /// Constructs refresh timing whose first event occurs after one full frame.
    #[must_use]
    pub const fn new(standard: VideoStandard) -> Self {
        let period = standard.cycles_per_vblank();
        Self {
            standard,
            now: Deadline::ZERO,
            next: Deadline::new(period),
        }
    }

    /// Returns the selected video standard.
    #[must_use]
    pub const fn standard(self) -> VideoStandard {
        self.standard
    }

    /// Returns current system-clock time.
    #[must_use]
    pub const fn now(self) -> Deadline {
        self.now
    }

    /// Returns the next exact `VBlank` deadline.
    #[must_use]
    pub const fn next_deadline(self) -> Deadline {
        self.next
    }

    /// Advances system time, requests each crossed `VBlank`, and returns its count.
    ///
    /// # Errors
    ///
    /// Returns [`TimerError::ClockOverflow`] if a timestamp exceeds `u64`.
    pub fn advance<S: InterruptSink>(
        &mut self,
        ticks: Ticks,
        sink: &mut S,
    ) -> Result<u64, TimerError> {
        let end = self.now.checked_advance(ticks)?;
        let period = Ticks::new(self.standard.cycles_per_vblank());
        let mut events = 0_u64;
        while self.next <= end {
            sink.request(InterruptSource::VBlank);
            events = events.checked_add(1).ok_or(TimerError::ClockOverflow)?;
            self.next = self.next.checked_advance(period)?;
        }
        self.now = end;
        Ok(events)
    }
}

fn decode_register(address: u32) -> Result<(TimerId, TimerRegister), TimerError> {
    let relative = address
        .checked_sub(TIMER_BASE)
        .ok_or(TimerError::InvalidRegister { address })?;
    let id = match relative / COUNTER_STRIDE {
        0 => TimerId::Timer0,
        1 => TimerId::Timer1,
        2 => TimerId::Timer2,
        _ => return Err(TimerError::InvalidRegister { address }),
    };
    let register = match relative % COUNTER_STRIDE {
        0 => TimerRegister::Counter,
        4 => TimerRegister::Mode,
        8 => TimerRegister::Target,
        _ => return Err(TimerError::InvalidRegister { address }),
    };
    Ok((id, register))
}

#[cfg(test)]
mod tests {
    use upse_clock::Ticks;
    use upse_ps1_irq::{I_STAT, InterruptController, InterruptSource};
    use upse_r3000::{Bus, BusFault, Cpu, ResetProfile};

    use super::{
        ClockInput, InterruptSink, MODE_IRQ_OVERFLOW, MODE_IRQ_REPEAT, MODE_IRQ_REQUEST,
        MODE_IRQ_TARGET, MODE_IRQ_TOGGLE, MODE_REACHED_OVERFLOW, MODE_REACHED_TARGET,
        MODE_RESET_TARGET, RootCounters, TIMER_BASE, TimerError, TimerId, TimerRegister,
        VBlankClock, VideoStandard,
    };

    #[derive(Default)]
    struct Log(Vec<InterruptSource>);

    impl InterruptSink for Log {
        fn request(&mut self, source: InterruptSource) {
            self.0.push(source);
        }
    }

    #[test]
    fn target_overflow_flags_and_mode_read_side_effects_are_exact() {
        let mut timers = RootCounters::new();
        let mut log = Log::default();
        timers.write_register(TimerId::Timer0, TimerRegister::Target, 3);
        timers.write_register(
            TimerId::Timer0,
            TimerRegister::Mode,
            MODE_RESET_TARGET | MODE_IRQ_TARGET | MODE_IRQ_REPEAT,
        );
        timers
            .advance(ClockInput::System, Ticks::new(7), &mut log)
            .unwrap();
        assert_eq!(
            timers.read_register(TimerId::Timer0, TimerRegister::Counter),
            1
        );
        let mode = timers.read_register(TimerId::Timer0, TimerRegister::Mode);
        assert_ne!(mode & MODE_REACHED_TARGET, 0);
        assert_ne!(mode & MODE_IRQ_REQUEST, 0);
        assert_eq!(
            timers.read_register(TimerId::Timer0, TimerRegister::Mode) & MODE_REACHED_TARGET,
            0
        );
        assert_eq!(log.0, [InterruptSource::Timer0, InterruptSource::Timer0]);

        timers.write_register(TimerId::Timer2, TimerRegister::Counter, u16::MAX);
        timers.write_register(
            TimerId::Timer2,
            TimerRegister::Mode,
            MODE_IRQ_OVERFLOW | MODE_IRQ_REPEAT,
        );
        timers.write_register(TimerId::Timer2, TimerRegister::Counter, u16::MAX);
        timers
            .advance(ClockInput::System, Ticks::new(1), &mut log)
            .unwrap();
        let mode = timers.read_register(TimerId::Timer2, TimerRegister::Mode);
        assert_ne!(mode & MODE_REACHED_OVERFLOW, 0);
    }

    #[test]
    fn one_shot_repeat_toggle_and_simultaneous_edges_match_register_policy() {
        let mut timers = RootCounters::new();
        let mut log = Log::default();
        timers.write_register(TimerId::Timer0, TimerRegister::Target, 1);
        timers.write_register(
            TimerId::Timer0,
            TimerRegister::Mode,
            MODE_RESET_TARGET | MODE_IRQ_TARGET,
        );
        timers
            .advance(ClockInput::System, Ticks::new(4), &mut log)
            .unwrap();
        assert_eq!(log.0, [InterruptSource::Timer0]);

        timers.write_register(TimerId::Timer1, TimerRegister::Target, 1);
        timers.write_register(
            TimerId::Timer1,
            TimerRegister::Mode,
            MODE_RESET_TARGET | MODE_IRQ_TARGET | MODE_IRQ_REPEAT | MODE_IRQ_TOGGLE,
        );
        timers
            .advance(ClockInput::System, Ticks::new(4), &mut log)
            .unwrap();
        assert_eq!(
            log.0,
            [
                InterruptSource::Timer0,
                InterruptSource::Timer1,
                InterruptSource::Timer1
            ]
        );

        timers.write_register(TimerId::Timer2, TimerRegister::Mode, 0);
        timers.write_register(TimerId::Timer2, TimerRegister::Counter, u16::MAX);
        timers.write_register(TimerId::Timer2, TimerRegister::Target, 0);
        timers.write_register(
            TimerId::Timer2,
            TimerRegister::Mode,
            MODE_IRQ_TARGET | MODE_IRQ_OVERFLOW | MODE_IRQ_REPEAT,
        );
        timers.write_register(TimerId::Timer2, TimerRegister::Counter, u16::MAX);
        timers
            .advance(ClockInput::System, Ticks::new(1), &mut log)
            .unwrap();
        let mode = timers.read_register(TimerId::Timer2, TimerRegister::Mode);
        assert_eq!(
            mode & (MODE_REACHED_TARGET | MODE_REACHED_OVERFLOW),
            MODE_REACHED_TARGET | MODE_REACHED_OVERFLOW
        );
        assert_eq!(log.0.last(), Some(&InterruptSource::Timer2));
    }

    #[test]
    fn clock_sources_divider_and_blank_synchronization_are_typed() {
        let mut timers = RootCounters::new();
        let mut log = Log::default();
        timers.write_register(TimerId::Timer0, TimerRegister::Mode, 1 << 8);
        timers
            .advance(ClockInput::System, Ticks::new(10), &mut log)
            .unwrap();
        timers
            .advance(ClockInput::DotClock, Ticks::new(3), &mut log)
            .unwrap();
        assert_eq!(
            timers.read_register(TimerId::Timer0, TimerRegister::Counter),
            3
        );

        timers.write_register(TimerId::Timer1, TimerRegister::Mode, 1 << 8);
        timers
            .advance(ClockInput::HBlank, Ticks::new(4), &mut log)
            .unwrap();
        assert_eq!(
            timers.read_register(TimerId::Timer1, TimerRegister::Counter),
            4
        );

        timers.write_register(TimerId::Timer2, TimerRegister::Mode, 1 << 8);
        timers
            .advance(ClockInput::System, Ticks::new(15), &mut log)
            .unwrap();
        assert_eq!(
            timers.read_register(TimerId::Timer2, TimerRegister::Counter),
            15
        );

        timers.write_register(TimerId::Timer2, TimerRegister::Mode, 1 << 9);
        timers
            .advance(ClockInput::System, Ticks::new(15), &mut log)
            .unwrap();
        assert_eq!(
            timers.read_register(TimerId::Timer2, TimerRegister::Counter),
            1
        );
        timers
            .advance(ClockInput::System, Ticks::new(1), &mut log)
            .unwrap();
        assert_eq!(
            timers.read_register(TimerId::Timer2, TimerRegister::Counter),
            2
        );

        timers.write_register(TimerId::Timer0, TimerRegister::Mode, 1);
        timers.set_hblank(true);
        timers
            .advance(ClockInput::System, Ticks::new(5), &mut log)
            .unwrap();
        assert_eq!(
            timers.read_register(TimerId::Timer0, TimerRegister::Counter),
            0
        );
        timers.set_hblank(false);
        timers
            .advance(ClockInput::System, Ticks::new(2), &mut log)
            .unwrap();
        assert_eq!(
            timers.read_register(TimerId::Timer0, TimerRegister::Counter),
            2
        );
    }

    #[test]
    fn register_decode_rejects_holes_and_preserves_low_halfword() {
        let mut timers = RootCounters::new();
        timers.write(TIMER_BASE + 8, 0xaaaa_1234).unwrap();
        assert_eq!(timers.read(TIMER_BASE + 8).unwrap(), 0x1234);
        assert_eq!(
            timers.read(TIMER_BASE + 2),
            Err(TimerError::InvalidRegister {
                address: TIMER_BASE + 2
            })
        );
    }

    #[test]
    fn refresh_deadlines_have_no_long_run_drift() {
        for (standard, seconds, expected) in [
            (VideoStandard::Ntsc, 10_000_u64, 600_000_u64),
            (VideoStandard::Pal, 10_000_u64, 500_000_u64),
        ] {
            let mut refresh = VBlankClock::new(standard);
            let mut log = CountingSink::default();
            for _ in 0..seconds {
                assert_eq!(
                    refresh
                        .advance(Ticks::new(super::CPU_HZ), &mut log)
                        .unwrap(),
                    standard.refresh_hz()
                );
            }
            assert_eq!(log.events, expected);
            assert_eq!(
                refresh.next_deadline().get(),
                (expected + 1) * standard.cycles_per_vblank()
            );
        }
    }

    #[derive(Default)]
    struct CountingSink {
        events: u64,
    }

    impl InterruptSink for CountingSink {
        fn request(&mut self, source: InterruptSource) {
            assert_eq!(source, InterruptSource::VBlank);
            self.events += 1;
        }
    }

    struct CpuBus {
        words: Vec<u32>,
        irq: InterruptController,
    }

    impl CpuBus {
        fn code_word(&self, address: u32) -> Result<u32, BusFault> {
            let index =
                usize::try_from(address / 4).map_err(|_| BusFault::new("bad code address"))?;
            self.words
                .get(index)
                .copied()
                .ok_or_else(|| BusFault::new("bad code address"))
        }
    }

    impl Bus for CpuBus {
        fn read_u8(&mut self, _address: u32) -> Result<u8, BusFault> {
            Err(BusFault::new("byte access is not used"))
        }

        fn read_u16(&mut self, _address: u32) -> Result<u16, BusFault> {
            Err(BusFault::new("halfword access is not used"))
        }

        fn read_u32(&mut self, address: u32) -> Result<u32, BusFault> {
            if address == I_STAT {
                return self
                    .irq
                    .read(address)
                    .map_err(|error| BusFault::new(error.to_string()));
            }
            self.code_word(address)
        }

        fn write_u8(&mut self, _address: u32, _value: u8) -> Result<(), BusFault> {
            Err(BusFault::new("byte access is not used"))
        }

        fn write_u16(&mut self, _address: u32, _value: u16) -> Result<(), BusFault> {
            Err(BusFault::new("halfword access is not used"))
        }

        fn write_u32(&mut self, address: u32, value: u32) -> Result<(), BusFault> {
            self.irq
                .write(address, value)
                .map_err(|error| BusFault::new(error.to_string()))
        }

        fn interrupt_pending(&self) -> bool {
            self.irq.pending()
        }
    }

    #[test]
    fn tiny_cpu_program_polls_and_acknowledges_timer_irq_in_trace_order() {
        let mut bus = CpuBus {
            words: vec![
                0x3c08_1f80, // lui t0,0x1f80
                0x8d09_1070, // lw t1,0x1070(t0)
                0x0000_0000, // load-delay slot
                0x3129_0010, // andi t1,t1,Timer0
                0x1120_fffc, // beq t1,zero,0x00000004
                0x0000_0000, // branch-delay slot
                0xad00_1070, // sw zero,0x1070(t0)
                0x2402_0001, // addiu v0,zero,1
            ],
            irq: InterruptController::new(),
        };
        let mut timers = RootCounters::new();
        timers.write_register(TimerId::Timer0, TimerRegister::Target, 8);
        timers.write_register(
            TimerId::Timer0,
            TimerRegister::Mode,
            MODE_RESET_TARGET | MODE_IRQ_TARGET,
        );
        let mut cpu = Cpu::new(ResetProfile {
            pc: 0,
            exception_vector: 0x80,
            bootstrap_exception_vector: 0x80,
            status: 0,
            processor_id: 2,
        });
        let mut trace = Vec::new();
        for _ in 0..32 {
            let outcome = cpu.step(&mut bus).unwrap();
            trace.push(outcome.pc);
            timers
                .advance(
                    ClockInput::System,
                    Ticks::new(u64::from(outcome.cycles)),
                    &mut bus.irq,
                )
                .unwrap();
            if cpu.register(2) == Some(1) {
                break;
            }
        }
        assert_eq!(cpu.register(2), Some(1));
        assert_eq!(bus.irq.status(), 0);
        assert_eq!(&trace[..6], &[0, 4, 8, 12, 16, 20]);
        assert!(trace.ends_with(&[4, 8, 12, 16, 20, 24, 28]));
    }
}
