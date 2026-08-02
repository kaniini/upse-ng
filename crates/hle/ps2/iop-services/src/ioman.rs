// SPDX-License-Identifier: LGPL-2.1-or-later

use crate::{ServiceError, ServiceMemory};

const FILE_CAPACITY: usize = 16;
const FIRST_FILE_DESCRIPTOR: usize = 3;
const MAX_IO_BYTES: usize = 16 * 1024 * 1024;
const FIO_O_RDONLY: u32 = 1;
const WRITE_FLAGS: u32 = 0x0f02;
const EBADF: i32 = 9;
const EACCES: i32 = 13;
const EINVAL: i32 = 22;
const EMFILE: i32 = 24;
const EROFS: i32 = 30;

/// Immutable file source used by the read-only IOP file manager.
pub trait ReadOnlyFileSystem {
    /// Returns complete immutable file contents.
    ///
    /// # Errors
    ///
    /// Returns a stable diagnostic for an invalid or missing path.
    fn file(&self, path: &str) -> Result<&[u8], String>;
}

impl ReadOnlyFileSystem for upse_psf2_vfs::Psf2Vfs {
    fn file(&self, path: &str) -> Result<&[u8], String> {
        self.file(path).map_err(|error| error.to_string())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OpenFile {
    path: String,
    offset: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IoManager {
    files: [Option<OpenFile>; FILE_CAPACITY],
}

impl IoManager {
    pub(crate) fn new() -> Self {
        Self {
            files: std::array::from_fn(|_| None),
        }
    }

    pub(crate) fn reset(&mut self) {
        self.files = std::array::from_fn(|_| None);
    }

    pub(crate) fn open<F: ReadOnlyFileSystem>(&mut self, fs: &F, path: &str, flags: u32) -> i32 {
        if flags & WRITE_FLAGS != 0 || flags & 3 != FIO_O_RDONLY {
            return -EACCES;
        }
        let path = normalize_device_path(path);
        if fs.file(&path).is_err() {
            return -2;
        }
        let Some(index) = self.files[FIRST_FILE_DESCRIPTOR..]
            .iter()
            .position(Option::is_none)
            .map(|index| index + FIRST_FILE_DESCRIPTOR)
        else {
            return -EMFILE;
        };
        self.files[index] = Some(OpenFile { path, offset: 0 });
        i32::try_from(index).unwrap_or(-EMFILE)
    }

    pub(crate) fn close(&mut self, fd: u32) -> i32 {
        let Ok(index) = usize::try_from(fd) else {
            return -EBADF;
        };
        let Some(slot) = self.files.get_mut(index) else {
            return -EBADF;
        };
        if slot.take().is_none() {
            return -EBADF;
        }
        0
    }

    pub(crate) fn read<F: ReadOnlyFileSystem, M: ServiceMemory>(
        &mut self,
        fs: &F,
        fd: u32,
        address: u32,
        size: u32,
        memory: &mut M,
    ) -> Result<i32, ServiceError> {
        let size = usize::try_from(size).map_err(|_| ServiceError::InvalidArgument {
            operation: "ioman read",
            detail: "size exceeds host width",
        })?;
        if size > MAX_IO_BYTES {
            return Err(ServiceError::ResourceLimit("single ioman read"));
        }
        let Ok(index) = usize::try_from(fd) else {
            return Ok(-EBADF);
        };
        let Some(file) = self.files.get_mut(index).and_then(Option::as_mut) else {
            return Ok(-EBADF);
        };
        let data = fs.file(&file.path).map_err(ServiceError::Vfs)?;
        let remaining = data.get(file.offset..).unwrap_or_default();
        let count = remaining.len().min(size);
        write_memory(memory, address, &remaining[..count])?;
        file.offset += count;
        Ok(i32::try_from(count).unwrap_or(i32::MAX))
    }

    pub(crate) fn seek<F: ReadOnlyFileSystem>(
        &mut self,
        fs: &F,
        fd: u32,
        offset: i32,
        whence: u32,
    ) -> i32 {
        let Ok(index) = usize::try_from(fd) else {
            return -EBADF;
        };
        let Some(file) = self.files.get_mut(index).and_then(Option::as_mut) else {
            return -EBADF;
        };
        let Ok(data) = fs.file(&file.path) else {
            return -EBADF;
        };
        let base = match whence {
            0 => 0_i64,
            1 => i64::try_from(file.offset).unwrap_or(i64::MAX),
            2 => i64::try_from(data.len()).unwrap_or(i64::MAX),
            _ => return -EINVAL,
        };
        let position = base.saturating_add(i64::from(offset));
        let Ok(position) = usize::try_from(position) else {
            return -EINVAL;
        };
        file.offset = position;
        i32::try_from(position).unwrap_or(i32::MAX)
    }

    pub(crate) fn getstat<F: ReadOnlyFileSystem, M: ServiceMemory>(
        fs: &F,
        path: &str,
        address: u32,
        memory: &mut M,
    ) -> Result<i32, ServiceError> {
        let path = normalize_device_path(path);
        let Ok(data) = fs.file(&path) else {
            return Ok(-2);
        };
        let mut stat = [0_u8; 40];
        stat[..4].copy_from_slice(&0x0124_u32.to_le_bytes());
        stat[8..12].copy_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_le_bytes());
        write_memory(memory, address, &stat)?;
        Ok(0)
    }

    pub(crate) const fn write_error() -> i32 {
        -EROFS
    }
}

fn normalize_device_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path
        .split_once(':')
        .map_or(path.as_str(), |(_, remainder)| remainder);
    path.trim_start_matches('/').to_owned()
}

fn write_memory<M: ServiceMemory>(
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
