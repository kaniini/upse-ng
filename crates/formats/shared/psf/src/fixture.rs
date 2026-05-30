// SPDX-License-Identifier: LGPL-2.1-or-later
use std::io::Write;

use crc32fast::hash;
use flate2::{Compression, write::ZlibEncoder};

use crate::PsfVersion;

/// Builder for small, project-generated PSF conformance fixtures.
#[derive(Clone, Debug)]
pub struct PsfBuilder {
    version: PsfVersion,
    reserved: Vec<u8>,
    program: Vec<u8>,
    tags: Vec<(String, String)>,
}

impl PsfBuilder {
    /// Starts a container of the selected version.
    #[must_use]
    pub fn new(version: PsfVersion) -> Self {
        Self {
            version,
            reserved: Vec::new(),
            program: Vec::new(),
            tags: Vec::new(),
        }
    }

    /// Replaces the reserved-section bytes.
    #[must_use]
    pub fn reserved(mut self, bytes: impl AsRef<[u8]>) -> Self {
        self.reserved = bytes.as_ref().to_vec();
        self
    }

    /// Replaces the uncompressed program bytes.
    #[must_use]
    pub fn program(mut self, bytes: impl AsRef<[u8]>) -> Self {
        self.program = bytes.as_ref().to_vec();
        self
    }

    /// Appends one raw text tag line.
    #[must_use]
    pub fn tag(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.tags.push((key.into(), value.into()));
        self
    }

    /// Encodes a valid container using zlib and CRC-32.
    ///
    /// # Panics
    ///
    /// Panics only if the in-memory zlib writer reports an impossible I/O error
    /// or a deliberately enormous fixture exceeds the PSF 32-bit size fields.
    #[must_use]
    pub fn build(self) -> Vec<u8> {
        let compressed = if self.program.is_empty() {
            Vec::new()
        } else {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&self.program).expect("writing to Vec");
            encoder.finish().expect("writing to Vec")
        };
        let reserved_len = u32::try_from(self.reserved.len()).expect("fixture reserved fits u32");
        let compressed_len =
            u32::try_from(compressed.len()).expect("fixture compressed program fits u32");
        let mut output =
            Vec::with_capacity(16 + self.reserved.len() + compressed.len() + self.tags.len() * 32);
        output.extend_from_slice(b"PSF");
        output.push(self.version.byte());
        output.extend_from_slice(&reserved_len.to_le_bytes());
        output.extend_from_slice(&compressed_len.to_le_bytes());
        output.extend_from_slice(&hash(&compressed).to_le_bytes());
        output.extend_from_slice(&self.reserved);
        output.extend_from_slice(&compressed);
        if !self.tags.is_empty() {
            output.extend_from_slice(b"[TAG]");
            for (key, value) in self.tags {
                output.extend_from_slice(key.as_bytes());
                output.push(b'=');
                output.extend_from_slice(value.as_bytes());
                output.push(b'\n');
            }
        }
        output
    }
}
