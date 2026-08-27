// SPDX-License-Identifier: LGPL-2.1-or-later
//! Deterministic IOP threads and synchronization objects.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use upse_clock::{Deadline, Ticks};
use upse_scheduler::{EventId, Scheduler};

use crate::dispatch::{RETURN_ENTRY, THREAD_RETURN_ENTRY};
use crate::{CallbackRequest, CpuContext, FixedTable, GuestRange, KernelError};

const V0: usize = 2;
const V1: usize = 3;
const A0: usize = 4;
const A1: usize = 5;
const A2: usize = 6;
const A3: usize = 7;
const GP: usize = 28;
const RA: usize = 31;
const THREAD_CAPACITY: usize = 64;
const SEMAPHORE_CAPACITY: usize = 64;
const EVENT_FLAG_CAPACITY: usize = 64;
const MESSAGE_BOX_CAPACITY: usize = 64;
const FIXED_POOL_CAPACITY: usize = 32;
const VARIABLE_POOL_CAPACITY: usize = 32;
const ALARM_CAPACITY: usize = 32;
const CALLBACK_DEPTH: usize = 8;
const MIN_THREAD_PRIORITY: u32 = 1;
const MAX_THREAD_PRIORITY: u32 = 126;
const MIN_STACK_SIZE: u32 = 0x100;
const WAIT_EVENT_BASE: u32 = 0x1000;
const ALARM_EVENT_BASE: u32 = 0x2000;
const WAIT_ANY: u32 = 1;
const WAIT_CLEAR: u32 = 0x10;

/// Thread construction parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadSpec {
    /// Initial guest entry point.
    pub entry: u32,
    /// First byte of the guest stack allocation.
    pub stack: u32,
    /// Stack byte count.
    pub stack_size: u32,
    /// Initial IOP priority, where a lower value runs first.
    pub priority: u32,
    /// Guest global pointer captured from the creating thread.
    pub global_pointer: u32,
    /// Guest thread attributes retained for inspection.
    pub attributes: u32,
    /// Guest-defined option word retained for inspection.
    pub option: u32,
}

/// Reason a thread is not runnable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitReason {
    /// Waiting for an explicit wakeup.
    Sleep,
    /// Waiting for a delay deadline.
    Delay,
    /// Waiting for the start of vertical blanking.
    VBlankStart,
    /// Waiting for the end of vertical blanking.
    VBlankEnd,
    /// Waiting for a temporary module-start thread to finish.
    ModuleStart(u32),
    /// Waiting for one semaphore count.
    Semaphore(u32),
    /// Waiting for an event-flag condition.
    EventFlag {
        /// Event-flag identifier.
        id: u32,
        /// Requested bit pattern.
        pattern: u32,
        /// `WAIT_ANY`/`WAIT_CLEAR` mode bits.
        mode: u32,
    },
    /// Waiting for one message pointer.
    MessageBox(u32),
    /// Waiting for one fixed-pool block.
    FixedPool(u32),
    /// Waiting for one variable-pool allocation.
    VariablePool {
        /// Variable-pool identifier.
        id: u32,
        /// Requested byte count.
        size: u32,
    },
}

/// Guest-visible thread lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadState {
    /// Created or exited and available to start.
    Dormant,
    /// Runnable and present in a priority queue.
    Ready,
    /// Currently installed in the emulated CPU.
    Running,
    /// Blocked on a kernel operation.
    Waiting(WaitReason),
}

/// One BIOS-owned IOP thread record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Thread {
    spec: ThreadSpec,
    context: CpuContext,
    state: ThreadState,
    current_priority: u32,
    wakeup_count: u32,
    ready_order: u64,
}

impl Thread {
    /// Returns the immutable creation parameters.
    #[must_use]
    pub const fn spec(&self) -> ThreadSpec {
        self.spec
    }

    /// Returns the saved guest CPU context.
    #[must_use]
    pub const fn context(&self) -> &CpuContext {
        &self.context
    }

    /// Returns the lifecycle state.
    #[must_use]
    pub const fn state(&self) -> ThreadState {
        self.state
    }

    /// Returns the current scheduling priority.
    #[must_use]
    pub const fn priority(&self) -> u32 {
        self.current_priority
    }

    /// Returns the number of retained wakeups.
    #[must_use]
    pub const fn wakeup_count(&self) -> u32 {
        self.wakeup_count
    }
}

/// Boundary at which the kernel may select another ready thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RescheduleReason {
    /// Normal import or syscall return.
    HleReturn,
    /// Explicit thread rotation.
    Yield,
    /// Return from a hardware interrupt.
    InterruptReturn,
    /// A synchronization object made a thread runnable.
    ObjectSignal,
    /// A delay, timeout, or alarm became due.
    Timer,
    /// Start or end of vertical blanking.
    VBlank,
}

/// Result of one scheduler decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduleAction {
    /// Previously running thread, if any.
    pub previous: Option<u32>,
    /// Thread installed after the decision, if any.
    pub current: Option<u32>,
    /// Whether the emulated CPU context was replaced.
    pub switched: bool,
}

/// Semaphore construction parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemaphoreSpec {
    /// Initial available count.
    pub initial: u32,
    /// Maximum available count.
    pub maximum: u32,
    /// Guest attributes; zero selects FIFO waiters and one selects priority.
    pub attributes: u32,
}

/// Event-flag construction parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventFlagSpec {
    /// Initial bit pattern.
    pub bits: u32,
    /// Guest attributes; bit one permits multiple waiters.
    pub attributes: u32,
}

/// Message-box construction parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageBoxSpec {
    /// Guest attributes; zero selects FIFO ordering and one selects priority.
    pub attributes: u32,
}

/// Fixed-pool construction parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixedPoolSpec {
    /// First guest byte owned by the pool.
    pub base: u32,
    /// Bytes in each block.
    pub block_size: u32,
    /// Number of blocks.
    pub blocks: u32,
    /// Guest attributes; zero selects FIFO ordering and one selects priority.
    pub attributes: u32,
}

/// Variable-pool construction parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VariablePoolSpec {
    /// First guest byte owned by the pool.
    pub base: u32,
    /// Total pool bytes.
    pub size: u32,
    /// Guest attributes; zero selects FIFO ordering and one selects priority.
    pub attributes: u32,
}

/// One event emitted while advancing kernel time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelEvent {
    /// A delayed or timed-out thread became ready.
    ThreadReady(u32),
    /// A one-shot alarm requested a guest callback.
    Alarm {
        /// Alarm identifier.
        id: u32,
        /// Callback description.
        callback: CallbackRequest,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Semaphore {
    count: u32,
    maximum: u32,
    attributes: u32,
    waiters: VecDeque<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EventFlag {
    bits: u32,
    attributes: u32,
    waiters: VecDeque<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MessageBox {
    attributes: u32,
    messages: VecDeque<u32>,
    waiters: VecDeque<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FixedPool {
    attributes: u32,
    block_size: u32,
    free: VecDeque<u32>,
    allocated: BTreeSet<u32>,
    waiters: VecDeque<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FreeRegion {
    address: u32,
    size: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VariablePool {
    attributes: u32,
    free: Vec<FreeRegion>,
    allocated: BTreeMap<u32, u32>,
    waiters: VecDeque<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Alarm {
    entry: u32,
    argument: u32,
}

/// Instance-owned IOP scheduler and synchronization state.
#[derive(Clone, Debug)]
pub struct Kernel {
    threads: FixedTable<Thread, THREAD_CAPACITY>,
    semaphores: FixedTable<Semaphore, SEMAPHORE_CAPACITY>,
    event_flags: FixedTable<EventFlag, EVENT_FLAG_CAPACITY>,
    message_boxes: FixedTable<MessageBox, MESSAGE_BOX_CAPACITY>,
    fixed_pools: FixedTable<FixedPool, FIXED_POOL_CAPACITY>,
    variable_pools: FixedTable<VariablePool, VARIABLE_POOL_CAPACITY>,
    alarms: FixedTable<Alarm, ALARM_CAPACITY>,
    scheduler: Scheduler,
    now: Deadline,
    current_thread: Option<u32>,
    next_ready_order: u64,
    callback_contexts: Vec<CpuContext>,
}

impl Kernel {
    /// Creates empty reset kernel state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            threads: FixedTable::new(1),
            semaphores: FixedTable::new(1),
            event_flags: FixedTable::new(1),
            message_boxes: FixedTable::new(1),
            fixed_pools: FixedTable::new(1),
            variable_pools: FixedTable::new(1),
            alarms: FixedTable::new(1),
            scheduler: Scheduler::new(),
            now: Deadline::ZERO,
            current_thread: None,
            next_ready_order: 0,
            callback_contexts: Vec::new(),
        }
    }

    /// Restores empty reset state without retaining host threads or events.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Returns current emulated kernel time.
    #[must_use]
    pub const fn now(&self) -> Deadline {
        self.now
    }

    /// Returns the currently running thread identifier.
    #[must_use]
    pub const fn current_thread(&self) -> Option<u32> {
        self.current_thread
    }

    /// Returns one thread record.
    #[must_use]
    pub fn thread(&self, id: u32) -> Option<&Thread> {
        self.threads.get(id)
    }

    /// Iterates over threads in identifier order.
    pub fn threads(&self) -> impl Iterator<Item = (u32, &Thread)> {
        self.threads.iter()
    }

    /// Returns the next timed deadline.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Deadline> {
        self.scheduler.next_deadline()
    }

    /// Creates a dormant IOP thread.
    ///
    /// # Errors
    ///
    /// Returns the documented entry, stack, priority, attribute, or capacity error.
    pub fn create_thread(
        &mut self,
        spec: ThreadSpec,
        guest_range: GuestRange,
    ) -> Result<u32, KernelError> {
        validate_thread_attributes(spec.attributes)?;
        validate_priority(spec.priority)?;
        if spec.stack_size < MIN_STACK_SIZE {
            return Err(KernelError::IllegalStackSize);
        }
        guest_range
            .validate(spec.entry, 4, 4)
            .map_err(|_| KernelError::IllegalEntry)?;
        let stack_size =
            usize::try_from(spec.stack_size).map_err(|_| KernelError::IllegalStackSize)?;
        guest_range
            .validate(spec.stack, stack_size, 16)
            .map_err(|_| KernelError::IllegalStackSize)?;
        let stack_top = spec
            .stack
            .checked_add(spec.stack_size)
            .ok_or(KernelError::IllegalStackSize)?;
        self.threads.insert(Thread {
            spec,
            context: CpuContext::reset(spec.entry, stack_top),
            state: ThreadState::Dormant,
            current_priority: spec.priority,
            wakeup_count: 0,
            ready_order: 0,
        })
    }

    /// Starts a dormant thread and installs its argument in `a0`.
    ///
    /// # Errors
    ///
    /// Returns an identifier/state error or insertion-sequence exhaustion.
    pub fn start_thread(&mut self, id: u32, argument: u32) -> Result<(), KernelError> {
        self.start_thread_with_context(id, [argument, 0, 0, 0], THREAD_RETURN_ENTRY)
    }

    /// Starts a dormant HLE-owned thread with four arguments and a selected
    /// return sentinel.
    ///
    /// # Errors
    ///
    /// Returns an identifier/state error or insertion-sequence exhaustion.
    pub fn start_thread_with_context(
        &mut self,
        id: u32,
        arguments: [u32; 4],
        return_address: u32,
    ) -> Result<(), KernelError> {
        let state = self
            .threads
            .get(id)
            .ok_or(KernelError::UnknownThreadId)?
            .state;
        if state != ThreadState::Dormant {
            return Err(KernelError::NotDormant);
        }
        let ready_order = self.take_ready_order()?;
        let thread = self
            .threads
            .get_mut(id)
            .ok_or(KernelError::UnknownThreadId)?;
        thread.context = CpuContext::reset(
            thread.spec.entry,
            thread.spec.stack.wrapping_add(thread.spec.stack_size),
        );
        thread.context.set_register(A0, arguments[0]);
        thread.context.set_register(A1, arguments[1]);
        thread.context.set_register(A2, arguments[2]);
        thread.context.set_register(A3, arguments[3]);
        thread.context.set_register(GP, thread.spec.global_pointer);
        thread.context.set_register(RA, return_address);
        thread.current_priority = thread.spec.priority;
        thread.wakeup_count = 0;
        thread.ready_order = ready_order;
        thread.state = ThreadState::Ready;
        Ok(())
    }

    /// Blocks the current requester until a temporary module-start thread
    /// completes.
    ///
    /// # Errors
    ///
    /// Returns an identifier, execution-context, or scheduling error.
    pub fn wait_module_start(
        &mut self,
        child: u32,
        context: &mut CpuContext,
    ) -> Result<ScheduleAction, KernelError> {
        if self
            .threads
            .get(child)
            .is_none_or(|thread| thread.state != ThreadState::Ready)
        {
            return Err(KernelError::UnknownThreadId);
        }
        self.block_current(context, WaitReason::ModuleStart(child), None)
    }

    /// Completes a module-start wait and installs the two return registers in
    /// the requester context.
    ///
    /// # Errors
    ///
    /// Returns an identifier or wait-state error.
    pub fn complete_module_start(
        &mut self,
        requester: u32,
        child: u32,
        module_id: u32,
        resident: u32,
    ) -> Result<(), KernelError> {
        if !self.wait_matches(requester, WaitReason::ModuleStart(child)) {
            return Err(KernelError::NotWaiting);
        }
        let module_id = i32::try_from(module_id).map_err(|_| KernelError::IllegalObject)?;
        self.complete_wait(requester, module_id, Some(resident))
    }

    /// Deletes a dormant thread.
    ///
    /// # Errors
    ///
    /// Returns an identifier or lifecycle error.
    pub fn delete_thread(&mut self, id: u32) -> Result<Thread, KernelError> {
        let thread = self.threads.get(id).ok_or(KernelError::UnknownThreadId)?;
        if thread.state != ThreadState::Dormant {
            return Err(KernelError::NotDormant);
        }
        self.threads.remove(id).ok_or(KernelError::UnknownThreadId)
    }

    /// Exits the current thread and selects the next ready thread.
    ///
    /// # Errors
    ///
    /// Returns an execution-context or scheduling error.
    pub fn exit_current(
        &mut self,
        context: &mut CpuContext,
    ) -> Result<ScheduleAction, KernelError> {
        let id = self
            .current_thread
            .take()
            .ok_or(KernelError::IllegalContext)?;
        let thread = self
            .threads
            .get_mut(id)
            .ok_or(KernelError::UnknownThreadId)?;
        thread.context = context.clone();
        thread.state = ThreadState::Dormant;
        thread.wakeup_count = 0;
        self.scheduler.cancel(wait_event(id));
        self.select(context, Some(id), true)
    }

    /// Exits and deletes the current thread before selecting another.
    ///
    /// # Errors
    ///
    /// Returns an execution-context or scheduling error.
    pub fn exit_delete_current(
        &mut self,
        context: &mut CpuContext,
    ) -> Result<ScheduleAction, KernelError> {
        let id = self
            .current_thread
            .take()
            .ok_or(KernelError::IllegalContext)?;
        self.scheduler.cancel(wait_event(id));
        self.threads
            .remove(id)
            .ok_or(KernelError::UnknownThreadId)?;
        self.select(context, Some(id), true)
    }

    /// Changes a thread priority, or queries it when `priority` is zero.
    ///
    /// # Errors
    ///
    /// Returns an identifier or priority error.
    pub fn change_priority(&mut self, id: u32, priority: u32) -> Result<u32, KernelError> {
        let thread = self
            .threads
            .get_mut(id)
            .ok_or(KernelError::UnknownThreadId)?;
        let old = thread.current_priority;
        if priority != 0 {
            validate_priority(priority)?;
            thread.current_priority = priority;
        }
        Ok(old)
    }

    /// Saves the active context and applies deterministic priority scheduling.
    ///
    /// # Errors
    ///
    /// Returns an execution-context or ready-sequence error.
    pub fn reschedule(
        &mut self,
        context: &mut CpuContext,
        reason: RescheduleReason,
    ) -> Result<ScheduleAction, KernelError> {
        let force = reason == RescheduleReason::Yield;
        let previous = self.current_thread;
        if let Some(id) = previous {
            let thread = self
                .threads
                .get_mut(id)
                .ok_or(KernelError::UnknownThreadId)?;
            if thread.state != ThreadState::Running {
                return Err(KernelError::IllegalContext);
            }
            thread.context = context.clone();
        }
        self.select(context, previous, force)
    }

    /// Puts the current thread to sleep or consumes a retained wakeup.
    ///
    /// # Errors
    ///
    /// Returns an execution-context or scheduling error.
    pub fn sleep_current(
        &mut self,
        context: &mut CpuContext,
    ) -> Result<ScheduleAction, KernelError> {
        let id = self.current_thread.ok_or(KernelError::IllegalContext)?;
        let thread = self
            .threads
            .get_mut(id)
            .ok_or(KernelError::UnknownThreadId)?;
        if thread.wakeup_count != 0 {
            thread.wakeup_count -= 1;
            return Ok(ScheduleAction {
                previous: Some(id),
                current: Some(id),
                switched: false,
            });
        }
        self.block_current(context, WaitReason::Sleep, None)
    }

    /// Wakes a sleeping thread or retains one future wakeup.
    ///
    /// # Errors
    ///
    /// Returns an identifier or counter-overflow error.
    pub fn wakeup_thread(&mut self, id: u32) -> Result<bool, KernelError> {
        let sleeping = self
            .threads
            .get(id)
            .ok_or(KernelError::UnknownThreadId)?
            .state
            == ThreadState::Waiting(WaitReason::Sleep);
        if sleeping {
            self.complete_wait(id, 0, None)?;
            return Ok(true);
        }
        let thread = self
            .threads
            .get_mut(id)
            .ok_or(KernelError::UnknownThreadId)?;
        thread.wakeup_count = thread
            .wakeup_count
            .checked_add(1)
            .ok_or(KernelError::NoMemory)?;
        Ok(false)
    }

    /// Delays the current thread for an exact number of kernel ticks.
    ///
    /// # Errors
    ///
    /// Returns an execution-context, clock, or scheduling error.
    pub fn delay_current(
        &mut self,
        ticks: Ticks,
        context: &mut CpuContext,
    ) -> Result<ScheduleAction, crate::BiosError> {
        if ticks == Ticks::ZERO {
            return self
                .reschedule(context, RescheduleReason::Yield)
                .map_err(Into::into);
        }
        let deadline = self.now.checked_advance(ticks)?;
        self.block_current(context, WaitReason::Delay, Some(deadline))
            .map_err(Into::into)
    }

    /// Blocks the current thread until the selected vertical-blank boundary.
    ///
    /// Zero selects `VBlank` start and one selects `VBlank` end.
    ///
    /// # Errors
    ///
    /// Returns an object, execution-context, or scheduling error.
    pub fn wait_vblank(
        &mut self,
        phase: u32,
        context: &mut CpuContext,
    ) -> Result<ScheduleAction, KernelError> {
        let reason = match phase {
            0 => WaitReason::VBlankStart,
            1 => WaitReason::VBlankEnd,
            _ => return Err(KernelError::IllegalObject),
        };
        self.block_current(context, reason, None)
    }

    /// Makes every thread waiting on one vertical-blank boundary runnable.
    ///
    /// # Errors
    ///
    /// Returns an object or ready-sequence error.
    pub fn notify_vblank(&mut self, phase: u32) -> Result<Vec<u32>, KernelError> {
        let reason = match phase {
            0 => WaitReason::VBlankStart,
            1 => WaitReason::VBlankEnd,
            _ => return Err(KernelError::IllegalObject),
        };
        let waiters = self
            .threads
            .iter()
            .filter_map(|(id, thread)| (thread.state == ThreadState::Waiting(reason)).then_some(id))
            .collect::<Vec<_>>();
        for id in &waiters {
            self.complete_wait(*id, 0, None)?;
        }
        Ok(waiters)
    }

    /// Cancels any wait and makes the thread ready with `KE_RELEASE_WAIT`.
    ///
    /// # Errors
    ///
    /// Returns an identifier or state error.
    pub fn release_wait(&mut self, id: u32) -> Result<(), KernelError> {
        let ThreadState::Waiting(reason) = self
            .threads
            .get(id)
            .ok_or(KernelError::UnknownThreadId)?
            .state
        else {
            return Err(KernelError::NotWaiting);
        };
        self.remove_waiter(id, reason);
        self.complete_wait(id, KernelError::ReleaseWait.code(), None)
    }

    /// Creates a semaphore.
    ///
    /// # Errors
    ///
    /// Returns an attribute, count, or capacity error.
    pub fn create_semaphore(&mut self, spec: SemaphoreSpec) -> Result<u32, KernelError> {
        validate_attributes(spec.attributes)?;
        if spec.maximum == 0 || spec.initial > spec.maximum {
            return Err(KernelError::IllegalSize);
        }
        self.semaphores.insert(Semaphore {
            count: spec.initial,
            maximum: spec.maximum,
            attributes: spec.attributes,
            waiters: VecDeque::new(),
        })
    }

    /// Deletes a semaphore and releases every waiter with `KE_WAIT_DELETE`.
    ///
    /// # Errors
    ///
    /// Returns an unknown-object error.
    pub fn delete_semaphore(&mut self, id: u32) -> Result<(), KernelError> {
        let semaphore = self
            .semaphores
            .remove(id)
            .ok_or(KernelError::UnknownSemaphoreId)?;
        self.release_deleted_waiters(semaphore.waiters)
    }

    /// Waits for a semaphore, optionally with an HLE deadline.
    ///
    /// # Errors
    ///
    /// Returns an identifier, context, clock, or scheduling error.
    pub fn wait_semaphore(
        &mut self,
        id: u32,
        timeout: Option<Ticks>,
        context: &mut CpuContext,
    ) -> Result<Option<ScheduleAction>, crate::BiosError> {
        let semaphore = self
            .semaphores
            .get_mut(id)
            .ok_or(KernelError::UnknownSemaphoreId)?;
        if semaphore.count != 0 {
            semaphore.count -= 1;
            return Ok(None);
        }
        let thread_id = self.current_thread.ok_or(KernelError::CannotWait)?;
        insert_waiter(
            &mut semaphore.waiters,
            thread_id,
            semaphore.attributes,
            &self.threads,
        );
        let deadline = timeout
            .map(|ticks| self.now.checked_advance(ticks))
            .transpose()?;
        self.block_current(context, WaitReason::Semaphore(id), deadline)
            .map(Some)
            .map_err(Into::into)
    }

    /// Polls a semaphore without blocking.
    ///
    /// # Errors
    ///
    /// Returns an unknown-object or zero-count error.
    pub fn poll_semaphore(&mut self, id: u32) -> Result<(), KernelError> {
        let semaphore = self
            .semaphores
            .get_mut(id)
            .ok_or(KernelError::UnknownSemaphoreId)?;
        if semaphore.count == 0 {
            return Err(KernelError::SemaphoreZero);
        }
        semaphore.count -= 1;
        Ok(())
    }

    /// Signals one semaphore waiter or increments its count.
    ///
    /// # Errors
    ///
    /// Returns an unknown-object, count-overflow, or scheduling error.
    pub fn signal_semaphore(&mut self, id: u32) -> Result<Option<u32>, KernelError> {
        loop {
            let waiter = self
                .semaphores
                .get_mut(id)
                .ok_or(KernelError::UnknownSemaphoreId)?
                .waiters
                .pop_front();
            let Some(thread_id) = waiter else {
                let semaphore = self
                    .semaphores
                    .get_mut(id)
                    .ok_or(KernelError::UnknownSemaphoreId)?;
                if semaphore.count == semaphore.maximum {
                    return Err(KernelError::SemaphoreOverflow);
                }
                semaphore.count += 1;
                return Ok(None);
            };
            if self.wait_matches(thread_id, WaitReason::Semaphore(id)) {
                self.complete_wait(thread_id, 0, None)?;
                return Ok(Some(thread_id));
            }
        }
    }

    /// Creates an event flag.
    ///
    /// # Errors
    ///
    /// Returns an attribute or capacity error.
    pub fn create_event_flag(&mut self, spec: EventFlagSpec) -> Result<u32, KernelError> {
        if spec.attributes & !0x103 != 0 {
            return Err(KernelError::IllegalAttribute);
        }
        self.event_flags.insert(EventFlag {
            bits: spec.bits,
            attributes: spec.attributes,
            waiters: VecDeque::new(),
        })
    }

    /// Deletes an event flag and releases every waiter.
    ///
    /// # Errors
    ///
    /// Returns an unknown-object error.
    pub fn delete_event_flag(&mut self, id: u32) -> Result<(), KernelError> {
        let event = self
            .event_flags
            .remove(id)
            .ok_or(KernelError::UnknownEventFlagId)?;
        self.release_deleted_waiters(event.waiters)
    }

    /// Waits for an event-flag pattern and returns matching bits when immediate.
    ///
    /// # Errors
    ///
    /// Returns a mode, pattern, identifier, context, clock, or scheduling error.
    pub fn wait_event_flag(
        &mut self,
        id: u32,
        pattern: u32,
        mode: u32,
        timeout: Option<Ticks>,
        context: &mut CpuContext,
    ) -> Result<Result<u32, ScheduleAction>, crate::BiosError> {
        validate_event_wait(pattern, mode)?;
        let thread_id = self.current_thread.ok_or(KernelError::CannotWait)?;
        let event = self
            .event_flags
            .get_mut(id)
            .ok_or(KernelError::UnknownEventFlagId)?;
        if let Some(bits) = consume_event_bits(event, pattern, mode) {
            return Ok(Ok(bits));
        }
        if event.attributes & 2 == 0 && !event.waiters.is_empty() {
            return Err(KernelError::EventFlagMultiple.into());
        }
        insert_waiter(
            &mut event.waiters,
            thread_id,
            event.attributes & 1,
            &self.threads,
        );
        let deadline = timeout
            .map(|ticks| self.now.checked_advance(ticks))
            .transpose()?;
        let action = self.block_current(
            context,
            WaitReason::EventFlag { id, pattern, mode },
            deadline,
        )?;
        Ok(Err(action))
    }

    /// Polls an event flag without blocking.
    ///
    /// # Errors
    ///
    /// Returns a mode, pattern, identifier, or unsatisfied-condition error.
    pub fn poll_event_flag(
        &mut self,
        id: u32,
        pattern: u32,
        mode: u32,
    ) -> Result<u32, KernelError> {
        validate_event_wait(pattern, mode)?;
        let event = self
            .event_flags
            .get_mut(id)
            .ok_or(KernelError::UnknownEventFlagId)?;
        consume_event_bits(event, pattern, mode).ok_or(KernelError::EventFlagCondition)
    }

    /// Sets event-flag bits and wakes every newly satisfied waiter in queue order.
    ///
    /// # Errors
    ///
    /// Returns an unknown-object or scheduling error.
    pub fn set_event_flag(&mut self, id: u32, bits: u32) -> Result<Vec<u32>, KernelError> {
        let event = self
            .event_flags
            .get_mut(id)
            .ok_or(KernelError::UnknownEventFlagId)?;
        event.bits |= bits;
        let queued = std::mem::take(&mut event.waiters);
        let mut retained = VecDeque::new();
        let mut ready = Vec::new();
        for thread_id in queued {
            let reason =
                self.threads
                    .get(thread_id)
                    .and_then(|thread| match thread.state {
                        ThreadState::Waiting(
                            reason @ WaitReason::EventFlag { id: wait_id, .. },
                        ) if wait_id == id => Some(reason),
                        _ => None,
                    });
            let Some(WaitReason::EventFlag { pattern, mode, .. }) = reason else {
                continue;
            };
            let matched = {
                let event = self
                    .event_flags
                    .get_mut(id)
                    .ok_or(KernelError::UnknownEventFlagId)?;
                consume_event_bits(event, pattern, mode)
            };
            if let Some(value) = matched {
                self.complete_wait(thread_id, 0, Some(value))?;
                ready.push(thread_id);
            } else {
                retained.push_back(thread_id);
            }
        }
        self.event_flags
            .get_mut(id)
            .ok_or(KernelError::UnknownEventFlagId)?
            .waiters = retained;
        Ok(ready)
    }

    /// Clears event-flag bits using the supplied retain mask.
    ///
    /// # Errors
    ///
    /// Returns an unknown-object error.
    pub fn clear_event_flag(&mut self, id: u32, mask: u32) -> Result<u32, KernelError> {
        let event = self
            .event_flags
            .get_mut(id)
            .ok_or(KernelError::UnknownEventFlagId)?;
        let old = event.bits;
        event.bits &= mask;
        Ok(old)
    }

    /// Creates an empty message box.
    ///
    /// # Errors
    ///
    /// Returns an attribute or capacity error.
    pub fn create_message_box(&mut self, spec: MessageBoxSpec) -> Result<u32, KernelError> {
        validate_attributes(spec.attributes)?;
        self.message_boxes.insert(MessageBox {
            attributes: spec.attributes,
            messages: VecDeque::new(),
            waiters: VecDeque::new(),
        })
    }

    /// Deletes a message box and releases every waiter.
    ///
    /// # Errors
    ///
    /// Returns an unknown-object error.
    pub fn delete_message_box(&mut self, id: u32) -> Result<(), KernelError> {
        let message_box = self
            .message_boxes
            .remove(id)
            .ok_or(KernelError::UnknownMessageBoxId)?;
        self.release_deleted_waiters(message_box.waiters)
    }

    /// Sends a non-null aligned guest message pointer.
    ///
    /// # Errors
    ///
    /// Returns a pointer, identifier, or scheduling error.
    pub fn send_message(&mut self, id: u32, message: u32) -> Result<Option<u32>, KernelError> {
        if message == 0 || message & 3 != 0 {
            return Err(KernelError::IllegalObject);
        }
        loop {
            let waiter = self
                .message_boxes
                .get_mut(id)
                .ok_or(KernelError::UnknownMessageBoxId)?
                .waiters
                .pop_front();
            let Some(thread_id) = waiter else {
                self.message_boxes
                    .get_mut(id)
                    .ok_or(KernelError::UnknownMessageBoxId)?
                    .messages
                    .push_back(message);
                return Ok(None);
            };
            if self.wait_matches(thread_id, WaitReason::MessageBox(id)) {
                self.complete_wait(thread_id, 0, Some(message))?;
                return Ok(Some(thread_id));
            }
        }
    }

    /// Receives a message immediately or blocks the current thread.
    ///
    /// # Errors
    ///
    /// Returns an identifier, context, clock, or scheduling error.
    pub fn receive_message(
        &mut self,
        id: u32,
        timeout: Option<Ticks>,
        context: &mut CpuContext,
    ) -> Result<Result<u32, ScheduleAction>, crate::BiosError> {
        let thread_id = self.current_thread.ok_or(KernelError::CannotWait)?;
        let message_box = self
            .message_boxes
            .get_mut(id)
            .ok_or(KernelError::UnknownMessageBoxId)?;
        if let Some(message) = message_box.messages.pop_front() {
            return Ok(Ok(message));
        }
        insert_waiter(
            &mut message_box.waiters,
            thread_id,
            message_box.attributes,
            &self.threads,
        );
        let deadline = timeout
            .map(|ticks| self.now.checked_advance(ticks))
            .transpose()?;
        let action = self.block_current(context, WaitReason::MessageBox(id), deadline)?;
        Ok(Err(action))
    }

    /// Polls a message box without blocking.
    ///
    /// # Errors
    ///
    /// Returns an identifier or no-message error.
    pub fn poll_message(&mut self, id: u32) -> Result<u32, KernelError> {
        self.message_boxes
            .get_mut(id)
            .ok_or(KernelError::UnknownMessageBoxId)?
            .messages
            .pop_front()
            .ok_or(KernelError::MessageBoxNoMessage)
    }

    /// Creates a fixed-size memory pool over an existing guest range.
    ///
    /// # Errors
    ///
    /// Returns an attribute, range, size, or capacity error.
    pub fn create_fixed_pool(
        &mut self,
        spec: FixedPoolSpec,
        guest_range: GuestRange,
    ) -> Result<u32, KernelError> {
        validate_attributes(spec.attributes)?;
        if spec.block_size == 0 || spec.blocks == 0 || spec.block_size & 3 != 0 {
            return Err(KernelError::IllegalMemorySize);
        }
        let size = spec
            .block_size
            .checked_mul(spec.blocks)
            .ok_or(KernelError::IllegalMemorySize)?;
        let size_usize = usize::try_from(size).map_err(|_| KernelError::IllegalMemorySize)?;
        guest_range
            .validate(spec.base, size_usize, 4)
            .map_err(|_| KernelError::IllegalMemoryBlock)?;
        let mut free = VecDeque::new();
        for index in 0..spec.blocks {
            free.push_back(spec.base + index * spec.block_size);
        }
        self.fixed_pools.insert(FixedPool {
            attributes: spec.attributes,
            block_size: spec.block_size,
            free,
            allocated: BTreeSet::new(),
            waiters: VecDeque::new(),
        })
    }

    /// Deletes an unused fixed pool and releases waiters.
    ///
    /// # Errors
    ///
    /// Returns an identifier or in-use error.
    pub fn delete_fixed_pool(&mut self, id: u32) -> Result<(), KernelError> {
        let pool = self
            .fixed_pools
            .get(id)
            .ok_or(KernelError::UnknownFixedPoolId)?;
        if !pool.allocated.is_empty() {
            return Err(KernelError::MemoryInUse);
        }
        let pool = self
            .fixed_pools
            .remove(id)
            .ok_or(KernelError::UnknownFixedPoolId)?;
        self.release_deleted_waiters(pool.waiters)
    }

    /// Allocates a fixed block immediately or blocks the current thread.
    ///
    /// # Errors
    ///
    /// Returns an identifier, context, clock, or scheduling error.
    pub fn allocate_fixed(
        &mut self,
        id: u32,
        timeout: Option<Ticks>,
        context: &mut CpuContext,
    ) -> Result<Result<u32, ScheduleAction>, crate::BiosError> {
        let thread_id = self.current_thread.ok_or(KernelError::CannotWait)?;
        let pool = self
            .fixed_pools
            .get_mut(id)
            .ok_or(KernelError::UnknownFixedPoolId)?;
        if let Some(address) = pool.free.pop_front() {
            pool.allocated.insert(address);
            return Ok(Ok(address));
        }
        insert_waiter(&mut pool.waiters, thread_id, pool.attributes, &self.threads);
        let deadline = timeout
            .map(|ticks| self.now.checked_advance(ticks))
            .transpose()?;
        let action = self.block_current(context, WaitReason::FixedPool(id), deadline)?;
        Ok(Err(action))
    }

    /// Frees a fixed block, assigning it directly to the first valid waiter.
    ///
    /// # Errors
    ///
    /// Returns an identifier, block, or scheduling error.
    pub fn free_fixed(&mut self, id: u32, address: u32) -> Result<Option<u32>, KernelError> {
        let removed = self
            .fixed_pools
            .get_mut(id)
            .ok_or(KernelError::UnknownFixedPoolId)?
            .allocated
            .remove(&address);
        if !removed {
            return Err(KernelError::IllegalMemoryBlock);
        }
        loop {
            let waiter = self
                .fixed_pools
                .get_mut(id)
                .ok_or(KernelError::UnknownFixedPoolId)?
                .waiters
                .pop_front();
            let Some(thread_id) = waiter else {
                self.fixed_pools
                    .get_mut(id)
                    .ok_or(KernelError::UnknownFixedPoolId)?
                    .free
                    .push_back(address);
                return Ok(None);
            };
            if self.wait_matches(thread_id, WaitReason::FixedPool(id)) {
                self.fixed_pools
                    .get_mut(id)
                    .ok_or(KernelError::UnknownFixedPoolId)?
                    .allocated
                    .insert(address);
                self.complete_wait(thread_id, 0, Some(address))?;
                return Ok(Some(thread_id));
            }
        }
    }

    /// Returns one fixed-pool block size.
    ///
    /// # Errors
    ///
    /// Returns an unknown-object error.
    pub fn fixed_block_size(&self, id: u32) -> Result<u32, KernelError> {
        self.fixed_pools
            .get(id)
            .map(|pool| pool.block_size)
            .ok_or(KernelError::UnknownFixedPoolId)
    }

    /// Creates a variable-size first-fit memory pool.
    ///
    /// # Errors
    ///
    /// Returns an attribute, range, size, or capacity error.
    pub fn create_variable_pool(
        &mut self,
        spec: VariablePoolSpec,
        guest_range: GuestRange,
    ) -> Result<u32, KernelError> {
        validate_attributes(spec.attributes)?;
        if spec.size == 0 || spec.size & 3 != 0 {
            return Err(KernelError::IllegalMemorySize);
        }
        let size = usize::try_from(spec.size).map_err(|_| KernelError::IllegalMemorySize)?;
        guest_range
            .validate(spec.base, size, 4)
            .map_err(|_| KernelError::IllegalMemoryBlock)?;
        self.variable_pools.insert(VariablePool {
            attributes: spec.attributes,
            free: vec![FreeRegion {
                address: spec.base,
                size: spec.size,
            }],
            allocated: BTreeMap::new(),
            waiters: VecDeque::new(),
        })
    }

    /// Deletes an unused variable pool and releases waiters.
    ///
    /// # Errors
    ///
    /// Returns an identifier or in-use error.
    pub fn delete_variable_pool(&mut self, id: u32) -> Result<(), KernelError> {
        let pool = self
            .variable_pools
            .get(id)
            .ok_or(KernelError::UnknownVariablePoolId)?;
        if !pool.allocated.is_empty() {
            return Err(KernelError::MemoryInUse);
        }
        let pool = self
            .variable_pools
            .remove(id)
            .ok_or(KernelError::UnknownVariablePoolId)?;
        self.release_deleted_waiters(pool.waiters)
    }

    /// Allocates variable-pool memory immediately or blocks the current thread.
    ///
    /// # Errors
    ///
    /// Returns a size, identifier, context, clock, or scheduling error.
    pub fn allocate_variable(
        &mut self,
        id: u32,
        size: u32,
        timeout: Option<Ticks>,
        context: &mut CpuContext,
    ) -> Result<Result<u32, ScheduleAction>, crate::BiosError> {
        let size = align_pool_size(size)?;
        let thread_id = self.current_thread.ok_or(KernelError::CannotWait)?;
        let pool = self
            .variable_pools
            .get_mut(id)
            .ok_or(KernelError::UnknownVariablePoolId)?;
        if let Some(address) = allocate_region(pool, size) {
            return Ok(Ok(address));
        }
        insert_waiter(&mut pool.waiters, thread_id, pool.attributes, &self.threads);
        let deadline = timeout
            .map(|ticks| self.now.checked_advance(ticks))
            .transpose()?;
        let action =
            self.block_current(context, WaitReason::VariablePool { id, size }, deadline)?;
        Ok(Err(action))
    }

    /// Frees one variable allocation and services satisfiable waiters in order.
    ///
    /// # Errors
    ///
    /// Returns an identifier, block, or scheduling error.
    pub fn free_variable(&mut self, id: u32, address: u32) -> Result<Vec<u32>, KernelError> {
        let size = self
            .variable_pools
            .get_mut(id)
            .ok_or(KernelError::UnknownVariablePoolId)?
            .allocated
            .remove(&address)
            .ok_or(KernelError::IllegalMemoryBlock)?;
        {
            let pool = self
                .variable_pools
                .get_mut(id)
                .ok_or(KernelError::UnknownVariablePoolId)?;
            pool.free.push(FreeRegion { address, size });
            coalesce_regions(&mut pool.free);
        }
        let queued = std::mem::take(
            &mut self
                .variable_pools
                .get_mut(id)
                .ok_or(KernelError::UnknownVariablePoolId)?
                .waiters,
        );
        let mut retained = VecDeque::new();
        let mut ready = Vec::new();
        for thread_id in queued {
            let requested = self
                .threads
                .get(thread_id)
                .and_then(|thread| match thread.state {
                    ThreadState::Waiting(WaitReason::VariablePool { id: wait_id, size })
                        if wait_id == id =>
                    {
                        Some(size)
                    }
                    _ => None,
                });
            let Some(requested) = requested else {
                continue;
            };
            let allocation = allocate_region(
                self.variable_pools
                    .get_mut(id)
                    .ok_or(KernelError::UnknownVariablePoolId)?,
                requested,
            );
            if let Some(block) = allocation {
                self.complete_wait(thread_id, 0, Some(block))?;
                ready.push(thread_id);
            } else {
                retained.push_back(thread_id);
            }
        }
        self.variable_pools
            .get_mut(id)
            .ok_or(KernelError::UnknownVariablePoolId)?
            .waiters = retained;
        Ok(ready)
    }

    /// Creates a one-shot guest alarm.
    ///
    /// # Errors
    ///
    /// Returns an entry, clock, capacity, or scheduler error.
    pub fn set_alarm(
        &mut self,
        delay: Ticks,
        entry: u32,
        argument: u32,
        guest_range: GuestRange,
    ) -> Result<u32, crate::BiosError> {
        guest_range
            .validate(entry, 4, 4)
            .map_err(|_| KernelError::IllegalEntry)?;
        let deadline = self.now.checked_advance(delay)?;
        let id = self.alarms.next_id().ok_or(KernelError::NoMemory)?;
        self.scheduler.schedule(alarm_event(id), deadline)?;
        self.alarms.insert_at(id, Alarm { entry, argument })?;
        Ok(id)
    }

    /// Cancels an alarm and returns its callback entry.
    ///
    /// # Errors
    ///
    /// Returns an illegal-object error for an unknown alarm.
    pub fn cancel_alarm(&mut self, id: u32) -> Result<u32, KernelError> {
        let alarm = self.alarms.remove(id).ok_or(KernelError::IllegalObject)?;
        self.scheduler.cancel(alarm_event(id));
        Ok(alarm.entry)
    }

    /// Advances to an absolute timestamp and dispatches due events in FIFO order.
    ///
    /// Object signals invoked before this method at the same timestamp take
    /// effect first; timed events are then processed in scheduler insertion order.
    ///
    /// # Errors
    ///
    /// Returns an invalid-time or ready-sequence error.
    pub fn advance_to(&mut self, now: Deadline) -> Result<Vec<KernelEvent>, KernelError> {
        if now < self.now {
            return Err(KernelError::IllegalSize);
        }
        self.now = now;
        let mut events = Vec::new();
        while let Some(due) = self.scheduler.pop_due(now) {
            let raw = due.id.get();
            if (WAIT_EVENT_BASE..ALARM_EVENT_BASE).contains(&raw) {
                let id = raw - WAIT_EVENT_BASE;
                let reason = self.threads.get(id).and_then(|thread| match thread.state {
                    ThreadState::Waiting(reason) => Some(reason),
                    _ => None,
                });
                if let Some(reason) = reason {
                    self.remove_waiter(id, reason);
                    let result = if reason == WaitReason::Delay {
                        0
                    } else {
                        KernelError::ReleaseWait.code()
                    };
                    self.complete_wait(id, result, None)?;
                    events.push(KernelEvent::ThreadReady(id));
                }
                continue;
            }
            if raw >= ALARM_EVENT_BASE
                && let Some(alarm) = self.alarms.remove(raw - ALARM_EVENT_BASE)
            {
                let id = raw - ALARM_EVENT_BASE;
                events.push(KernelEvent::Alarm {
                    id,
                    callback: CallbackRequest {
                        entry: alarm.entry,
                        argument: alarm.argument,
                        return_address: RETURN_ENTRY,
                        source: id,
                    },
                });
            }
        }
        Ok(events)
    }

    /// Saves the interrupted context and enters a guest callback.
    ///
    /// # Errors
    ///
    /// Returns an execution-context error when nesting exceeds the fixed bound.
    pub fn enter_callback(
        &mut self,
        context: &mut CpuContext,
        callback: CallbackRequest,
    ) -> Result<(), KernelError> {
        if self.callback_contexts.len() == CALLBACK_DEPTH {
            return Err(KernelError::IllegalContext);
        }
        self.callback_contexts.push(context.clone());
        callback.apply(context);
        Ok(())
    }

    /// Restores the most recently interrupted callback context.
    ///
    /// # Errors
    ///
    /// Returns an execution-context error without an active callback.
    pub fn return_from_callback(&mut self, context: &mut CpuContext) -> Result<(), KernelError> {
        *context = self
            .callback_contexts
            .pop()
            .ok_or(KernelError::IllegalContext)?;
        Ok(())
    }

    fn block_current(
        &mut self,
        context: &mut CpuContext,
        reason: WaitReason,
        deadline: Option<Deadline>,
    ) -> Result<ScheduleAction, KernelError> {
        let id = self.current_thread.take().ok_or(KernelError::CannotWait)?;
        let thread = self
            .threads
            .get_mut(id)
            .ok_or(KernelError::UnknownThreadId)?;
        thread.context = context.clone();
        thread.state = ThreadState::Waiting(reason);
        if let Some(deadline) = deadline {
            self.scheduler
                .schedule(wait_event(id), deadline)
                .map_err(|_| KernelError::NoMemory)?;
        }
        self.select(context, Some(id), true)
    }

    fn select(
        &mut self,
        context: &mut CpuContext,
        previous: Option<u32>,
        force: bool,
    ) -> Result<ScheduleAction, KernelError> {
        let candidate = self.best_ready();
        let should_switch = match (self.current_thread, candidate) {
            (None, Some(_)) => true,
            (None | Some(_), None) => false,
            (Some(current), Some(candidate)) => {
                let current_priority = self
                    .threads
                    .get(current)
                    .ok_or(KernelError::UnknownThreadId)?
                    .current_priority;
                let candidate_priority = self
                    .threads
                    .get(candidate)
                    .ok_or(KernelError::UnknownThreadId)?
                    .current_priority;
                force || candidate_priority < current_priority
            }
        };
        if !should_switch {
            return Ok(ScheduleAction {
                previous,
                current: self.current_thread,
                switched: false,
            });
        }
        if let Some(current) = self.current_thread.take() {
            let order = self.take_ready_order()?;
            let thread = self
                .threads
                .get_mut(current)
                .ok_or(KernelError::UnknownThreadId)?;
            thread.context = context.clone();
            thread.ready_order = order;
            thread.state = ThreadState::Ready;
        }
        let candidate = candidate.ok_or(KernelError::IllegalContext)?;
        let thread = self
            .threads
            .get_mut(candidate)
            .ok_or(KernelError::UnknownThreadId)?;
        thread.state = ThreadState::Running;
        *context = thread.context.clone();
        self.current_thread = Some(candidate);
        Ok(ScheduleAction {
            previous,
            current: Some(candidate),
            switched: previous != Some(candidate),
        })
    }

    fn best_ready(&self) -> Option<u32> {
        self.threads
            .iter()
            .filter(|(_, thread)| thread.state == ThreadState::Ready)
            .min_by_key(|(_, thread)| (thread.current_priority, thread.ready_order))
            .map(|(id, _)| id)
    }

    fn take_ready_order(&mut self) -> Result<u64, KernelError> {
        let order = self.next_ready_order;
        self.next_ready_order = self
            .next_ready_order
            .checked_add(1)
            .ok_or(KernelError::NoMemory)?;
        Ok(order)
    }

    fn wait_matches(&self, id: u32, expected: WaitReason) -> bool {
        self.threads
            .get(id)
            .is_some_and(|thread| thread.state == ThreadState::Waiting(expected))
    }

    fn complete_wait(
        &mut self,
        id: u32,
        result: i32,
        value: Option<u32>,
    ) -> Result<(), KernelError> {
        self.scheduler.cancel(wait_event(id));
        let order = self.take_ready_order()?;
        let thread = self
            .threads
            .get_mut(id)
            .ok_or(KernelError::UnknownThreadId)?;
        if !matches!(thread.state, ThreadState::Waiting(_)) {
            return Err(KernelError::NotWaiting);
        }
        thread
            .context
            .set_register(V0, u32::from_ne_bytes(result.to_ne_bytes()));
        if let Some(value) = value {
            thread.context.set_register(V1, value);
        }
        thread.ready_order = order;
        thread.state = ThreadState::Ready;
        Ok(())
    }

    fn remove_waiter(&mut self, id: u32, reason: WaitReason) {
        match reason {
            WaitReason::Sleep
            | WaitReason::Delay
            | WaitReason::VBlankStart
            | WaitReason::VBlankEnd
            | WaitReason::ModuleStart(_) => {}
            WaitReason::Semaphore(object) => {
                if let Some(value) = self.semaphores.get_mut(object) {
                    value.waiters.retain(|waiter| *waiter != id);
                }
            }
            WaitReason::EventFlag { id: object, .. } => {
                if let Some(value) = self.event_flags.get_mut(object) {
                    value.waiters.retain(|waiter| *waiter != id);
                }
            }
            WaitReason::MessageBox(object) => {
                if let Some(value) = self.message_boxes.get_mut(object) {
                    value.waiters.retain(|waiter| *waiter != id);
                }
            }
            WaitReason::FixedPool(object) => {
                if let Some(value) = self.fixed_pools.get_mut(object) {
                    value.waiters.retain(|waiter| *waiter != id);
                }
            }
            WaitReason::VariablePool { id: object, .. } => {
                if let Some(value) = self.variable_pools.get_mut(object) {
                    value.waiters.retain(|waiter| *waiter != id);
                }
            }
        }
    }

    fn release_deleted_waiters(&mut self, waiters: VecDeque<u32>) -> Result<(), KernelError> {
        for id in waiters {
            if self
                .threads
                .get(id)
                .is_some_and(|thread| matches!(thread.state, ThreadState::Waiting(_)))
            {
                self.complete_wait(id, KernelError::WaitDeleted.code(), None)?;
            }
        }
        Ok(())
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_priority(priority: u32) -> Result<(), KernelError> {
    if (MIN_THREAD_PRIORITY..=MAX_THREAD_PRIORITY).contains(&priority) {
        Ok(())
    } else {
        Err(KernelError::IllegalPriority)
    }
}

fn validate_attributes(attributes: u32) -> Result<(), KernelError> {
    if attributes & !0x101 == 0 {
        Ok(())
    } else {
        Err(KernelError::IllegalAttribute)
    }
}

fn validate_thread_attributes(attributes: u32) -> Result<(), KernelError> {
    const THREAD_ATTRIBUTE_MASK: u32 = 0x0330_0008;
    if attributes & !THREAD_ATTRIBUTE_MASK == 0 {
        Ok(())
    } else {
        Err(KernelError::IllegalAttribute)
    }
}

fn validate_event_wait(pattern: u32, mode: u32) -> Result<(), KernelError> {
    if pattern == 0 {
        return Err(KernelError::EventFlagIllegalPattern);
    }
    if mode & !(WAIT_ANY | WAIT_CLEAR) != 0 {
        return Err(KernelError::IllegalMode);
    }
    Ok(())
}

fn consume_event_bits(event: &mut EventFlag, pattern: u32, mode: u32) -> Option<u32> {
    let satisfied = if mode & WAIT_ANY != 0 {
        event.bits & pattern != 0
    } else {
        event.bits & pattern == pattern
    };
    if !satisfied {
        return None;
    }
    let value = event.bits;
    if mode & WAIT_CLEAR != 0 {
        event.bits &= !pattern;
    }
    Some(value)
}

fn insert_waiter<const N: usize>(
    waiters: &mut VecDeque<u32>,
    thread_id: u32,
    attributes: u32,
    threads: &FixedTable<Thread, N>,
) {
    if attributes & 1 == 0 {
        waiters.push_back(thread_id);
        return;
    }
    let priority = threads
        .get(thread_id)
        .map_or(MAX_THREAD_PRIORITY + 1, Thread::priority);
    let position = waiters.iter().position(|id| {
        threads
            .get(*id)
            .is_some_and(|thread| thread.priority() > priority)
    });
    if let Some(position) = position {
        waiters.insert(position, thread_id);
    } else {
        waiters.push_back(thread_id);
    }
}

fn align_pool_size(size: u32) -> Result<u32, KernelError> {
    if size == 0 {
        return Err(KernelError::IllegalMemorySize);
    }
    size.checked_add(3)
        .map(|value| value & !3)
        .ok_or(KernelError::IllegalMemorySize)
}

fn allocate_region(pool: &mut VariablePool, size: u32) -> Option<u32> {
    let index = pool.free.iter().position(|region| region.size >= size)?;
    let address = pool.free[index].address;
    pool.free[index].address = pool.free[index].address.checked_add(size)?;
    pool.free[index].size -= size;
    if pool.free[index].size == 0 {
        pool.free.remove(index);
    }
    pool.allocated.insert(address, size);
    Some(address)
}

fn coalesce_regions(regions: &mut Vec<FreeRegion>) {
    regions.sort_by_key(|region| region.address);
    let mut output: Vec<FreeRegion> = Vec::with_capacity(regions.len());
    for region in regions.drain(..) {
        if let Some(previous) = output.last_mut()
            && previous.address.checked_add(previous.size) == Some(region.address)
        {
            previous.size += region.size;
            continue;
        }
        output.push(region);
    }
    *regions = output;
}

fn wait_event(id: u32) -> EventId {
    EventId::new(WAIT_EVENT_BASE + id)
}

fn alarm_event(id: u32) -> EventId {
    EventId::new(ALARM_EVENT_BASE + id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANGE: GuestRange = GuestRange {
        start: 0,
        end: 0x20_0000,
    };

    fn thread_spec(index: u32, priority: u32) -> ThreadSpec {
        ThreadSpec {
            entry: 0x10_000 + index * 0x100,
            stack: 0x18_0000 + index * 0x1000,
            stack_size: 0x800,
            priority,
            global_pointer: 0x1234_0000 + index,
            attributes: 0,
            option: index,
        }
    }

    fn start_threads(priorities: &[u32]) -> (Kernel, CpuContext, Vec<u32>) {
        let mut kernel = Kernel::new();
        let mut ids = Vec::new();
        for (index, priority) in priorities.iter().copied().enumerate() {
            let index = u32::try_from(index).unwrap();
            let id = kernel
                .create_thread(thread_spec(index, priority), RANGE)
                .unwrap();
            kernel.start_thread(id, 0x1000 + index).unwrap();
            ids.push(id);
        }
        let mut context = CpuContext::reset(0, 0);
        kernel
            .reschedule(&mut context, RescheduleReason::HleReturn)
            .unwrap();
        (kernel, context, ids)
    }

    #[test]
    fn priority_lifecycle_sleep_delay_and_ready_queues_are_exact() {
        let mut kernel = Kernel::new();
        assert_eq!(
            kernel.create_thread(thread_spec(0, 0), RANGE),
            Err(KernelError::IllegalPriority)
        );
        let mut short = thread_spec(0, 20);
        short.stack_size = MIN_STACK_SIZE - 1;
        assert_eq!(
            kernel.create_thread(short, RANGE),
            Err(KernelError::IllegalStackSize)
        );

        let low = kernel.create_thread(thread_spec(0, 40), RANGE).unwrap();
        let high_a = kernel.create_thread(thread_spec(1, 20), RANGE).unwrap();
        let high_b = kernel.create_thread(thread_spec(2, 20), RANGE).unwrap();
        kernel.start_thread(low, 0xaa).unwrap();
        let mut context = CpuContext::reset(0, 0);
        let action = kernel
            .reschedule(&mut context, RescheduleReason::HleReturn)
            .unwrap();
        assert_eq!(action.current, Some(low));
        assert_eq!(context.register(A0), Some(0xaa));
        assert_eq!(context.register(RA), Some(THREAD_RETURN_ENTRY));
        assert_eq!(
            context.register(GP),
            Some(thread_spec(0, 40).global_pointer)
        );

        kernel.start_thread(high_a, 0xbb).unwrap();
        kernel.start_thread(high_b, 0xcc).unwrap();
        let action = kernel
            .reschedule(&mut context, RescheduleReason::HleReturn)
            .unwrap();
        assert_eq!(action.current, Some(high_a));
        assert_eq!(kernel.thread(low).unwrap().state(), ThreadState::Ready);
        assert_eq!(context.register(A0), Some(0xbb));
        assert_eq!(
            context.register(GP),
            Some(thread_spec(1, 20).global_pointer)
        );

        let action = kernel
            .reschedule(&mut context, RescheduleReason::Yield)
            .unwrap();
        assert_eq!(action.current, Some(high_b));
        let action = kernel.sleep_current(&mut context).unwrap();
        assert_eq!(action.current, Some(high_a));
        assert_eq!(
            kernel.thread(high_b).unwrap().state(),
            ThreadState::Waiting(WaitReason::Sleep)
        );
        assert!(kernel.wakeup_thread(high_b).unwrap());
        let action = kernel
            .reschedule(&mut context, RescheduleReason::ObjectSignal)
            .unwrap();
        assert!(!action.switched);
        assert_eq!(action.current, Some(high_a));
        assert_eq!(
            kernel
                .reschedule(&mut context, RescheduleReason::Yield)
                .unwrap()
                .current,
            Some(high_b)
        );

        assert_eq!(
            kernel
                .delay_current(Ticks::new(10), &mut context)
                .unwrap()
                .current,
            Some(high_a)
        );
        assert!(kernel.advance_to(Deadline::new(9)).unwrap().is_empty());
        assert_eq!(
            kernel.advance_to(Deadline::new(10)).unwrap(),
            [KernelEvent::ThreadReady(high_b)]
        );
        assert_eq!(
            kernel
                .reschedule(&mut context, RescheduleReason::VBlank)
                .unwrap()
                .current,
            Some(high_a)
        );
        kernel.change_priority(low, 10).unwrap();
        assert_eq!(
            kernel
                .reschedule(&mut context, RescheduleReason::HleReturn)
                .unwrap()
                .current,
            Some(low)
        );
        assert_eq!(
            kernel.exit_current(&mut context).unwrap().current,
            Some(high_b)
        );
        assert_eq!(kernel.thread(low).unwrap().state(), ThreadState::Dormant);
        kernel.delete_thread(low).unwrap();
        assert!(kernel.thread(low).is_none());
    }

    #[test]
    fn vblank_waits_wake_on_the_selected_boundary() {
        let (mut kernel, mut context, ids) = start_threads(&[10, 20]);
        assert_eq!(
            kernel.wait_vblank(0, &mut context).unwrap().current,
            Some(ids[1])
        );
        assert_eq!(
            kernel.thread(ids[0]).unwrap().state(),
            ThreadState::Waiting(WaitReason::VBlankStart)
        );
        assert!(kernel.notify_vblank(1).unwrap().is_empty());
        assert_eq!(kernel.notify_vblank(0).unwrap(), [ids[0]]);
        assert_eq!(
            kernel
                .reschedule(&mut context, RescheduleReason::VBlank)
                .unwrap()
                .current,
            Some(ids[0])
        );

        kernel.wait_vblank(1, &mut context).unwrap();
        assert!(kernel.notify_vblank(0).unwrap().is_empty());
        assert_eq!(kernel.notify_vblank(1).unwrap(), [ids[0]]);
        assert_eq!(
            kernel.wait_vblank(2, &mut context),
            Err(KernelError::IllegalObject)
        );
    }

    #[test]
    fn module_start_threads_return_results_to_the_requester() {
        let mut kernel = Kernel::new();
        let requester = kernel.create_thread(thread_spec(0, 10), RANGE).unwrap();
        kernel.start_thread(requester, 0).unwrap();
        let mut context = CpuContext::reset(0, 0);
        kernel
            .reschedule(&mut context, RescheduleReason::HleReturn)
            .unwrap();

        let child = kernel.create_thread(thread_spec(1, 8), RANGE).unwrap();
        kernel
            .start_thread_with_context(child, [1, 2, 3, 4], RETURN_ENTRY)
            .unwrap();
        let action = kernel.wait_module_start(child, &mut context).unwrap();
        assert_eq!(action.current, Some(child));
        assert_eq!(context.register(A0), Some(1));
        assert_eq!(context.register(A1), Some(2));
        assert_eq!(context.register(A2), Some(3));
        assert_eq!(context.register(A3), Some(4));
        assert_eq!(context.register(RA), Some(RETURN_ENTRY));
        assert_eq!(
            kernel.thread(requester).unwrap().state(),
            ThreadState::Waiting(WaitReason::ModuleStart(child))
        );

        kernel
            .complete_module_start(requester, child, 7, 2)
            .unwrap();
        let action = kernel.exit_delete_current(&mut context).unwrap();
        assert_eq!(action.current, Some(requester));
        assert_eq!(context.register(V0), Some(7));
        assert_eq!(context.register(V1), Some(2));
        assert!(kernel.thread(child).is_none());
    }

    #[test]
    fn semaphores_preempt_and_deletion_releases_waiters() {
        let (mut kernel, mut context, ids) = start_threads(&[10, 30]);
        let high = ids[0];
        let low = ids[1];
        let semaphore = kernel
            .create_semaphore(SemaphoreSpec {
                initial: 0,
                maximum: 1,
                attributes: 1,
            })
            .unwrap();
        assert_eq!(
            kernel.poll_semaphore(semaphore),
            Err(KernelError::SemaphoreZero)
        );
        let action = kernel
            .wait_semaphore(semaphore, None, &mut context)
            .unwrap()
            .unwrap();
        assert_eq!(action.current, Some(low));
        assert_eq!(kernel.signal_semaphore(semaphore).unwrap(), Some(high));
        assert_eq!(
            kernel
                .reschedule(&mut context, RescheduleReason::ObjectSignal)
                .unwrap()
                .current,
            Some(high)
        );
        assert_eq!(context.register(V0), Some(0));

        kernel
            .wait_semaphore(semaphore, None, &mut context)
            .unwrap();
        kernel.delete_semaphore(semaphore).unwrap();
        assert_eq!(
            kernel.thread(high).unwrap().context().register(V0),
            Some(u32::from_ne_bytes(
                KernelError::WaitDeleted.code().to_ne_bytes()
            ))
        );
        assert_eq!(
            kernel.signal_semaphore(semaphore),
            Err(KernelError::UnknownSemaphoreId)
        );
    }

    #[test]
    fn event_flags_cover_all_any_clear_multiple_and_timeout() {
        let (mut kernel, mut context, ids) = start_threads(&[10, 20, 30]);
        let first = ids[0];
        let second = ids[1];
        let event = kernel
            .create_event_flag(EventFlagSpec {
                bits: 0,
                attributes: 3,
            })
            .unwrap();
        assert!(
            kernel
                .wait_event_flag(event, 3, 0, None, &mut context)
                .unwrap()
                .is_err()
        );
        assert_eq!(kernel.current_thread(), Some(second));
        assert!(
            kernel
                .wait_event_flag(event, 4, WAIT_ANY, None, &mut context)
                .unwrap()
                .is_err()
        );
        assert_eq!(kernel.set_event_flag(event, 7).unwrap(), [first, second]);
        assert_eq!(
            kernel.thread(first).unwrap().context().register(V1),
            Some(7)
        );
        assert_eq!(kernel.poll_event_flag(event, 1, WAIT_CLEAR).unwrap(), 7);
        assert_eq!(
            kernel.poll_event_flag(event, 1, 0),
            Err(KernelError::EventFlagCondition)
        );
        assert_eq!(
            kernel.poll_event_flag(event, 0, 0),
            Err(KernelError::EventFlagIllegalPattern)
        );

        let single = kernel
            .create_event_flag(EventFlagSpec {
                bits: 0,
                attributes: 0,
            })
            .unwrap();
        kernel
            .reschedule(&mut context, RescheduleReason::HleReturn)
            .unwrap();
        let _ = kernel
            .wait_event_flag(single, 1, 0, Some(Ticks::new(4)), &mut context)
            .unwrap();
        assert_eq!(kernel.advance_to(Deadline::new(4)).unwrap().len(), 1);
        assert_eq!(
            kernel.thread(first).unwrap().context().register(V0),
            Some(u32::from_ne_bytes(
                KernelError::ReleaseWait.code().to_ne_bytes()
            ))
        );
    }

    #[test]
    fn message_boxes_transfer_exact_guest_pointers() {
        let (mut kernel, mut context, ids) = start_threads(&[10, 20]);
        let high = ids[0];
        let message_box = kernel
            .create_message_box(MessageBoxSpec { attributes: 0 })
            .unwrap();
        assert_eq!(
            kernel.poll_message(message_box),
            Err(KernelError::MessageBoxNoMessage)
        );
        assert!(
            kernel
                .receive_message(message_box, None, &mut context)
                .unwrap()
                .is_err()
        );
        assert_eq!(
            kernel.send_message(message_box, 0x12_340).unwrap(),
            Some(high)
        );
        assert_eq!(
            kernel.thread(high).unwrap().context().register(V1),
            Some(0x12_340)
        );
        assert_eq!(kernel.send_message(message_box, 0x12_344).unwrap(), None);
        assert_eq!(kernel.poll_message(message_box).unwrap(), 0x12_344);
        assert_eq!(
            kernel.send_message(message_box, 1),
            Err(KernelError::IllegalObject)
        );
    }

    #[test]
    fn fixed_and_variable_pools_reuse_and_coalesce_exact_ranges() {
        let (mut kernel, mut context, ids) = start_threads(&[10, 20]);
        let high = ids[0];
        let fixed = kernel
            .create_fixed_pool(
                FixedPoolSpec {
                    base: 0x10_0000,
                    block_size: 0x20,
                    blocks: 1,
                    attributes: 0,
                },
                RANGE,
            )
            .unwrap();
        let address = kernel
            .allocate_fixed(fixed, None, &mut context)
            .unwrap()
            .unwrap();
        assert_eq!(address, 0x10_0000);
        assert!(
            kernel
                .allocate_fixed(fixed, None, &mut context)
                .unwrap()
                .is_err()
        );
        assert_eq!(
            kernel.delete_fixed_pool(fixed),
            Err(KernelError::MemoryInUse)
        );
        assert_eq!(kernel.free_fixed(fixed, address).unwrap(), Some(high));
        assert_eq!(
            kernel.thread(high).unwrap().context().register(V1),
            Some(address)
        );
        kernel
            .reschedule(&mut context, RescheduleReason::ObjectSignal)
            .unwrap();
        kernel.free_fixed(fixed, address).unwrap();
        kernel.delete_fixed_pool(fixed).unwrap();

        let variable = kernel
            .create_variable_pool(
                VariablePoolSpec {
                    base: 0x11_0000,
                    size: 0x100,
                    attributes: 0,
                },
                RANGE,
            )
            .unwrap();
        let first = kernel
            .allocate_variable(variable, 0x21, None, &mut context)
            .unwrap()
            .unwrap();
        let second = kernel
            .allocate_variable(variable, 0x40, None, &mut context)
            .unwrap()
            .unwrap();
        assert_eq!((first, second), (0x11_0000, 0x11_0024));
        kernel.free_variable(variable, first).unwrap();
        kernel.free_variable(variable, second).unwrap();
        let whole = kernel
            .allocate_variable(variable, 0x100, None, &mut context)
            .unwrap()
            .unwrap();
        assert_eq!(whole, 0x11_0000);
        kernel.free_variable(variable, whole).unwrap();
        kernel.delete_variable_pool(variable).unwrap();
    }

    #[test]
    fn equal_timestamp_delays_and_alarms_are_fifo_and_callbacks_restore() {
        let (mut kernel, mut context, ids) = start_threads(&[10, 20]);
        let delayed = ids[0];
        kernel.delay_current(Ticks::new(10), &mut context).unwrap();
        let first_alarm = kernel
            .set_alarm(Ticks::new(10), 0x10_000, 0xaa, RANGE)
            .unwrap();
        let second_alarm = kernel
            .set_alarm(Ticks::new(10), 0x10_100, 0xbb, RANGE)
            .unwrap();
        let events = kernel.advance_to(Deadline::new(10)).unwrap();
        assert_eq!(events[0], KernelEvent::ThreadReady(delayed));
        assert!(matches!(
            events[1],
            KernelEvent::Alarm { id, .. } if id == first_alarm
        ));
        assert!(matches!(
            events[2],
            KernelEvent::Alarm { id, .. } if id == second_alarm
        ));

        let original = context.clone();
        let KernelEvent::Alarm { callback, .. } = events[1] else {
            panic!("expected alarm");
        };
        kernel.enter_callback(&mut context, callback).unwrap();
        assert_eq!(context.pc, 0x10_000);
        assert_eq!(context.register(A0), Some(0xaa));
        assert_eq!(context.register(RA), Some(RETURN_ENTRY));
        kernel.return_from_callback(&mut context).unwrap();
        assert_eq!(context, original);
        assert_eq!(
            kernel.cancel_alarm(first_alarm),
            Err(KernelError::IllegalObject)
        );
    }

    #[test]
    fn object_tables_are_fixed_and_failed_creation_is_non_mutating() {
        let mut kernel = Kernel::new();
        for _ in 0..SEMAPHORE_CAPACITY {
            kernel
                .create_semaphore(SemaphoreSpec {
                    initial: 0,
                    maximum: 1,
                    attributes: 0,
                })
                .unwrap();
        }
        assert_eq!(
            kernel.create_semaphore(SemaphoreSpec {
                initial: 0,
                maximum: 1,
                attributes: 0,
            }),
            Err(KernelError::NoMemory)
        );
        assert_eq!(
            kernel.create_event_flag(EventFlagSpec {
                bits: 0,
                attributes: 4,
            }),
            Err(KernelError::IllegalAttribute)
        );
        assert_eq!(
            kernel.create_fixed_pool(
                FixedPoolSpec {
                    base: 0,
                    block_size: 0,
                    blocks: 1,
                    attributes: 0,
                },
                RANGE,
            ),
            Err(KernelError::IllegalMemorySize)
        );
        assert_eq!(kernel.threads().count(), 0);
    }
}
