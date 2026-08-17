// SPDX-License-Identifier: LGPL-2.1-or-later

use std::collections::{BTreeMap, BTreeSet};

use upse_iop_irq::{InterruptController, InterruptSource};
use upse_iop_services::{
    BackendError, BackendPayload, BackendRequest, BackendResponse, BiosServices, ServiceAction,
    ServiceContext, ServiceFamily, ServiceMemory,
};
use upse_iop_timers::{
    IopTimers, TIMER0_BASE, TIMER1_BASE, TIMER2_BASE, TIMER3_BASE, TIMER4_BASE, TIMER5_BASE,
    TimerId,
};
use upse_irx::IrxModule;
use upse_ps2_bios::{
    AllocationMode, BiosHle, CallbackRequest, EventFlagSpec, GuestRange, KernelError,
    RescheduleReason, SemaphoreSpec, ThreadSpec,
};
use upse_ps2_spu2::{
    CORE_ATTR_EFFECT_ENABLE, CORE_ATTR_ENABLE, CORE_ATTR_IRQ_ENABLE, CORE_ATTR_UNMUTE,
    MMIX_INPUT_A_DRY_LEFT, MMIX_INPUT_A_DRY_RIGHT, MMIX_INPUT_B_DRY_LEFT, MMIX_INPUT_B_DRY_RIGHT,
    MMIX_VOICE_DRY_LEFT, MMIX_VOICE_DRY_RIGHT, SPU2_BASE, Spu2,
};

use crate::{
    BiosMemoryAdapter, ModuleFrame, bind_module_imports, bios_context_from_service,
    service_context_from_bios,
};

const V0: usize = 2;
const V1: usize = 3;
const A0: usize = 4;
const A1: usize = 5;
const A2: usize = 6;
const A3: usize = 7;
const GP: usize = 28;
const RA: usize = 31;
const RETURN_ENTRY: u32 = upse_ps2_bios::RETURN_ENTRY;
const DEFAULT_THREAD_STACK: u32 = 16 * 1024;
const CORE_STRIDE: u32 = 0x400;
const VOICE_STRIDE: u32 = 0x10;
const VOICE_ADDRESS_BASE: u32 = 0x1c0;
const VOICE_ADDRESS_STRIDE: u32 = 0x0c;
const PRIMARY_BASE: u32 = 0x760;
const PRIMARY_STRIDE: u32 = 0x28;
// Some sound drivers fold the BIOS clock into a 24-bit counter and require a
// realistic nonzero boot epoch to distinguish startup from counter wrap.
pub(super) const SYSTEM_CLOCK_EPOCH: u64 = 0x0be0_0000;
const TIMER_COUNT: usize = 6;
const TIMER_BASES: [u32; TIMER_COUNT] = [
    TIMER0_BASE,
    TIMER1_BASE,
    TIMER2_BASE,
    TIMER3_BASE,
    TIMER4_BASE,
    TIMER5_BASE,
];
const TIMER_ALLOCATION_ORDER: [usize; TIMER_COUNT] = [2, 5, 4, 3, 0, 1];

#[derive(Clone, Copy, Debug, Default)]
struct TimerSlot {
    allocated: bool,
    control: u16,
    compare: u32,
    handler: Option<(u32, u32)>,
    callback_pending: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TimerManager {
    slots: [TimerSlot; TIMER_COUNT],
}

impl TimerManager {
    fn allocate(&mut self, source: u32, size: u32, prescale: u32) -> u32 {
        for index in TIMER_ALLOCATION_ORDER {
            let compatible = match index {
                0 => source & 0x0b != 0 && size == 16 && prescale <= 1,
                1 => source & 0x0d != 0 && size == 16 && prescale <= 1,
                2 => source & 1 != 0 && size == 16 && prescale <= 8,
                3 => source & 5 != 0 && size == 32 && prescale <= 1,
                4 | 5 => source & 1 != 0 && size == 32 && prescale <= 256,
                _ => false,
            };
            if compatible && !self.slots[index].allocated {
                self.slots[index] = TimerSlot {
                    allocated: true,
                    ..TimerSlot::default()
                };
                return timer_handle(index);
            }
        }
        kernel_code(-150)
    }

    fn index(&self, handle: u32) -> Option<usize> {
        let index = usize::try_from(handle >> 28).ok()?.checked_sub(1)?;
        (index < TIMER_COUNT
            && self.slots[index].allocated
            && handle.wrapping_shl(4) == TIMER_BASES[index])
            .then_some(index)
    }

    pub(crate) fn callback(&mut self, timer: TimerId) -> Option<CallbackRequest> {
        let index = timer as usize;
        let slot = &mut self.slots[index];
        let (entry, argument) = slot.handler?;
        if slot.callback_pending {
            return None;
        }
        slot.callback_pending = true;
        Some(CallbackRequest {
            entry,
            argument,
            return_address: RETURN_ENTRY,
            source: index as u32,
        })
    }

    pub(crate) fn finish_callback(
        &mut self,
        timer: TimerId,
        result: u32,
        timers: &mut IopTimers,
    ) -> Result<(), upse_iop_timers::TimerError> {
        let index = timer as usize;
        let slot = &mut self.slots[index];
        slot.callback_pending = false;
        if result == 0 {
            slot.control = 0;
            timers.write_u32(TIMER_BASES[index] + 4, 0)
        } else {
            slot.compare = result;
            timers.write_u32(TIMER_BASES[index] + 8, result)
        }
    }
}

pub(crate) struct MachineServices<'a> {
    pub(crate) bios: &'a mut BiosHle,
    pub(crate) sound: &'a mut Spu2,
    pub(crate) irq: &'a mut InterruptController,
    pub(crate) timers: &'a mut IopTimers,
    pub(crate) timer_manager: &'a mut TimerManager,
    pub(crate) interrupts_enabled: &'a mut bool,
    pub(crate) enabled_interrupts: &'a mut BTreeSet<u32>,
    pub(crate) thread_stacks: &'a mut BTreeMap<u32, u32>,
    pub(crate) module_frames: &'a mut Vec<ModuleFrame>,
    pub(crate) module_entry_active: bool,
    pub(crate) interrupt_context: bool,
}

impl MachineServices<'_> {
    fn kernel_result(result: Result<u32, KernelError>) -> BackendResponse {
        BackendResponse::returning(match result {
            Ok(value) => value,
            Err(error) => u32::from_ne_bytes(error.code().to_ne_bytes()),
        })
    }

    fn schedule(
        &mut self,
        service: &mut ServiceContext,
        reason: RescheduleReason,
    ) -> Result<BackendResponse, BackendError> {
        let mut cpu = bios_context_from_service(service);
        let schedule = self
            .bios
            .kernel_mut()
            .reschedule(&mut cpu, reason)
            .map_err(backend)?;
        service_context_from_bios(&cpu, service);
        Ok(BackendResponse {
            v0: cpu.register(V0).unwrap_or(0),
            v1: cpu.register(V1),
            action: if schedule.switched {
                ServiceAction::ContextSwitch
            } else {
                ServiceAction::Return
            },
        })
    }

    fn dispatch_system_memory(
        &mut self,
        ordinal: u16,
        [a0, a1, a2, _]: [u32; 4],
    ) -> Result<BackendResponse, BackendError> {
        let response = match ordinal {
            3 => BackendResponse::returning(0),
            4 => Self::kernel_result(
                AllocationMode::try_from(a0)
                    .and_then(|mode| self.bios.memory_mut().allocate(mode, a1, a2))
                    .map(|allocation| allocation.address),
            ),
            5 => Self::kernel_result(self.bios.memory_mut().free(a0).map(|_| 0)),
            6 => BackendResponse::returning(self.bios.memory().memory_size()),
            7 => BackendResponse::returning(self.bios.memory().maximum_free()),
            8 => BackendResponse::returning(self.bios.memory().total_free()),
            9 => Self::kernel_result(
                self.bios
                    .memory()
                    .block(a0)
                    .map(|allocation| allocation.address)
                    .ok_or(KernelError::IllegalMemoryBlock),
            ),
            10 => Self::kernel_result(
                self.bios
                    .memory()
                    .block(a0)
                    .map(|allocation| allocation.requested_size)
                    .ok_or(KernelError::IllegalMemoryBlock),
            ),
            _ => return Err(unsupported("sysmem", ordinal)),
        };
        Ok(response)
    }

    fn dispatch_thread<M: ServiceMemory>(
        &mut self,
        ordinal: u16,
        [a0, a1, _, _]: [u32; 4],
        context: &mut ServiceContext,
        memory: &mut M,
    ) -> Result<BackendResponse, BackendError> {
        match ordinal {
            4 => {
                let attributes = read_u32(memory, a0)?;
                let option = read_u32(memory, a0 + 4)?;
                let entry = read_u32(memory, a0 + 8)?;
                let stack_size = read_u32(memory, a0 + 12)?.max(DEFAULT_THREAD_STACK);
                let priority = read_u32(memory, a0 + 16)?;
                let stack =
                    match self
                        .bios
                        .memory_mut()
                        .allocate(AllocationMode::First, stack_size, 0)
                    {
                        Ok(allocation) => allocation.address,
                        Err(error) => return Ok(Self::kernel_result(Err(error))),
                    };
                let result = self
                    .bios
                    .kernel_mut()
                    .create_thread(
                        ThreadSpec {
                            entry,
                            stack,
                            stack_size,
                            priority,
                            global_pointer: context.register(GP).unwrap_or(0),
                            attributes,
                            option,
                        },
                        guest_range(memory),
                    )
                    .inspect(|id| {
                        self.thread_stacks.insert(*id, stack);
                    });
                if result.is_err() {
                    let _ = self.bios.memory_mut().free(stack);
                }
                Ok(Self::kernel_result(result))
            }
            5 => {
                let result = self.bios.kernel_mut().delete_thread(a0).map(|_| 0);
                if result.is_ok()
                    && let Some(stack) = self.thread_stacks.remove(&a0)
                {
                    let _ = self.bios.memory_mut().free(stack);
                }
                Ok(Self::kernel_result(result))
            }
            6 | 7 => {
                if let Err(error) = self.bios.kernel_mut().start_thread(a0, a1) {
                    return Ok(Self::kernel_result(Err(error)));
                }
                if self.module_entry_active || self.bios.kernel().current_thread().is_none() {
                    Ok(BackendResponse::returning(0))
                } else {
                    self.schedule(context, RescheduleReason::HleReturn)
                }
            }
            8 => {
                let mut cpu = bios_context_from_service(context);
                let action = self
                    .bios
                    .kernel_mut()
                    .exit_current(&mut cpu)
                    .map_err(backend)?;
                service_context_from_bios(&cpu, context);
                Ok(schedule_response(&cpu, action.switched))
            }
            9 => {
                let current = self.bios.kernel().current_thread();
                let mut cpu = bios_context_from_service(context);
                let action = self
                    .bios
                    .kernel_mut()
                    .exit_delete_current(&mut cpu)
                    .map_err(backend)?;
                if let Some(id) = current
                    && let Some(stack) = self.thread_stacks.remove(&id)
                {
                    let _ = self.bios.memory_mut().free(stack);
                }
                service_context_from_bios(&cpu, context);
                Ok(schedule_response(&cpu, action.switched))
            }
            14 => Ok(Self::kernel_result(
                self.bios.kernel_mut().change_priority(a0, a1),
            )),
            16 => self.schedule(context, RescheduleReason::Yield),
            18 | 19 => Ok(Self::kernel_result(
                self.bios.kernel_mut().release_wait(a0).map(|()| 0),
            )),
            20 => Ok(BackendResponse::returning(
                self.bios.kernel().current_thread().unwrap_or(0),
            )),
            24 => {
                let mut cpu = bios_context_from_service(context);
                let action = self
                    .bios
                    .kernel_mut()
                    .sleep_current(&mut cpu)
                    .map_err(backend)?;
                service_context_from_bios(&cpu, context);
                Ok(schedule_response(&cpu, action.switched))
            }
            25 | 26 => Ok(Self::kernel_result(
                self.bios.kernel_mut().wakeup_thread(a0).map(|_| 0),
            )),
            33 => {
                let mut cpu = bios_context_from_service(context);
                let action = self
                    .bios
                    .kernel_mut()
                    .delay_current(upse_clock::Ticks::new(u64::from(a0) * 37), &mut cpu)
                    .map_err(backend)?;
                service_context_from_bios(&cpu, context);
                Ok(schedule_response(&cpu, action.switched))
            }
            34 => {
                let clock = SYSTEM_CLOCK_EPOCH
                    .wrapping_add(self.bios.kernel().now().get())
                    .to_le_bytes();
                memory.write(a0, &clock).map_err(backend)?;
                Ok(BackendResponse::returning(0))
            }
            _ => Err(unsupported("thbase", ordinal)),
        }
    }

    fn dispatch_semaphore<M: ServiceMemory>(
        &mut self,
        ordinal: u16,
        [a0, _, _, _]: [u32; 4],
        context: &mut ServiceContext,
        memory: &mut M,
    ) -> Result<BackendResponse, BackendError> {
        match ordinal {
            4 => {
                let attributes = read_u32(memory, a0)?;
                let initial = read_u32(memory, a0 + 8)?;
                let maximum = read_u32(memory, a0 + 12)?;
                Ok(Self::kernel_result(
                    self.bios.kernel_mut().create_semaphore(SemaphoreSpec {
                        initial,
                        maximum,
                        attributes,
                    }),
                ))
            }
            5 => Ok(Self::kernel_result(
                self.bios.kernel_mut().delete_semaphore(a0).map(|()| 0),
            )),
            6 | 7 => Ok(Self::kernel_result(
                self.bios.kernel_mut().signal_semaphore(a0).map(|_| 0),
            )),
            8 => {
                let mut cpu = bios_context_from_service(context);
                let action = self
                    .bios
                    .kernel_mut()
                    .wait_semaphore(a0, None, &mut cpu)
                    .map_err(backend)?;
                service_context_from_bios(&cpu, context);
                Ok(schedule_response(
                    &cpu,
                    action.is_some_and(|schedule| schedule.switched),
                ))
            }
            9 => Ok(Self::kernel_result(
                self.bios.kernel_mut().poll_semaphore(a0).map(|()| 0),
            )),
            _ => Err(unsupported("thsemap", ordinal)),
        }
    }

    fn dispatch_event<M: ServiceMemory>(
        &mut self,
        ordinal: u16,
        [a0, a1, a2, a3]: [u32; 4],
        context: &mut ServiceContext,
        memory: &mut M,
    ) -> Result<BackendResponse, BackendError> {
        match ordinal {
            4 => {
                let attributes = read_u32(memory, a0)?;
                let bits = read_u32(memory, a0 + 8)?;
                Ok(Self::kernel_result(
                    self.bios
                        .kernel_mut()
                        .create_event_flag(EventFlagSpec { bits, attributes }),
                ))
            }
            5 => Ok(Self::kernel_result(
                self.bios.kernel_mut().delete_event_flag(a0).map(|()| 0),
            )),
            6 | 7 => Ok(Self::kernel_result(
                self.bios.kernel_mut().set_event_flag(a0, a1).map(|_| 0),
            )),
            8 | 9 => Ok(Self::kernel_result(
                self.bios.kernel_mut().clear_event_flag(a0, a1),
            )),
            10 => {
                let mut cpu = bios_context_from_service(context);
                let result = self
                    .bios
                    .kernel_mut()
                    .wait_event_flag(a0, a1, a2, None, &mut cpu)
                    .map_err(backend)?;
                service_context_from_bios(&cpu, context);
                match result {
                    Ok(bits) => {
                        if a3 != 0 {
                            memory.write(a3, &bits.to_le_bytes()).map_err(backend)?;
                        }
                        Ok(BackendResponse::returning(0))
                    }
                    Err(schedule) => Ok(schedule_response(&cpu, schedule.switched)),
                }
            }
            11 => {
                let result = self.bios.kernel_mut().poll_event_flag(a0, a1, a2);
                match result {
                    Ok(bits) => {
                        if a3 != 0 {
                            memory.write(a3, &bits.to_le_bytes()).map_err(backend)?;
                        }
                        Ok(BackendResponse::returning(0))
                    }
                    Err(error) => Ok(Self::kernel_result(Err(error))),
                }
            }
            _ => Err(unsupported("thevent", ordinal)),
        }
    }

    fn dispatch_timer<M: ServiceMemory>(
        &mut self,
        ordinal: u16,
        [a0, a1, a2, a3]: [u32; 4],
        memory: &M,
    ) -> Result<BackendResponse, BackendError> {
        let invalid = || BackendResponse::returning(kernel_code(-157));
        match ordinal {
            4 => Ok(BackendResponse::returning(
                self.timer_manager.allocate(a0, a1, a2),
            )),
            6 => {
                let Some(index) = self.timer_manager.index(a0) else {
                    return Ok(invalid());
                };
                self.timers
                    .write_u32(TIMER_BASES[index] + 4, 0)
                    .map_err(backend)?;
                self.timer_manager.slots[index] = TimerSlot::default();
                Ok(BackendResponse::returning(0))
            }
            20 => {
                let Some(index) = self.timer_manager.index(a0) else {
                    return Ok(invalid());
                };
                if a2 != 0 && memory.range().validate(a2, 4, 4).is_err() {
                    return Ok(BackendResponse::returning(kernel_code(-402)));
                }
                let slot = &mut self.timer_manager.slots[index];
                if slot.control != 0 {
                    return Ok(BackendResponse::returning(kernel_code(-156)));
                }
                slot.compare = a1;
                slot.handler = (a2 != 0).then_some((a2, a3));
                Ok(BackendResponse::returning(0))
            }
            22 => {
                let Some(index) = self.timer_manager.index(a0) else {
                    return Ok(invalid());
                };
                let mode = match a2 {
                    0 | 1 | 3 | 5 | 7 => a2 as u16,
                    _ => return Ok(BackendResponse::returning(kernel_code(-405))),
                };
                let external = match a1 {
                    1 => 0,
                    2 | 4 => 0x0100,
                    _ => return Ok(BackendResponse::returning(kernel_code(-152))),
                };
                let prescale = match a3 {
                    1 => 0,
                    8 if index < 3 => 0x0200,
                    8 => 0x2000,
                    16 => 0x4000,
                    256 => 0x6000,
                    _ => return Ok(BackendResponse::returning(kernel_code(-153))),
                };
                let slot = &mut self.timer_manager.slots[index];
                if slot.control != 0 {
                    return Ok(BackendResponse::returning(kernel_code(-154)));
                }
                slot.control = mode | external | prescale;
                Ok(BackendResponse::returning(0))
            }
            23 => {
                let Some(index) = self.timer_manager.index(a0) else {
                    return Ok(invalid());
                };
                let slot = &mut self.timer_manager.slots[index];
                let mut control = slot.control;
                if slot.handler.is_some() {
                    control |= 0x0058;
                    self.timers
                        .write_u32(TIMER_BASES[index] + 8, slot.compare)
                        .map_err(backend)?;
                    self.irq
                        .set_mask(self.irq.mask() | timer_interrupt(index).bit());
                }
                self.timers
                    .write_u32(TIMER_BASES[index], 0)
                    .map_err(backend)?;
                self.timers
                    .write_u32(TIMER_BASES[index] + 4, u32::from(control))
                    .map_err(backend)?;
                slot.control = control;
                Ok(BackendResponse::returning(0))
            }
            24 => {
                let Some(index) = self.timer_manager.index(a0) else {
                    return Ok(invalid());
                };
                self.timers
                    .write_u32(TIMER_BASES[index] + 4, 0)
                    .map_err(backend)?;
                self.irq
                    .set_mask(self.irq.mask() & !timer_interrupt(index).bit());
                self.timer_manager.slots[index].control = 0;
                Ok(BackendResponse::returning(0))
            }
            _ => Err(unsupported("timrman", ordinal)),
        }
    }

    fn dispatch_handlers<M: ServiceMemory>(
        &mut self,
        request: &BackendRequest,
        context: &mut ServiceContext,
        memory: &mut M,
    ) -> Result<BackendResponse, BackendError> {
        let [a0, a1, a2, a3] = request.arguments;
        if request.family == ServiceFamily::Interrupt {
            return match request.import.ordinal {
                3 | 15 | 16 => Ok(BackendResponse::returning(0)),
                23 => Ok(BackendResponse::returning(u32::from(
                    self.interrupt_context,
                ))),
                6 => {
                    if let Some(source) = interrupt_source(a0) {
                        self.irq.set_mask(self.irq.mask() | source.bit());
                    } else if a0 < 64 {
                        self.enabled_interrupts.insert(a0);
                    } else {
                        return Ok(Self::kernel_result(Err(KernelError::IllegalInterruptCode)));
                    }
                    Ok(BackendResponse::returning(0))
                }
                7 => {
                    let enabled = if let Some(source) = interrupt_source(a0) {
                        let enabled = self.irq.mask() & source.bit() != 0;
                        self.irq.set_mask(self.irq.mask() & !source.bit());
                        enabled
                    } else if a0 < 64 {
                        self.enabled_interrupts.remove(&a0)
                    } else {
                        return Ok(Self::kernel_result(Err(KernelError::IllegalInterruptCode)));
                    };
                    if a1 != 0 {
                        memory
                            .write(a1, &u32::from(enabled).to_le_bytes())
                            .map_err(backend)?;
                    }
                    Ok(BackendResponse::returning(0))
                }
                8 => {
                    let was_enabled = *self.interrupts_enabled;
                    *self.interrupts_enabled = false;
                    Ok(BackendResponse::returning(if was_enabled {
                        0
                    } else {
                        kernel_code(-102)
                    }))
                }
                9 => {
                    *self.interrupts_enabled = true;
                    Ok(BackendResponse::returning(0))
                }
                17 => {
                    let was_enabled = *self.interrupts_enabled;
                    if a0 != 0 {
                        memory
                            .write(a0, &u32::from(was_enabled).to_le_bytes())
                            .map_err(backend)?;
                    }
                    *self.interrupts_enabled = false;
                    Ok(BackendResponse::returning(if was_enabled {
                        0
                    } else {
                        kernel_code(-102)
                    }))
                }
                18 => {
                    *self.interrupts_enabled = a0 != 0;
                    Ok(BackendResponse::returning(0))
                }
                24 => Ok(BackendResponse::returning(
                    context.register(29).unwrap_or(0),
                )),
                _ => {
                    let result = match request.import.ordinal {
                        4 => self.bios.handlers_mut().register_interrupt(
                            a0,
                            a1,
                            a2,
                            a3,
                            guest_range(memory),
                        ),
                        5 => self.bios.handlers_mut().release_interrupt(a0).map(|_| ()),
                        _ => {
                            return Err(unsupported(
                                &request.import.library,
                                request.import.ordinal,
                            ));
                        }
                    };
                    Ok(Self::kernel_result(result.map(|()| 0)))
                }
            };
        }
        let result = match (request.family, request.import.ordinal) {
            (ServiceFamily::Exception, 4) => {
                self.bios
                    .handlers_mut()
                    .register_exception(a0, a1, guest_range(memory))
            }
            (ServiceFamily::Exception, 5) => {
                self.bios.handlers_mut().release_exception(a0).map(|_| ())
            }
            (ServiceFamily::VBlank, 8) => {
                self.bios
                    .handlers_mut()
                    .register_vblank(a0, a1, a2, a3, guest_range(memory))
            }
            (ServiceFamily::VBlank, 9) => {
                self.bios.handlers_mut().release_vblank(a0, a1).map(|_| ())
            }
            _ => {
                return Err(unsupported(&request.import.library, request.import.ordinal));
            }
        };
        Ok(Self::kernel_result(result.map(|()| 0)))
    }

    fn dispatch_module<M: ServiceMemory>(
        &mut self,
        request: BackendRequest,
        context: &mut ServiceContext,
        memory: &mut M,
    ) -> Result<BackendResponse, BackendError> {
        let result_address = request.arguments[3];
        let BackendPayload::Module {
            path,
            bytes,
            arguments,
            start,
        } = request.payload
        else {
            return Err(BackendError::new("module request has no VFS payload"));
        };
        let irx = IrxModule::parse(&path, &bytes).map_err(backend)?;
        let id = {
            let mut guest = BiosMemoryAdapter(memory);
            self.bios.load_module(&irx, &mut guest).map_err(|error| {
                BackendError::new(format!(
                    "cannot load {path} (image {:#x}, alignment {:#x}): {error}",
                    irx.allocation_size(),
                    irx.alignment()
                ))
            })?
        };
        bind_module_imports(self.bios, memory, id).map_err(backend)?;
        if !start {
            return Ok(BackendResponse::returning(id));
        }
        let (argument_address, argument_count) =
            marshal_module_arguments(self.bios, memory, &path, &arguments)?;
        let invocation = {
            let mut guest = BiosMemoryAdapter(memory);
            self.bios
                .modules_mut()
                .begin_start(id, &mut guest)
                .map_err(backend)?
        };
        self.module_frames.push(ModuleFrame {
            module_id: id,
            caller: Some(context.clone()),
            argument_allocation: Some(argument_address),
            result_address: (result_address != 0).then_some(result_address),
        });
        context.pc = invocation.entry;
        context.set_register(A0, argument_count);
        context.set_register(A1, argument_address);
        context.set_register(A2, 0);
        context.set_register(
            A3,
            self.bios
                .modules()
                .get(id)
                .map_or(0, upse_ps2_bios::ModuleRecord::info_address),
        );
        context.set_register(GP, invocation.global_pointer);
        context.set_register(RA, RETURN_ENTRY);
        Ok(BackendResponse {
            v0: id,
            v1: None,
            action: ServiceAction::CallModule,
        })
    }

    fn dispatch_sound<M: ServiceMemory>(
        &mut self,
        ordinal: u16,
        [a0, a1, a2, a3]: [u32; 4],
        context: &ServiceContext,
        memory: &mut M,
    ) -> Result<BackendResponse, BackendError> {
        match ordinal {
            4 => {
                initialize_sound(self.sound).map_err(backend)?;
                Ok(BackendResponse::returning(0))
            }
            5 => {
                write_sound_parameter(self.sound, a0 as u16, a1 as u16).map_err(backend)?;
                Ok(BackendResponse::returning(0))
            }
            6 => Ok(BackendResponse::returning(u32::from(
                read_sound_parameter(self.sound, a0 as u16).map_err(backend)?,
            ))),
            7 => {
                write_sound_switch(self.sound, a0 as u16, a1).map_err(backend)?;
                Ok(BackendResponse::returning(0))
            }
            8 => Ok(BackendResponse::returning(
                read_sound_switch(self.sound, a0 as u16).map_err(backend)?,
            )),
            9 => {
                write_sound_address(self.sound, a0 as u16, a1).map_err(backend)?;
                Ok(BackendResponse::returning(0))
            }
            10 => Ok(BackendResponse::returning(
                read_sound_address(self.sound, a0 as u16).map_err(backend)?,
            )),
            11 => {
                set_core_attribute(self.sound, a0 as u16, a1 as u16).map_err(backend)?;
                Ok(BackendResponse::returning(0))
            }
            12 => Ok(BackendResponse::returning(u32::from(
                get_core_attribute(self.sound, a0 as u16).map_err(backend)?,
            ))),
            13 => Ok(BackendResponse::returning(note_to_pitch(a0, a1, a2, a3))),
            14 => Ok(BackendResponse::returning(pitch_to_note(a0, a1, a2))),
            17 => self.voice_transfer(a0, a1, a2, a3, context, memory),
            18 => self.block_transfer(a0, a1, a2, a3, memory),
            23 | 25 | 31..=33 => {
                apply_effect_attribute(self.sound, a0, a1, memory)?;
                Ok(BackendResponse::returning(0))
            }
            3 | 19..=22 | 24 | 26..=30 => Ok(BackendResponse::returning(0)),
            _ => Err(unsupported("libsd", ordinal)),
        }
    }

    fn voice_transfer<M: ServiceMemory>(
        &mut self,
        channel: u32,
        mode: u32,
        iop_address: u32,
        spu_address: u32,
        context: &ServiceContext,
        memory: &mut M,
    ) -> Result<BackendResponse, BackendError> {
        if mode & 3 == 2 {
            return Ok(BackendResponse::returning(0));
        }
        let size = read_u32(memory, context.register(29).unwrap_or(0).wrapping_add(16))?;
        transfer_sound(
            self.sound,
            channel,
            mode,
            iop_address,
            spu_address,
            size,
            memory,
        )?;
        Ok(BackendResponse::returning(size))
    }

    fn block_transfer<M: ServiceMemory>(
        &mut self,
        channel: u32,
        mode: u32,
        iop_address: u32,
        size: u32,
        memory: &mut M,
    ) -> Result<BackendResponse, BackendError> {
        if mode & 3 == 2 {
            return Ok(BackendResponse::returning(0));
        }
        transfer_sound(self.sound, channel, mode, iop_address, 0, size, memory)?;
        Ok(BackendResponse::returning(0))
    }
}

impl BiosServices for MachineServices<'_> {
    fn dispatch<M: ServiceMemory>(
        &mut self,
        request: BackendRequest,
        context: &mut ServiceContext,
        memory: &mut M,
    ) -> Result<BackendResponse, BackendError> {
        let ordinal = request.import.ordinal;
        let library = request.import.library.clone();
        let module_id = request.import.module_id;
        let pc = request.import.pc;
        let arguments = request.arguments;
        let result = match request.family {
            ServiceFamily::SystemMemory => self.dispatch_system_memory(ordinal, arguments),
            ServiceFamily::Thread => self.dispatch_thread(ordinal, arguments, context, memory),
            ServiceFamily::Semaphore => {
                self.dispatch_semaphore(ordinal, arguments, context, memory)
            }
            ServiceFamily::EventFlag => self.dispatch_event(ordinal, arguments, context, memory),
            ServiceFamily::Timer => self.dispatch_timer(ordinal, arguments, memory),
            ServiceFamily::Exception | ServiceFamily::Interrupt | ServiceFamily::VBlank => {
                self.dispatch_handlers(&request, context, memory)
            }
            ServiceFamily::ModuleLoader if matches!(ordinal, 6 | 7) => {
                self.dispatch_module(request, context, memory)
            }
            ServiceFamily::Sound => self.dispatch_sound(ordinal, arguments, context, memory),
            ServiceFamily::LoadCore if matches!(ordinal, 3..=17 | 20..=27) => {
                Ok(BackendResponse::returning(0))
            }
            ServiceFamily::Dma if matches!(ordinal, 3..=35) => Ok(BackendResponse::returning(0)),
            _ => Err(unsupported(&request.import.library, ordinal)),
        };
        result.map_err(|error| {
            BackendError::new(format!(
                "{library} ordinal {ordinal} for module {module_id} at PC {pc:#010x}: {error}"
            ))
        })
    }
}

fn initialize_sound(sound: &mut Spu2) -> Result<(), upse_ps2_spu2::Spu2Error> {
    for core in 0..2 {
        let base = core_base(core);
        sound.write_register(base + 0x19a, CORE_ATTR_ENABLE | CORE_ATTR_UNMUTE)?;
        sound.write_register(base + 0x188, 0xffff)?;
        sound.write_register(base + 0x18a, 0x00ff)?;
        sound.write_register(base + 0x190, 0xffff)?;
        sound.write_register(base + 0x192, 0x00ff)?;
        sound.write_register(
            base + 0x198,
            if core == 0 {
                MMIX_VOICE_DRY_LEFT | MMIX_VOICE_DRY_RIGHT
            } else {
                MMIX_INPUT_A_DRY_LEFT
                    | MMIX_INPUT_A_DRY_RIGHT
                    | MMIX_INPUT_B_DRY_LEFT
                    | MMIX_INPUT_B_DRY_RIGHT
                    | MMIX_VOICE_DRY_LEFT
                    | MMIX_VOICE_DRY_RIGHT
            },
        )?;
        let primary = primary_base(core);
        sound.write_register(primary, if core == 0 { 0 } else { 0x3fff })?;
        sound.write_register(primary + 2, if core == 0 { 0 } else { 0x3fff })?;
        sound.write_register(primary + 8, if core == 0 { 0 } else { 0x7fff })?;
        sound.write_register(primary + 0x0a, if core == 0 { 0 } else { 0x7fff })?;
    }
    Ok(())
}

fn parameter_address(entry: u16) -> Result<u32, BackendError> {
    let core = usize::from(entry & 1);
    let kind = u32::from(entry >> 8);
    let voice = u32::from((entry & 0x3e) >> 1);
    match kind {
        0..=7 if voice < 24 => Ok(core_base(core) + voice * VOICE_STRIDE + kind * 2),
        8 => Ok(core_base(core) + 0x198),
        9..=18 => Ok(primary_base(core) + (kind - 9) * 2),
        _ => Err(BackendError::new(format!(
            "invalid libsd parameter selector {entry:#06x}"
        ))),
    }
}

fn switch_address(entry: u16) -> Result<u32, BackendError> {
    let core = usize::from(entry & 1);
    let local = match entry >> 8 {
        0x13 => 0x180,
        0x14 => 0x184,
        0x15 => 0x1a0,
        0x16 => 0x1a4,
        0x17 => 0x340,
        0x18 => 0x188,
        0x19 => 0x18c,
        0x1a => 0x190,
        0x1b => 0x194,
        _ => {
            return Err(BackendError::new(format!(
                "invalid libsd switch selector {entry:#06x}"
            )));
        }
    };
    Ok(core_base(core) + local)
}

fn address_register(entry: u16) -> Result<(u32, bool), BackendError> {
    let core = usize::from(entry & 1);
    let voice = u32::from((entry & 0x3e) >> 1);
    let result = match entry >> 8 {
        0x1c => (core_base(core) + 0x2e0, false),
        0x1d => (core_base(core) + 0x33c, true),
        0x1e => (core_base(core) + 0x1a8, false),
        0x1f => (core_base(core) + 0x19c, false),
        0x20 if voice < 24 => (
            core_base(core) + VOICE_ADDRESS_BASE + voice * VOICE_ADDRESS_STRIDE,
            false,
        ),
        0x21 if voice < 24 => (
            core_base(core) + VOICE_ADDRESS_BASE + voice * VOICE_ADDRESS_STRIDE + 4,
            false,
        ),
        0x22 if voice < 24 => (
            core_base(core) + VOICE_ADDRESS_BASE + voice * VOICE_ADDRESS_STRIDE + 8,
            false,
        ),
        _ => {
            return Err(BackendError::new(format!(
                "invalid libsd address selector {entry:#06x}"
            )));
        }
    };
    Ok(result)
}

fn write_sound_parameter(
    sound: &mut Spu2,
    entry: u16,
    value: u16,
) -> Result<(), upse_ps2_spu2::Spu2Error> {
    let address =
        parameter_address(entry).map_err(|_| upse_ps2_spu2::Spu2Error::InvalidRegister {
            address: SPU2_BASE + 0x800,
        })?;
    sound.write_register(address, value)
}

fn read_sound_parameter(sound: &Spu2, entry: u16) -> Result<u16, upse_ps2_spu2::Spu2Error> {
    let address =
        parameter_address(entry).map_err(|_| upse_ps2_spu2::Spu2Error::InvalidRegister {
            address: SPU2_BASE + 0x800,
        })?;
    sound.read_register(address)
}

fn write_sound_switch(
    sound: &mut Spu2,
    entry: u16,
    value: u32,
) -> Result<(), upse_ps2_spu2::Spu2Error> {
    let address = switch_address(entry).map_err(|_| upse_ps2_spu2::Spu2Error::InvalidRegister {
        address: SPU2_BASE + 0x800,
    })?;
    sound.write_register(address, value as u16)?;
    sound.write_register(address + 2, (value >> 16) as u16)
}

fn read_sound_switch(sound: &Spu2, entry: u16) -> Result<u32, upse_ps2_spu2::Spu2Error> {
    let address = switch_address(entry).map_err(|_| upse_ps2_spu2::Spu2Error::InvalidRegister {
        address: SPU2_BASE + 0x800,
    })?;
    Ok(u32::from(sound.read_register(address)?)
        | (u32::from(sound.read_register(address + 2)?) << 16))
}

fn write_sound_address(
    sound: &mut Spu2,
    entry: u16,
    value: u32,
) -> Result<(), upse_ps2_spu2::Spu2Error> {
    let (address, high_only) =
        address_register(entry).map_err(|_| upse_ps2_spu2::Spu2Error::InvalidRegister {
            address: SPU2_BASE + 0x800,
        })?;
    sound.write_register(address, (value >> 17) as u16)?;
    if !high_only {
        sound.write_register(address + 2, ((value >> 1) & 0xfff8) as u16)?;
    }
    Ok(())
}

fn read_sound_address(sound: &Spu2, entry: u16) -> Result<u32, upse_ps2_spu2::Spu2Error> {
    let (address, high_only) =
        address_register(entry).map_err(|_| upse_ps2_spu2::Spu2Error::InvalidRegister {
            address: SPU2_BASE + 0x800,
        })?;
    let high = u32::from(sound.read_register(address)?) << 17;
    let low = if high_only {
        0x1ffff
    } else {
        u32::from(sound.read_register(address + 2)?) << 1
    };
    Ok((high | low) & 0x1f_ffff)
}

fn set_core_attribute(
    sound: &mut Spu2,
    entry: u16,
    value: u16,
) -> Result<(), upse_ps2_spu2::Spu2Error> {
    let core = usize::from(entry & 1);
    let address = core_base(core) + 0x19a;
    let mut attributes = sound.read_register(address)?;
    match entry & !1 {
        2 => set_attribute_bit(&mut attributes, CORE_ATTR_EFFECT_ENABLE, value),
        4 => set_attribute_bit(&mut attributes, CORE_ATTR_IRQ_ENABLE, value),
        6 => set_attribute_bit(&mut attributes, CORE_ATTR_UNMUTE, value),
        8 => attributes = (attributes & !(0x3f << 8)) | ((value & 0x3f) << 8),
        _ => return Ok(()),
    }
    sound.write_register(address, attributes | CORE_ATTR_ENABLE)
}

fn get_core_attribute(sound: &Spu2, entry: u16) -> Result<u16, upse_ps2_spu2::Spu2Error> {
    let attributes = sound.read_register(core_base(usize::from(entry & 1)) + 0x19a)?;
    Ok(match entry & !1 {
        2 => u16::from(attributes & CORE_ATTR_EFFECT_ENABLE != 0),
        4 => u16::from(attributes & CORE_ATTR_IRQ_ENABLE != 0),
        6 => u16::from(attributes & CORE_ATTR_UNMUTE != 0),
        8 => (attributes >> 8) & 0x3f,
        _ => 0,
    })
}

fn set_attribute_bit(attributes: &mut u16, bit: u16, value: u16) {
    if value & 1 == 0 {
        *attributes &= !bit;
    } else {
        *attributes |= bit;
    }
}

fn apply_effect_attribute<M: ServiceMemory>(
    sound: &mut Spu2,
    core: u32,
    address: u32,
    memory: &M,
) -> Result<(), BackendError> {
    if address == 0 {
        return Ok(());
    }
    let core = usize::try_from(core & 1).unwrap_or(0);
    let depth_left = read_u16(memory, address + 8)?;
    let depth_right = read_u16(memory, address + 10)?;
    sound
        .write_register(primary_base(core) + 4, depth_left)
        .map_err(backend)?;
    sound
        .write_register(primary_base(core) + 6, depth_right)
        .map_err(backend)?;
    let attr_address = core_base(core) + 0x19a;
    let attributes = sound.read_register(attr_address).map_err(backend)?;
    sound
        .write_register(attr_address, attributes | CORE_ATTR_EFFECT_ENABLE)
        .map_err(backend)
}

fn transfer_sound<M: ServiceMemory>(
    sound: &mut Spu2,
    _channel: u32,
    mode: u32,
    iop_address: u32,
    spu_address: u32,
    size: u32,
    memory: &mut M,
) -> Result<(), BackendError> {
    let size = usize::try_from(size).map_err(|_| BackendError::new("sound transfer size width"))?;
    if mode.trailing_zeros() >= 2 {
        let mut bytes = vec![0; size];
        memory.read(iop_address, &mut bytes).map_err(backend)?;
        sound
            .load_ram(usize::try_from(spu_address).unwrap_or(usize::MAX), &bytes)
            .map_err(backend)
    } else {
        let start = usize::try_from(spu_address).unwrap_or(usize::MAX);
        let bytes = sound
            .ram()
            .get(start..start.saturating_add(size))
            .ok_or_else(|| BackendError::new("sound transfer leaves SPU2 RAM"))?;
        memory.write(iop_address, bytes).map_err(backend)
    }
}

fn note_to_pitch(center_note: u32, center_fine: u32, note: u32, fine: u32) -> u32 {
    let center = i64::from(center_note) * 128 + i64::from(center_fine);
    let target = i64::from(note) * 128 + i64::from(fine as i16);
    let semitones = (target - center) as f64 / (12.0 * 128.0);
    (4096.0 * 2_f64.powf(semitones)).clamp(0.0, 16383.0) as u32
}

fn pitch_to_note(center_note: u32, center_fine: u32, pitch: u32) -> u32 {
    if pitch == 0 {
        return 0;
    }
    let delta = (f64::from(pitch) / 4096.0).log2() * 12.0 * 128.0;
    let packed = (f64::from(center_note * 128 + center_fine) + delta).round() as i64;
    u32::try_from(packed.max(0)).unwrap_or(u32::MAX)
}

const fn core_base(core: usize) -> u32 {
    SPU2_BASE + core as u32 * CORE_STRIDE
}

const fn primary_base(core: usize) -> u32 {
    SPU2_BASE + PRIMARY_BASE + core as u32 * PRIMARY_STRIDE
}

fn schedule_response(cpu: &upse_ps2_bios::CpuContext, switched: bool) -> BackendResponse {
    BackendResponse {
        v0: cpu.register(V0).unwrap_or(0),
        v1: cpu.register(V1),
        action: if switched {
            ServiceAction::ContextSwitch
        } else {
            ServiceAction::Return
        },
    }
}

fn guest_range<M: ServiceMemory>(memory: &M) -> GuestRange {
    let range = memory.range();
    GuestRange {
        start: range.start,
        end: range.end,
    }
}

fn marshal_module_arguments<M: ServiceMemory>(
    bios: &mut BiosHle,
    memory: &mut M,
    path: &str,
    arguments: &[u8],
) -> Result<(u32, u32), BackendError> {
    let mut argument_offsets = Vec::new();
    let mut cursor = 0;
    while cursor < arguments.len() {
        argument_offsets.push(cursor);
        let terminator = arguments[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| BackendError::new("module argument is not NUL-terminated"))?;
        cursor = cursor
            .checked_add(terminator + 1)
            .ok_or_else(|| BackendError::new("module argument block overflows host width"))?;
    }
    let argc = argument_offsets
        .len()
        .checked_add(1)
        .ok_or_else(|| BackendError::new("module argument count overflows host width"))?;
    let pointer_bytes = argc
        .checked_add(1)
        .and_then(|count| count.checked_mul(4))
        .ok_or_else(|| BackendError::new("module argv table overflows host width"))?;
    let launch_name = format!("/{path}");
    let string_bytes = launch_name
        .len()
        .checked_add(1)
        .and_then(|size| size.checked_add(arguments.len()))
        .ok_or_else(|| BackendError::new("module argv strings overflow host width"))?;
    let size = pointer_bytes
        .checked_add(string_bytes)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| BackendError::new("module argv block exceeds IOP width"))?;
    let allocation = bios
        .memory_mut()
        .allocate(AllocationMode::First, size, 0)
        .map_err(backend)?;
    let mut block = vec![0_u8; size as usize];
    let strings_address = allocation.address + u32::try_from(pointer_bytes).unwrap_or(0);
    block[..4].copy_from_slice(&strings_address.to_le_bytes());
    let mut string_cursor = pointer_bytes;
    block[string_cursor..string_cursor + launch_name.len()].copy_from_slice(launch_name.as_bytes());
    string_cursor += launch_name.len() + 1;
    let argument_base = string_cursor;
    block[argument_base..argument_base + arguments.len()].copy_from_slice(arguments);
    for (index, offset) in argument_offsets.into_iter().enumerate() {
        let address = allocation.address
            + u32::try_from(argument_base + offset)
                .map_err(|_| BackendError::new("module argv address exceeds IOP width"))?;
        let pointer = (index + 1) * 4;
        block[pointer..pointer + 4].copy_from_slice(&address.to_le_bytes());
    }
    if let Err(error) = memory.write(allocation.address, &block) {
        let _ = bios.memory_mut().free(allocation.address);
        return Err(backend(error));
    }
    Ok((allocation.address, u32::try_from(argc).unwrap_or(u32::MAX)))
}

fn interrupt_source(value: u32) -> Option<InterruptSource> {
    u8::try_from(value)
        .ok()
        .and_then(InterruptSource::from_index)
}

const fn timer_handle(index: usize) -> u32 {
    ((index as u32 + 1) << 28) | (TIMER_BASES[index] >> 4)
}

const fn timer_interrupt(index: usize) -> InterruptSource {
    match index {
        0 => InterruptSource::Timer0,
        1 => InterruptSource::Timer1,
        2 => InterruptSource::Timer2,
        3 => InterruptSource::Timer3,
        4 => InterruptSource::Timer4,
        _ => InterruptSource::Timer5,
    }
}

const fn kernel_code(value: i32) -> u32 {
    u32::from_ne_bytes(value.to_ne_bytes())
}

fn read_u16<M: ServiceMemory>(memory: &M, address: u32) -> Result<u16, BackendError> {
    let mut bytes = [0; 2];
    memory.read(address, &mut bytes).map_err(backend)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32<M: ServiceMemory>(memory: &M, address: u32) -> Result<u32, BackendError> {
    let mut bytes = [0; 4];
    memory.read(address, &mut bytes).map_err(backend)?;
    Ok(u32::from_le_bytes(bytes))
}

fn unsupported(library: &str, ordinal: u16) -> BackendError {
    BackendError::new(format!(
        "machine adapter does not implement {library} ordinal {ordinal}"
    ))
}

#[allow(clippy::needless_pass_by_value)]
fn backend(error: impl ToString) -> BackendError {
    BackendError::new(error.to_string())
}
