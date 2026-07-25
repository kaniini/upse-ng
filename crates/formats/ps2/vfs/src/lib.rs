// SPDX-License-Identifier: LGPL-2.1-or-later
//! Safe, bounded PSF2 virtual filesystem parsing and overlay assembly.
//!
//! The resulting filesystem owns all file data and has no access to the host
//! filesystem. Construction validates and decompresses every reachable entry,
//! so later IOP-facing reads cannot encounter malformed container data.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    sync::Arc,
};

use flate2::read::ZlibDecoder;
use thiserror::Error;
use upse_psf::{Psf2LoadPlan, PsfVersion};

const NAME_BYTES: usize = 36;
const ENTRY_BYTES: usize = 48;
const SPEC_MAX_PATH_BYTES: usize = 255;

/// Resource bounds applied while assembling all PSF2 filesystem layers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsLimits {
    /// Maximum directory nesting below the root.
    pub max_depth: usize,
    /// Maximum number of parsed directory entries across all layers.
    pub max_entries: usize,
    /// Maximum normalized path length, additionally capped by the format at 255 bytes.
    pub max_path_bytes: usize,
    /// Maximum compressed blocks across all files and layers.
    pub max_blocks: usize,
    /// Maximum uncompressed size of one file.
    pub max_file_bytes: usize,
    /// Maximum uncompressed size of one block.
    pub max_block_bytes: usize,
    /// Maximum aggregate declared file bytes across all layers.
    pub max_aggregate_bytes: usize,
}

impl Default for VfsLimits {
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_entries: 65_536,
            max_path_bytes: SPEC_MAX_PATH_BYTES,
            max_blocks: 262_144,
            max_file_bytes: 64 * 1024 * 1024,
            max_block_bytes: 1024 * 1024,
            max_aggregate_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Specific reason a PSF2 filesystem could not be constructed or queried.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum VfsErrorKind {
    /// A load-plan layer was not PSF2.
    #[error("filesystem layer is not PSF2")]
    WrongVersion,
    /// A directory, entry, size table, or compressed block was truncated.
    #[error("truncated filesystem data")]
    Truncated,
    /// An entry name did not follow the PSF2 filename rules.
    #[error("invalid entry name")]
    InvalidName,
    /// An entry's offset and size fields did not describe a valid node.
    #[error("invalid directory entry")]
    InvalidEntry,
    /// A child offset did not follow its directory entry.
    #[error("child offset does not follow its directory entry")]
    OffsetOrder,
    /// Integer offset arithmetic overflowed.
    #[error("filesystem offset overflow")]
    OffsetOverflow,
    /// A configured resource limit was exceeded.
    #[error("resource limit exceeded: {0}")]
    LimitExceeded(&'static str),
    /// A compressed file block was not a complete zlib stream.
    #[error("invalid zlib block: {0}")]
    InvalidZlib(String),
    /// A block did not expand to its declared length.
    #[error("block expands to {actual} bytes, expected {expected}")]
    BlockSizeMismatch {
        /// Required uncompressed block length.
        expected: usize,
        /// Observed uncompressed block length.
        actual: usize,
    },
    /// A lookup path was malformed or exceeded the format limit.
    #[error("invalid lookup path")]
    InvalidPath,
    /// A requested file does not exist.
    #[error("file not found")]
    NotFound,
    /// A requested file path names a directory.
    #[error("path is a directory")]
    IsDirectory,
}

/// Structured PSF2 filesystem diagnostic.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{origin}:{offset}: {path}: {kind}")]
pub struct VfsError {
    /// Logical PSF2 layer origin or `lookup` for a query error.
    pub origin: String,
    /// Byte offset in the layer's reserved section.
    pub offset: usize,
    /// Normalized path being parsed or queried.
    pub path: String,
    /// Specific failure.
    pub kind: VfsErrorKind,
}

impl VfsError {
    fn layer(origin: &str, offset: usize, path: &str, kind: VfsErrorKind) -> Self {
        Self {
            origin: origin.to_owned(),
            offset,
            path: display_path(path),
            kind,
        }
    }

    fn lookup(path: &str, kind: VfsErrorKind) -> Self {
        Self {
            origin: "lookup".to_owned(),
            offset: 0,
            path: path.to_owned(),
            kind,
        }
    }
}

/// Kind of an immutable filesystem node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    /// Directory node.
    Directory,
    /// Regular file node.
    File,
}

/// An immutable, fully validated PSF2 virtual filesystem.
#[derive(Clone, Debug, Default)]
pub struct Psf2Vfs {
    files: BTreeMap<String, Arc<[u8]>>,
    directories: BTreeSet<String>,
}

impl Psf2Vfs {
    /// Builds the exact filesystem map from a resolved PSF2 load plan.
    ///
    /// Layers are applied in plan order. A later entry replaces an earlier
    /// case-insensitive name; directories from multiple layers are merged,
    /// while file/directory type conflicts replace the earlier subtree.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError`] before publishing a filesystem if any reachable
    /// entry is malformed or a construction bound is exceeded.
    pub fn from_load_plan(plan: &Psf2LoadPlan, limits: VfsLimits) -> Result<Self, VfsError> {
        let mut builder = Builder::new(limits);
        for layer in &plan.layers {
            if layer.container.version() != PsfVersion::Psf2 {
                return Err(VfsError::layer(
                    &layer.origin,
                    0,
                    "/",
                    VfsErrorKind::WrongVersion,
                ));
            }
            let mut parsed = Layer::default();
            builder.parse_directory(
                &layer.origin,
                layer.container.reserved(),
                0,
                "",
                0,
                &mut parsed,
            )?;
            builder.apply(parsed);
        }
        Ok(Self {
            files: builder.files,
            directories: builder.directories,
        })
    }

    /// Returns the number of regular files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Reports whether no regular files are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Iterates over normalized file paths and complete immutable contents.
    #[must_use]
    pub fn files(&self) -> impl ExactSizeIterator<Item = (&str, &[u8])> {
        self.files
            .iter()
            .map(|(path, data)| (path.as_str(), data.as_ref()))
    }

    /// Iterates over normalized directory paths. The root is represented by an empty string.
    pub fn directories(&self) -> impl Iterator<Item = &str> {
        self.directories.iter().map(String::as_str)
    }

    /// Looks up a path without exposing mutable filesystem state.
    ///
    /// # Errors
    ///
    /// Returns [`VfsErrorKind::InvalidPath`] for a malformed path.
    pub fn node_kind(&self, path: &str) -> Result<Option<NodeKind>, VfsError> {
        let normalized = normalize_lookup(path, SPEC_MAX_PATH_BYTES)
            .map_err(|kind| VfsError::lookup(path, kind))?;
        if self.files.contains_key(&normalized) {
            Ok(Some(NodeKind::File))
        } else if self.directories.contains(&normalized) {
            Ok(Some(NodeKind::Directory))
        } else {
            Ok(None)
        }
    }

    /// Returns one complete immutable file.
    ///
    /// # Errors
    ///
    /// Returns a structured diagnostic for invalid, missing, or directory paths.
    pub fn file(&self, path: &str) -> Result<&[u8], VfsError> {
        let normalized = normalize_lookup(path, SPEC_MAX_PATH_BYTES)
            .map_err(|kind| VfsError::lookup(path, kind))?;
        if let Some(data) = self.files.get(&normalized) {
            return Ok(data);
        }
        let kind = if self.directories.contains(&normalized) {
            VfsErrorKind::IsDirectory
        } else {
            VfsErrorKind::NotFound
        };
        Err(VfsError::lookup(path, kind))
    }

    /// Copies a bounded range from a file, returning zero at end of file.
    ///
    /// # Errors
    ///
    /// Returns a structured diagnostic for invalid, missing, or directory paths.
    pub fn read(&self, path: &str, offset: usize, output: &mut [u8]) -> Result<usize, VfsError> {
        let data = self.file(path)?;
        let Some(remaining) = data.get(offset..) else {
            return Ok(0);
        };
        let count = remaining.len().min(output.len());
        output[..count].copy_from_slice(&remaining[..count]);
        Ok(count)
    }
}

#[derive(Default)]
struct Layer {
    files: BTreeMap<String, Arc<[u8]>>,
    directories: BTreeSet<String>,
}

struct Builder {
    limits: VfsLimits,
    entries: usize,
    blocks: usize,
    aggregate_bytes: usize,
    files: BTreeMap<String, Arc<[u8]>>,
    directories: BTreeSet<String>,
}

impl Builder {
    fn new(limits: VfsLimits) -> Self {
        let mut directories = BTreeSet::new();
        directories.insert(String::new());
        Self {
            limits,
            entries: 0,
            blocks: 0,
            aggregate_bytes: 0,
            files: BTreeMap::new(),
            directories,
        }
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn parse_directory(
        &mut self,
        origin: &str,
        input: &[u8],
        offset: usize,
        parent: &str,
        depth: usize,
        layer: &mut Layer,
    ) -> Result<(), VfsError> {
        if depth > self.limits.max_depth {
            return Err(VfsError::layer(
                origin,
                offset,
                parent,
                VfsErrorKind::LimitExceeded("directory depth"),
            ));
        }
        let count = read_u32(input, offset)
            .ok_or_else(|| VfsError::layer(origin, offset, parent, VfsErrorKind::Truncated))?
            as usize;
        self.charge_entries(origin, offset, parent, count)?;
        let table_start = offset
            .checked_add(4)
            .ok_or_else(|| VfsError::layer(origin, offset, parent, VfsErrorKind::OffsetOverflow))?;
        let table_bytes = count
            .checked_mul(ENTRY_BYTES)
            .ok_or_else(|| VfsError::layer(origin, offset, parent, VfsErrorKind::OffsetOverflow))?;
        let table_end = table_start
            .checked_add(table_bytes)
            .ok_or_else(|| VfsError::layer(origin, offset, parent, VfsErrorKind::OffsetOverflow))?;
        if table_end > input.len() {
            return Err(VfsError::layer(
                origin,
                input.len(),
                parent,
                VfsErrorKind::Truncated,
            ));
        }

        layer.directories.insert(parent.to_owned());
        for index in 0..count {
            let entry_offset = table_start + index * ENTRY_BYTES;
            let name = parse_name(&input[entry_offset..entry_offset + NAME_BYTES])
                .map_err(|kind| VfsError::layer(origin, entry_offset, parent, kind))?;
            let path = join_path(parent, &name);
            if path.len() > self.limits.max_path_bytes.min(SPEC_MAX_PATH_BYTES) {
                return Err(VfsError::layer(
                    origin,
                    entry_offset,
                    &path,
                    VfsErrorKind::LimitExceeded("path bytes"),
                ));
            }
            let child_offset = read_u32(input, entry_offset + 36).expect("checked entry") as usize;
            let size = read_u32(input, entry_offset + 40).expect("checked entry") as usize;
            let block_size = read_u32(input, entry_offset + 44).expect("checked entry") as usize;

            match (child_offset, size, block_size) {
                (0, 0, 0) => Self::insert_file(layer, path, Arc::from([])),
                (directory_offset, 0, 0) => {
                    if directory_offset <= entry_offset {
                        return Err(VfsError::layer(
                            origin,
                            entry_offset + 36,
                            &path,
                            VfsErrorKind::OffsetOrder,
                        ));
                    }
                    Self::insert_directory(layer, &path);
                    self.parse_directory(origin, input, directory_offset, &path, depth + 1, layer)?;
                }
                (file_offset, file_size, file_block_size)
                    if file_offset != 0 && file_size != 0 && file_block_size != 0 =>
                {
                    if file_offset <= entry_offset {
                        return Err(VfsError::layer(
                            origin,
                            entry_offset + 36,
                            &path,
                            VfsErrorKind::OffsetOrder,
                        ));
                    }
                    let data = self.parse_file(
                        origin,
                        input,
                        file_offset,
                        file_size,
                        file_block_size,
                        &path,
                    )?;
                    Self::insert_file(layer, path, Arc::from(data));
                }
                _ => {
                    return Err(VfsError::layer(
                        origin,
                        entry_offset + 36,
                        &path,
                        VfsErrorKind::InvalidEntry,
                    ));
                }
            }
        }
        Ok(())
    }

    fn parse_file(
        &mut self,
        origin: &str,
        input: &[u8],
        offset: usize,
        size: usize,
        block_size: usize,
        path: &str,
    ) -> Result<Vec<u8>, VfsError> {
        if size > self.limits.max_file_bytes {
            return Err(VfsError::layer(
                origin,
                offset,
                path,
                VfsErrorKind::LimitExceeded("file bytes"),
            ));
        }
        if block_size > self.limits.max_block_bytes {
            return Err(VfsError::layer(
                origin,
                offset,
                path,
                VfsErrorKind::LimitExceeded("block bytes"),
            ));
        }
        self.aggregate_bytes = self
            .aggregate_bytes
            .checked_add(size)
            .ok_or_else(|| VfsError::layer(origin, offset, path, VfsErrorKind::OffsetOverflow))?;
        if self.aggregate_bytes > self.limits.max_aggregate_bytes {
            return Err(VfsError::layer(
                origin,
                offset,
                path,
                VfsErrorKind::LimitExceeded("aggregate file bytes"),
            ));
        }
        let block_count = size.div_ceil(block_size);
        self.blocks = self
            .blocks
            .checked_add(block_count)
            .ok_or_else(|| VfsError::layer(origin, offset, path, VfsErrorKind::OffsetOverflow))?;
        if self.blocks > self.limits.max_blocks {
            return Err(VfsError::layer(
                origin,
                offset,
                path,
                VfsErrorKind::LimitExceeded("block count"),
            ));
        }
        let table_bytes = block_count
            .checked_mul(4)
            .ok_or_else(|| VfsError::layer(origin, offset, path, VfsErrorKind::OffsetOverflow))?;
        let mut compressed_offset = offset
            .checked_add(table_bytes)
            .ok_or_else(|| VfsError::layer(origin, offset, path, VfsErrorKind::OffsetOverflow))?;
        if compressed_offset > input.len() {
            return Err(VfsError::layer(
                origin,
                input.len(),
                path,
                VfsErrorKind::Truncated,
            ));
        }

        let mut output = Vec::with_capacity(size);
        for block in 0..block_count {
            let size_offset = offset + block * 4;
            let compressed_size = read_u32(input, size_offset).ok_or_else(|| {
                VfsError::layer(origin, size_offset, path, VfsErrorKind::Truncated)
            })? as usize;
            let compressed_end =
                compressed_offset
                    .checked_add(compressed_size)
                    .ok_or_else(|| {
                        VfsError::layer(
                            origin,
                            compressed_offset,
                            path,
                            VfsErrorKind::OffsetOverflow,
                        )
                    })?;
            let compressed = input
                .get(compressed_offset..compressed_end)
                .ok_or_else(|| {
                    VfsError::layer(origin, input.len(), path, VfsErrorKind::Truncated)
                })?;
            let expected = block_size.min(size - output.len());
            decompress_block(
                origin,
                compressed_offset,
                path,
                compressed,
                expected,
                &mut output,
            )?;
            compressed_offset = compressed_end;
        }
        debug_assert_eq!(output.len(), size);
        Ok(output)
    }

    fn charge_entries(
        &mut self,
        origin: &str,
        offset: usize,
        path: &str,
        count: usize,
    ) -> Result<(), VfsError> {
        self.entries = self
            .entries
            .checked_add(count)
            .ok_or_else(|| VfsError::layer(origin, offset, path, VfsErrorKind::OffsetOverflow))?;
        if self.entries > self.limits.max_entries {
            return Err(VfsError::layer(
                origin,
                offset,
                path,
                VfsErrorKind::LimitExceeded("entry count"),
            ));
        }
        Ok(())
    }

    fn insert_file(layer: &mut Layer, path: String, data: Arc<[u8]>) {
        remove_subtree(&mut layer.files, &mut layer.directories, &path);
        layer.files.insert(path, data);
    }

    fn insert_directory(layer: &mut Layer, path: &str) {
        layer.files.remove(path);
        layer.directories.insert(path.to_owned());
    }

    fn apply(&mut self, layer: Layer) {
        for directory in layer.directories {
            if directory.is_empty() {
                continue;
            }
            self.files.remove(&directory);
            self.directories.insert(directory);
        }
        for (path, data) in layer.files {
            remove_subtree(&mut self.files, &mut self.directories, &path);
            self.files.insert(path, data);
        }
    }
}

fn decompress_block(
    origin: &str,
    offset: usize,
    path: &str,
    compressed: &[u8],
    expected: usize,
    output: &mut Vec<u8>,
) -> Result<(), VfsError> {
    let mut decoder = ZlibDecoder::new(compressed);
    let mut block = Vec::with_capacity(expected);
    let mut buffer = [0_u8; 8192];
    loop {
        let count = decoder.read(&mut buffer).map_err(|error| {
            VfsError::layer(
                origin,
                offset,
                path,
                VfsErrorKind::InvalidZlib(error.to_string()),
            )
        })?;
        if count == 0 {
            break;
        }
        if block
            .len()
            .checked_add(count)
            .is_none_or(|length| length > expected)
        {
            return Err(VfsError::layer(
                origin,
                offset,
                path,
                VfsErrorKind::BlockSizeMismatch {
                    expected,
                    actual: block.len().saturating_add(count),
                },
            ));
        }
        block.extend_from_slice(&buffer[..count]);
    }
    if decoder.total_in() != compressed.len() as u64 {
        return Err(VfsError::layer(
            origin,
            offset + usize::try_from(decoder.total_in()).unwrap_or(usize::MAX),
            path,
            VfsErrorKind::InvalidZlib("trailing compressed data".to_owned()),
        ));
    }
    if block.len() != expected {
        return Err(VfsError::layer(
            origin,
            offset,
            path,
            VfsErrorKind::BlockSizeMismatch {
                expected,
                actual: block.len(),
            },
        ));
    }
    output.extend_from_slice(&block);
    Ok(())
}

fn parse_name(raw: &[u8]) -> Result<String, VfsErrorKind> {
    let length = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    if length == 0 || raw[length..].iter().any(|byte| *byte != 0) {
        return Err(VfsErrorKind::InvalidName);
    }
    let name = &raw[..length];
    if name
        .iter()
        .any(|byte| !(32..=126).contains(byte) || matches!(byte, b'/' | b'\\' | b':'))
    {
        return Err(VfsErrorKind::InvalidName);
    }
    Ok(name
        .iter()
        .map(u8::to_ascii_lowercase)
        .map(char::from)
        .collect())
}

fn normalize_lookup(path: &str, max_path_bytes: usize) -> Result<String, VfsErrorKind> {
    if !path.is_ascii() || path.len() > max_path_bytes {
        return Err(VfsErrorKind::InvalidPath);
    }
    let mut components = Vec::new();
    for component in path.split(['/', '\\']) {
        if component.is_empty() {
            continue;
        }
        if component == "."
            || component == ".."
            || component.len() > NAME_BYTES
            || component
                .bytes()
                .any(|byte| !(32..=126).contains(&byte) || byte == b':')
        {
            return Err(VfsErrorKind::InvalidPath);
        }
        components.push(component.to_ascii_lowercase());
    }
    let normalized = components.join("/");
    if normalized.len() > max_path_bytes {
        return Err(VfsErrorKind::InvalidPath);
    }
    Ok(normalized)
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn display_path(path: &str) -> String {
    if path.is_empty() {
        "/".to_owned()
    } else {
        format!("/{path}")
    }
}

fn remove_subtree(
    files: &mut BTreeMap<String, Arc<[u8]>>,
    directories: &mut BTreeSet<String>,
    path: &str,
) {
    let prefix = format!("{path}/");
    files.retain(|candidate, _| candidate != path && !candidate.starts_with(&prefix));
    directories.retain(|candidate| candidate != path && !candidate.starts_with(&prefix));
}

fn read_u32(input: &[u8], offset: usize) -> Option<u32> {
    let bytes = input.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
}
