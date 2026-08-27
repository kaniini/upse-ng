// SPDX-License-Identifier: LGPL-2.1-or-later
//! Complete firmware-free PSF2 playback machine.
//!
//! PSF2 execution is intentionally limited to the PS2 IOP. This composition
//! has no Emotion Engine state and exposes no firmware-image input path.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

mod services;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use thiserror::Error;
use upse_clock::{ClockError, Deadline, RateConverter, Ticks};
use upse_iop_dma::{DmaEvent, SoundDmaChannel};
use upse_iop_machine::{
    HardwareEventKind, IopMachine, MachineConfig as IopMachineConfig,
    MachineError as IopMachineError,
};
use upse_iop_memory::{IopMemory, MemoryError, OpenBusPolicy, RAM_SIZE};
use upse_iop_services::{
    GuestAddressRange, ImportRequest, IopServices, ServiceAction, ServiceContext, ServiceError,
    ServiceMemory, ServiceMemoryError, describe_import,
};
use upse_iop_timers::{CounterBoundary, TimerId, TimingEvent};
use upse_irx::{IrxError, IrxModule, ResidentState};
use upse_ps2_bios::{
    AllocationMode, BiosError, BiosHle, CpuContext, DispatchCall, GuestMemory, GuestMemoryError,
    GuestRange, KernelError, KernelEvent, RETURN_ENTRY, RescheduleReason, THREAD_RETURN_ENTRY,
    ThreadSpec,
};
use upse_ps2_spu2::{SAMPLE_RATE, Spu2, Spu2Error};
use upse_psf::Psf2LoadPlan;
use upse_psf2_vfs::{Psf2Vfs, VfsError, VfsLimits};
use upse_r3000::{Cpu, Exception, StepEvent};

use services::{MachineServices, TimerManager};

const IOP_CLOCK_HZ: u64 = 36_864_000;
const AUDIO_CHUNK_FRAMES: usize = 256;
const IDLE_ADVANCE_CYCLES: u64 = 768;
const ROOT_STACK_SIZE: u32 = 64 * 1024;
const ARGUMENT_BLOCK_SIZE: u32 = 256;
const MODULE_START_PRIORITY: u32 = 8;
const RA: usize = 31;
const V0: usize = 2;
const A0: usize = 4;
const A1: usize = 5;
const GP: usize = 28;

/// Complete PSF2 construction policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MachineConfig {
    /// Handling of otherwise unmapped IOP addresses.
    pub open_bus: OpenBusPolicy,
    /// Bounds applied while constructing the overlaid PSF2 filesystem.
    pub vfs_limits: VfsLimits,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            open_bus: OpenBusPolicy::Strict,
            vfs_limits: VfsLimits::default(),
        }
    }
}

/// Kind of work performed by one composed-machine boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MachineStepKind {
    /// One guest instruction or architectural exception.
    Cpu(StepEvent),
    /// One named IOP import was handled by BIOS HLE.
    Import,
    /// A module entry returned through the guarded HLE sentinel.
    ModuleReturn,
    /// A kernel or interrupt callback returned through the HLE sentinel.
    CallbackReturn,
    /// A guest thread returned from its entry function and became dormant.
    ThreadReturn,
    /// Devices advanced while no guest thread was runnable.
    Idle,
}

/// Observable result of one CPU/HLE boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MachineStep {
    /// IOP cycles consumed at this boundary.
    pub cycles: u64,
    /// Execution path selected by the composition.
    pub kind: MachineStepKind,
}

/// End-to-end PSF2 machine failure.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum MachineError {
    /// The overlaid PSF2 filesystem is malformed.
    #[error("PSF2 virtual filesystem failure: {0}")]
    Vfs(#[from] VfsError),
    /// The required root module is absent.
    #[error("PSF2 virtual filesystem does not contain psf2.irx")]
    MissingRootModule,
    /// An IOP module is malformed.
    #[error("PSF2 IRX failure: {0}")]
    Irx(#[from] IrxError),
    /// BIOS construction or module lifecycle failed.
    #[error("PS2 BIOS HLE failure: {0}")]
    Bios(#[from] BiosError),
    /// A BIOS-compatible kernel operation failed during composition.
    #[error("PS2 BIOS kernel failure: {0}")]
    Kernel(#[from] KernelError),
    /// Named IOP import dispatch failed.
    #[error("PS2 IOP service failure: {0}")]
    Service(#[from] ServiceError),
    /// Bare IOP CPU or device execution failed.
    #[error("PS2 IOP machine failure: {0}")]
    Hardware(#[from] IopMachineError),
    /// Direct IOP RAM setup failed.
    #[error("PS2 IOP memory failure: {0}")]
    Memory(#[from] MemoryError),
    /// SPU2 rendering failed.
    #[error("PS2 SPU2 failure: {0}")]
    Spu2(#[from] Spu2Error),
    /// An HLE service could not access IOP RAM.
    #[error("PS2 HLE guest memory failure: {0}")]
    HleMemory(#[from] ServiceMemoryError),
    /// Emulated clock conversion overflowed.
    #[error("PS2 machine clock overflow")]
    ClockOverflow,
    /// A guest exception reached no HLE boundary.
    #[error("unhandled IOP exception {exception:?} at PC {pc:#010x}")]
    UnhandledException {
        /// Exception raised by the R3000.
        exception: Exception,
        /// Faulting instruction address.
        pc: u32,
    },
    /// A guarded return was encountered without matching host state.
    #[error("unexpected PS2 HLE return at PC {pc:#010x}")]
    UnexpectedReturn {
        /// Return-sentinel address.
        pc: u32,
    },
    /// An import table could not be connected to HLE or a resident IRX.
    #[error(
        "unresolved IOP import {library} version {version:#06x} ordinal {ordinal:#06x} in module {module_id}"
    )]
    UnresolvedImport {
        /// Imported library.
        library: String,
        /// Required library version.
        version: u16,
        /// Imported ordinal.
        ordinal: u16,
        /// Importing module.
        module_id: u32,
    },
    /// An audio output slice does not match the requested frame count.
    #[error("PSF2 output has {actual} samples, expected {expected}")]
    OutputSize {
        /// Required scalar samples.
        expected: usize,
        /// Supplied scalar samples.
        actual: usize,
    },
    /// The guest returned an undefined IRX residency code.
    #[error("module {module_id} returned invalid resident code {value:#010x}")]
    ResidentCode {
        /// Returning module.
        module_id: u32,
        /// Raw entry result.
        value: u32,
    },
}

impl From<ClockError> for MachineError {
    fn from(_: ClockError) -> Self {
        Self::ClockOverflow
    }
}

#[derive(Clone, Debug)]
struct ModuleFrame {
    module_id: u32,
    thread_id: u32,
    requester: Option<u32>,
    argument_allocation: Option<u32>,
    result_address: Option<u32>,
}

#[derive(Clone, Debug)]
struct MachineState {
    hardware: Box<IopMachine<Spu2>>,
    bios: Box<BiosHle>,
    services: IopServices<Psf2Vfs>,
    sample_clock: RateConverter,
    pending_audio: VecDeque<i16>,
    module_frames: Vec<ModuleFrame>,
    thread_stacks: BTreeMap<u32, u32>,
    callbacks: VecDeque<upse_ps2_bios::CallbackRequest>,
    interrupt_callbacks: VecDeque<upse_ps2_bios::CallbackRequest>,
    timer_callbacks: VecDeque<(TimerId, upse_ps2_bios::CallbackRequest)>,
    timer_manager: TimerManager,
    dma_events: u64,
    callback_active: Option<ActiveCallback>,
    interrupts_enabled: bool,
    enabled_interrupts: BTreeSet<u32>,
    halted: bool,
}

fn new_hardware(open_bus: OpenBusPolicy) -> Box<IopMachine<Spu2>> {
    Box::new(IopMachine::new(
        Spu2::new(),
        IopMachineConfig {
            open_bus,
            ..IopMachineConfig::default()
        },
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveCallback {
    Kernel,
    Interrupt,
    Timer(TimerId),
}

/// Fully composed IOP-only PSF2 machine with a post-load reset snapshot.
#[derive(Clone, Debug)]
pub struct Ps2Machine {
    state: Box<MachineState>,
    reset: Box<MachineState>,
}

impl Ps2Machine {
    /// Builds the overlaid VFS, relocates `psf2.irx`, binds imports, and enters
    /// its module start function with a synthetic `argv[0]`.
    ///
    /// # Errors
    ///
    /// Returns a structured filesystem, IRX, BIOS, memory, or clock failure.
    pub fn from_plan(plan: &Psf2LoadPlan, config: MachineConfig) -> Result<Self, MachineError> {
        let vfs = Psf2Vfs::from_load_plan(plan, config.vfs_limits)?;
        let root_bytes = vfs
            .file("psf2.irx")
            .map_err(|_| MachineError::MissingRootModule)?;
        let irx = IrxModule::parse("/psf2.irx", root_bytes)?;
        let mut hardware = new_hardware(config.open_bus);
        let mut bios = Box::new(BiosHle::new()?);
        {
            let mut guest = IopRam(hardware.memory_mut());
            bios.reset(&mut guest)?;
        }
        let root_id = {
            let mut guest = IopRam(hardware.memory_mut());
            bios.load_module(&irx, &mut guest)?
        };
        {
            let mut memory = IopRam(hardware.memory_mut());
            bind_module_imports(&mut bios, &mut memory, root_id)?;
        }
        let invocation = {
            let mut guest = IopRam(hardware.memory_mut());
            bios.modules_mut().begin_start(root_id, &mut guest)?
        };
        let stack = bios
            .memory_mut()
            .allocate(AllocationMode::First, ROOT_STACK_SIZE, 0)?;
        let arguments =
            bios.memory_mut()
                .allocate(AllocationMode::First, ARGUMENT_BLOCK_SIZE, 0)?;
        let string_address = arguments.address + 4;
        hardware
            .memory_mut()
            .write_u32(arguments.address, string_address)?;
        hardware
            .memory_mut()
            .load_ram(string_address, b"/psf2.irx\0")?;
        let loader_thread = bios.kernel_mut().create_thread(
            ThreadSpec {
                entry: invocation.entry,
                stack: stack.address,
                stack_size: stack.requested_size,
                priority: 1,
                global_pointer: invocation.global_pointer,
                attributes: 0,
                option: 0,
            },
            GuestRange {
                start: 0,
                end: RAM_SIZE as u32,
            },
        )?;
        bios.kernel_mut().start_thread(loader_thread, 1)?;
        let mut context = CpuContext::reset(0, 0);
        bios.kernel_mut()
            .reschedule(&mut context, RescheduleReason::HleReturn)?;
        context.set_register(A0, 1);
        context.set_register(A1, arguments.address);
        context.set_register(GP, invocation.global_pointer);
        context.set_register(RA, RETURN_ENTRY);
        apply_bios_context(hardware.cpu_mut(), &context);

        let state = Box::new(MachineState {
            hardware,
            bios,
            services: IopServices::new(vfs),
            sample_clock: RateConverter::new(IOP_CLOCK_HZ, u64::from(SAMPLE_RATE))?,
            pending_audio: VecDeque::new(),
            module_frames: vec![ModuleFrame {
                module_id: root_id,
                thread_id: loader_thread,
                requester: None,
                argument_allocation: None,
                result_address: None,
            }],
            thread_stacks: BTreeMap::from([(loader_thread, stack.address)]),
            callbacks: VecDeque::new(),
            interrupt_callbacks: VecDeque::new(),
            timer_callbacks: VecDeque::new(),
            timer_manager: TimerManager::default(),
            dma_events: 0,
            callback_active: None,
            interrupts_enabled: true,
            enabled_interrupts: BTreeSet::new(),
            halted: false,
        });
        Ok(Self {
            reset: state.clone(),
            state,
        })
    }

    /// Restores the complete post-load snapshot without reparsing any module.
    pub fn reset(&mut self) {
        self.state = self.reset.clone();
    }

    /// Returns current emulated IOP time.
    #[must_use]
    pub const fn now(&self) -> Deadline {
        self.state.hardware.now()
    }

    /// Returns the current guest program counter for diagnostics.
    #[must_use]
    pub fn pc(&self) -> u32 {
        self.state.hardware.cpu().pc()
    }

    /// Returns the IOP CPU for debugger-style machine diagnostics.
    #[must_use]
    pub const fn cpu(&self) -> &Cpu {
        self.state.hardware.cpu()
    }

    /// Returns IOP memory for debugger-style machine diagnostics.
    #[must_use]
    pub const fn memory(&self) -> &IopMemory {
        self.state.hardware.memory()
    }

    /// Returns the loaded module containing the current PC, when any.
    #[must_use]
    pub fn current_module(&self) -> Option<&upse_ps2_bios::ModuleRecord> {
        self.state.bios.modules().containing(self.pc())
    }

    /// Returns one loaded module by its BIOS identifier.
    #[must_use]
    pub fn module(&self, id: u32) -> Option<&upse_ps2_bios::ModuleRecord> {
        self.state.bios.modules().get(id)
    }

    /// Removes BIOS/stdio text emitted since the preceding call.
    pub fn take_tty(&mut self) -> Vec<String> {
        self.state.services.take_tty()
    }

    /// Returns the number of observed sound-DMA lifecycle events.
    #[must_use]
    pub const fn dma_events(&self) -> u64 {
        self.state.dma_events
    }

    /// Executes one guest, import, return, or idle boundary.
    ///
    /// # Errors
    ///
    /// Returns a structured machine, BIOS, service, or sound failure.
    pub fn step(&mut self) -> Result<MachineStep, MachineError> {
        if self.state.hardware.cpu().pc() == RETURN_ENTRY {
            return self.handle_return();
        }
        if self.state.hardware.cpu().pc() == THREAD_RETURN_ENTRY {
            return self.handle_thread_return();
        }
        self.enter_pending_callback()?;
        if self.state.callback_active.is_none()
            && self.state.bios.kernel().current_thread().is_none()
        {
            let mut context = bios_context(self.state.hardware.cpu());
            let schedule = self
                .state
                .bios
                .kernel_mut()
                .reschedule(&mut context, RescheduleReason::HleReturn)?;
            if schedule.current.is_some() {
                apply_bios_context(self.state.hardware.cpu_mut(), &context);
                self.state.halted = false;
            } else {
                self.advance_idle(IDLE_ADVANCE_CYCLES)?;
                return Ok(MachineStep {
                    cycles: IDLE_ADVANCE_CYCLES,
                    kind: MachineStepKind::Idle,
                });
            }
        }
        if self.state.halted || self.in_idle_loop()? {
            self.advance_idle(IDLE_ADVANCE_CYCLES)?;
            return Ok(MachineStep {
                cycles: IDLE_ADVANCE_CYCLES,
                kind: MachineStepKind::Idle,
            });
        }
        let before = self.state.hardware.cpu().clone();
        let result = self.state.hardware.step_without_external_interrupts()?;
        let cycles = u64::from(result.cpu.cycles);
        self.after_device_advance(cycles)?;
        match result.cpu.event {
            StepEvent::Instruction => Ok(MachineStep {
                cycles,
                kind: MachineStepKind::Cpu(StepEvent::Instruction),
            }),
            StepEvent::Exception(Exception::Break) => {
                let entry = result.cpu.pc;
                let service_cycles = self.dispatch_import(&before, entry)?;
                Ok(MachineStep {
                    cycles: cycles
                        .checked_add(service_cycles)
                        .ok_or(MachineError::ClockOverflow)?,
                    kind: MachineStepKind::Import,
                })
            }
            StepEvent::Exception(exception) => Err(MachineError::UnhandledException {
                exception,
                pc: result.cpu.pc,
            }),
        }
    }

    /// Runs until exactly `frames` interleaved signed-integer frames are ready.
    ///
    /// # Errors
    ///
    /// Returns [`MachineError::OutputSize`] for a mismatched output slice or
    /// propagates execution, HLE, and SPU2 failures.
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

    fn dispatch_import(&mut self, cpu: &Cpu, entry: u32) -> Result<u64, MachineError> {
        let caller_pc = cpu.register(RA).unwrap_or(0).wrapping_sub(8);
        let module_id = self
            .state
            .bios
            .modules()
            .containing(caller_pc)
            .map_or(0, upse_ps2_bios::ModuleRecord::id);
        let call = self
            .state
            .bios
            .dispatch()
            .resolve(entry, 0, module_id, caller_pc)?;
        let DispatchCall::Import(call) = call else {
            return Err(call.unknown().into());
        };
        let import = ImportRequest {
            library: call.library,
            version: call.version,
            ordinal: call.ordinal,
            module_id: call.module_id,
            pc: call.pc,
        };
        let mut context = service_context(cpu);
        context.pc = context.register(RA).unwrap_or(0);
        let interrupt_context =
            self.state.callback_active.is_some() || !self.state.interrupts_enabled;
        let outcome = {
            let MachineState {
                hardware,
                bios,
                services,
                module_frames,
                thread_stacks,
                interrupts_enabled,
                enabled_interrupts,
                timer_manager,
                ..
            } = &mut *self.state;
            let module_entry_active = !module_frames.is_empty();
            let (memory, sound, irq, timers) = hardware.memory_sound_interrupts_and_timers_mut();
            let mut memory = IopRam(memory);
            let mut backend = MachineServices {
                bios: bios.as_mut(),
                sound,
                irq,
                timers,
                timer_manager,
                interrupts_enabled,
                enabled_interrupts,
                thread_stacks,
                module_frames,
                module_entry_active,
                interrupt_context,
            };
            services.dispatch(import, &mut context, &mut memory, &mut backend)?
        };
        if outcome.action == ServiceAction::Return {
            context.pc = context.register(RA).unwrap_or(0);
        }
        apply_service_context(self.state.hardware.cpu_mut(), &context);
        if outcome.action == ServiceAction::Halt {
            self.state.halted = true;
        }
        Ok(0)
    }

    fn handle_return(&mut self) -> Result<MachineStep, MachineError> {
        if let Some(active) = self.state.callback_active {
            let mut context = bios_context(self.state.hardware.cpu());
            match active {
                ActiveCallback::Kernel => {
                    self.state
                        .bios
                        .kernel_mut()
                        .return_from_callback(&mut context)?;
                }
                ActiveCallback::Interrupt => {
                    let memory = IopRam(self.state.hardware.memory_mut());
                    self.state
                        .bios
                        .dispatch_mut()
                        .return_from_exception(&mut context, &memory)?;
                }
                ActiveCallback::Timer(timer) => {
                    let result = context.register(V0).unwrap_or(0);
                    self.state
                        .bios
                        .kernel_mut()
                        .return_from_callback(&mut context)?;
                    self.state
                        .timer_manager
                        .finish_callback(timer, result, self.state.hardware.timers_mut())
                        .map_err(IopMachineError::Timer)?;
                    let source = timer_interrupt(timer);
                    let retained =
                        self.state.hardware.interrupt_controller().status() & !source.bit();
                    self.state
                        .hardware
                        .interrupt_controller_mut()
                        .acknowledge(retained);
                }
            }
            apply_bios_context(self.state.hardware.cpu_mut(), &context);
            self.state.callback_active = None;
            return Ok(MachineStep {
                cycles: 1,
                kind: MachineStepKind::CallbackReturn,
            });
        }
        let Some(frame) = self.state.module_frames.pop() else {
            return Err(MachineError::UnexpectedReturn { pc: RETURN_ENTRY });
        };
        let result = self.state.hardware.cpu().register(V0).unwrap_or(0);
        let resident = match result {
            0 => ResidentState::Resident,
            1 => ResidentState::NotResident,
            2 => ResidentState::Removable,
            value => {
                return Err(MachineError::ResidentCode {
                    module_id: frame.module_id,
                    value,
                });
            }
        };
        {
            let mut memory = IopRam(self.state.hardware.memory_mut());
            self.state
                .bios
                .modules_mut()
                .complete_start(frame.module_id, resident, &mut memory)?;
        }
        if let Some(address) = frame.argument_allocation {
            let _ = self.state.bios.memory_mut().free(address);
        }
        if let Some(address) = frame.result_address {
            self.state
                .hardware
                .memory_mut()
                .write_u32(address, result)?;
        }
        let current = self.state.bios.kernel().current_thread();
        if current != Some(frame.thread_id) {
            return Err(KernelError::IllegalContext.into());
        }
        if let Some(requester) = frame.requester {
            self.state.bios.kernel_mut().complete_module_start(
                requester,
                frame.thread_id,
                frame.module_id,
                result,
            )?;
        }
        let mut context = bios_context(self.state.hardware.cpu());
        let schedule = self
            .state
            .bios
            .kernel_mut()
            .exit_delete_current(&mut context)?;
        if let Some(stack) = self.state.thread_stacks.remove(&frame.thread_id) {
            let _ = self.state.bios.memory_mut().free(stack);
        }
        if schedule.current.is_some() {
            apply_bios_context(self.state.hardware.cpu_mut(), &context);
            self.state.halted = false;
        } else {
            self.state.halted = true;
        }
        Ok(MachineStep {
            cycles: 1,
            kind: MachineStepKind::ModuleReturn,
        })
    }

    fn handle_thread_return(&mut self) -> Result<MachineStep, MachineError> {
        let mut context = bios_context(self.state.hardware.cpu());
        let schedule = self.state.bios.kernel_mut().exit_current(&mut context)?;
        if schedule.current.is_some() {
            apply_bios_context(self.state.hardware.cpu_mut(), &context);
            self.state.halted = false;
        } else {
            self.state.halted = true;
        }
        Ok(MachineStep {
            cycles: 1,
            kind: MachineStepKind::ThreadReturn,
        })
    }

    fn enter_pending_callback(&mut self) -> Result<(), MachineError> {
        if self.state.callback_active.is_some() || !self.state.interrupts_enabled {
            return Ok(());
        }
        if let Some(source) = self.state.hardware.interrupt_controller().first_pending()
            && let Ok(callback) = self.state.bios.handlers().dispatch_interrupt(source as u32)
        {
            let mut context = bios_context(self.state.hardware.cpu());
            {
                let mut memory = IopRam(self.state.hardware.memory_mut());
                self.state
                    .bios
                    .dispatch_mut()
                    .enter_interrupt(&mut context, &mut memory)?;
            }
            callback.apply(&mut context);
            apply_bios_context(self.state.hardware.cpu_mut(), &context);
            self.state.callback_active = Some(ActiveCallback::Interrupt);
            return Ok(());
        }
        if let Some(callback) = self.state.interrupt_callbacks.pop_front() {
            let mut context = bios_context(self.state.hardware.cpu());
            {
                let mut memory = IopRam(self.state.hardware.memory_mut());
                self.state
                    .bios
                    .dispatch_mut()
                    .enter_interrupt(&mut context, &mut memory)?;
            }
            callback.apply(&mut context);
            apply_bios_context(self.state.hardware.cpu_mut(), &context);
            self.state.callback_active = Some(ActiveCallback::Interrupt);
            return Ok(());
        }
        if let Some((timer, callback)) = self.state.timer_callbacks.pop_front() {
            let mut context = bios_context(self.state.hardware.cpu());
            self.state
                .bios
                .kernel_mut()
                .enter_callback(&mut context, callback)?;
            apply_bios_context(self.state.hardware.cpu_mut(), &context);
            self.state.callback_active = Some(ActiveCallback::Timer(timer));
            return Ok(());
        }
        if let Some(callback) = self.state.callbacks.pop_front() {
            let mut context = bios_context(self.state.hardware.cpu());
            self.state
                .bios
                .kernel_mut()
                .enter_callback(&mut context, callback)?;
            apply_bios_context(self.state.hardware.cpu_mut(), &context);
            self.state.callback_active = Some(ActiveCallback::Kernel);
        }
        Ok(())
    }

    fn in_idle_loop(&self) -> Result<bool, MachineError> {
        let pc = self.state.hardware.cpu().pc();
        if pc & 3 != 0 {
            return Ok(false);
        }
        let instruction = self.state.hardware.memory().read_u32(pc)?;
        let branch_to_self = instruction == 0x1000_ffff;
        let jump_to_self = instruction >> 26 == 2
            && ((pc.wrapping_add(4) & 0xf000_0000) | ((instruction & 0x03ff_ffff) << 2)) == pc;
        if !branch_to_self && !jump_to_self {
            return Ok(false);
        }
        Ok(self.state.hardware.memory().read_u32(pc.wrapping_add(4))? == 0)
    }

    fn advance_idle(&mut self, cycles: u64) -> Result<(), MachineError> {
        self.state.hardware.advance_devices(cycles)?;
        self.after_device_advance(cycles)
    }

    fn after_device_advance(&mut self, cycles: u64) -> Result<(), MachineError> {
        let due = self.state.sample_clock.advance(Ticks::new(cycles))?.get();
        self.render_due_frames(due)?;
        {
            let (sound, irq) = self.state.hardware.sound_and_interrupt_controller_mut();
            sound.drain_irq(irq);
        }
        let events = self.state.hardware.take_hardware_events();
        for event in events {
            if matches!(event.kind, HardwareEventKind::Dma(_)) {
                self.state.dma_events = self.state.dma_events.saturating_add(1);
            }
            if let HardwareEventKind::Dma(DmaEvent::Completed { channel, .. }) = event.kind {
                let interrupt = match channel {
                    SoundDmaChannel::Core0 => 36,
                    SoundDmaChannel::Core1 => 40,
                };
                if self.state.enabled_interrupts.contains(&interrupt)
                    && let Ok(callback) = self.state.bios.handlers().dispatch_interrupt(interrupt)
                {
                    self.state.interrupt_callbacks.push_back(callback);
                }
            }
            if let HardwareEventKind::Timing(TimingEvent::Counter {
                timer,
                boundary: CounterBoundary::Target,
            }) = event.kind
                && let Some(callback) = self.state.timer_manager.callback(timer)
            {
                self.state.timer_callbacks.push_back((timer, callback));
            }
            if let HardwareEventKind::Timing(TimingEvent::VBlankStart) = event.kind {
                self.state.bios.kernel_mut().notify_vblank(0)?;
                if let Ok(callbacks) = self.state.bios.handlers().dispatch_vblank(0) {
                    self.state.callbacks.extend(callbacks);
                }
            }
            if let HardwareEventKind::Timing(TimingEvent::VBlankEnd) = event.kind {
                self.state.bios.kernel_mut().notify_vblank(1)?;
                if let Ok(callbacks) = self.state.bios.handlers().dispatch_vblank(1) {
                    self.state.callbacks.extend(callbacks);
                }
            }
        }
        for event in self
            .state
            .bios
            .kernel_mut()
            .advance_to(self.state.hardware.now())?
        {
            if let KernelEvent::Alarm { callback, .. } = event {
                self.state.callbacks.push_back(callback);
            }
        }
        Ok(())
    }

    fn render_due_frames(&mut self, mut frames: u64) -> Result<(), MachineError> {
        let mut buffer = [0_i16; AUDIO_CHUNK_FRAMES * 2];
        while frames != 0 {
            let chunk = frames.min(AUDIO_CHUNK_FRAMES as u64) as usize;
            let samples = chunk * 2;
            self.state
                .hardware
                .sound_mut()
                .render(chunk, &mut buffer[..samples])?;
            self.state
                .pending_audio
                .extend(buffer[..samples].iter().copied());
            frames -= chunk as u64;
        }
        Ok(())
    }
}

pub(crate) fn bind_module_imports<M: ServiceMemory>(
    bios: &mut BiosHle,
    memory: &mut M,
    module_id: u32,
) -> Result<(), MachineError> {
    let imports = bios
        .modules()
        .get(module_id)
        .ok_or(KernelError::UnknownModule)?
        .imports()
        .to_vec();
    for library in imports {
        for stub in library.stubs {
            let target =
                match bios
                    .modules_mut()
                    .bind_import(&library.name, library.version, stub.ordinal)
                {
                    Ok(binding) => binding.address,
                    Err(KernelError::LibraryNotFound)
                        if describe_import(&library.name, stub.ordinal).is_some() =>
                    {
                        let call = upse_ps2_bios::ImportCall {
                            library: library.name.clone(),
                            version: library.version,
                            ordinal: stub.ordinal,
                            module_id,
                            pc: stub.address,
                        };
                        let mut guest = BiosMemoryAdapter(memory);
                        bios.dispatch_mut()
                            .allocate_import(&mut guest, call)?
                            .address
                    }
                    Err(KernelError::LibraryNotFound) => {
                        return Err(MachineError::UnresolvedImport {
                            library: library.name.clone(),
                            version: library.version,
                            ordinal: stub.ordinal,
                            module_id,
                        });
                    }
                    Err(error) => return Err(error.into()),
                };
            let jump = 0x0800_0000 | ((target >> 2) & 0x03ff_ffff);
            memory.write(stub.address, &jump.to_le_bytes())?;
        }
    }
    Ok(())
}

pub(crate) struct BiosMemoryAdapter<'a, M>(pub(crate) &'a mut M);

impl<M: ServiceMemory> GuestMemory for BiosMemoryAdapter<'_, M> {
    fn range(&self) -> GuestRange {
        let range = self.0.range();
        GuestRange {
            start: range.start,
            end: range.end,
        }
    }

    fn read(&self, address: u32, output: &mut [u8]) -> Result<(), GuestMemoryError> {
        self.0
            .read(address, output)
            .map_err(|error| GuestMemoryError::new(error.to_string()))
    }

    fn write(&mut self, address: u32, input: &[u8]) -> Result<(), GuestMemoryError> {
        self.0
            .write(address, input)
            .map_err(|error| GuestMemoryError::new(error.to_string()))
    }
}

pub(crate) struct IopRam<'a>(pub(crate) &'a mut IopMemory);

impl ServiceMemory for IopRam<'_> {
    fn range(&self) -> GuestAddressRange {
        GuestAddressRange {
            start: 0,
            end: RAM_SIZE as u32,
        }
    }

    fn read(&self, address: u32, output: &mut [u8]) -> Result<(), ServiceMemoryError> {
        read_iop_ram(self.0, address, output)
            .map_err(|error| ServiceMemoryError::new(error.to_string()))
    }

    fn write(&mut self, address: u32, input: &[u8]) -> Result<(), ServiceMemoryError> {
        write_iop_ram(self.0, address, input)
            .map_err(|error| ServiceMemoryError::new(error.to_string()))
    }
}

impl GuestMemory for IopRam<'_> {
    fn range(&self) -> GuestRange {
        GuestRange {
            start: 0,
            end: RAM_SIZE as u32,
        }
    }

    fn read(&self, address: u32, output: &mut [u8]) -> Result<(), GuestMemoryError> {
        read_iop_ram(self.0, address, output)
            .map_err(|error| GuestMemoryError::new(error.to_string()))
    }

    fn write(&mut self, address: u32, input: &[u8]) -> Result<(), GuestMemoryError> {
        write_iop_ram(self.0, address, input)
            .map_err(|error| GuestMemoryError::new(error.to_string()))
    }
}

fn read_iop_ram(memory: &IopMemory, address: u32, output: &mut [u8]) -> Result<(), MemoryError> {
    for (offset, byte) in output.iter_mut().enumerate() {
        *byte = memory.read_u8(address.wrapping_add(offset as u32))?;
    }
    Ok(())
}

fn write_iop_ram(memory: &mut IopMemory, address: u32, input: &[u8]) -> Result<(), MemoryError> {
    for (offset, byte) in input.iter().copied().enumerate() {
        memory.write_u8(address.wrapping_add(offset as u32), byte)?;
    }
    Ok(())
}

fn service_context(cpu: &Cpu) -> ServiceContext {
    let mut context = ServiceContext::reset(cpu.pc(), cpu.register(29).unwrap_or(0));
    for index in 0..32 {
        context.set_register(index, cpu.register(index).unwrap_or(0));
    }
    context.hi = cpu.hi();
    context.lo = cpu.lo();
    context.status = cpu.cop0().status;
    context.cause = cpu.cop0().cause;
    context.epc = cpu.cop0().epc;
    context
}

pub(crate) fn bios_context_from_service(context: &ServiceContext) -> CpuContext {
    let mut result = CpuContext::reset(context.pc, context.register(29).unwrap_or(0));
    for (index, value) in context.registers().iter().copied().enumerate() {
        result.set_register(index, value);
    }
    result.hi = context.hi;
    result.lo = context.lo;
    result.status = context.status;
    result.cause = context.cause;
    result.epc = context.epc;
    result
}

pub(crate) fn service_context_from_bios(context: &CpuContext, service: &mut ServiceContext) {
    for (index, value) in context.registers().iter().copied().enumerate() {
        service.set_register(index, value);
    }
    service.hi = context.hi;
    service.lo = context.lo;
    service.status = context.status;
    service.cause = context.cause;
    service.epc = context.epc;
    service.pc = context.pc;
}

fn bios_context(cpu: &Cpu) -> CpuContext {
    let service = service_context(cpu);
    bios_context_from_service(&service)
}

fn apply_service_context(cpu: &mut Cpu, context: &ServiceContext) {
    for (index, value) in context.registers().iter().copied().enumerate() {
        cpu.set_register(index, value);
    }
    cpu.set_hi_lo(context.hi, context.lo);
    cpu.cop0_mut().status = context.status;
    cpu.cop0_mut().cause = context.cause;
    cpu.cop0_mut().epc = context.epc;
    cpu.set_pc(context.pc);
}

fn apply_bios_context(cpu: &mut Cpu, context: &CpuContext) {
    let mut service = ServiceContext::reset(context.pc, context.register(29).unwrap_or(0));
    service_context_from_bios(context, &mut service);
    apply_service_context(cpu, &service);
}

const fn timer_interrupt(timer: TimerId) -> upse_iop_irq::InterruptSource {
    match timer {
        TimerId::Timer0 => upse_iop_irq::InterruptSource::Timer0,
        TimerId::Timer1 => upse_iop_irq::InterruptSource::Timer1,
        TimerId::Timer2 => upse_iop_irq::InterruptSource::Timer2,
        TimerId::Timer3 => upse_iop_irq::InterruptSource::Timer3,
        TimerId::Timer4 => upse_iop_irq::InterruptSource::Timer4,
        TimerId::Timer5 => upse_iop_irq::InterruptSource::Timer5,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{Compression, write::ZlibEncoder};
    use upse_ps2_bios::{ModuleState, ThreadState};
    use upse_psf::{
        DependencyLimits, LoadPlan, MemoryResolver, ParseLimits, PsfBuilder, PsfVersion, load_plan,
    };

    use super::{MachineConfig, Ps2Machine};

    const PHOFF: usize = 52;
    const METADATA_OFFSET: usize = 0x80;
    const IMAGE_OFFSET: usize = 0x100;
    const IMAGE_FILE_SIZE: usize = 0x500;
    const IMAGE_MEMORY_SIZE: usize = 0x600;
    const THREAD_ENTRY: usize = 0x190;
    const IOMAN_TABLE: usize = 0x200;
    const LIBSD_TABLE: usize = 0x240;
    const THBASE_TABLE: usize = 0x290;
    const MODLOAD_TABLE: usize = 0x2d0;
    const THREAD_SPEC: usize = 0x300;
    const PATH: usize = 0x340;
    const CHILD_PATH: usize = 0x360;
    const BUFFER: usize = 0x400;

    #[derive(Clone, Copy)]
    struct Import {
        table: usize,
        index: usize,
    }

    impl Import {
        const fn address(self) -> usize {
            self.table + 20 + self.index * 8
        }
    }

    const IOMAN_OPEN: Import = Import {
        table: IOMAN_TABLE,
        index: 0,
    };
    const IOMAN_READ: Import = Import {
        table: IOMAN_TABLE,
        index: 1,
    };
    const SD_INIT: Import = Import {
        table: LIBSD_TABLE,
        index: 0,
    };
    const SD_SET_PARAM: Import = Import {
        table: LIBSD_TABLE,
        index: 1,
    };
    const SD_SET_SWITCH: Import = Import {
        table: LIBSD_TABLE,
        index: 2,
    };
    const SD_SET_ADDR: Import = Import {
        table: LIBSD_TABLE,
        index: 3,
    };
    const SD_VOICE_TRANS: Import = Import {
        table: LIBSD_TABLE,
        index: 4,
    };
    const CREATE_THREAD: Import = Import {
        table: THBASE_TABLE,
        index: 0,
    };
    const START_THREAD: Import = Import {
        table: THBASE_TABLE,
        index: 1,
    };
    const GET_SYSTEM_TIME: Import = Import {
        table: THBASE_TABLE,
        index: 2,
    };
    const LOAD_START_MODULE: Import = Import {
        table: MODLOAD_TABLE,
        index: 0,
    };

    fn fixture_plan() -> upse_psf::Psf2LoadPlan {
        let irx = generated_irx();
        let child = resident_irx();
        let sample = [
            0x00, 0x07, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            0x11, 0x11,
        ];
        let reserved = filesystem(&[
            ("psf2.irx", &irx),
            ("child.irx", &child),
            ("sample.adpcm", &sample),
        ]);
        let psf = PsfBuilder::new(PsfVersion::Psf2)
            .reserved(reserved)
            .tag("title", "Generated PSF2")
            .tag("length", "0.02")
            .build();
        let LoadPlan::Psf2(plan) = load_plan(
            "generated.psf2",
            &psf,
            &mut MemoryResolver::new(),
            ParseLimits::default(),
            DependencyLimits::default(),
        )
        .unwrap() else {
            panic!("generated fixture selected the wrong format")
        };
        plan
    }

    #[test]
    fn generated_module_reads_vfs_schedules_thread_and_emits_integer_golden() {
        let mut machine = Ps2Machine::from_plan(&fixture_plan(), MachineConfig::default()).unwrap();
        let mut first = [0_i16; 32];
        machine.render(16, &mut first).unwrap();
        assert_eq!(
            first,
            [
                1_491, 1_491, 3_582, 3_582, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093,
                4_093, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093,
                4_093, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093, 4_093,
            ]
        );
        assert!(machine.dma_events() >= 2);
        assert_eq!(
            machine
                .state
                .bios
                .modules()
                .find("child")
                .map(upse_ps2_bios::ModuleRecord::state),
            Some(ModuleState::Resident)
        );
        assert_eq!(
            &machine.state.hardware.sound().ram()[0x1000..0x1010],
            &[
                0x00, 0x07, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
                0x11, 0x11
            ]
        );
        let fixture = machine.state.bios.modules().find("fixture").unwrap();
        let clock_address =
            fixture.image_allocation().address + u32::try_from(BUFFER).unwrap() + 32;
        let clock = u64::from(
            machine
                .state
                .hardware
                .memory()
                .read_u32(clock_address)
                .unwrap(),
        ) | (u64::from(
            machine
                .state
                .hardware
                .memory()
                .read_u32(clock_address + 4)
                .unwrap(),
        ) << 32);
        assert!(clock >= super::services::SYSTEM_CLOCK_EPOCH);
        assert!(
            clock <= super::services::SYSTEM_CLOCK_EPOCH + machine.state.bios.kernel().now().get()
        );
        for _ in 0..1_000 {
            if machine
                .state
                .bios
                .kernel()
                .threads()
                .any(|(_, thread)| thread.state() == ThreadState::Running)
            {
                break;
            }
            machine.step().unwrap();
        }
        let thread_states = machine
            .state
            .bios
            .kernel()
            .threads()
            .map(|(id, thread)| (id, thread.state()))
            .collect::<Vec<_>>();
        assert!(
            thread_states
                .iter()
                .any(|(_, state)| *state == ThreadState::Running),
            "thread states: {thread_states:?}, pc={:#010x}",
            machine.pc()
        );

        let snapshot = first;
        machine.reset();
        let mut replay = [0_i16; 32];
        machine.render(16, &mut replay).unwrap();
        assert_eq!(replay, snapshot);
    }

    #[test]
    fn psf2_composition_has_no_ee_or_firmware_dependency() {
        let manifest = include_str!("../Cargo.toml");
        assert!(!manifest.contains("upse-ee"));
        assert!(!manifest.contains("firmware"));
        let constructor: fn(
            &upse_psf::Psf2LoadPlan,
            MachineConfig,
        ) -> Result<Ps2Machine, super::MachineError> = Ps2Machine::from_plan;
        let _ = constructor;
    }

    #[test]
    fn large_snapshots_are_heap_backed() {
        assert_eq!(
            std::mem::size_of::<Ps2Machine>(),
            2 * std::mem::size_of::<usize>()
        );
    }

    fn generated_irx() -> Vec<u8> {
        let mut image = vec![0_u8; IMAGE_MEMORY_SIZE];
        let mut code = Vec::new();

        emit_addu(&mut code, 18, 31, 0);
        emit_addiu(&mut code, 4, 28, PATH as i16);
        emit_addiu(&mut code, 5, 0, 1);
        emit_call(&mut code, IOMAN_OPEN);
        emit_addu(&mut code, 16, 2, 0);
        emit_addu(&mut code, 4, 16, 0);
        emit_addiu(&mut code, 5, 28, BUFFER as i16);
        emit_addiu(&mut code, 6, 0, 16);
        emit_call(&mut code, IOMAN_READ);
        emit_addiu(&mut code, 4, 28, (BUFFER + 32) as i16);
        emit_call(&mut code, GET_SYSTEM_TIME);
        emit_addiu(&mut code, 4, 28, CHILD_PATH as i16);
        emit_addiu(&mut code, 5, 0, 0);
        emit_addiu(&mut code, 6, 0, 0);
        emit_call(&mut code, LOAD_START_MODULE);

        emit_addiu(&mut code, 4, 0, 0);
        emit_call(&mut code, SD_INIT);
        emit_addiu(&mut code, 8, 0, 16);
        emit_sw(&mut code, 8, 16, 29);
        emit_addiu(&mut code, 4, 0, 1);
        emit_addiu(&mut code, 5, 0, 0);
        emit_addiu(&mut code, 6, 28, BUFFER as i16);
        emit_addiu(&mut code, 7, 0, 0x1000);
        emit_call(&mut code, SD_VOICE_TRANS);
        for (selector, value) in [
            (0x0001, 0x3fff),
            (0x0101, 0x3fff),
            (0x0201, 0x1000),
            (0x0301, 0x00ff),
            (0x0401, 0x1f00),
        ] {
            emit_addiu(&mut code, 4, 0, selector);
            emit_addiu(&mut code, 5, 0, value);
            emit_call(&mut code, SD_SET_PARAM);
        }
        emit_addiu(&mut code, 4, 0, 0x2001);
        emit_addiu(&mut code, 5, 0, 0x1000);
        emit_call(&mut code, SD_SET_ADDR);
        emit_lui(&mut code, 8, 0x1f80);
        emit_addiu(&mut code, 9, 28, BUFFER as i16);
        emit_sw(&mut code, 9, 0x1500, 8);
        emit_addiu(&mut code, 9, 0, 4);
        emit_sw(&mut code, 9, 0x1504, 8);
        emit_addiu(&mut code, 9, 0, 8);
        emit_sw(&mut code, 9, 0x1570, 8);
        emit_lui(&mut code, 9, 0x0100);
        emit_ori(&mut code, 9, 9, 1);
        emit_sw(&mut code, 9, 0x1508, 8);
        emit_addiu(&mut code, 4, 0, 0x1501);
        emit_addiu(&mut code, 5, 0, 1);
        emit_call(&mut code, SD_SET_SWITCH);

        emit_addiu(&mut code, 8, 28, THREAD_SPEC as i16);
        emit_addiu(&mut code, 9, 28, THREAD_ENTRY as i16);
        emit_sw(&mut code, 9, 8, 8);
        emit_addu(&mut code, 4, 8, 0);
        emit_call(&mut code, CREATE_THREAD);
        emit_addu(&mut code, 17, 2, 0);
        emit_addu(&mut code, 4, 17, 0);
        emit_addiu(&mut code, 5, 0, 0);
        emit_call(&mut code, START_THREAD);
        emit_addiu(&mut code, 2, 0, 0);
        emit_jr(&mut code, 18);
        code.push(0);
        assert!(code.len() * 4 <= THREAD_ENTRY);
        write_words(&mut image, 0, &code);
        write_words(&mut image, THREAD_ENTRY, &[0x1000_ffff, 0]);

        write_import_table(&mut image, IOMAN_TABLE, "ioman", 0x0104, &[4, 6]);
        write_import_table(&mut image, LIBSD_TABLE, "libsd", 0x0105, &[4, 5, 7, 9, 17]);
        write_import_table(&mut image, THBASE_TABLE, "thbase", 0x0102, &[4, 6, 34]);
        write_import_table(&mut image, MODLOAD_TABLE, "modload", 0x0107, &[7]);
        put_u32(&mut image, THREAD_SPEC + 12, 0x400);
        put_u32(&mut image, THREAD_SPEC + 16, 32);
        image[PATH..PATH + 20].copy_from_slice(b"host0:/sample.adpcm\0");
        image[CHILD_PATH..CHILD_PATH + 10].copy_from_slice(b"child.irx\0");

        let mut elf = vec![0_u8; IMAGE_OFFSET + IMAGE_FILE_SIZE];
        elf[..16].copy_from_slice(b"\x7fELF\x01\x01\x01\0\0\0\0\0\0\0\0\0");
        put_u16(&mut elf, 16, 0xff81);
        put_u16(&mut elf, 18, 8);
        put_u32(&mut elf, 20, 1);
        put_u32(&mut elf, 24, 0);
        put_u32(&mut elf, 28, PHOFF);
        put_u32(&mut elf, 32, 0);
        put_u16(&mut elf, 40, 52);
        put_u16(&mut elf, 42, 32);
        put_u16(&mut elf, 44, 2);
        program_header(&mut elf, 0, 0x7000_0080, METADATA_OFFSET, 0, 34, 34, 4);
        program_header(
            &mut elf,
            1,
            1,
            IMAGE_OFFSET,
            0,
            IMAGE_FILE_SIZE,
            IMAGE_MEMORY_SIZE,
            16,
        );
        put_u32(&mut elf, METADATA_OFFSET, u32::MAX);
        put_u32(&mut elf, METADATA_OFFSET + 4, 0);
        put_u32(&mut elf, METADATA_OFFSET + 8, 0);
        put_u32(&mut elf, METADATA_OFFSET + 12, THREAD_ENTRY + 8);
        put_u32(
            &mut elf,
            METADATA_OFFSET + 16,
            IMAGE_FILE_SIZE - THREAD_ENTRY - 8,
        );
        put_u32(
            &mut elf,
            METADATA_OFFSET + 20,
            IMAGE_MEMORY_SIZE - IMAGE_FILE_SIZE,
        );
        put_u16(&mut elf, METADATA_OFFSET + 24, 0x0100);
        elf[METADATA_OFFSET + 26..METADATA_OFFSET + 34].copy_from_slice(b"fixture\0");
        elf[IMAGE_OFFSET..IMAGE_OFFSET + IMAGE_FILE_SIZE]
            .copy_from_slice(&image[..IMAGE_FILE_SIZE]);
        elf
    }

    fn resident_irx() -> Vec<u8> {
        let words = [0x2402_0000, (31 << 21) | 8, 0];
        let image = words
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        let mut elf = vec![0_u8; IMAGE_OFFSET + image.len()];
        elf[..16].copy_from_slice(b"\x7fELF\x01\x01\x01\0\0\0\0\0\0\0\0\0");
        put_u16(&mut elf, 16, 0xff81);
        put_u16(&mut elf, 18, 8);
        put_u32(&mut elf, 20, 1);
        put_u32(&mut elf, 24, 0);
        put_u32(&mut elf, 28, PHOFF);
        put_u32(&mut elf, 32, 0);
        put_u16(&mut elf, 40, 52);
        put_u16(&mut elf, 42, 32);
        put_u16(&mut elf, 44, 2);
        program_header(&mut elf, 0, 0x7000_0080, METADATA_OFFSET, 0, 32, 32, 4);
        program_header(&mut elf, 1, 1, IMAGE_OFFSET, 0, image.len(), image.len(), 4);
        put_u32(&mut elf, METADATA_OFFSET, u32::MAX);
        put_u32(&mut elf, METADATA_OFFSET + 4, 0);
        put_u32(&mut elf, METADATA_OFFSET + 8, 0);
        put_u32(&mut elf, METADATA_OFFSET + 12, image.len());
        put_u32(&mut elf, METADATA_OFFSET + 16, 0);
        put_u32(&mut elf, METADATA_OFFSET + 20, 0);
        put_u16(&mut elf, METADATA_OFFSET + 24, 0x0100);
        elf[METADATA_OFFSET + 26..METADATA_OFFSET + 32].copy_from_slice(b"child\0");
        elf[IMAGE_OFFSET..].copy_from_slice(&image);
        elf
    }

    fn write_import_table(
        image: &mut [u8],
        offset: usize,
        name: &str,
        version: u16,
        ordinals: &[u16],
    ) {
        put_u32(image, offset, 0x41e0_0000);
        put_u16(image, offset + 8, version);
        image[offset + 12..offset + 12 + name.len()].copy_from_slice(name.as_bytes());
        for (index, ordinal) in ordinals.iter().copied().enumerate() {
            let stub = offset + 20 + index * 8;
            put_u32(image, stub, 0x03e0_0008);
            put_u32(image, stub + 4, 0x2400_0000 | u32::from(ordinal));
        }
    }

    fn filesystem(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut output = vec![0; 4 + entries.len() * 48];
        put_u32(&mut output, 0, entries.len());
        for (index, (name, data)) in entries.iter().enumerate() {
            let entry = 4 + index * 48;
            output[entry..entry + name.len()].copy_from_slice(name.as_bytes());
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(data).unwrap();
            let block = encoder.finish().unwrap();
            let data_offset = output.len();
            output.extend_from_slice(&u32::try_from(block.len()).unwrap().to_le_bytes());
            output.extend_from_slice(&block);
            put_u32(&mut output, entry + 36, data_offset);
            put_u32(&mut output, entry + 40, data.len());
            put_u32(&mut output, entry + 44, data.len());
        }
        output
    }

    #[allow(clippy::too_many_arguments)]
    fn program_header(
        elf: &mut [u8],
        index: usize,
        kind: u32,
        offset: usize,
        address: u32,
        file_size: usize,
        memory_size: usize,
        alignment: u32,
    ) {
        let at = PHOFF + index * 32;
        put_u32(elf, at, kind);
        put_u32(elf, at + 4, offset);
        put_u32(elf, at + 8, address);
        put_u32(elf, at + 12, address);
        put_u32(elf, at + 16, file_size);
        put_u32(elf, at + 20, memory_size);
        put_u32(elf, at + 24, 7);
        put_u32(elf, at + 28, alignment);
    }

    fn emit_call(words: &mut Vec<u32>, import: Import) {
        emit_addiu(words, 25, 28, import.address() as i16);
        words.push((25 << 21) | (31 << 11) | 9);
        words.push(0);
    }

    fn emit_addiu(words: &mut Vec<u32>, rt: u32, rs: u32, immediate: i16) {
        words.push(
            (0x09 << 26)
                | (rs << 21)
                | (rt << 16)
                | u32::from(u16::from_ne_bytes(immediate.to_ne_bytes())),
        );
    }

    fn emit_lui(words: &mut Vec<u32>, rt: u32, immediate: u16) {
        words.push((0x0f << 26) | (rt << 16) | u32::from(immediate));
    }

    fn emit_ori(words: &mut Vec<u32>, rt: u32, rs: u32, immediate: u16) {
        words.push((0x0d << 26) | (rs << 21) | (rt << 16) | u32::from(immediate));
    }

    fn emit_addu(words: &mut Vec<u32>, rd: u32, rs: u32, rt: u32) {
        words.push((rs << 21) | (rt << 16) | (rd << 11) | 0x21);
    }

    fn emit_sw(words: &mut Vec<u32>, rt: u32, offset: i16, base: u32) {
        words.push(
            (0x2b << 26)
                | (base << 21)
                | (rt << 16)
                | u32::from(u16::from_ne_bytes(offset.to_ne_bytes())),
        );
    }

    fn emit_jr(words: &mut Vec<u32>, register: u32) {
        words.push((register << 21) | 8);
    }

    fn write_words(output: &mut [u8], offset: usize, words: &[u32]) {
        for (index, word) in words.iter().copied().enumerate() {
            put_u32(output, offset + index * 4, word);
        }
    }

    fn put_u16(output: &mut [u8], offset: usize, value: u16) {
        output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(output: &mut [u8], offset: usize, value: impl TryInto<u32>) {
        let value = value.try_into().ok().unwrap();
        output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}
