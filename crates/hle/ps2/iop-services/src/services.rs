// SPDX-License-Identifier: LGPL-2.1-or-later

use crate::ioman::{IoManager, ReadOnlyFileSystem};
use crate::matrix::version_compatible;
use crate::{
    BackendError, ServiceContext, ServiceDescription, ServiceError, ServiceFamily, ServiceMemory,
    SupportLevel, describe_import,
};

const V0: usize = 2;
const V1: usize = 3;
const MAX_GUEST_STRING: usize = 4096;
const MAX_MODULE_ARGUMENTS: usize = 4096;
const IOP_SYSTEM_CLOCK_HZ: u64 = 36_864_000;

/// Complete import identity supplied by the IRX linker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportRequest {
    /// Eight-byte IOP library name.
    pub library: String,
    /// Required packed major/minor version.
    pub version: u16,
    /// Function ordinal.
    pub ordinal: u16,
    /// Calling module identifier.
    pub module_id: u32,
    /// Original guest call-site PC.
    pub pc: u32,
}

/// Optional owned data prepared for a backend operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendPayload {
    /// No additional data.
    None,
    /// A module read entirely from the immutable PSF2 VFS.
    Module {
        /// Normalized guest path.
        path: String,
        /// Validated IRX bytes.
        bytes: Vec<u8>,
        /// Bounded module argument block.
        arguments: Vec<u8>,
        /// Whether the loader should start the module after relocation.
        start: bool,
    },
}

/// One typed request crossing the BIOS/machine adapter boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendRequest {
    /// Complete import identity.
    pub import: ImportRequest,
    /// Selected service family.
    pub family: ServiceFamily,
    /// Public function name.
    pub symbol: &'static str,
    /// Four conventional arguments.
    pub arguments: [u32; 4],
    /// Optional service-prepared data.
    pub payload: BackendPayload,
}

/// Control-flow effect of a completed import.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceAction {
    /// Return normally to the guest caller.
    Return,
    /// The calling thread blocked and the backend installed another context.
    ContextSwitch,
    /// The backend prepared a guest module entry invocation.
    CallModule,
    /// Guest execution requested termination.
    Halt,
}

/// Result returned by a BIOS/machine adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackendResponse {
    /// Raw value for `v0` when returning normally.
    pub v0: u32,
    /// Optional secondary value for `v1`.
    pub v1: Option<u32>,
    /// Control-flow action.
    pub action: ServiceAction,
}

impl BackendResponse {
    /// Constructs a normal single-register return.
    #[must_use]
    pub const fn returning(v0: u32) -> Self {
        Self {
            v0,
            v1: None,
            action: ServiceAction::Return,
        }
    }
}

/// Narrow adapter implemented by the conformance harness or complete machine.
pub trait BiosServices {
    /// Performs one operation not owned by the import layer.
    ///
    /// # Errors
    ///
    /// Returns a structured machine/kernel diagnostic.
    fn dispatch<M: ServiceMemory>(
        &mut self,
        request: BackendRequest,
        context: &mut ServiceContext,
        memory: &mut M,
    ) -> Result<BackendResponse, BackendError>;
}

/// Observable result of one import dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceOutcome {
    /// Raw guest `v0` after the operation.
    pub v0: u32,
    /// Control-flow action.
    pub action: ServiceAction,
}

/// Instance-owned import dispatcher and local service state.
#[derive(Clone, Debug)]
pub struct IopServices<F> {
    filesystem: F,
    ioman: IoManager,
    tty: Vec<String>,
    sif_initialized: bool,
    sif_flags: [u32; 2],
    sif_registers: [u32; 32],
    ssbus_registers: [u32; 14],
    strtok_next: Option<u32>,
}

impl<F> IopServices<F> {
    /// Constructs reset services over an immutable filesystem.
    #[must_use]
    pub fn new(filesystem: F) -> Self {
        Self {
            filesystem,
            ioman: IoManager::new(),
            tty: Vec::new(),
            sif_initialized: false,
            sif_flags: [0; 2],
            sif_registers: [0; 32],
            ssbus_registers: [0; 14],
            strtok_next: None,
        }
    }

    /// Clears descriptors and local device state without replacing the VFS.
    pub fn reset(&mut self) {
        self.ioman.reset();
        self.tty.clear();
        self.sif_initialized = false;
        self.sif_flags = [0; 2];
        self.sif_registers = [0; 32];
        self.ssbus_registers = [0; 14];
        self.strtok_next = None;
    }

    /// Returns the immutable filesystem.
    #[must_use]
    pub const fn filesystem(&self) -> &F {
        &self.filesystem
    }

    /// Removes accumulated TTY writes in call order.
    pub fn take_tty(&mut self) -> Vec<String> {
        std::mem::take(&mut self.tty)
    }
}

impl<F: ReadOnlyFileSystem> IopServices<F> {
    /// Dispatches one validated named import.
    ///
    /// # Errors
    ///
    /// Returns precise library/version/ordinal, guest-memory, VFS, local
    /// resource, or backend diagnostics.
    pub fn dispatch<B: BiosServices, M: ServiceMemory>(
        &mut self,
        import: ImportRequest,
        context: &mut ServiceContext,
        memory: &mut M,
        backend: &mut B,
    ) -> Result<ServiceOutcome, ServiceError> {
        let description = describe_import(&import.library, import.ordinal)
            .ok_or_else(|| unknown_import(&import))?;
        if !version_compatible(description.provided_version, import.version) {
            return Err(ServiceError::VersionMismatch {
                library: import.library,
                provided: description.provided_version,
                required: import.version,
                module_id: import.module_id,
                pc: import.pc,
            });
        }
        match description.support {
            SupportLevel::ReturnOnly => Ok(Self::finish_local(context, 0, ServiceAction::Return)),
            SupportLevel::Unsupported => Err(ServiceError::UnsupportedImport {
                library: import.library,
                symbol: description.symbol,
                ordinal: import.ordinal,
                module_id: import.module_id,
                pc: import.pc,
            }),
            SupportLevel::Backend => {
                let request = BackendRequest {
                    arguments: context.arguments(),
                    family: description.family,
                    symbol: description.symbol,
                    payload: BackendPayload::None,
                    import,
                };
                let response = backend.dispatch(request, context, memory)?;
                Ok(Self::finish_backend(context, response))
            }
            SupportLevel::Local => {
                self.dispatch_local(import, description, context, memory, backend)
            }
        }
    }

    fn dispatch_local<B: BiosServices, M: ServiceMemory>(
        &mut self,
        import: ImportRequest,
        description: ServiceDescription,
        context: &mut ServiceContext,
        memory: &mut M,
        backend: &mut B,
    ) -> Result<ServiceOutcome, ServiceError> {
        let arguments = context.arguments();
        match description.family {
            ServiceFamily::ModuleLoader => {
                let path = read_c_string(memory, arguments[0], MAX_GUEST_STRING)?;
                let normalized = normalize_module_path(&path);
                let bytes = self
                    .filesystem
                    .file(&normalized)
                    .map_err(ServiceError::Vfs)?
                    .to_vec();
                let size =
                    usize::try_from(arguments[1]).map_err(|_| ServiceError::InvalidArgument {
                        operation: "module arguments",
                        detail: "argument size exceeds host width",
                    })?;
                if size > MAX_MODULE_ARGUMENTS {
                    return Err(ServiceError::ResourceLimit("module arguments"));
                }
                let module_arguments = read_bytes(memory, arguments[2], size)?;
                let request = BackendRequest {
                    arguments,
                    family: description.family,
                    symbol: description.symbol,
                    payload: BackendPayload::Module {
                        path: normalized,
                        bytes,
                        arguments: module_arguments,
                        start: import.ordinal == 7,
                    },
                    import,
                };
                let response = backend.dispatch(request, context, memory)?;
                Ok(Self::finish_backend(context, response))
            }
            ServiceFamily::Ioman => self.dispatch_ioman(import.ordinal, context, memory),
            ServiceFamily::Sysclib => {
                let value = crate::sysclib::dispatch(
                    import.ordinal,
                    context,
                    memory,
                    &mut self.strtok_next,
                )?;
                Ok(Self::finish_local(context, value, ServiceAction::Return))
            }
            ServiceFamily::Stdio => {
                let value =
                    crate::sysclib::dispatch_stdio(import.ordinal, context, memory, &mut self.tty)?;
                Ok(Self::finish_local(context, value, ServiceAction::Return))
            }
            ServiceFamily::SystemMemory => {
                let value = crate::sysclib::dispatch_kprintf(context, memory, &mut self.tty)?;
                Ok(Self::finish_local(context, value, ServiceAction::Return))
            }
            ServiceFamily::Thread => {
                let value = Self::dispatch_clock(import.ordinal, arguments, memory)?;
                Ok(Self::finish_local(context, value, ServiceAction::Return))
            }
            ServiceFamily::SifManager | ServiceFamily::SifCommand | ServiceFamily::Ssbus => {
                let value = self.dispatch_bus(description.family, import.ordinal, arguments)?;
                Ok(Self::finish_local(context, value, ServiceAction::Return))
            }
            _ => Err(unknown_import(&import)),
        }
    }

    fn dispatch_ioman<M: ServiceMemory>(
        &mut self,
        ordinal: u16,
        context: &mut ServiceContext,
        memory: &mut M,
    ) -> Result<ServiceOutcome, ServiceError> {
        let [a0, a1, a2, _] = context.arguments();
        let value = match ordinal {
            4 => {
                let path = read_c_string(memory, a0, MAX_GUEST_STRING)?;
                self.ioman.open(&self.filesystem, &path, a1)
            }
            5 => self.ioman.close(a0),
            6 => self.ioman.read(&self.filesystem, a0, a1, a2, memory)?,
            7 => {
                if a0 == 1 || a0 == 2 {
                    let bytes = read_bytes(memory, a1, usize::try_from(a2).unwrap_or(usize::MAX))?;
                    self.tty.push(String::from_utf8_lossy(&bytes).into_owned());
                    i32::try_from(bytes.len()).unwrap_or(i32::MAX)
                } else {
                    IoManager::write_error()
                }
            }
            8 => self.ioman.seek(
                &self.filesystem,
                a0,
                i32::from_ne_bytes(a1.to_ne_bytes()),
                a2,
            ),
            16 => {
                let path = read_c_string(memory, a0, MAX_GUEST_STRING)?;
                IoManager::getstat(&self.filesystem, &path, a1, memory)?
            }
            _ => unreachable!("matrix routes only local ioman operations"),
        };
        let raw = u32::from_ne_bytes(value.to_ne_bytes());
        context.set_register(V0, raw);
        Ok(ServiceOutcome {
            v0: raw,
            action: ServiceAction::Return,
        })
    }

    fn dispatch_clock<M: ServiceMemory>(
        ordinal: u16,
        arguments: [u32; 4],
        memory: &mut M,
    ) -> Result<u32, ServiceError> {
        match ordinal {
            39 => {
                let ticks = u64::from(arguments[0]) * IOP_SYSTEM_CLOCK_HZ / 1_000_000;
                write_u64(memory, arguments[1], ticks)?;
                Ok(0)
            }
            40 => {
                let ticks = read_u64(memory, arguments[0])?;
                let seconds = ticks / IOP_SYSTEM_CLOCK_HZ;
                let usec = (ticks % IOP_SYSTEM_CLOCK_HZ) * 1_000_000 / IOP_SYSTEM_CLOCK_HZ;
                if arguments[1] != 0 {
                    write_u32(
                        memory,
                        arguments[1],
                        u32::try_from(seconds).unwrap_or(u32::MAX),
                    )?;
                }
                if arguments[2] != 0 {
                    write_u32(
                        memory,
                        arguments[2],
                        u32::try_from(usec).unwrap_or(u32::MAX),
                    )?;
                }
                Ok(0)
            }
            _ => unreachable!("matrix routes only local clock operations"),
        }
    }

    fn dispatch_bus(
        &mut self,
        family: ServiceFamily,
        ordinal: u16,
        arguments: [u32; 4],
    ) -> Result<u32, ServiceError> {
        match family {
            ServiceFamily::SifManager => match ordinal {
                4 | 5 => {
                    self.sif_initialized = true;
                    Ok(0)
                }
                6 | 25..=27 => Ok(0),
                21 => Ok(self.sif_flags[0]),
                22 => {
                    self.sif_flags[0] = arguments[0];
                    Ok(arguments[0])
                }
                23 => Ok(self.sif_flags[1]),
                24 => {
                    self.sif_flags[1] = arguments[0];
                    Ok(arguments[0])
                }
                28 => {
                    self.sif_registers[0] = arguments[0];
                    Ok(0)
                }
                29 => Ok(u32::from(self.sif_initialized)),
                _ => Err(ServiceError::InvalidArgument {
                    operation: "sifman",
                    detail: "local ordinal is not modeled",
                }),
            },
            ServiceFamily::SifCommand => {
                match ordinal {
                    4 => {
                        self.sif_initialized = true;
                        Ok(0)
                    }
                    5 => {
                        self.sif_initialized = false;
                        Ok(0)
                    }
                    6 => {
                        let index = usize::try_from(arguments[0]).unwrap_or(usize::MAX);
                        self.sif_registers.get(index).copied().ok_or(
                            ServiceError::InvalidArgument {
                                operation: "sceSifGetSreg",
                                detail: "register index is outside the fixed table",
                            },
                        )
                    }
                    7 => {
                        let index = usize::try_from(arguments[0]).unwrap_or(usize::MAX);
                        let register = self.sif_registers.get_mut(index).ok_or(
                            ServiceError::InvalidArgument {
                                operation: "sceSifSetSreg",
                                detail: "register index is outside the fixed table",
                            },
                        )?;
                        *register = arguments[1];
                        Ok(0)
                    }
                    8 | 9 => Ok(0),
                    _ => unreachable!("matrix routes only local sifcmd operations"),
                }
            }
            ServiceFamily::Ssbus => {
                let offset = ordinal
                    .checked_sub(4)
                    .ok_or(ServiceError::InvalidArgument {
                        operation: "ssbusc",
                        detail: "ordinal precedes register table",
                    })?;
                let index = usize::from(offset / 2);
                let register =
                    self.ssbus_registers
                        .get_mut(index)
                        .ok_or(ServiceError::InvalidArgument {
                            operation: "ssbusc",
                            detail: "register index is outside the fixed table",
                        })?;
                if offset & 1 == 0 {
                    let old = *register;
                    *register = arguments[1];
                    Ok(old)
                } else {
                    Ok(*register)
                }
            }
            _ => unreachable!("only bus-local families reach this dispatcher"),
        }
    }

    fn finish_backend(context: &mut ServiceContext, response: BackendResponse) -> ServiceOutcome {
        if response.action == ServiceAction::Return {
            context.set_register(V0, response.v0);
            if let Some(value) = response.v1 {
                context.set_register(V1, value);
            }
        }
        ServiceOutcome {
            v0: response.v0,
            action: response.action,
        }
    }

    fn finish_local(
        context: &mut ServiceContext,
        value: u32,
        action: ServiceAction,
    ) -> ServiceOutcome {
        context.set_register(V0, value);
        ServiceOutcome { v0: value, action }
    }
}

fn unknown_import(import: &ImportRequest) -> ServiceError {
    ServiceError::UnknownImport {
        library: import.library.clone(),
        version: import.version,
        ordinal: import.ordinal,
        module_id: import.module_id,
        pc: import.pc,
    }
}

pub(crate) fn read_c_string<M: ServiceMemory>(
    memory: &M,
    address: u32,
    limit: usize,
) -> Result<String, ServiceError> {
    let mut bytes = Vec::new();
    for offset in 0..limit {
        let offset = u32::try_from(offset).map_err(|_| ServiceError::ResourceLimit("string"))?;
        let address = address
            .checked_add(offset)
            .ok_or(ServiceError::InvalidArgument {
                operation: "guest string",
                detail: "address overflow",
            })?;
        let mut byte = [0];
        read_memory(memory, address, &mut byte)?;
        if byte[0] == 0 {
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        bytes.push(byte[0]);
    }
    Err(ServiceError::UnterminatedString { address })
}

pub(crate) fn read_bytes<M: ServiceMemory>(
    memory: &M,
    address: u32,
    size: usize,
) -> Result<Vec<u8>, ServiceError> {
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = vec![0; size];
    read_memory(memory, address, &mut bytes)?;
    Ok(bytes)
}

pub(crate) fn read_memory<M: ServiceMemory>(
    memory: &M,
    address: u32,
    output: &mut [u8],
) -> Result<(), ServiceError> {
    memory
        .range()
        .validate(address, output.len(), 1)
        .and_then(|()| memory.read(address, output))
        .map_err(|source| ServiceError::GuestMemory {
            address,
            size: output.len(),
            source,
        })
}

pub(crate) fn write_memory<M: ServiceMemory>(
    memory: &mut M,
    address: u32,
    input: &[u8],
) -> Result<(), ServiceError> {
    memory
        .range()
        .validate(address, input.len(), 1)
        .and_then(|()| memory.write(address, input))
        .map_err(|source| ServiceError::GuestMemory {
            address,
            size: input.len(),
            source,
        })
}

pub(crate) fn read_u32<M: ServiceMemory>(memory: &M, address: u32) -> Result<u32, ServiceError> {
    let mut bytes = [0; 4];
    read_memory(memory, address, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

pub(crate) fn read_u64<M: ServiceMemory>(memory: &M, address: u32) -> Result<u64, ServiceError> {
    let mut bytes = [0; 8];
    read_memory(memory, address, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

pub(crate) fn write_u32<M: ServiceMemory>(
    memory: &mut M,
    address: u32,
    value: u32,
) -> Result<(), ServiceError> {
    write_memory(memory, address, &value.to_le_bytes())
}

pub(crate) fn write_u64<M: ServiceMemory>(
    memory: &mut M,
    address: u32,
    value: u64,
) -> Result<(), ServiceError> {
    write_memory(memory, address, &value.to_le_bytes())
}

fn normalize_module_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path
        .split_once(':')
        .map_or(path.as_str(), |(_, remainder)| remainder);
    path.trim_start_matches('/').to_owned()
}
