// SPDX-License-Identifier: LGPL-2.1-or-later
//! Stable-order scheduling for emulated device events.

use std::collections::{BTreeSet, HashMap};

use thiserror::Error;
use upse_clock::Deadline;

/// Identifies a replaceable event owned by a component.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EventId(u32);

impl EventId {
    /// Constructs an event identifier.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the integer identifier.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// An event removed from the scheduler for dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DueEvent {
    /// Component-defined event identifier.
    pub id: EventId,
    /// Exact emulated timestamp at which the event is due.
    pub deadline: Deadline,
}

/// Scheduler failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SchedulerError {
    /// The deterministic insertion sequence was exhausted.
    #[error("scheduler insertion sequence exhausted")]
    SequenceExhausted,
}

/// A replaceable event queue with stable FIFO ordering at equal deadlines.
#[derive(Clone, Debug, Default)]
pub struct Scheduler {
    queue: BTreeSet<(Deadline, u64, EventId)>,
    active: HashMap<EventId, (Deadline, u64)>,
    next_sequence: u64,
}

impl Scheduler {
    /// Constructs an empty scheduler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of active component events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.active.len()
    }

    /// Reports whether no events are scheduled.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Inserts or replaces an event.
    ///
    /// Replacement receives a new sequence number and therefore follows events
    /// already scheduled at the same deadline.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerError::SequenceExhausted`] after exhausting the stable
    /// insertion counter without altering the queue.
    pub fn schedule(&mut self, id: EventId, deadline: Deadline) -> Result<(), SchedulerError> {
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(SchedulerError::SequenceExhausted)?;
        if let Some((old_deadline, old_sequence)) = self.active.remove(&id) {
            self.queue.remove(&(old_deadline, old_sequence, id));
        }
        self.queue.insert((deadline, sequence, id));
        self.active.insert(id, (deadline, sequence));
        Ok(())
    }

    /// Cancels an active event and reports whether it existed.
    pub fn cancel(&mut self, id: EventId) -> bool {
        let Some((deadline, sequence)) = self.active.remove(&id) else {
            return false;
        };
        self.queue.remove(&(deadline, sequence, id))
    }

    /// Returns the next deadline without removing its event.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Deadline> {
        self.queue.first().map(|entry| entry.0)
    }

    /// Removes the earliest event when it is due at or before `now`.
    pub fn pop_due(&mut self, now: Deadline) -> Option<DueEvent> {
        let &(deadline, sequence, id) = self.queue.first()?;
        if deadline > now {
            return None;
        }
        self.queue.remove(&(deadline, sequence, id));
        self.active.remove(&id);
        Some(DueEvent { id, deadline })
    }

    /// Removes all active events while retaining a valid sequence order.
    pub fn clear(&mut self) {
        self.queue.clear();
        self.active.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{EventId, Scheduler, SchedulerError};
    use upse_clock::Deadline;

    #[test]
    fn equal_deadlines_are_fifo() {
        let mut scheduler = Scheduler::new();
        for id in [3, 1, 2] {
            scheduler
                .schedule(EventId::new(id), Deadline::new(10))
                .unwrap();
        }
        let actual: Vec<_> = (0..3)
            .map(|_| scheduler.pop_due(Deadline::new(10)).unwrap().id.get())
            .collect();
        assert_eq!(actual, [3, 1, 2]);
    }

    #[test]
    fn replacement_and_rescheduling_during_dispatch_are_stable() {
        let mut scheduler = Scheduler::new();
        scheduler
            .schedule(EventId::new(1), Deadline::new(3))
            .unwrap();
        scheduler
            .schedule(EventId::new(2), Deadline::new(3))
            .unwrap();
        scheduler
            .schedule(EventId::new(1), Deadline::new(3))
            .unwrap();
        assert_eq!(
            scheduler.pop_due(Deadline::new(3)).unwrap().id,
            EventId::new(2)
        );
        scheduler
            .schedule(EventId::new(2), Deadline::new(3))
            .unwrap();
        assert_eq!(
            scheduler.pop_due(Deadline::new(3)).unwrap().id,
            EventId::new(1)
        );
        assert_eq!(
            scheduler.pop_due(Deadline::new(3)).unwrap().id,
            EventId::new(2)
        );
    }

    #[test]
    fn cancellation_empty_and_horizon_behavior() {
        let mut scheduler = Scheduler::new();
        assert!(scheduler.is_empty());
        assert!(!scheduler.cancel(EventId::new(7)));
        scheduler
            .schedule(EventId::new(7), Deadline::new(u64::MAX))
            .unwrap();
        assert_eq!(scheduler.pop_due(Deadline::new(u64::MAX - 1)), None);
        assert!(scheduler.cancel(EventId::new(7)));
        assert!(scheduler.is_empty());
    }

    #[test]
    fn sequence_overflow_is_explicit_and_does_not_insert() {
        let mut scheduler = Scheduler::new();
        scheduler.next_sequence = u64::MAX;
        assert_eq!(
            scheduler.schedule(EventId::new(1), Deadline::ZERO),
            Err(SchedulerError::SequenceExhausted)
        );
        assert!(scheduler.is_empty());
    }
}
