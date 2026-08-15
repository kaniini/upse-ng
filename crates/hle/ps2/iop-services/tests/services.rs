// SPDX-License-Identifier: LGPL-2.1-or-later
//! Import-service conformance harness over the real PS2 BIOS foundation.

use std::collections::{BTreeMap, VecDeque};

use upse_iop_services::{
    BackendError, BackendPayload, BackendRequest, BackendResponse, BiosServices, GuestAddressRange,
    ImportRequest, IopServices, ReadOnlyFileSystem, ServiceAction, ServiceContext, ServiceError,
    ServiceFamily, ServiceMemory, ServiceMemoryError,
};
use upse_irx::{IrxModule, ResidentState};
use upse_ps2_bios::{
    AllocationMode, BiosHle, CpuContext, EventFlagSpec, GuestMemory, GuestMemoryError, GuestRange,
    KernelError, RescheduleReason, SemaphoreSpec, ThreadSpec,
};

const RAM_SIZE: usize = 0x20_0000;

#[derive(Clone, Debug, Default)]
struct MockFs(BTreeMap<String, Vec<u8>>);

impl MockFs {
    fn with(mut self, path: &str, bytes: Vec<u8>) -> Self {
        self.0.insert(path.to_owned(), bytes);
        self
    }
}

impl ReadOnlyFileSystem for MockFs {
    fn file(&self, path: &str) -> Result<&[u8], String> {
        self.0
            .get(path)
            .map(Vec::as_slice)
            .ok_or_else(|| format!("missing {path}"))
    }
}

#[derive(Clone, Debug)]
struct Memory(Vec<u8>);

impl Memory {
    fn new() -> Self {
        Self(vec![0; RAM_SIZE])
    }

    fn put(&mut self, address: u32, bytes: &[u8]) {
        let start = usize::try_from(address).unwrap();
        self.0[start..start + bytes.len()].copy_from_slice(bytes);
    }
}

impl ServiceMemory for Memory {
    fn range(&self) -> GuestAddressRange {
        GuestAddressRange {
            start: 0,
            end: u32::try_from(self.0.len()).unwrap(),
        }
    }

    fn read(&self, address: u32, output: &mut [u8]) -> Result<(), ServiceMemoryError> {
        let start = usize::try_from(address).unwrap();
        output.copy_from_slice(
            self.0
                .get(start..start + output.len())
                .ok_or_else(|| ServiceMemoryError::new("outside test RAM"))?,
        );
        Ok(())
    }

    fn write(&mut self, address: u32, input: &[u8]) -> Result<(), ServiceMemoryError> {
        let start = usize::try_from(address).unwrap();
        self.0
            .get_mut(start..start + input.len())
            .ok_or_else(|| ServiceMemoryError::new("outside test RAM"))?
            .copy_from_slice(input);
        Ok(())
    }
}

impl GuestMemory for Memory {
    fn range(&self) -> GuestRange {
        GuestRange {
            start: 0,
            end: u32::try_from(self.0.len()).unwrap(),
        }
    }

    fn read(&self, address: u32, output: &mut [u8]) -> Result<(), GuestMemoryError> {
        ServiceMemory::read(self, address, output)
            .map_err(|error| GuestMemoryError::new(error.to_string()))
    }

    fn write(&mut self, address: u32, input: &[u8]) -> Result<(), GuestMemoryError> {
        ServiceMemory::write(self, address, input)
            .map_err(|error| GuestMemoryError::new(error.to_string()))
    }
}

#[derive(Default)]
struct RecordingBackend {
    requests: VecDeque<BackendRequest>,
}

impl BiosServices for RecordingBackend {
    fn dispatch<M: ServiceMemory>(
        &mut self,
        request: BackendRequest,
        _context: &mut ServiceContext,
        _memory: &mut M,
    ) -> Result<BackendResponse, BackendError> {
        let value = 0x1000 + u32::from(request.import.ordinal);
        self.requests.push_back(request);
        Ok(BackendResponse::returning(value))
    }
}

struct BiosHarness {
    bios: BiosHle,
    thread_stacks: BTreeMap<u32, u32>,
}

impl BiosHarness {
    fn new(memory: &mut Memory) -> Self {
        let mut bios = BiosHle::new().unwrap();
        bios.reset(memory).unwrap();
        Self {
            bios,
            thread_stacks: BTreeMap::new(),
        }
    }

    fn kernel_result(result: Result<u32, KernelError>) -> BackendResponse {
        BackendResponse::returning(match result {
            Ok(value) => value,
            Err(error) => u32::from_ne_bytes(error.code().to_ne_bytes()),
        })
    }

    fn apply_schedule(
        service: &mut ServiceContext,
        cpu: &CpuContext,
        switched: bool,
    ) -> BackendResponse {
        copy_from_bios(cpu, service);
        BackendResponse {
            v0: cpu.register(2).unwrap_or(0),
            v1: cpu.register(3),
            action: if switched {
                ServiceAction::ContextSwitch
            } else {
                ServiceAction::Return
            },
        }
    }
}

impl BiosServices for BiosHarness {
    #[allow(clippy::too_many_lines)]
    fn dispatch<M: ServiceMemory>(
        &mut self,
        request: BackendRequest,
        context: &mut ServiceContext,
        memory: &mut M,
    ) -> Result<BackendResponse, BackendError> {
        let [a0, a1, a2, a3] = request.arguments;
        match (request.family, request.import.ordinal) {
            (ServiceFamily::SystemMemory, 4) => {
                let result = AllocationMode::try_from(a0)
                    .and_then(|mode| self.bios.memory_mut().allocate(mode, a1, a2))
                    .map(|allocation| allocation.address);
                Ok(Self::kernel_result(result))
            }
            (ServiceFamily::SystemMemory, 5) => Ok(Self::kernel_result(
                self.bios.memory_mut().free(a0).map(|_| 0),
            )),
            (ServiceFamily::SystemMemory, 6) => {
                Ok(BackendResponse::returning(self.bios.memory().memory_size()))
            }
            (ServiceFamily::SystemMemory, 7) => Ok(BackendResponse::returning(
                self.bios.memory().maximum_free(),
            )),
            (ServiceFamily::SystemMemory, 8) => {
                Ok(BackendResponse::returning(self.bios.memory().total_free()))
            }
            (ServiceFamily::Thread, 4) => {
                let attributes = read_word(memory, a0)?;
                let option = read_word(memory, a0 + 4)?;
                let entry = read_word(memory, a0 + 8)?;
                let stack_size = read_word(memory, a0 + 12)?;
                let priority = read_word(memory, a0 + 16)?;
                let stack =
                    match self
                        .bios
                        .memory_mut()
                        .allocate(AllocationMode::First, stack_size, 0)
                    {
                        Ok(allocation) => allocation.address,
                        Err(error) => return Ok(Self::kernel_result(Err(error))),
                    };
                let range = bios_range(memory);
                let result = self
                    .bios
                    .kernel_mut()
                    .create_thread(
                        ThreadSpec {
                            entry,
                            stack,
                            stack_size,
                            priority,
                            attributes,
                            option,
                        },
                        range,
                    )
                    .inspect(|id| {
                        self.thread_stacks.insert(*id, stack);
                    });
                if result.is_err() {
                    self.bios.memory_mut().free(stack).unwrap();
                }
                Ok(Self::kernel_result(result))
            }
            (ServiceFamily::Thread, 5) => {
                let result = self.bios.kernel_mut().delete_thread(a0).map(|_| 0);
                if result.is_ok()
                    && let Some(stack) = self.thread_stacks.remove(&a0)
                {
                    self.bios.memory_mut().free(stack).unwrap();
                }
                Ok(Self::kernel_result(result))
            }
            (ServiceFamily::Thread, 6 | 7) => {
                let result = self.bios.kernel_mut().start_thread(a0, a1);
                if let Err(error) = result {
                    return Ok(Self::kernel_result(Err(error)));
                }
                let mut cpu = copy_to_bios(context);
                let schedule = self
                    .bios
                    .kernel_mut()
                    .reschedule(&mut cpu, RescheduleReason::HleReturn)
                    .map_err(|error| BackendError::new(error.to_string()))?;
                Ok(Self::apply_schedule(context, &cpu, schedule.switched))
            }
            (ServiceFamily::Thread, 20) => Ok(BackendResponse::returning(
                self.bios.kernel().current_thread().unwrap_or(0),
            )),
            (ServiceFamily::Thread, 24) => {
                let mut cpu = copy_to_bios(context);
                let schedule = self
                    .bios
                    .kernel_mut()
                    .sleep_current(&mut cpu)
                    .map_err(|error| BackendError::new(error.to_string()))?;
                Ok(Self::apply_schedule(context, &cpu, schedule.switched))
            }
            (ServiceFamily::Thread, 25 | 26) => Ok(Self::kernel_result(
                self.bios.kernel_mut().wakeup_thread(a0).map(|_| 0),
            )),
            (ServiceFamily::Semaphore, 4) => {
                let result = (|| {
                    let attributes = read_word(memory, a0)?;
                    let initial = read_word(memory, a0 + 8)?;
                    let maximum = read_word(memory, a0 + 12)?;
                    self.bios
                        .kernel_mut()
                        .create_semaphore(SemaphoreSpec {
                            initial,
                            maximum,
                            attributes,
                        })
                        .map_err(|error| BackendError::new(error.to_string()))
                })();
                match result {
                    Ok(id) => Ok(BackendResponse::returning(id)),
                    Err(error) => Err(error),
                }
            }
            (ServiceFamily::Semaphore, 6 | 7) => Ok(Self::kernel_result(
                self.bios.kernel_mut().signal_semaphore(a0).map(|_| 0),
            )),
            (ServiceFamily::Semaphore, 8) => {
                let mut cpu = copy_to_bios(context);
                let result = self
                    .bios
                    .kernel_mut()
                    .wait_semaphore(a0, None, &mut cpu)
                    .map_err(|error| BackendError::new(error.to_string()))?;
                let switched = result.is_some_and(|action| action.switched);
                Ok(Self::apply_schedule(context, &cpu, switched))
            }
            (ServiceFamily::EventFlag, 4) => {
                let bits = read_word(memory, a0 + 8)?;
                let attributes = read_word(memory, a0)?;
                Ok(Self::kernel_result(
                    self.bios
                        .kernel_mut()
                        .create_event_flag(EventFlagSpec { bits, attributes }),
                ))
            }
            (ServiceFamily::EventFlag, 6 | 7) => Ok(Self::kernel_result(
                self.bios.kernel_mut().set_event_flag(a0, a1).map(|_| 0),
            )),
            (ServiceFamily::Exception, 4) => Ok(Self::kernel_result(
                self.bios
                    .handlers_mut()
                    .register_exception(a0, a1, bios_range(memory))
                    .map(|()| 0),
            )),
            (ServiceFamily::Interrupt, 4) => Ok(Self::kernel_result(
                self.bios
                    .handlers_mut()
                    .register_interrupt(a0, a1, a2, a3, bios_range(memory))
                    .map(|()| 0),
            )),
            (ServiceFamily::VBlank, 8) => Ok(Self::kernel_result(
                self.bios
                    .handlers_mut()
                    .register_vblank(a0, a1, a2, a3, bios_range(memory))
                    .map(|()| 0),
            )),
            (ServiceFamily::ModuleLoader, 6 | 7) => {
                let BackendPayload::Module {
                    path,
                    bytes,
                    arguments,
                    start,
                } = request.payload
                else {
                    return Err(BackendError::new("module request has no VFS payload"));
                };
                let irx = IrxModule::parse(path, &bytes)
                    .map_err(|error| BackendError::new(error.to_string()))?;
                let mut guest = BiosMemoryAdapter(memory);
                let id = self
                    .bios
                    .load_module(&irx, &mut guest)
                    .map_err(|error| BackendError::new(error.to_string()))?;
                if !start {
                    return Ok(BackendResponse::returning(id));
                }
                let invocation = self
                    .bios
                    .modules_mut()
                    .begin_start(id, &mut guest)
                    .map_err(|error| BackendError::new(error.to_string()))?;
                context.pc = invocation.entry;
                context.set_register(4, u32::try_from(arguments.len()).unwrap_or(u32::MAX));
                context.set_register(28, invocation.global_pointer);
                Ok(BackendResponse {
                    v0: id,
                    v1: None,
                    action: ServiceAction::CallModule,
                })
            }
            _ => Err(BackendError::new(format!(
                "test adapter does not implement {} ordinal {}",
                request.import.library, request.import.ordinal
            ))),
        }
    }
}

struct BiosMemoryAdapter<'a, M>(&'a mut M);

impl<M: ServiceMemory> GuestMemory for BiosMemoryAdapter<'_, M> {
    fn range(&self) -> GuestRange {
        bios_range(self.0)
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

fn bios_range<M: ServiceMemory>(memory: &M) -> GuestRange {
    let range = memory.range();
    GuestRange {
        start: range.start,
        end: range.end,
    }
}

fn read_word<M: ServiceMemory>(memory: &M, address: u32) -> Result<u32, BackendError> {
    let mut bytes = [0; 4];
    memory
        .read(address, &mut bytes)
        .map_err(|error| BackendError::new(error.to_string()))?;
    Ok(u32::from_le_bytes(bytes))
}

fn copy_to_bios(context: &ServiceContext) -> CpuContext {
    let mut cpu = CpuContext::reset(context.pc, context.register(29).unwrap_or(0));
    for (index, value) in context.registers().iter().copied().enumerate() {
        cpu.set_register(index, value);
    }
    cpu.hi = context.hi;
    cpu.lo = context.lo;
    cpu.status = context.status;
    cpu.cause = context.cause;
    cpu.epc = context.epc;
    cpu
}

fn copy_from_bios(context: &CpuContext, service: &mut ServiceContext) {
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

fn import(library: &str, version: u16, ordinal: u16) -> ImportRequest {
    ImportRequest {
        library: library.to_owned(),
        version,
        ordinal,
        module_id: 7,
        pc: 0x12_3450,
    }
}

fn set_arguments(context: &mut ServiceContext, arguments: [u32; 4]) {
    for (index, argument) in arguments.into_iter().enumerate() {
        context.set_register(4 + index, argument);
    }
}

#[test]
fn every_backend_family_preserves_import_context_and_registers() {
    let families = [
        ("sysmem", 0x0101, 4, ServiceFamily::SystemMemory),
        ("loadcore", 0x0101, 6, ServiceFamily::LoadCore),
        ("excepman", 0x0101, 4, ServiceFamily::Exception),
        ("intrman", 0x0101, 4, ServiceFamily::Interrupt),
        ("dmacman", 0x0101, 4, ServiceFamily::Dma),
        ("thbase", 0x0101, 4, ServiceFamily::Thread),
        ("thsemap", 0x0101, 4, ServiceFamily::Semaphore),
        ("thevent", 0x0101, 4, ServiceFamily::EventFlag),
        ("thmsgbx", 0x0101, 4, ServiceFamily::MessageBox),
        ("thfpool", 0x0101, 4, ServiceFamily::FixedPool),
        ("thvpool", 0x0101, 4, ServiceFamily::VariablePool),
        ("heaplib", 0x0101, 4, ServiceFamily::Heap),
        ("timrman", 0x0101, 4, ServiceFamily::Timer),
        ("libsd", 0x0105, 4, ServiceFamily::Sound),
        ("vblank", 0x0101, 8, ServiceFamily::VBlank),
    ];
    let mut services = IopServices::new(MockFs::default());
    let mut backend = RecordingBackend::default();
    let mut memory = Memory::new();
    let mut context = ServiceContext::reset(0x1000, 0x1f_0000);
    set_arguments(&mut context, [1, 2, 3, 4]);
    for (library, version, ordinal, family) in families {
        let outcome = services
            .dispatch(
                import(library, version, ordinal),
                &mut context,
                &mut memory,
                &mut backend,
            )
            .unwrap();
        assert_eq!(outcome.v0, 0x1000 + u32::from(ordinal));
        let request = backend.requests.pop_front().unwrap();
        assert_eq!(request.family, family);
        assert_eq!(request.arguments, [1, 2, 3, 4]);
        assert_eq!(request.import.module_id, 7);
        assert_eq!(request.import.pc, 0x12_3450);
    }
}

#[test]
fn ioman_is_read_only_bounded_and_has_no_host_fallback() {
    let fs = MockFs::default().with("data/test.bin", b"abcdef".to_vec());
    let mut services = IopServices::new(fs);
    let mut backend = RecordingBackend::default();
    let mut memory = Memory::new();
    let mut context = ServiceContext::reset(0x1000, 0x1f_0000);
    memory.put(0x1000, b"host0:/data/test.bin\0");
    set_arguments(&mut context, [0x1000, 1, 0, 0]);
    let opened = services
        .dispatch(
            import("ioman", 0x0101, 4),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap();
    assert_eq!(opened.v0, 3);

    set_arguments(&mut context, [3, 0x2000, 4, 0]);
    assert_eq!(
        services
            .dispatch(
                import("ioman", 0x0101, 6),
                &mut context,
                &mut memory,
                &mut backend,
            )
            .unwrap()
            .v0,
        4
    );
    assert_eq!(&memory.0[0x2000..0x2004], b"abcd");
    set_arguments(&mut context, [3, u32::MAX, 1, 0]);
    assert_eq!(
        services
            .dispatch(
                import("ioman", 0x0101, 8),
                &mut context,
                &mut memory,
                &mut backend,
            )
            .unwrap()
            .v0,
        3
    );
    set_arguments(&mut context, [3, 0x2000, 1, 0]);
    assert_eq!(
        i32::from_ne_bytes(
            services
                .dispatch(
                    import("ioman", 0x0101, 7),
                    &mut context,
                    &mut memory,
                    &mut backend,
                )
                .unwrap()
                .v0
                .to_ne_bytes()
        ),
        -30
    );
    memory.put(0x1100, b"host:/etc/passwd\0");
    set_arguments(&mut context, [0x1100, 1, 0, 0]);
    assert_eq!(
        i32::from_ne_bytes(
            services
                .dispatch(
                    import("ioman", 0x0101, 4),
                    &mut context,
                    &mut memory,
                    &mut backend,
                )
                .unwrap()
                .v0
                .to_ne_bytes()
        ),
        -2
    );
}

#[test]
fn sysclib_stdio_and_clock_are_guest_visible() {
    let mut services = IopServices::new(MockFs::default());
    let mut backend = RecordingBackend::default();
    let mut memory = Memory::new();
    let mut context = ServiceContext::reset(0x1000, 0x1f_0000);
    memory.put(0x1000, b"copy me\0");
    set_arguments(&mut context, [0x2000, 0x1000, 8, 0]);
    services
        .dispatch(
            import("sysclib", 0x0101, 12),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap();
    assert_eq!(&memory.0[0x2000..0x2008], b"copy me\0");

    memory.put(0x1010, b"bcopy!\0");
    set_arguments(&mut context, [0x1010, 0x2010, 7, 0]);
    services
        .dispatch(
            import("sysclib", 0x0101, 16),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap();
    assert_eq!(&memory.0[0x2010..0x2017], b"bcopy!\0");

    memory.put(0x1100, b"value=%d %s\n\0");
    memory.put(0x1200, b"ok\0");
    set_arguments(&mut context, [0x1100, 42, 0x1200, 0]);
    services
        .dispatch(
            import("stdio", 0x0102, 4),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap();
    assert_eq!(services.take_tty(), ["value=42 ok\n"]);

    memory.put(0x1400, b"music%03d.bgm %#08x %-5s!\0");
    memory.put(0x1500, b"ok\0");
    memory.put(0x1f_0010, &0x1500_u32.to_le_bytes());
    set_arguments(&mut context, [0x1600, 0x1400, 1, 0x2a]);
    services
        .dispatch(
            import("sysclib", 0x0101, 19),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap();
    let expected = b"music001.bgm 0x00002a ok   !\0";
    assert_eq!(&memory.0[0x1600..0x1600 + expected.len()], expected);

    set_arguments(&mut context, [1_000_000, 0x1300, 0, 0]);
    services
        .dispatch(
            import("thbase", 0x0101, 39),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap();
    assert_eq!(
        u64::from_le_bytes(memory.0[0x1300..0x1308].try_into().unwrap()),
        36_864_000
    );
}

#[test]
fn sif_and_ssbus_are_iop_local() {
    let mut services = IopServices::new(MockFs::default());
    let mut backend = RecordingBackend::default();
    let mut memory = Memory::new();
    let mut context = ServiceContext::reset(0x1000, 0x1f_0000);
    set_arguments(&mut context, [3, 0x55aa, 0, 0]);
    services
        .dispatch(
            import("sifcmd", 0x0101, 7),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap();
    set_arguments(&mut context, [3, 0, 0, 0]);
    assert_eq!(
        services
            .dispatch(
                import("sifcmd", 0x0101, 6),
                &mut context,
                &mut memory,
                &mut backend,
            )
            .unwrap()
            .v0,
        0x55aa
    );
    set_arguments(&mut context, [0x1800, 1, 0, 0]);
    let transfer = services
        .dispatch(
            import("sifman", 0x0101, 7),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap()
        .v0;
    assert_ne!(transfer, 0);
    set_arguments(&mut context, [transfer, 0, 0, 0]);
    assert_eq!(
        services
            .dispatch(
                import("sifman", 0x0101, 8),
                &mut context,
                &mut memory,
                &mut backend,
            )
            .unwrap()
            .v0,
        u32::MAX
    );
    set_arguments(&mut context, [2, 0x1234, 0, 0]);
    services
        .dispatch(
            import("ssbusc", 0x0101, 4),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap();
    assert_eq!(
        services
            .dispatch(
                import("ssbusc", 0x0101, 5),
                &mut context,
                &mut memory,
                &mut backend,
            )
            .unwrap()
            .v0,
        0x1234
    );
}

#[test]
fn bios_adapter_runs_memory_threads_objects_handlers_and_dynamic_irx() {
    let fixture = ps2sdk_irx_fixture();
    let fs = MockFs::default().with("modules/child.irx", fixture);
    let mut services = IopServices::new(fs);
    let mut memory = Memory::new();
    let mut backend = BiosHarness::new(&mut memory);
    let mut context = ServiceContext::reset(0x1000, 0x1f_0000);

    set_arguments(&mut context, [0, 0x1000, 0, 0]);
    let allocation = services
        .dispatch(
            import("sysmem", 0x0101, 4),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap()
        .v0;
    assert!(allocation >= 0x1_0000);

    memory.put(0x2000, &0x0200_0000_u32.to_le_bytes());
    memory.put(0x2004, &7_u32.to_le_bytes());
    memory.put(0x2008, &0x10_000_u32.to_le_bytes());
    memory.put(0x200c, &0x800_u32.to_le_bytes());
    memory.put(0x2010, &20_u32.to_le_bytes());
    set_arguments(&mut context, [0x2000, 0, 0, 0]);
    let thread = services
        .dispatch(
            import("thbase", 0x0101, 4),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap()
        .v0;
    set_arguments(&mut context, [thread, 0x99, 0, 0]);
    let started = services
        .dispatch(
            import("thbase", 0x0101, 6),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap();
    assert_eq!(started.action, ServiceAction::ContextSwitch);
    assert_eq!(context.pc, 0x10_000);
    assert_eq!(context.register(4), Some(0x99));

    memory.put(0x2100, &0_u32.to_le_bytes());
    memory.put(0x2108, &0_u32.to_le_bytes());
    memory.put(0x210c, &1_u32.to_le_bytes());
    set_arguments(&mut context, [0x2100, 0, 0, 0]);
    let semaphore = services
        .dispatch(
            import("thsemap", 0x0101, 4),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap()
        .v0;
    assert_eq!(semaphore, 1);

    set_arguments(&mut context, [2, 0x10_100, 0, 0]);
    assert_eq!(
        services
            .dispatch(
                import("excepman", 0x0101, 4),
                &mut context,
                &mut memory,
                &mut backend,
            )
            .unwrap()
            .v0,
        0
    );

    memory.put(0x2200, b"host:modules/child.irx\0");
    set_arguments(&mut context, [0x2200, 0, 0, 0]);
    let module = services
        .dispatch(
            import("modload", 0x0101, 7),
            &mut context,
            &mut memory,
            &mut backend,
        )
        .unwrap();
    assert_eq!(module.action, ServiceAction::CallModule);
    let id = module.v0;
    assert_eq!(backend.bios.modules().find("fixture").unwrap().id(), id);
    backend
        .bios
        .modules_mut()
        .complete_start(id, ResidentState::Removable, &mut memory)
        .unwrap();
}

#[test]
fn version_unknown_and_ee_dependent_imports_never_succeed_silently() {
    let mut services = IopServices::new(MockFs::default());
    let mut backend = RecordingBackend::default();
    let mut memory = Memory::new();
    let mut context = ServiceContext::reset(0x1000, 0x1f_0000);
    assert!(matches!(
        services.dispatch(
            import("sysmem", 0x0201, 4),
            &mut context,
            &mut memory,
            &mut backend,
        ),
        Err(ServiceError::VersionMismatch { .. })
    ));
    assert!(matches!(
        services.dispatch(
            import("mystery", 0x0101, 4),
            &mut context,
            &mut memory,
            &mut backend,
        ),
        Err(ServiceError::UnknownImport { .. })
    ));
    assert!(matches!(
        services.dispatch(
            import("sifcmd", 0x0101, 16),
            &mut context,
            &mut memory,
            &mut backend,
        ),
        Err(ServiceError::UnsupportedImport { .. })
    ));
    assert!(backend.requests.is_empty());
}

fn ps2sdk_irx_fixture() -> Vec<u8> {
    const PHOFF: usize = 52;
    const IOPMOD_OFFSET: usize = 0xa0;
    const IMAGE_OFFSET: usize = 0x100;
    const IMAGE_SIZE: usize = 0xc0;
    const MEMORY_SIZE: usize = 0xd0;
    const REL_OFFSET: usize = 0x1c0;
    const SHOFF: usize = 0x1c8;

    let mut elf = vec![0_u8; SHOFF + 3 * 40];
    elf[..16].copy_from_slice(b"\x7fELF\x01\x01\x01\0\0\0\0\0\0\0\0\0");
    put_u16(&mut elf, 16, 0xff81);
    put_u16(&mut elf, 18, 8);
    put_u32(&mut elf, 20, 1);
    put_u32(&mut elf, 28, u32::try_from(PHOFF).unwrap());
    put_u32(&mut elf, 32, u32::try_from(SHOFF).unwrap());
    put_u16(&mut elf, 40, 52);
    put_u16(&mut elf, 42, 32);
    put_u16(&mut elf, 44, 2);
    put_u16(&mut elf, 46, 40);
    put_u16(&mut elf, 48, 3);
    program_header(&mut elf, 0, 0x7000_0080, IOPMOD_OFFSET, 0, 27, 27, 4);
    program_header(&mut elf, 1, 1, IMAGE_OFFSET, 0, IMAGE_SIZE, MEMORY_SIZE, 16);
    put_u32(&mut elf, IOPMOD_OFFSET, 0x40);
    put_u32(&mut elf, IOPMOD_OFFSET + 8, 0x30);
    put_u32(&mut elf, IOPMOD_OFFSET + 12, 0xb0);
    put_u32(&mut elf, IOPMOD_OFFSET + 16, 0x10);
    put_u32(&mut elf, IOPMOD_OFFSET + 20, 0x10);
    put_u16(&mut elf, IOPMOD_OFFSET + 24, 0x0102);
    put_u32(&mut elf, IMAGE_OFFSET + 0x40, 0x48);
    put_u16(&mut elf, IMAGE_OFFSET + 0x44, 0x0102);
    elf[IMAGE_OFFSET + 0x48..IMAGE_OFFSET + 0x50].copy_from_slice(b"fixture\0");
    let export = IMAGE_OFFSET + 0x90;
    put_u32(&mut elf, export, 0x41c0_0000);
    put_u16(&mut elf, export + 8, 0x0102);
    elf[export + 12..export + 20].copy_from_slice(b"fixture\0");
    put_u32(&mut elf, export + 20, 0x10);
    put_u32(&mut elf, REL_OFFSET, 0xa4);
    put_u32(&mut elf, REL_OFFSET + 4, 2);
    section_header(&mut elf, 1, 1, IMAGE_OFFSET, IMAGE_SIZE, 0, 16, 0);
    section_header(&mut elf, 2, 9, REL_OFFSET, 8, 1, 4, 8);
    elf
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
    let at = 52 + index * 32;
    put_u32(elf, at, kind);
    put_u32(elf, at + 4, u32::try_from(offset).unwrap());
    put_u32(elf, at + 8, address);
    put_u32(elf, at + 12, address);
    put_u32(elf, at + 16, u32::try_from(file_size).unwrap());
    put_u32(elf, at + 20, u32::try_from(memory_size).unwrap());
    put_u32(elf, at + 24, 7);
    put_u32(elf, at + 28, alignment);
}

#[allow(clippy::too_many_arguments)]
fn section_header(
    elf: &mut [u8],
    index: usize,
    kind: u32,
    offset: usize,
    size: usize,
    info: u32,
    alignment: u32,
    entry_size: u32,
) {
    let at = 0x1c8 + index * 40;
    put_u32(elf, at + 4, kind);
    put_u32(elf, at + 8, 6);
    put_u32(elf, at + 16, u32::try_from(offset).unwrap());
    put_u32(elf, at + 20, u32::try_from(size).unwrap());
    put_u32(elf, at + 28, info);
    put_u32(elf, at + 32, alignment);
    put_u32(elf, at + 36, entry_size);
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
