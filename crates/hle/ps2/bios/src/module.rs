// SPDX-License-Identifier: LGPL-2.1-or-later

use upse_irx::{
    ExportLibrary, ImportLibrary, IrxModule, IrxVariant, MemoryRange, ResidentState, TargetError,
    TargetMemory,
};

use crate::dispatch::MODULE_HEAD_ADDRESS;
use crate::memory::{write_guest, write_u16, write_u32};
use crate::{
    Allocation, AllocationMode, BiosError, FixedTable, GuestMemory, KernelError, SystemMemory,
};

const MODULE_INFO_SIZE: u32 = 48;
const MAX_MODULES: usize = crate::DEFAULT_MODULE_CAPACITY;
const MAX_LIBRARIES: usize = crate::DEFAULT_LIBRARY_CAPACITY;

/// Guest-visible loadcore module state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModuleState {
    /// Loaded but not entered.
    Loaded,
    /// Entry point is currently running.
    Starting,
    /// Started with `MODULE_RESIDENT_END`.
    Resident,
    /// Started with `MODULE_REMOVABLE_END`.
    Removable,
    /// Stop entry is currently running.
    Stopping,
    /// Stopped module which was not declared removable.
    Stopped,
    /// Stopped module which may be unloaded.
    StoppedRemovable,
    /// Entry returned `MODULE_NO_RESIDENT_END` and may be unloaded.
    NotResident,
}

impl ModuleState {
    const fn new_flags(self) -> u16 {
        match self {
            Self::Loaded => 1,
            Self::Starting => 2,
            Self::Resident => 3,
            Self::Removable => 0x13,
            Self::Stopping => 0x15,
            Self::Stopped => 7,
            Self::StoppedRemovable => 0x17,
            Self::NotResident => 6,
        }
    }
}

/// Exact 48-byte `loadcore` module descriptor visible to IOP code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModuleInfo {
    /// Next descriptor in the BIOS-owned list.
    pub next: u32,
    /// Null-terminated module-name address.
    pub name: u32,
    /// Packed module version.
    pub version: u16,
    /// Loadcore state and removable flags.
    pub new_flags: u16,
    /// Positive module identifier.
    pub id: u16,
    /// Legacy module flags.
    pub flags: u16,
    /// Module entry point.
    pub entry: u32,
    /// Module global pointer.
    pub global_pointer: u32,
    /// First text byte.
    pub text_start: u32,
    /// Declared text byte count.
    pub text_size: u32,
    /// Declared initialized-data byte count.
    pub data_size: u32,
    /// Declared BSS byte count.
    pub bss_size: u32,
    /// Reserved field kept at zero.
    pub unused1: u32,
    /// Reserved field kept at zero.
    pub unused2: u32,
}

impl ModuleInfo {
    /// Encodes the PS2SDK-compatible little-endian structure.
    #[must_use]
    pub fn encode(self) -> [u8; MODULE_INFO_SIZE as usize] {
        let mut bytes = [0; MODULE_INFO_SIZE as usize];
        put_u32(&mut bytes, 0, self.next);
        put_u32(&mut bytes, 4, self.name);
        put_u16(&mut bytes, 8, self.version);
        put_u16(&mut bytes, 10, self.new_flags);
        put_u16(&mut bytes, 12, self.id);
        put_u16(&mut bytes, 14, self.flags);
        put_u32(&mut bytes, 16, self.entry);
        put_u32(&mut bytes, 20, self.global_pointer);
        put_u32(&mut bytes, 24, self.text_start);
        put_u32(&mut bytes, 28, self.text_size);
        put_u32(&mut bytes, 32, self.data_size);
        put_u32(&mut bytes, 36, self.bss_size);
        put_u32(&mut bytes, 40, self.unused1);
        put_u32(&mut bytes, 44, self.unused2);
        bytes
    }
}

/// Registered export library owned by a BIOS instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportRegistration {
    /// Module which registered the library, or zero for a built-in service.
    pub module_id: u32,
    /// Eight-character IOP library name.
    pub name: String,
    /// Packed major/minor version.
    pub version: u16,
    /// Export table mode.
    pub mode: u16,
    /// Guest export-table address.
    pub table_address: u32,
    /// Function pointers indexed by ordinal.
    pub functions: Vec<u32>,
    users: u32,
}

impl ExportRegistration {
    /// Returns the number of acquired import bindings.
    #[must_use]
    pub const fn users(&self) -> u32 {
        self.users
    }
}

/// Resolved export function and the registration which owns it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedImport {
    /// Fixed-table export registration identifier.
    pub library_id: u32,
    /// Guest function address.
    pub address: u32,
}

/// Loaded module entry-point invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModuleInvocation {
    /// Module identifier passed to the runtime.
    pub module_id: u32,
    /// Guest entry address.
    pub entry: u32,
    /// Guest global-pointer value.
    pub global_pointer: u32,
}

/// One loaded IOP module and its owned allocations/link descriptions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleRecord {
    id: u32,
    name: String,
    version: u16,
    state: ModuleState,
    info_address: u32,
    info: ModuleInfo,
    metadata_allocation: Allocation,
    image_allocation: Allocation,
    imports: Vec<ImportLibrary>,
    export_ids: Vec<u32>,
}

impl ModuleRecord {
    /// Returns the positive module identifier.
    #[must_use]
    pub const fn id(&self) -> u32 {
        self.id
    }

    /// Returns the module name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the packed module version.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Returns current loadcore lifecycle state.
    #[must_use]
    pub const fn state(&self) -> ModuleState {
        self.state
    }

    /// Returns the guest address of the 48-byte module descriptor.
    #[must_use]
    pub const fn info_address(&self) -> u32 {
        self.info_address
    }

    /// Returns the guest-visible descriptor.
    #[must_use]
    pub const fn info(&self) -> ModuleInfo {
        self.info
    }

    /// Returns the module image allocation.
    #[must_use]
    pub const fn image_allocation(&self) -> Allocation {
        self.image_allocation
    }

    /// Returns validated import tables discovered by the IRX loader.
    #[must_use]
    pub fn imports(&self) -> &[ImportLibrary] {
        &self.imports
    }

    /// Returns export registration identifiers owned by the module.
    #[must_use]
    pub fn export_ids(&self) -> &[u32] {
        &self.export_ids
    }
}

/// Fixed-capacity loadcore module and export-library registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleRegistry {
    modules: FixedTable<ModuleRecord, MAX_MODULES>,
    libraries: FixedTable<ExportRegistration, MAX_LIBRARIES>,
    head: u32,
}

impl ModuleRegistry {
    /// Constructs an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            modules: FixedTable::new(1),
            libraries: FixedTable::new(1),
            head: 0,
        }
    }

    /// Drops all module and library records.
    pub fn reset(&mut self) {
        self.modules.clear();
        self.libraries.clear();
        self.head = 0;
    }

    /// Returns the guest module-list head.
    #[must_use]
    pub const fn head(&self) -> u32 {
        self.head
    }

    /// Returns the number of loaded modules.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.modules.len()
    }

    /// Reports whether no modules are loaded.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// Finds a module by numeric identifier.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<&ModuleRecord> {
        self.modules.get(id)
    }

    /// Finds a module by exact IOP name.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&ModuleRecord> {
        self.modules
            .iter()
            .find_map(|(_, module)| (module.name == name).then_some(module))
    }

    /// Finds the module whose allocation contains a guest address.
    #[must_use]
    pub fn containing(&self, address: u32) -> Option<&ModuleRecord> {
        self.modules.iter().find_map(|(_, module)| {
            let allocation = module.image_allocation;
            let end = allocation.address.checked_add(allocation.size)?;
            (address >= allocation.address && address < end).then_some(module)
        })
    }

    /// Relocates an IRX into system memory and publishes its module descriptor.
    ///
    /// # Errors
    ///
    /// Returns a structured IRX, memory, registry, or guest-memory failure. All
    /// host allocations and table entries are rolled back on failure.
    #[allow(clippy::too_many_lines)]
    pub fn load<M: GuestMemory>(
        &mut self,
        irx: &IrxModule,
        allocator: &mut SystemMemory,
        guest: &mut M,
    ) -> Result<u32, BiosError> {
        let module_id = self.modules.next_id().ok_or(KernelError::NoMemory)?;
        let module_id_u16 = u16::try_from(module_id).map_err(|_| KernelError::NoMemory)?;
        let description = irx.description();
        validate_module_name(&description.name)?;

        let name_size = u32::try_from(description.name.len())
            .ok()
            .and_then(|size| size.checked_add(1))
            .ok_or(KernelError::IllegalSize)?;
        let metadata_size = MODULE_INFO_SIZE
            .checked_add(name_size)
            .ok_or(KernelError::IllegalSize)?;
        let metadata = allocator.allocate(AllocationMode::First, metadata_size, 0)?;
        let image_mode = if description.variant == IrxVariant::Executable {
            AllocationMode::Address
        } else {
            AllocationMode::First
        };
        let image_address = if image_mode == AllocationMode::Address {
            irx.preferred_address()
        } else {
            0
        };
        let alignment = irx.alignment().max(crate::SYSMEM_QUANTUM);
        let image = match allocator.allocate_aligned(
            image_mode,
            irx.allocation_size(),
            image_address,
            alignment,
        ) {
            Ok(allocation) => allocation,
            Err(error) => {
                allocator
                    .free(metadata.address)
                    .map_err(BiosError::Kernel)?;
                return Err(error.into());
            }
        };

        let loaded = {
            let mut target = GuestTarget {
                guest,
                range: MemoryRange {
                    start: image.address,
                    end: image.address + image.requested_size,
                },
            };
            match irx.load_into(image.address, &mut target) {
                Ok(loaded) => loaded,
                Err(error) => {
                    allocator.free(image.address).map_err(BiosError::Kernel)?;
                    allocator
                        .free(metadata.address)
                        .map_err(BiosError::Kernel)?;
                    return Err(error.into());
                }
            }
        };

        if let Err(error) = self.validate_exports(&loaded.exports) {
            allocator.free(image.address).map_err(BiosError::Kernel)?;
            allocator
                .free(metadata.address)
                .map_err(BiosError::Kernel)?;
            return Err(error.into());
        }

        let name_address = metadata.address + MODULE_INFO_SIZE;
        let info = ModuleInfo {
            next: self.head,
            name: name_address,
            version: description.version,
            new_flags: ModuleState::Loaded.new_flags(),
            id: module_id_u16,
            flags: 0,
            entry: loaded.entry,
            global_pointer: loaded.global_pointer,
            text_start: loaded.allocation.address,
            text_size: description.text_size,
            data_size: description.data_size,
            bss_size: description.bss_size,
            unused1: 0,
            unused2: 0,
        };
        let mut name = description.name.as_bytes().to_vec();
        name.push(0);
        if let Err(error) = write_guest(guest, metadata.address, &info.encode())
            .and_then(|()| write_guest(guest, name_address, &name))
            .and_then(|()| write_u32(guest, MODULE_HEAD_ADDRESS, metadata.address))
        {
            allocator.free(image.address).map_err(BiosError::Kernel)?;
            allocator
                .free(metadata.address)
                .map_err(BiosError::Kernel)?;
            return Err(error);
        }

        let mut export_ids = Vec::with_capacity(loaded.exports.len());
        for export in loaded.exports.iter().cloned() {
            match self.insert_export(module_id, export) {
                Ok(id) => export_ids.push(id),
                Err(error) => {
                    for id in export_ids {
                        let _ = self.libraries.remove(id);
                    }
                    let _ = write_u32(guest, MODULE_HEAD_ADDRESS, self.head);
                    allocator.free(image.address).map_err(BiosError::Kernel)?;
                    allocator
                        .free(metadata.address)
                        .map_err(BiosError::Kernel)?;
                    return Err(error.into());
                }
            }
        }
        let record = ModuleRecord {
            id: module_id,
            name: description.name.clone(),
            version: description.version,
            state: ModuleState::Loaded,
            info_address: metadata.address,
            info,
            metadata_allocation: metadata,
            image_allocation: image,
            imports: loaded.imports,
            export_ids,
        };
        if let Err(error) = self.modules.insert_at(module_id, record) {
            self.libraries
                .iter()
                .filter_map(|(id, library)| (library.module_id == module_id).then_some(id))
                .collect::<Vec<_>>()
                .into_iter()
                .for_each(|id| {
                    let _ = self.libraries.remove(id);
                });
            let _ = write_u32(guest, MODULE_HEAD_ADDRESS, self.head);
            allocator.free(image.address).map_err(BiosError::Kernel)?;
            allocator
                .free(metadata.address)
                .map_err(BiosError::Kernel)?;
            return Err(error.into());
        }
        self.head = metadata.address;
        Ok(module_id)
    }

    /// Begins a module start entry and updates its guest-visible state.
    ///
    /// # Errors
    ///
    /// Returns a lifecycle or guest-memory diagnostic.
    pub fn begin_start<M: GuestMemory>(
        &mut self,
        id: u32,
        guest: &mut M,
    ) -> Result<ModuleInvocation, BiosError> {
        let module = self.modules.get(id).ok_or(KernelError::UnknownModule)?;
        match module.state {
            ModuleState::Loaded | ModuleState::Stopped | ModuleState::StoppedRemovable => {}
            ModuleState::NotResident => return Err(KernelError::NotRemovable.into()),
            ModuleState::Starting
            | ModuleState::Resident
            | ModuleState::Removable
            | ModuleState::Stopping => return Err(KernelError::AlreadyStarted.into()),
        }
        let invocation = ModuleInvocation {
            module_id: id,
            entry: module.info.entry,
            global_pointer: module.info.global_pointer,
        };
        self.set_state(id, ModuleState::Starting, guest)?;
        Ok(invocation)
    }

    /// Completes a module entry with its resident return code.
    ///
    /// # Errors
    ///
    /// Returns a lifecycle, resident-code, or guest-memory diagnostic.
    pub fn complete_start<M: GuestMemory>(
        &mut self,
        id: u32,
        result: ResidentState,
        guest: &mut M,
    ) -> Result<ModuleState, BiosError> {
        let module = self.modules.get(id).ok_or(KernelError::UnknownModule)?;
        if module.state != ModuleState::Starting {
            return Err(KernelError::NotStarted.into());
        }
        let state = match result {
            ResidentState::Resident => ModuleState::Resident,
            ResidentState::NotResident => ModuleState::NotResident,
            ResidentState::Removable => ModuleState::Removable,
            ResidentState::Unstarted => {
                return Err(BiosError::InvalidState(
                    "module returned no resident result",
                ));
            }
        };
        self.set_state(id, state, guest)?;
        Ok(state)
    }

    /// Begins the stop entry for a removable module.
    ///
    /// # Errors
    ///
    /// Returns a lifecycle or guest-memory diagnostic.
    pub fn begin_stop<M: GuestMemory>(
        &mut self,
        id: u32,
        guest: &mut M,
    ) -> Result<ModuleInvocation, BiosError> {
        let module = self.modules.get(id).ok_or(KernelError::UnknownModule)?;
        match module.state {
            ModuleState::Removable => {}
            ModuleState::Resident => return Err(KernelError::NotRemovable.into()),
            ModuleState::Loaded | ModuleState::NotResident => {
                return Err(KernelError::NotStarted.into());
            }
            ModuleState::Starting => return Err(KernelError::CannotStop.into()),
            ModuleState::Stopping => return Err(KernelError::AlreadyStopping.into()),
            ModuleState::Stopped | ModuleState::StoppedRemovable => {
                return Err(KernelError::AlreadyStopped.into());
            }
        }
        let invocation = ModuleInvocation {
            module_id: id,
            entry: module.info.entry,
            global_pointer: module.info.global_pointer,
        };
        self.set_state(id, ModuleState::Stopping, guest)?;
        Ok(invocation)
    }

    /// Completes a stop entry. A nonzero result restores removable residency.
    ///
    /// # Errors
    ///
    /// Returns a lifecycle or guest-memory diagnostic.
    pub fn complete_stop<M: GuestMemory>(
        &mut self,
        id: u32,
        result: i32,
        guest: &mut M,
    ) -> Result<ModuleState, BiosError> {
        let module = self.modules.get(id).ok_or(KernelError::UnknownModule)?;
        if module.state != ModuleState::Stopping {
            return Err(KernelError::CannotStop.into());
        }
        let state = if result == 0 {
            ModuleState::StoppedRemovable
        } else {
            ModuleState::Removable
        };
        self.set_state(id, state, guest)?;
        Ok(state)
    }

    /// Unlinks a load-only, no-resident, or stopped-removable module and frees
    /// both of its allocations.
    ///
    /// # Errors
    ///
    /// Returns a lifecycle, in-use-library, guest-memory, or allocator error.
    pub fn unload<M: GuestMemory>(
        &mut self,
        id: u32,
        allocator: &mut SystemMemory,
        guest: &mut M,
    ) -> Result<ModuleRecord, BiosError> {
        let module = self.modules.get(id).ok_or(KernelError::UnknownModule)?;
        match module.state {
            ModuleState::Loaded | ModuleState::NotResident | ModuleState::StoppedRemovable => {}
            ModuleState::Stopped => return Err(KernelError::NotRemovable.into()),
            _ => return Err(KernelError::NotStopped.into()),
        }
        if module.export_ids.iter().any(|id| {
            self.libraries
                .get(*id)
                .is_some_and(|library| library.users != 0)
        }) {
            return Err(KernelError::LibraryInUse.into());
        }
        if allocator
            .block(module.metadata_allocation.address)
            .is_none()
            || allocator.block(module.image_allocation.address).is_none()
        {
            return Err(BiosError::InvalidState("module allocation is missing"));
        }

        let removed_info = module.info_address;
        let next = module.info.next;
        let predecessor = self.modules.iter().find_map(|(other_id, other)| {
            (other_id != id && other.info.next == removed_info)
                .then_some((other_id, other.info_address))
        });
        match predecessor {
            Some((_, address)) => write_u32(guest, address, next)?,
            None if self.head == removed_info => write_u32(guest, MODULE_HEAD_ADDRESS, next)?,
            None => return Err(BiosError::InvalidState("module is absent from guest list")),
        }

        if let Some((predecessor_id, _)) = predecessor {
            let predecessor = self
                .modules
                .get_mut(predecessor_id)
                .ok_or(BiosError::InvalidState("module predecessor disappeared"))?;
            predecessor.info.next = next;
        } else {
            self.head = next;
        }
        let removed = self
            .modules
            .remove(id)
            .ok_or(BiosError::InvalidState("module disappeared during unload"))?;
        for export_id in &removed.export_ids {
            let _ = self.libraries.remove(*export_id);
        }
        allocator.free(removed.image_allocation.address)?;
        allocator.free(removed.metadata_allocation.address)?;
        Ok(removed)
    }

    /// Registers a validated export library.
    ///
    /// # Errors
    ///
    /// Returns a BIOS-compatible duplicate, format, or capacity error.
    pub fn register_export(
        &mut self,
        module_id: u32,
        library: ExportLibrary,
    ) -> Result<u32, KernelError> {
        self.validate_exports(std::slice::from_ref(&library))?;
        self.insert_export(module_id, library)
    }

    /// Releases an export library that has no acquired users.
    ///
    /// # Errors
    ///
    /// Returns a BIOS-compatible unknown or in-use error.
    pub fn release_export(&mut self, id: u32) -> Result<ExportRegistration, KernelError> {
        let library = self.libraries.get(id).ok_or(KernelError::LibraryNotFound)?;
        if library.users != 0 {
            return Err(KernelError::LibraryInUse);
        }
        self.libraries
            .remove(id)
            .ok_or(KernelError::LibraryNotFound)
    }

    /// Resolves and acquires a compatible named export ordinal.
    ///
    /// # Errors
    ///
    /// Returns a BIOS-compatible library or ordinal error.
    pub fn bind_import(
        &mut self,
        name: &str,
        required_version: u16,
        ordinal: u16,
    ) -> Result<ResolvedImport, KernelError> {
        validate_library_name(name)?;
        let (id, library) = self
            .libraries
            .iter_mut()
            .find(|(_, library)| {
                library.name == name && version_compatible(library.version, required_version)
            })
            .ok_or(KernelError::LibraryNotFound)?;
        let address = library
            .functions
            .get(usize::from(ordinal))
            .copied()
            .filter(|address| *address != 0)
            .ok_or(KernelError::IllegalLibrary)?;
        library.users = library.users.checked_add(1).ok_or(KernelError::NoMemory)?;
        Ok(ResolvedImport {
            library_id: id,
            address,
        })
    }

    /// Releases one acquired import binding.
    ///
    /// # Errors
    ///
    /// Returns a BIOS-compatible unknown or invalid-use error.
    pub fn unbind_import(&mut self, library_id: u32) -> Result<(), KernelError> {
        let library = self
            .libraries
            .get_mut(library_id)
            .ok_or(KernelError::LibraryNotFound)?;
        library.users = library
            .users
            .checked_sub(1)
            .ok_or(KernelError::IllegalLibrary)?;
        Ok(())
    }

    /// Returns a registered export library.
    #[must_use]
    pub fn export(&self, id: u32) -> Option<&ExportRegistration> {
        self.libraries.get(id)
    }

    fn set_state<M: GuestMemory>(
        &mut self,
        id: u32,
        state: ModuleState,
        guest: &mut M,
    ) -> Result<(), BiosError> {
        let module = self.modules.get(id).ok_or(KernelError::UnknownModule)?;
        write_u16(guest, module.info_address + 10, state.new_flags())?;
        let module = self.modules.get_mut(id).ok_or(BiosError::InvalidState(
            "module disappeared during state change",
        ))?;
        module.state = state;
        module.info.new_flags = state.new_flags();
        Ok(())
    }

    fn validate_exports(&self, exports: &[ExportLibrary]) -> Result<(), KernelError> {
        if exports.len() > self.libraries.capacity() - self.libraries.len() {
            return Err(KernelError::NoMemory);
        }
        for (index, export) in exports.iter().enumerate() {
            validate_library_name(&export.name)?;
            if export.functions.is_empty() {
                return Err(KernelError::IllegalLibrary);
            }
            if self
                .libraries
                .iter()
                .any(|(_, library)| library.name == export.name)
                || exports[..index]
                    .iter()
                    .any(|earlier| earlier.name == export.name)
            {
                return Err(KernelError::LibraryFound);
            }
        }
        Ok(())
    }

    fn insert_export(
        &mut self,
        module_id: u32,
        library: ExportLibrary,
    ) -> Result<u32, KernelError> {
        self.libraries.insert(ExportRegistration {
            module_id,
            name: library.name,
            version: library.version,
            mode: library.mode,
            table_address: library.table_address,
            functions: library.functions,
            users: 0,
        })
    }
}

impl Default for ModuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

struct GuestTarget<'a, M> {
    guest: &'a mut M,
    range: MemoryRange,
}

impl<M: GuestMemory> TargetMemory for GuestTarget<'_, M> {
    fn range(&self) -> MemoryRange {
        self.range
    }

    fn write_image(&mut self, address: u32, image: &[u8]) -> Result<(), TargetError> {
        if !self.guest.range().contains(address, image.len()) {
            return Err(TargetError::new("IRX image lies outside guest RAM"));
        }
        self.guest
            .write(address, image)
            .map_err(|error| TargetError::new(error.to_string()))
    }
}

fn validate_module_name(name: &str) -> Result<(), KernelError> {
    if name.is_empty()
        || name.len() > 127
        || name
            .bytes()
            .any(|byte| byte == 0 || !byte.is_ascii_graphic())
    {
        return Err(KernelError::IllegalObject);
    }
    Ok(())
}

fn validate_library_name(name: &str) -> Result<(), KernelError> {
    if name.is_empty() || name.len() > 8 || !name.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(KernelError::IllegalLibrary);
    }
    Ok(())
}

const fn version_compatible(provided: u16, required: u16) -> bool {
    // Loadcore uses the minor version when replacing exports, but import
    // linking compares only the library name and major version.
    provided >> 8 == required >> 8
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GuestMemoryError, GuestRange};

    const PHOFF: usize = 52;
    const IOPMOD_OFFSET: usize = 0xa0;
    const IMAGE_OFFSET: usize = 0x100;
    const IMAGE_SIZE: usize = 0xc0;
    const MEMORY_SIZE: usize = 0xd0;
    const REL_OFFSET: usize = 0x1c0;
    const SHOFF: usize = 0x1c8;

    struct TestMemory(Vec<u8>);

    impl TestMemory {
        fn new() -> Self {
            Self(vec![0; 0x20_0000])
        }

        fn word(&self, address: u32) -> u32 {
            let offset = usize::try_from(address).unwrap();
            u32::from_le_bytes(self.0[offset..offset + 4].try_into().unwrap())
        }
    }

    impl GuestMemory for TestMemory {
        fn range(&self) -> GuestRange {
            GuestRange {
                start: 0,
                end: u32::try_from(self.0.len()).unwrap(),
            }
        }

        fn read(&self, address: u32, output: &mut [u8]) -> Result<(), GuestMemoryError> {
            let start = usize::try_from(address).unwrap();
            let end = start + output.len();
            output.copy_from_slice(
                self.0
                    .get(start..end)
                    .ok_or_else(|| GuestMemoryError::new("outside test RAM"))?,
            );
            Ok(())
        }

        fn write(&mut self, address: u32, input: &[u8]) -> Result<(), GuestMemoryError> {
            let start = usize::try_from(address).unwrap();
            let end = start + input.len();
            self.0
                .get_mut(start..end)
                .ok_or_else(|| GuestMemoryError::new("outside test RAM"))?
                .copy_from_slice(input);
            Ok(())
        }
    }

    #[test]
    fn module_info_layout_is_byte_exact() {
        let info = ModuleInfo {
            next: 0x0102_0304,
            name: 0x1112_1314,
            version: 0x2122,
            new_flags: 0x2324,
            id: 0x2526,
            flags: 0x2728,
            entry: 0x3132_3334,
            global_pointer: 0x4142_4344,
            text_start: 0x5152_5354,
            text_size: 0x6162_6364,
            data_size: 0x7172_7374,
            bss_size: 0x8182_8384,
            unused1: 0x9192_9394,
            unused2: 0xa1a2_a3a4,
        };
        assert_eq!(
            info.encode(),
            [
                4, 3, 2, 1, 20, 19, 18, 17, 34, 33, 36, 35, 38, 37, 40, 39, 52, 51, 50, 49, 68, 67,
                66, 65, 84, 83, 82, 81, 100, 99, 98, 97, 116, 115, 114, 113, 132, 131, 130, 129,
                148, 147, 146, 145, 164, 163, 162, 161,
            ]
        );
    }

    #[test]
    fn ps2sdk_irx_load_lookup_lifecycle_and_unload_do_not_leak() {
        let irx = IrxModule::parse("fixture.irx", &ps2sdk_irx_fixture()).unwrap();
        let mut guest = TestMemory::new();
        let mut allocator = SystemMemory::new(0x1_0000, 0x20_0000).unwrap();
        let initial_free = allocator.total_free();
        let mut modules = ModuleRegistry::new();
        let id = modules.load(&irx, &mut allocator, &mut guest).unwrap();
        let module = modules.get(id).unwrap();
        assert_eq!(module.name(), "fixture");
        assert_eq!(module.version(), 0x0102);
        assert_eq!(modules.find("fixture").unwrap().id(), id);
        assert_eq!(modules.containing(module.info.entry).unwrap().id(), id);
        assert_eq!(guest.word(MODULE_HEAD_ADDRESS), module.info_address());
        assert_eq!(guest.word(module.info_address() + 16), module.info.entry);
        let info_address = module.info_address();
        let entry = module.info.entry;

        let invocation = modules.begin_start(id, &mut guest).unwrap();
        assert_eq!(invocation.entry, entry);
        assert_eq!(
            modules
                .complete_start(id, ResidentState::Removable, &mut guest)
                .unwrap(),
            ModuleState::Removable
        );
        modules.begin_stop(id, &mut guest).unwrap();
        modules.complete_stop(id, 0, &mut guest).unwrap();
        let removed = modules.unload(id, &mut allocator, &mut guest).unwrap();
        assert_eq!(removed.info_address(), info_address);
        assert!(modules.is_empty());
        assert_eq!(guest.word(MODULE_HEAD_ADDRESS), 0);
        assert_eq!(allocator.total_free(), initial_free);
    }

    #[test]
    fn export_versions_ordinals_and_users_are_checked() {
        let mut modules = ModuleRegistry::new();
        let id = modules
            .register_export(
                0,
                ExportLibrary {
                    name: "sysclib".to_owned(),
                    version: 0x0103,
                    mode: 0,
                    table_address: 0x1000,
                    functions: vec![0x2000, 0x2010],
                },
            )
            .unwrap();
        assert_eq!(
            modules.bind_import("sysclib", 0x0104, 1).unwrap(),
            ResolvedImport {
                library_id: id,
                address: 0x2010
            }
        );
        assert_eq!(
            modules.bind_import("sysclib", 0x0200, 1),
            Err(KernelError::LibraryNotFound)
        );
        assert_eq!(modules.release_export(id), Err(KernelError::LibraryInUse));
        modules.unbind_import(id).unwrap();
        assert_eq!(modules.release_export(id).unwrap().name, "sysclib");
        assert_eq!(
            modules.bind_import("sysclib", 0x0102, 0),
            Err(KernelError::LibraryNotFound)
        );
    }

    #[test]
    fn invalid_module_arguments_leave_allocator_unchanged() {
        let mut guest = TestMemory::new();
        let allocator = SystemMemory::new(0x1_0000, 0x20_0000).unwrap();
        let initial = allocator.total_free();
        let mut modules = ModuleRegistry::new();
        assert_eq!(
            modules.begin_start(99, &mut guest),
            Err(KernelError::UnknownModule.into())
        );
        assert_eq!(
            modules.register_export(
                0,
                ExportLibrary {
                    name: "too-long-name".to_owned(),
                    version: 0x0100,
                    mode: 0,
                    table_address: 0,
                    functions: vec![1],
                }
            ),
            Err(KernelError::IllegalLibrary)
        );
        assert_eq!(allocator.total_free(), initial);
    }

    fn ps2sdk_irx_fixture() -> Vec<u8> {
        let mut elf = vec![0_u8; SHOFF + 3 * 40];
        elf[..16].copy_from_slice(b"\x7fELF\x01\x01\x01\0\0\0\0\0\0\0\0\0");
        put_u16(&mut elf, 16, 0xff81);
        put_u16(&mut elf, 18, 8);
        put_u32(&mut elf, 20, 1);
        put_u32(&mut elf, 24, 0);
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
        put_u32(&mut elf, IOPMOD_OFFSET + 4, 0);
        put_u32(&mut elf, IOPMOD_OFFSET + 8, 0x30);
        put_u32(&mut elf, IOPMOD_OFFSET + 12, 0xb0);
        put_u32(&mut elf, IOPMOD_OFFSET + 16, 0x10);
        put_u32(&mut elf, IOPMOD_OFFSET + 20, 0x10);
        put_u16(&mut elf, IOPMOD_OFFSET + 24, 0x0102);

        put_u32(&mut elf, IMAGE_OFFSET + 0x40, 0x48);
        put_u16(&mut elf, IMAGE_OFFSET + 0x44, 0x0102);
        elf[IMAGE_OFFSET + 0x48..IMAGE_OFFSET + 0x50].copy_from_slice(b"fixture\0");
        let import = IMAGE_OFFSET + 0x60;
        put_u32(&mut elf, import, 0x41e0_0000);
        put_u16(&mut elf, import + 8, 0x0103);
        elf[import + 12..import + 20].copy_from_slice(b"sysclib\0");
        put_u32(&mut elf, import + 20, 0x03e0_0008);
        put_u32(&mut elf, import + 24, 0x2400_000c);
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
        let at = PHOFF + index * 32;
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
        let at = SHOFF + index * 40;
        put_u32(elf, at + 4, kind);
        put_u32(elf, at + 8, 6);
        put_u32(elf, at + 16, u32::try_from(offset).unwrap());
        put_u32(elf, at + 20, u32::try_from(size).unwrap());
        put_u32(elf, at + 28, info);
        put_u32(elf, at + 32, alignment);
        put_u32(elf, at + 36, entry_size);
    }
}
