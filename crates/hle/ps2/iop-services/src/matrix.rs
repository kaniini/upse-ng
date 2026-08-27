// SPDX-License-Identifier: LGPL-2.1-or-later

/// IOP service family selected by an import library name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceFamily {
    /// System-memory manager.
    SystemMemory,
    /// Loadcore library/link manager.
    LoadCore,
    /// Module loader and lifecycle manager.
    ModuleLoader,
    /// Exception handler manager.
    Exception,
    /// Interrupt handler manager.
    Interrupt,
    /// IOP DMA manager.
    Dma,
    /// Base thread and alarm services.
    Thread,
    /// Semaphore services.
    Semaphore,
    /// Event-flag services.
    EventFlag,
    /// Message-box services.
    MessageBox,
    /// Fixed memory pools.
    FixedPool,
    /// Variable memory pools.
    VariablePool,
    /// Heap library.
    Heap,
    /// Hardware timers.
    Timer,
    /// SPU2 sound-driver services.
    Sound,
    /// Vertical-blank services.
    VBlank,
    /// C runtime helpers.
    Sysclib,
    /// TTY formatting and character output.
    Stdio,
    /// Read-only I/O manager.
    Ioman,
    /// SIF DMA surface.
    SifManager,
    /// SIF command-register surface.
    SifCommand,
    /// Subsystem-bus timing registers.
    Ssbus,
}

/// Implementation disposition for one known import.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupportLevel {
    /// Implemented entirely inside `upse-iop-services`.
    Local,
    /// Routed through the machine/kernel adapter.
    Backend,
    /// Documented return-only entry with no observable side effect.
    ReturnOnly,
    /// Known API intentionally unavailable to IOP-only PSF2 playback.
    Unsupported,
}

/// Resolved service-matrix entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceDescription {
    /// Selected family.
    pub family: ServiceFamily,
    /// Highest compatible packed version provided by the HLE profile.
    pub provided_version: u16,
    /// Exported symbol name.
    pub symbol: &'static str,
    /// Implementation disposition.
    pub support: SupportLevel,
}

#[derive(Clone, Copy)]
struct Library {
    family: ServiceFamily,
    version: u16,
    highest_ordinal: u16,
}

/// Looks up a known library and ordinal.
///
/// The library versions and ordinal names are independently transcribed from
/// the ps2dev PS2SDK import/export headers and tables (AFL-2.0). No service
/// implementation is derived from PS2SDK source.
#[must_use]
pub fn describe_import(library: &str, ordinal: u16) -> Option<ServiceDescription> {
    let library = library_description(library)?;
    if ordinal > library.highest_ordinal {
        return None;
    }
    let (symbol, support) = symbol(library.family, ordinal);
    Some(ServiceDescription {
        family: library.family,
        provided_version: library.version,
        symbol,
        support,
    })
}

fn library_description(name: &str) -> Option<Library> {
    let (family, version, highest_ordinal) = match name {
        "sysmem" => (ServiceFamily::SystemMemory, 0x0102, 15),
        "loadcore" => (ServiceFamily::LoadCore, 0x0103, 27),
        "modload" => (ServiceFamily::ModuleLoader, 0x0107, 31),
        "excepman" => (ServiceFamily::Exception, 0x0101, 8),
        "intrman" => (ServiceFamily::Interrupt, 0x0102, 31),
        "dmacman" => (ServiceFamily::Dma, 0x0102, 35),
        "thbase" => (ServiceFamily::Thread, 0x0102, 52),
        "thsemap" => (ServiceFamily::Semaphore, 0x0101, 12),
        "thevent" => (ServiceFamily::EventFlag, 0x0101, 14),
        "thmsgbx" => (ServiceFamily::MessageBox, 0x0101, 12),
        "thfpool" => (ServiceFamily::FixedPool, 0x0101, 12),
        "thvpool" => (ServiceFamily::VariablePool, 0x0101, 12),
        "heaplib" => (ServiceFamily::Heap, 0x0101, 17),
        "timrman" => (ServiceFamily::Timer, 0x0103, 28),
        "libsd" => (ServiceFamily::Sound, 0x0105, 33),
        "vblank" => (ServiceFamily::VBlank, 0x0101, 9),
        "sysclib" => (ServiceFamily::Sysclib, 0x0104, 44),
        "stdio" => (ServiceFamily::Stdio, 0x0103, 14),
        "ioman" => (ServiceFamily::Ioman, 0x0104, 24),
        "sifman" => (ServiceFamily::SifManager, 0x0101, 36),
        "sifcmd" => (ServiceFamily::SifCommand, 0x0101, 31),
        "ssbusc" => (ServiceFamily::Ssbus, 0x0101, 17),
        _ => return None,
    };
    Some(Library {
        family,
        version,
        highest_ordinal,
    })
}

#[allow(clippy::too_many_lines)]
fn symbol(family: ServiceFamily, ordinal: u16) -> (&'static str, SupportLevel) {
    use ServiceFamily as Family;
    use SupportLevel::{Backend, Local, ReturnOnly, Unsupported};

    if ordinal <= 2 {
        return ("return_only", ReturnOnly);
    }
    match (family, ordinal) {
        (Family::SystemMemory, 3) => ("GetSysmemInternalData", Backend),
        (Family::SystemMemory, 4) => ("AllocSysMemory", Backend),
        (Family::SystemMemory, 5) => ("FreeSysMemory", Backend),
        (Family::SystemMemory, 6) => ("QueryMemSize", Backend),
        (Family::SystemMemory, 7) => ("QueryMaxFreeMemSize", Backend),
        (Family::SystemMemory, 8) => ("QueryTotalFreeMemSize", Backend),
        (Family::SystemMemory, 9) => ("QueryBlockTopAddress", Backend),
        (Family::SystemMemory, 10) => ("QueryBlockSize", Backend),
        (Family::SystemMemory, 14) => ("Kprintf", Local),
        (Family::SystemMemory, 15) => ("KprintfSet", Unsupported),

        (Family::LoadCore, 3) => ("GetLoadcoreInternalData", Backend),
        (Family::LoadCore, 4 | 5) => ("FlushCache", ReturnOnly),
        (Family::LoadCore, 6) => ("RegisterLibraryEntries", Backend),
        (Family::LoadCore, 7) => ("ReleaseLibraryEntries", Backend),
        (Family::LoadCore, 8) => ("LinkLibraryEntries", Backend),
        (Family::LoadCore, 9) => ("UnLinkLibraryEntries", Backend),
        (Family::LoadCore, 10) => ("RegisterNonAutoLinkEntries", Backend),
        (Family::LoadCore, 11) => ("QueryLibraryEntryTable", Backend),
        (Family::LoadCore, 12 | 13) => ("BootMode", Backend),
        (Family::LoadCore, 14 | 15) => ("LibraryClientLock", Backend),
        (Family::LoadCore, 16 | 17) => ("ModuleRegistration", Backend),
        (Family::LoadCore, 20..=27) => ("ExtendedLoadcore", Backend),

        (Family::ModuleLoader, 3) => ("GetModloadInternalData", Backend),
        (Family::ModuleLoader, 5) => ("LoadModuleAddress", Backend),
        (Family::ModuleLoader, 6) => ("LoadModule", Local),
        (Family::ModuleLoader, 7) => ("LoadStartModule", Local),
        (Family::ModuleLoader, 8) => ("StartModule", Backend),
        (Family::ModuleLoader, 9 | 10) => ("LoadModuleBuffer", Backend),
        (Family::ModuleLoader, 16..=23 | 26..=30) => ("ModuleRuntime", Backend),

        (Family::Exception, 3..=8) => ("ExceptionManager", Backend),
        (Family::Interrupt, 3..=31) => ("InterruptManager", Backend),
        (Family::Dma, 3..=35) => ("DmaManager", Backend),
        (Family::Thread, 3..=38 | 41..=48) => ("ThreadManager", Backend),
        (Family::Thread, 39) => ("USec2SysClock", Local),
        (Family::Thread, 40) => ("SysClock2USec", Local),
        (Family::Semaphore, 3..=12) => ("Semaphore", Backend),
        (Family::EventFlag, 3..=14) => ("EventFlag", Backend),
        (Family::MessageBox, 3..=12) => ("MessageBox", Backend),
        (Family::FixedPool, 3..=12) => ("FixedPool", Backend),
        (Family::VariablePool, 3..=12) => ("VariablePool", Backend),
        (Family::Heap, 3..=15) => ("Heap", Backend),
        (Family::Timer, 3..=24) => ("TimerManager", Backend),
        (Family::Sound, 3..=33) => ("SoundDriver", Backend),
        (Family::VBlank, 3..=9) => ("VBlank", Backend),

        (Family::Sysclib, 18) => ("prnt", Unsupported),
        (Family::Sysclib, 4..=44) => (sysclib_symbol(ordinal), Local),
        (Family::Stdio, 4..=14) => (stdio_symbol(ordinal), Local),
        (Family::Ioman, 4..=8 | 16) => (ioman_symbol(ordinal), Local),
        (Family::Ioman, 20 | 21) => (ioman_symbol(ordinal), ReturnOnly),

        (Family::SifManager, 4..=8 | 21..=29) => ("SifState", Local),
        (Family::SifManager, 9..=20 | 30..=33) => ("SifEeTransfer", Unsupported),
        (Family::SifCommand, 4..=9) => ("SifCommandState", Local),
        (Family::SifCommand, 10..=29) => ("SifEeCommand", Unsupported),
        (Family::Ssbus, 4..=17) => ("SsbusRegister", Local),
        _ => ("reserved", Unsupported),
    }
}

fn sysclib_symbol(ordinal: u16) -> &'static str {
    const NAMES: [&str; 42] = [
        "setjmp",
        "longjmp",
        "toupper",
        "tolower",
        "look_ctype_table",
        "get_ctype_table",
        "memchr",
        "memcmp",
        "memcpy",
        "memmove",
        "memset",
        "bcmp",
        "bcopy",
        "bzero",
        "prnt",
        "sprintf",
        "strcat",
        "strchr",
        "strcmp",
        "strcpy",
        "strcspn",
        "index",
        "rindex",
        "strlen",
        "strncat",
        "strncmp",
        "strncpy",
        "strpbrk",
        "strrchr",
        "strspn",
        "strstr",
        "strtok",
        "strtol",
        "atob",
        "strtoul",
        "return_only",
        "wmemcopy",
        "wmemset",
        "vsprintf",
        "strtok_r",
        "negative_one",
        "reserved",
    ];
    NAMES
        .get(usize::from(ordinal.saturating_sub(4)))
        .copied()
        .unwrap_or("reserved")
}

fn stdio_symbol(ordinal: u16) -> &'static str {
    const NAMES: [&str; 11] = [
        "printf",
        "getchar",
        "putchar",
        "puts",
        "gets",
        "fdprintf",
        "fdgetc",
        "fdputc",
        "fdputs",
        "fdgets",
        "vfdprintf",
    ];
    NAMES
        .get(usize::from(ordinal.saturating_sub(4)))
        .copied()
        .unwrap_or("reserved")
}

fn ioman_symbol(ordinal: u16) -> &'static str {
    match ordinal {
        4 => "open",
        5 => "close",
        6 => "read",
        7 => "write",
        8 => "lseek",
        16 => "getstat",
        20 => "AddDrv",
        21 => "DelDrv",
        _ => "reserved",
    }
}

pub(crate) const fn version_compatible(provided: u16, required: u16) -> bool {
    // Loadcore uses the minor version when replacing exports, but import
    // linking compares only the library name and major version.
    provided >> 8 == required >> 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_versions_and_unknown_ordinals_are_explicit() {
        let thread = describe_import("thbase", 33).unwrap();
        assert_eq!(thread.family, ServiceFamily::Thread);
        assert_eq!(thread.provided_version, 0x0102);
        assert_eq!(thread.support, SupportLevel::Backend);
        assert_eq!(describe_import("ioman", 25), None);
        assert_eq!(describe_import("hostfs", 4), None);
        assert!(version_compatible(0x0103, 0x0101));
        assert!(version_compatible(0x0101, 0x0103));
        assert!(!version_compatible(0x0201, 0x0101));
    }
}
