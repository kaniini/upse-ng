// SPDX-License-Identifier: LGPL-2.1-or-later
//! Six PS2 IOP hardware counters and deterministic video refresh events.

#![allow(clippy::cast_possible_truncation)]

use thiserror::Error;
use upse_iop_irq::{InterruptSink, InterruptSource};

/// Native PS2 IOP system clock.
pub const CPU_HZ: u64 = 36_864_000;
/// Counter 0 register base.
pub const TIMER0_BASE: u32 = 0x1f80_1100;
/// Counter 1 register base.
pub const TIMER1_BASE: u32 = 0x1f80_1110;
/// Counter 2 register base.
pub const TIMER2_BASE: u32 = 0x1f80_1120;
/// Counter 3 register base.
pub const TIMER3_BASE: u32 = 0x1f80_1480;
/// Counter 4 register base.
pub const TIMER4_BASE: u32 = 0x1f80_1490;
/// Counter 5 register base.
pub const TIMER5_BASE: u32 = 0x1f80_14a0;
/// Last halfword occupied by an IOP counter register.
pub const TIMER_REGISTER_END: u32 = TIMER5_BASE + 0x0a;

const MODE_GATE_ENABLE: u16 = 1 << 0;
const MODE_GATE_MODE_MASK: u16 = 3 << 1;
const MODE_RESET_ON_TARGET: u16 = 1 << 3;
const MODE_IRQ_ON_TARGET: u16 = 1 << 4;
const MODE_IRQ_ON_OVERFLOW: u16 = 1 << 5;
const MODE_IRQ_REPEAT: u16 = 1 << 6;
const MODE_IRQ_TOGGLE: u16 = 1 << 7;
const MODE_EXTERNAL_CLOCK: u16 = 1 << 8;
const MODE_LOW_PRESCALE_8: u16 = 1 << 9;
const MODE_IRQ_REQUEST: u16 = 1 << 10;
const MODE_REACHED_TARGET: u16 = 1 << 11;
const MODE_REACHED_OVERFLOW: u16 = 1 << 12;
const MODE_HIGH_PRESCALE_MASK: u16 = 3 << 13;
const MODE_WRITABLE: u16 = 0x63ff;
const MAX_EVENTS_PER_ADVANCE: u64 = 1_000_000;

/// One of the six IOP counters.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum TimerId {
    /// 16-bit counter 0.
    Timer0 = 0,
    /// 16-bit counter 1.
    Timer1 = 1,
    /// 16-bit counter 2.
    Timer2 = 2,
    /// 32-bit counter 3.
    Timer3 = 3,
    /// 32-bit counter 4.
    Timer4 = 4,
    /// 32-bit counter 5.
    Timer5 = 5,
}

impl TimerId {
    const ALL: [Self; 6] = [
        Self::Timer0,
        Self::Timer1,
        Self::Timer2,
        Self::Timer3,
        Self::Timer4,
        Self::Timer5,
    ];

    const fn index(self) -> usize {
        self as usize
    }

    const fn width_mask(self) -> u32 {
        match self {
            Self::Timer0 | Self::Timer1 | Self::Timer2 => 0xffff,
            Self::Timer3 | Self::Timer4 | Self::Timer5 => u32::MAX,
        }
    }

    const fn interrupt(self) -> InterruptSource {
        match self {
            Self::Timer0 => InterruptSource::Timer0,
            Self::Timer1 => InterruptSource::Timer1,
            Self::Timer2 => InterruptSource::Timer2,
            Self::Timer3 => InterruptSource::Timer3,
            Self::Timer4 => InterruptSource::Timer4,
            Self::Timer5 => InterruptSource::Timer5,
        }
    }
}

/// A counter boundary observed by the machine or BIOS layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CounterBoundary {
    /// Counter reached its target register.
    Target,
    /// Counter wrapped through its width.
    Overflow,
}

/// Hardware timing event exposed without a BIOS dependency.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimingEvent {
    /// Counter target or overflow boundary.
    Counter {
        /// Counter that reached the boundary.
        timer: TimerId,
        /// Boundary kind.
        boundary: CounterBoundary,
    },
    /// Display entered vertical blank.
    VBlankStart,
    /// Display left vertical blank.
    VBlankEnd,
}

/// Observer used by machine and BIOS composition layers.
pub trait TimingObserver {
    /// Observes one deterministic hardware timing event.
    fn observe(&mut self, event: TimingEvent);
}

impl TimingObserver for Vec<TimingEvent> {
    fn observe(&mut self, event: TimingEvent) {
        self.push(event);
    }
}

/// Observer which intentionally discards events.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NoopObserver;

impl TimingObserver for NoopObserver {
    fn observe(&mut self, _event: TimingEvent) {}
}

/// Video refresh standard used by the IOP event source.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoStandard {
    /// NTSC-family 60/1.001 Hz refresh.
    #[default]
    Ntsc,
    /// PAL 50 Hz refresh.
    Pal,
}

/// Counter or refresh failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TimerError {
    /// Address is not a counter register.
    #[error("invalid IOP timer register address {address:#010x}")]
    InvalidRegister {
        /// Physical address supplied by the machine.
        address: u32,
    },
    /// A single advance would generate an unreasonable number of guest events.
    #[error("IOP timer {timer:?} exceeded {limit} events in one advance")]
    EventLimit {
        /// Counter producing events.
        timer: TimerId,
        /// Fixed safety limit.
        limit: u64,
    },
    /// Refresh phase arithmetic exceeded its internal representation.
    #[error("IOP refresh clock overflow")]
    RefreshOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Counter {
    count: u32,
    mode: u16,
    target: u32,
    prescale_remainder: u16,
    irq_fired: bool,
    gate_level: bool,
}

impl Counter {
    const fn new() -> Self {
        Self {
            count: 0,
            mode: MODE_IRQ_REQUEST,
            target: 0,
            prescale_remainder: 0,
            irq_fired: false,
            gate_level: false,
        }
    }

    fn running(self) -> bool {
        if self.mode & MODE_GATE_ENABLE == 0 {
            return true;
        }
        match (self.mode & MODE_GATE_MODE_MASK) >> 1 {
            0 => !self.gate_level,
            1 => self.gate_level,
            2 | 3 => true,
            _ => unreachable!("two-bit gate mode"),
        }
    }
}

/// Instance-owned set of all six IOP counters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IopTimers {
    counters: [Counter; 6],
}

impl Default for IopTimers {
    fn default() -> Self {
        Self::new()
    }
}

impl IopTimers {
    /// Constructs reset counter state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counters: [Counter::new(); 6],
        }
    }

    /// Returns one counter value.
    #[must_use]
    pub const fn count(&self, timer: TimerId) -> u32 {
        self.counters[timer.index()].count
    }

    /// Returns one target value.
    #[must_use]
    pub const fn target(&self, timer: TimerId) -> u32 {
        self.counters[timer.index()].target
    }

    /// Returns one mode value without clearing reached flags.
    #[must_use]
    pub const fn mode(&self, timer: TimerId) -> u16 {
        self.counters[timer.index()].mode
    }

    /// Changes the gate signal for counters using a blanking gate.
    pub fn set_gate(&mut self, timer: TimerId, asserted: bool) {
        let counter = &mut self.counters[timer.index()];
        let was_asserted = counter.gate_level;
        counter.gate_level = asserted;
        if counter.mode & MODE_GATE_ENABLE == 0 || was_asserted == asserted {
            return;
        }
        match ((counter.mode & MODE_GATE_MODE_MASK) >> 1, asserted) {
            (1 | 2, true) | (3, false) => counter.count = 0,
            _ => {}
        }
    }

    /// Reads a 32-bit counter register.
    ///
    /// Reading a mode register clears its reached-target and reached-overflow
    /// flags after returning their previous values.
    ///
    /// # Errors
    ///
    /// Returns [`TimerError::InvalidRegister`] outside the six counter blocks.
    pub fn read_u32(&mut self, address: u32) -> Result<u32, TimerError> {
        let (timer, register, half) = decode_register(address)?;
        if half != 0 {
            return Err(TimerError::InvalidRegister { address });
        }
        let counter = &mut self.counters[timer.index()];
        match register {
            0 => Ok(counter.count),
            1 => {
                let value = counter.mode;
                counter.mode &= !(MODE_REACHED_TARGET | MODE_REACHED_OVERFLOW);
                Ok(u32::from(value))
            }
            2 => Ok(counter.target),
            _ => unreachable!("validated timer register"),
        }
    }

    /// Writes a 32-bit counter register.
    ///
    /// # Errors
    ///
    /// Returns [`TimerError::InvalidRegister`] outside the six counter blocks.
    pub fn write_u32(&mut self, address: u32, value: u32) -> Result<(), TimerError> {
        let (timer, register, half) = decode_register(address)?;
        if half != 0 {
            return Err(TimerError::InvalidRegister { address });
        }
        let counter = &mut self.counters[timer.index()];
        match register {
            0 => counter.count = value & timer.width_mask(),
            1 => {
                counter.mode = (value as u16 & MODE_WRITABLE) | MODE_IRQ_REQUEST;
                counter.count = 0;
                counter.prescale_remainder = 0;
                counter.irq_fired = false;
            }
            2 => counter.target = value & timer.width_mask(),
            _ => unreachable!("validated timer register"),
        }
        Ok(())
    }

    /// Reads a counter-register halfword, including halves of 32-bit counters.
    ///
    /// # Errors
    ///
    /// Returns [`TimerError::InvalidRegister`] outside the six counter blocks.
    pub fn read_u16(&mut self, address: u32) -> Result<u16, TimerError> {
        let (timer, register, half) = decode_register(address)?;
        let counter = &mut self.counters[timer.index()];
        let value = match register {
            0 => counter.count,
            1 => {
                let value = u32::from(counter.mode);
                if half == 0 {
                    counter.mode &= !(MODE_REACHED_TARGET | MODE_REACHED_OVERFLOW);
                }
                value
            }
            2 => counter.target,
            _ => unreachable!("validated timer register"),
        };
        Ok(if half == 0 {
            value as u16
        } else {
            (value >> 16) as u16
        })
    }

    /// Writes a counter-register halfword, including halves of 32-bit counters.
    ///
    /// # Errors
    ///
    /// Returns [`TimerError::InvalidRegister`] outside the six counter blocks.
    pub fn write_u16(&mut self, address: u32, value: u16) -> Result<(), TimerError> {
        let (timer, register, half) = decode_register(address)?;
        if register == 1 {
            if half != 0 {
                return Ok(());
            }
            return self.write_u32(address, u32::from(value));
        }
        let counter = &mut self.counters[timer.index()];
        let destination = if register == 0 {
            &mut counter.count
        } else {
            &mut counter.target
        };
        if half == 0 {
            *destination = (*destination & 0xffff_0000) | u32::from(value);
        } else if timer.width_mask() == u32::MAX {
            *destination = (*destination & 0x0000_ffff) | (u32::from(value) << 16);
        }
        *destination &= timer.width_mask();
        Ok(())
    }

    /// Advances all system-clocked counters by IOP CPU cycles.
    ///
    /// # Errors
    ///
    /// Returns [`TimerError::EventLimit`] if hostile register values would
    /// produce excessive callbacks in one host operation.
    pub fn advance_cpu<S: InterruptSink, O: TimingObserver>(
        &mut self,
        cycles: u64,
        sink: &mut S,
        observer: &mut O,
    ) -> Result<(), TimerError> {
        for timer in TimerId::ALL {
            let counter = self.counters[timer.index()];
            if counter.mode & MODE_EXTERNAL_CLOCK != 0 || !counter.running() {
                continue;
            }
            let divisor = prescale(timer, counter.mode);
            let total = u64::from(counter.prescale_remainder) + cycles;
            self.counters[timer.index()].prescale_remainder = (total % divisor) as u16;
            self.advance_counter(timer, total / divisor, sink, observer)?;
        }
        Ok(())
    }

    /// Advances an explicitly clocked counter input.
    ///
    /// # Errors
    ///
    /// Returns [`TimerError::EventLimit`] for excessive callbacks.
    pub fn advance_external<S: InterruptSink, O: TimingObserver>(
        &mut self,
        timer: TimerId,
        ticks: u64,
        sink: &mut S,
        observer: &mut O,
    ) -> Result<(), TimerError> {
        if self.counters[timer.index()].mode & MODE_EXTERNAL_CLOCK != 0
            && self.counters[timer.index()].running()
        {
            self.advance_counter(timer, ticks, sink, observer)?;
        }
        Ok(())
    }

    fn advance_counter<S: InterruptSink, O: TimingObserver>(
        &mut self,
        timer: TimerId,
        mut ticks: u64,
        sink: &mut S,
        observer: &mut O,
    ) -> Result<(), TimerError> {
        let mask = timer.width_mask();
        let mut events = 0_u64;
        while ticks != 0 {
            let counter = &self.counters[timer.index()];
            let to_overflow = u64::from(mask - counter.count) + 1;
            let to_target = if counter.target > counter.count {
                u64::from(counter.target - counter.count)
            } else {
                to_overflow + u64::from(counter.target)
            };
            let step = ticks.min(to_overflow.min(to_target));
            let old_count = counter.count;
            let new_count = (u64::from(old_count) + step) & u64::from(mask);
            self.counters[timer.index()].count = new_count as u32;
            ticks -= step;

            let hit_target = step == to_target;
            let hit_overflow = step == to_overflow;
            if !hit_target && !hit_overflow {
                break;
            }
            events += u64::from(hit_target) + u64::from(hit_overflow);
            if events > MAX_EVENTS_PER_ADVANCE {
                return Err(TimerError::EventLimit {
                    timer,
                    limit: MAX_EVENTS_PER_ADVANCE,
                });
            }
            if hit_target {
                self.boundary(timer, CounterBoundary::Target, sink, observer);
                if self.counters[timer.index()].mode & MODE_RESET_ON_TARGET != 0 {
                    self.counters[timer.index()].count = 0;
                }
            }
            if hit_overflow {
                self.boundary(timer, CounterBoundary::Overflow, sink, observer);
            }
        }
        Ok(())
    }

    fn boundary<S: InterruptSink, O: TimingObserver>(
        &mut self,
        timer: TimerId,
        boundary: CounterBoundary,
        sink: &mut S,
        observer: &mut O,
    ) {
        let counter = &mut self.counters[timer.index()];
        let (status, enabled) = match boundary {
            CounterBoundary::Target => (MODE_REACHED_TARGET, MODE_IRQ_ON_TARGET),
            CounterBoundary::Overflow => (MODE_REACHED_OVERFLOW, MODE_IRQ_ON_OVERFLOW),
        };
        counter.mode |= status;
        observer.observe(TimingEvent::Counter { timer, boundary });
        if counter.mode & enabled == 0 || (counter.irq_fired && counter.mode & MODE_IRQ_REPEAT == 0)
        {
            return;
        }
        if counter.mode & MODE_IRQ_TOGGLE != 0 {
            counter.mode ^= MODE_IRQ_REQUEST;
            if counter.mode & MODE_IRQ_REQUEST != 0 {
                return;
            }
        } else {
            counter.mode &= !MODE_IRQ_REQUEST;
        }
        counter.irq_fired = true;
        sink.request(timer.interrupt());
    }
}

/// Deterministic start/end `VBlank` event source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefreshClock {
    standard: VideoStandard,
    phase_units: u64,
    in_vblank: bool,
}

impl RefreshClock {
    /// Constructs a refresh clock at the selected video standard.
    #[must_use]
    pub const fn new(standard: VideoStandard) -> Self {
        Self {
            standard,
            phase_units: 0,
            in_vblank: false,
        }
    }

    /// Returns the selected standard.
    #[must_use]
    pub const fn standard(&self) -> VideoStandard {
        self.standard
    }

    /// Reports whether the refresh source is currently in `VBlank`.
    #[must_use]
    pub const fn in_vblank(&self) -> bool {
        self.in_vblank
    }

    /// Advances refresh time and emits start before end for crossed boundaries.
    ///
    /// # Errors
    ///
    /// Returns [`TimerError::RefreshOverflow`] if phase arithmetic overflows.
    pub fn advance<S: InterruptSink, O: TimingObserver>(
        &mut self,
        cycles: u64,
        sink: &mut S,
        observer: &mut O,
    ) -> Result<(), TimerError> {
        let (rate_numerator, rate_denominator) = match self.standard {
            VideoStandard::Ntsc => (60_000_u64, 1_001_u64),
            VideoStandard::Pal => (50_u64, 1_u64),
        };
        let frame_units = CPU_HZ
            .checked_mul(rate_denominator)
            .ok_or(TimerError::RefreshOverflow)?;
        let blank_start = frame_units - frame_units / 10;
        let added = u128::from(cycles) * u128::from(rate_numerator);
        let mut remaining = added;
        while remaining != 0 {
            let boundary = if self.in_vblank {
                frame_units
            } else {
                blank_start
            };
            let distance = boundary - self.phase_units;
            if remaining < u128::from(distance) {
                self.phase_units +=
                    u64::try_from(remaining).map_err(|_| TimerError::RefreshOverflow)?;
                break;
            }
            remaining -= u128::from(distance);
            if self.in_vblank {
                self.phase_units = 0;
                self.in_vblank = false;
                observer.observe(TimingEvent::VBlankEnd);
                sink.request(InterruptSource::VBlankEnd);
            } else {
                self.phase_units = blank_start;
                self.in_vblank = true;
                observer.observe(TimingEvent::VBlankStart);
                sink.request(InterruptSource::VBlank);
            }
        }
        Ok(())
    }
}

fn prescale(timer: TimerId, mode: u16) -> u64 {
    match timer {
        TimerId::Timer0 | TimerId::Timer1 | TimerId::Timer2 => {
            if mode & MODE_LOW_PRESCALE_8 != 0 {
                8
            } else {
                1
            }
        }
        TimerId::Timer3 | TimerId::Timer4 | TimerId::Timer5 => {
            match (mode & MODE_HIGH_PRESCALE_MASK) >> 13 {
                0 => 1,
                1 => 8,
                2 => 16,
                3 => 256,
                _ => unreachable!("two-bit prescaler"),
            }
        }
    }
}

fn decode_register(address: u32) -> Result<(TimerId, u8, u8), TimerError> {
    for (timer, base) in [
        (TimerId::Timer0, TIMER0_BASE),
        (TimerId::Timer1, TIMER1_BASE),
        (TimerId::Timer2, TIMER2_BASE),
        (TimerId::Timer3, TIMER3_BASE),
        (TimerId::Timer4, TIMER4_BASE),
        (TimerId::Timer5, TIMER5_BASE),
    ] {
        let Some(offset) = address.checked_sub(base) else {
            continue;
        };
        if matches!(offset, 0 | 2 | 4 | 6 | 8 | 10) {
            return Ok((timer, (offset / 4) as u8, ((offset & 2) / 2) as u8));
        }
    }
    Err(TimerError::InvalidRegister { address })
}

#[cfg(test)]
mod tests {
    use upse_iop_irq::{InterruptController, InterruptSource};

    use super::{
        CPU_HZ, CounterBoundary, IopTimers, RefreshClock, TIMER0_BASE, TIMER2_BASE, TIMER3_BASE,
        TIMER5_BASE, TimerError, TimerId, TimingEvent, VideoStandard,
    };

    #[test]
    fn all_counter_registers_are_independent_and_width_correct() {
        let mut timers = IopTimers::new();
        for (index, base) in [
            TIMER0_BASE,
            TIMER0_BASE + 0x10,
            TIMER2_BASE,
            TIMER3_BASE,
            TIMER3_BASE + 0x10,
            TIMER5_BASE,
        ]
        .into_iter()
        .enumerate()
        {
            timers.write_u32(base, 0x1234_0000 | index as u32).unwrap();
            timers
                .write_u32(base + 8, 0xabcd_0000 | index as u32)
                .unwrap();
        }
        assert_eq!(timers.count(TimerId::Timer0), 0);
        assert_eq!(timers.count(TimerId::Timer2), 2);
        assert_eq!(timers.count(TimerId::Timer3), 0x1234_0003);
        assert_eq!(timers.target(TimerId::Timer5), 0xabcd_0005);
        timers.write_u16(TIMER3_BASE + 2, 0x5678).unwrap();
        assert_eq!(timers.read_u32(TIMER3_BASE).unwrap(), 0x5678_0003);
    }

    #[test]
    fn target_reset_prescale_and_irq_order_are_deterministic() {
        let mut timers = IopTimers::new();
        let mut irq = InterruptController::new();
        let mut events = Vec::new();
        timers.write_u32(TIMER2_BASE + 8, 3).unwrap();
        timers
            .write_u32(TIMER2_BASE + 4, (1 << 3) | (1 << 4) | (1 << 6) | (1 << 9))
            .unwrap();
        timers.advance_cpu(48, &mut irq, &mut events).unwrap();
        assert_eq!(timers.count(TimerId::Timer2), 0);
        assert_eq!(irq.status(), InterruptSource::Timer2.bit());
        assert_eq!(
            events,
            vec![
                TimingEvent::Counter {
                    timer: TimerId::Timer2,
                    boundary: CounterBoundary::Target
                },
                TimingEvent::Counter {
                    timer: TimerId::Timer2,
                    boundary: CounterBoundary::Target
                }
            ]
        );
        let mode = timers.read_u16(TIMER2_BASE + 4).unwrap();
        assert_ne!(mode & (1 << 11), 0);
        assert_eq!(timers.read_u16(TIMER2_BASE + 4).unwrap() & (1 << 11), 0);
    }

    #[test]
    fn overflow_and_external_clock_paths_are_observable() {
        let mut timers = IopTimers::new();
        let mut irq = InterruptController::new();
        let mut events = Vec::new();
        timers.write_u32(TIMER0_BASE + 4, 1 << 5).unwrap();
        timers.write_u32(TIMER0_BASE, 0xffff).unwrap();
        timers.advance_cpu(1, &mut irq, &mut events).unwrap();
        assert_eq!(events.len(), 2, "target zero and overflow share the wrap");
        assert_eq!(irq.status(), InterruptSource::Timer0.bit());

        timers.write_u32(TIMER3_BASE + 8, 2).unwrap();
        timers
            .write_u32(TIMER3_BASE + 4, (1 << 4) | (1 << 8))
            .unwrap();
        timers.advance_cpu(10, &mut irq, &mut events).unwrap();
        assert_eq!(timers.count(TimerId::Timer3), 0);
        timers
            .advance_external(TimerId::Timer3, 2, &mut irq, &mut events)
            .unwrap();
        assert_eq!(timers.count(TimerId::Timer3), 2);
    }

    #[test]
    fn refresh_emits_start_then_end_without_drift() {
        let mut refresh = RefreshClock::new(VideoStandard::Pal);
        let mut irq = InterruptController::new();
        let mut events = Vec::new();
        refresh.advance(CPU_HZ / 50, &mut irq, &mut events).unwrap();
        assert_eq!(events, [TimingEvent::VBlankStart, TimingEvent::VBlankEnd]);
        assert_eq!(
            irq.status(),
            InterruptSource::VBlank.bit() | InterruptSource::VBlankEnd.bit()
        );
        assert!(!refresh.in_vblank());
    }

    #[test]
    fn invalid_register_and_event_storm_are_bounded() {
        let mut timers = IopTimers::new();
        assert_eq!(
            timers.read_u32(TIMER2_BASE + 0x0c),
            Err(TimerError::InvalidRegister {
                address: TIMER2_BASE + 0x0c
            })
        );
        timers.write_u32(TIMER2_BASE + 8, 1).unwrap();
        timers
            .write_u32(TIMER2_BASE + 4, (1 << 3) | (1 << 4) | (1 << 6))
            .unwrap();
        let mut irq = InterruptController::new();
        let mut events = Vec::new();
        assert!(matches!(
            timers.advance_cpu(1_000_001, &mut irq, &mut events),
            Err(TimerError::EventLimit {
                timer: TimerId::Timer2,
                ..
            })
        ));
    }
}
