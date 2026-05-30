// SPDX-License-Identifier: LGPL-2.1-or-later
use std::io::Read;

use crc32fast::hash;
use flate2::read::ZlibDecoder;
use thiserror::Error;

use crate::Tags;

const HEADER_SIZE: usize = 16;
const TAG_MARKER: &[u8; 5] = b"[TAG]";
const PSF1_MAX_PROGRAM: usize = 2_033_664;

/// Supported PSF format versions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PsfVersion {
    /// Original-console PSF1.
    Psf1,
    /// IOP-only PSF2.
    Psf2,
}

impl PsfVersion {
    /// Parses a PSF version byte.
    ///
    /// # Errors
    ///
    /// Returns [`ParseErrorKind::UnsupportedVersion`] for any non-PSF1/PSF2
    /// version byte.
    pub fn from_byte(value: u8) -> Result<Self, ParseErrorKind> {
        match value {
            0x01 => Ok(Self::Psf1),
            0x02 => Ok(Self::Psf2),
            other => Err(ParseErrorKind::UnsupportedVersion(other)),
        }
    }

    /// Returns the on-disk version byte.
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Psf1 => 0x01,
            Self::Psf2 => 0x02,
        }
    }
}

/// A half-open byte range in the original input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRange {
    /// Inclusive start offset.
    pub start: usize,
    /// Exclusive end offset.
    pub end: usize,
}

impl ByteRange {
    /// Returns the byte count in this range.
    #[must_use]
    pub const fn len(self) -> usize {
        self.end - self.start
    }

    /// Reports whether the range is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// Resource bounds applied while parsing one container.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseLimits {
    /// Maximum complete input size.
    pub max_input_bytes: usize,
    /// Maximum reserved-section size.
    pub max_reserved_bytes: usize,
    /// Maximum decompressed program size.
    pub max_decompressed_bytes: usize,
    /// Maximum tag-data size, excluding the marker.
    pub max_tag_bytes: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 64 * 1024 * 1024,
            max_reserved_bytes: 32 * 1024 * 1024,
            max_decompressed_bytes: 16 * 1024 * 1024,
            max_tag_bytes: 50_000,
        }
    }
}

/// Parser stage associated with a structured failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseStage {
    /// Fixed-size PSF header.
    Header,
    /// Reserved area.
    Reserved,
    /// Compressed program framing or checksum.
    CompressedProgram,
    /// Zlib program decompression.
    Decompression,
    /// Optional tag marker or tag lines.
    Tags,
}

/// Specific parser failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ParseErrorKind {
    /// Input ended before the required bytes.
    #[error("truncated input")]
    Truncated,
    /// The three-byte signature was not `PSF`.
    #[error("invalid PSF signature")]
    InvalidSignature,
    /// The version byte is not supported by this library.
    #[error("unsupported PSF version 0x{0:02x}")]
    UnsupportedVersion(u8),
    /// Header offset arithmetic overflowed the host representation.
    #[error("section offset overflow")]
    OffsetOverflow,
    /// A configured resource limit was exceeded.
    #[error("resource limit exceeded: {0}")]
    LimitExceeded(&'static str),
    /// Compressed bytes did not match the declared CRC-32.
    #[error("compressed program CRC mismatch: expected {expected:08x}, got {actual:08x}")]
    CrcMismatch {
        /// Header checksum.
        expected: u32,
        /// Computed checksum.
        actual: u32,
    },
    /// The zlib stream was invalid or did not consume the complete program field.
    #[error("invalid zlib program: {0}")]
    InvalidZlib(String),
    /// Nonempty trailing data did not begin with `[TAG]`.
    #[error("invalid tag marker")]
    InvalidTagMarker,
    /// Tag text contained a forbidden null byte.
    #[error("tag text contains a null byte")]
    TagNull,
    /// A nonblank tag line did not contain an equals sign.
    #[error("malformed tag line")]
    MalformedTagLine,
    /// A tag key was not a valid C identifier.
    #[error("invalid tag key")]
    InvalidTagKey,
}

/// Structured parse error containing origin, stage, and byte offset.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{origin}:{offset}: {stage:?}: {kind}")]
pub struct ParseError {
    /// Logical input name supplied by the caller.
    pub origin: String,
    /// Parser stage.
    pub stage: ParseStage,
    /// Byte offset in the original input.
    pub offset: usize,
    /// Specific failure.
    pub kind: ParseErrorKind,
}

impl ParseError {
    fn new(origin: &str, stage: ParseStage, offset: usize, kind: ParseErrorKind) -> Self {
        Self {
            origin: origin.to_owned(),
            stage,
            offset,
            kind,
        }
    }
}

/// Validated, owned common PSF container data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PsfContainer {
    version: PsfVersion,
    reserved: Vec<u8>,
    compressed_program: Vec<u8>,
    program: Vec<u8>,
    tags: Tags,
    reserved_range: ByteRange,
    compressed_range: ByteRange,
    tag_range: Option<ByteRange>,
}

impl PsfContainer {
    /// Parses one complete container using conservative default limits.
    ///
    /// # Errors
    ///
    /// Returns a structured [`ParseError`] for malformed, unsupported, or
    /// resource-limit-exceeding input.
    pub fn parse(origin: impl AsRef<str>, input: &[u8]) -> Result<Self, ParseError> {
        Self::parse_with_limits(origin, input, ParseLimits::default())
    }

    /// Parses one complete container using caller-selected limits.
    ///
    /// # Errors
    ///
    /// Returns a structured [`ParseError`] for malformed, unsupported, or
    /// resource-limit-exceeding input.
    #[allow(clippy::too_many_lines)]
    pub fn parse_with_limits(
        origin: impl AsRef<str>,
        input: &[u8],
        limits: ParseLimits,
    ) -> Result<Self, ParseError> {
        let origin = origin.as_ref();
        if input.len() > limits.max_input_bytes {
            return Err(ParseError::new(
                origin,
                ParseStage::Header,
                0,
                ParseErrorKind::LimitExceeded("input bytes"),
            ));
        }
        if input.len() < HEADER_SIZE {
            return Err(ParseError::new(
                origin,
                ParseStage::Header,
                input.len(),
                ParseErrorKind::Truncated,
            ));
        }
        if &input[..3] != b"PSF" {
            return Err(ParseError::new(
                origin,
                ParseStage::Header,
                0,
                ParseErrorKind::InvalidSignature,
            ));
        }
        let version = PsfVersion::from_byte(input[3])
            .map_err(|kind| ParseError::new(origin, ParseStage::Header, 3, kind))?;
        let reserved_len = read_u32(input, 4) as usize;
        let compressed_len = read_u32(input, 8) as usize;
        let expected_crc = read_u32(input, 12);
        if reserved_len > limits.max_reserved_bytes {
            return Err(ParseError::new(
                origin,
                ParseStage::Reserved,
                HEADER_SIZE,
                ParseErrorKind::LimitExceeded("reserved bytes"),
            ));
        }
        let reserved_end = HEADER_SIZE.checked_add(reserved_len).ok_or_else(|| {
            ParseError::new(
                origin,
                ParseStage::Reserved,
                HEADER_SIZE,
                ParseErrorKind::OffsetOverflow,
            )
        })?;
        let program_end = reserved_end.checked_add(compressed_len).ok_or_else(|| {
            ParseError::new(
                origin,
                ParseStage::CompressedProgram,
                reserved_end,
                ParseErrorKind::OffsetOverflow,
            )
        })?;
        if program_end > input.len() {
            return Err(ParseError::new(
                origin,
                ParseStage::CompressedProgram,
                input.len(),
                ParseErrorKind::Truncated,
            ));
        }

        let reserved_range = ByteRange {
            start: HEADER_SIZE,
            end: reserved_end,
        };
        let compressed_range = ByteRange {
            start: reserved_end,
            end: program_end,
        };
        let compressed = &input[compressed_range.start..compressed_range.end];
        let actual_crc = hash(compressed);
        if actual_crc != expected_crc {
            return Err(ParseError::new(
                origin,
                ParseStage::CompressedProgram,
                compressed_range.start,
                ParseErrorKind::CrcMismatch {
                    expected: expected_crc,
                    actual: actual_crc,
                },
            ));
        }

        let effective_program_limit = if version == PsfVersion::Psf1 {
            limits.max_decompressed_bytes.min(PSF1_MAX_PROGRAM)
        } else {
            limits.max_decompressed_bytes
        };
        let program = decompress(
            origin,
            compressed,
            compressed_range.start,
            effective_program_limit,
        )?;

        let (tags, tag_range) = if program_end == input.len() {
            (Tags::default(), None)
        } else {
            let marker_end = program_end.checked_add(TAG_MARKER.len()).ok_or_else(|| {
                ParseError::new(
                    origin,
                    ParseStage::Tags,
                    program_end,
                    ParseErrorKind::OffsetOverflow,
                )
            })?;
            if marker_end > input.len() || &input[program_end..marker_end] != TAG_MARKER {
                return Err(ParseError::new(
                    origin,
                    ParseStage::Tags,
                    program_end,
                    ParseErrorKind::InvalidTagMarker,
                ));
            }
            let raw = &input[marker_end..];
            if raw.len() > limits.max_tag_bytes {
                return Err(ParseError::new(
                    origin,
                    ParseStage::Tags,
                    marker_end,
                    ParseErrorKind::LimitExceeded("tag bytes"),
                ));
            }
            let tags = Tags::parse(origin, raw, marker_end)?;
            (
                tags,
                Some(ByteRange {
                    start: marker_end,
                    end: input.len(),
                }),
            )
        };

        Ok(Self {
            version,
            reserved: input[reserved_range.start..reserved_range.end].to_vec(),
            compressed_program: compressed.to_vec(),
            program,
            tags,
            reserved_range,
            compressed_range,
            tag_range,
        })
    }

    /// Returns the parsed format version.
    #[must_use]
    pub const fn version(&self) -> PsfVersion {
        self.version
    }

    /// Returns the reserved section.
    #[must_use]
    pub fn reserved(&self) -> &[u8] {
        &self.reserved
    }

    /// Returns the exact compressed program bytes covered by the CRC.
    #[must_use]
    pub fn compressed_program(&self) -> &[u8] {
        &self.compressed_program
    }

    /// Returns the bounded, decompressed program bytes.
    #[must_use]
    pub fn program(&self) -> &[u8] {
        &self.program
    }

    /// Returns parsed ordered tags.
    #[must_use]
    pub const fn tags(&self) -> &Tags {
        &self.tags
    }

    /// Returns the original reserved-section range.
    #[must_use]
    pub const fn reserved_range(&self) -> ByteRange {
        self.reserved_range
    }

    /// Returns the original compressed-program range.
    #[must_use]
    pub const fn compressed_range(&self) -> ByteRange {
        self.compressed_range
    }

    /// Returns the original raw tag-data range, excluding `[TAG]`.
    #[must_use]
    pub const fn tag_range(&self) -> Option<ByteRange> {
        self.tag_range
    }
}

fn decompress(
    origin: &str,
    compressed: &[u8],
    offset: usize,
    limit: usize,
) -> Result<Vec<u8>, ParseError> {
    if compressed.is_empty() {
        return Ok(Vec::new());
    }
    let mut decoder = ZlibDecoder::new(compressed);
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = decoder.read(&mut buffer).map_err(|error| {
            ParseError::new(
                origin,
                ParseStage::Decompression,
                offset,
                ParseErrorKind::InvalidZlib(error.to_string()),
            )
        })?;
        if count == 0 {
            break;
        }
        if output
            .len()
            .checked_add(count)
            .is_none_or(|size| size > limit)
        {
            return Err(ParseError::new(
                origin,
                ParseStage::Decompression,
                offset,
                ParseErrorKind::LimitExceeded("decompressed program bytes"),
            ));
        }
        output.extend_from_slice(&buffer[..count]);
    }
    if decoder.total_in() != compressed.len() as u64 {
        return Err(ParseError::new(
            origin,
            ParseStage::Decompression,
            offset + usize::try_from(decoder.total_in()).unwrap_or(usize::MAX),
            ParseErrorKind::InvalidZlib("trailing compressed data".to_owned()),
        ));
    }
    Ok(output)
}

fn read_u32(input: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        input[offset..offset + 4]
            .try_into()
            .expect("checked header"),
    )
}

#[cfg(test)]
mod tests {
    use crc32fast::hash;

    use super::{ParseErrorKind, ParseLimits, ParseStage, PsfContainer, PsfVersion};
    use crate::PsfBuilder;

    #[test]
    fn parses_owned_sections_and_exact_ranges_by_version_not_extension() {
        let bytes = PsfBuilder::new(PsfVersion::Psf1)
            .reserved([1, 2, 3])
            .program([4, 5, 6, 7])
            .tag("title", "Synthetic")
            .build();
        let parsed = PsfContainer::parse("deliberately.psf2", &bytes).unwrap();
        assert_eq!(parsed.version(), PsfVersion::Psf1);
        assert_eq!(parsed.reserved(), [1, 2, 3]);
        assert_eq!(parsed.program(), [4, 5, 6, 7]);
        assert_eq!(parsed.reserved_range().start, 16);
        assert_eq!(parsed.reserved_range().len(), 3);
        assert_eq!(
            &bytes[parsed.compressed_range().start..parsed.compressed_range().end],
            parsed.compressed_program()
        );
        let tag_range = parsed.tag_range().unwrap();
        assert_eq!(&bytes[tag_range.start..tag_range.end], b"title=Synthetic\n");
    }

    #[test]
    fn rejects_every_fixed_header_truncation_and_bad_signature_or_version() {
        let valid = PsfBuilder::new(PsfVersion::Psf2).build();
        for length in 0..16 {
            assert_eq!(
                PsfContainer::parse("short", &valid[..length])
                    .unwrap_err()
                    .kind,
                ParseErrorKind::Truncated
            );
        }
        let mut bad = valid.clone();
        bad[0] = b'X';
        assert_eq!(
            PsfContainer::parse("bad", &bad).unwrap_err().kind,
            ParseErrorKind::InvalidSignature
        );
        bad = valid;
        bad[3] = 0x11;
        assert_eq!(
            PsfContainer::parse("bad", &bad).unwrap_err().kind,
            ParseErrorKind::UnsupportedVersion(0x11)
        );
    }

    #[test]
    fn validates_section_bounds_crc_and_zlib() {
        let valid = PsfBuilder::new(PsfVersion::Psf1).program([7; 4096]).build();
        let mut truncated = valid.clone();
        truncated.truncate(valid.len() - 1);
        assert!(PsfContainer::parse("truncated", &truncated).is_err());

        let mut crc = valid.clone();
        crc[12..16].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
        assert!(matches!(
            PsfContainer::parse("crc", &crc).unwrap_err().kind,
            ParseErrorKind::CrcMismatch { .. }
        ));

        let mut invalid = Vec::from(&b"PSF\x01\0\0\0\0\x04\0\0\0"[..]);
        invalid.extend_from_slice(&hash(b"nope").to_le_bytes());
        invalid.extend_from_slice(b"nope");
        assert!(matches!(
            PsfContainer::parse("zlib", &invalid).unwrap_err().kind,
            ParseErrorKind::InvalidZlib(_)
        ));
    }

    #[test]
    fn enforces_input_reserved_decompression_and_tag_limits() {
        let bytes = PsfBuilder::new(PsfVersion::Psf1).program([0; 1024]).build();
        let mut limits = ParseLimits {
            max_input_bytes: bytes.len() - 1,
            ..ParseLimits::default()
        };
        assert_eq!(
            PsfContainer::parse_with_limits("input", &bytes, limits)
                .unwrap_err()
                .kind,
            ParseErrorKind::LimitExceeded("input bytes")
        );
        limits = ParseLimits::default();
        limits.max_decompressed_bytes = 100;
        assert_eq!(
            PsfContainer::parse_with_limits("bomb", &bytes, limits)
                .unwrap_err()
                .kind,
            ParseErrorKind::LimitExceeded("decompressed program bytes")
        );
        let reserved = PsfBuilder::new(PsfVersion::Psf2).reserved([0; 32]).build();
        limits = ParseLimits::default();
        limits.max_reserved_bytes = 31;
        assert_eq!(
            PsfContainer::parse_with_limits("reserved", &reserved, limits)
                .unwrap_err()
                .kind,
            ParseErrorKind::LimitExceeded("reserved bytes")
        );
        let tags = PsfBuilder::new(PsfVersion::Psf2)
            .tag("comment", "long")
            .build();
        limits = ParseLimits::default();
        limits.max_tag_bytes = 4;
        assert_eq!(
            PsfContainer::parse_with_limits("tag", &tags, limits)
                .unwrap_err()
                .kind,
            ParseErrorKind::LimitExceeded("tag bytes")
        );
    }

    #[test]
    fn rejects_bad_tag_marker_lines_keys_and_nulls_with_offsets() {
        let base = PsfBuilder::new(PsfVersion::Psf2).build();
        let cases: &[(&[u8], ParseErrorKind)] = &[
            (b"wrong", ParseErrorKind::InvalidTagMarker),
            (b"[TAG]broken", ParseErrorKind::MalformedTagLine),
            (b"[TAG]9key=value", ParseErrorKind::InvalidTagKey),
            (b"[TAG]key=bad\0value", ParseErrorKind::TagNull),
        ];
        for (suffix, expected) in cases {
            let mut input = base.clone();
            input.extend_from_slice(suffix);
            let error = PsfContainer::parse("tags", &input).unwrap_err();
            assert_eq!(&error.kind, expected);
            assert_eq!(error.stage, ParseStage::Tags);
            assert!(error.offset >= base.len());
        }
    }

    #[test]
    fn bounded_arbitrary_inputs_never_panic_or_escape_limits() {
        let limits = ParseLimits {
            max_input_bytes: 2048,
            max_reserved_bytes: 1024,
            max_decompressed_bytes: 4096,
            max_tag_bytes: 512,
        };
        let mut state = 0x4d59_5df4_d0f3_3173_u64;
        for length in 0..2048 {
            let mut input = vec![0_u8; length];
            for byte in &mut input {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state.to_le_bytes()[0];
            }
            let _ = PsfContainer::parse_with_limits("generated-arbitrary", &input, limits);
        }
    }
}
