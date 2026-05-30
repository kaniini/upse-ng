// SPDX-License-Identifier: LGPL-2.1-or-later
use crate::{ParseError, ParseErrorKind, ParseStage};

/// One ordered tag, preserving original key and value bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tag {
    key_raw: Vec<u8>,
    value_raw: Vec<u8>,
    key: String,
    value: String,
}

impl Tag {
    /// Returns the decoded key with its original spelling.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the decoded value, replacing invalid host-code-page bytes.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the original key bytes after whitespace trimming.
    #[must_use]
    pub fn key_raw(&self) -> &[u8] {
        &self.key_raw
    }

    /// Returns the original value bytes after whitespace trimming.
    ///
    /// Adjacent multiline values are separated by a single newline byte.
    #[must_use]
    pub fn value_raw(&self) -> &[u8] {
        &self.value_raw
    }
}

/// Ordered PSF tags with case-insensitive lookup.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Tags {
    entries: Vec<Tag>,
}

impl Tags {
    pub(crate) fn parse(origin: &str, raw: &[u8], base: usize) -> Result<Self, ParseError> {
        if let Some(position) = raw.iter().position(|byte| *byte == 0) {
            return Err(ParseError {
                origin: origin.to_owned(),
                stage: ParseStage::Tags,
                offset: base + position,
                kind: ParseErrorKind::TagNull,
            });
        }

        let mut entries: Vec<Tag> = Vec::new();
        let mut line_start = 0_usize;
        for line in raw.split(|byte| *byte == b'\n') {
            let trimmed = trim_whitespace(line);
            if trimmed.is_empty() {
                line_start = line_start.saturating_add(line.len() + 1);
                continue;
            }
            let Some(equals) = trimmed.iter().position(|byte| *byte == b'=') else {
                return Err(ParseError {
                    origin: origin.to_owned(),
                    stage: ParseStage::Tags,
                    offset: base + line_start,
                    kind: ParseErrorKind::MalformedTagLine,
                });
            };
            let key = trim_whitespace(&trimmed[..equals]);
            let value = trim_whitespace(&trimmed[equals + 1..]);
            if !valid_identifier(key) {
                return Err(ParseError {
                    origin: origin.to_owned(),
                    stage: ParseStage::Tags,
                    offset: base + line_start,
                    kind: ParseErrorKind::InvalidTagKey,
                });
            }
            let key_string = String::from_utf8_lossy(key).into_owned();
            if let Some(previous) = entries.last_mut()
                && previous.key.eq_ignore_ascii_case(&key_string)
            {
                previous.value_raw.push(b'\n');
                previous.value_raw.extend_from_slice(value);
                previous.value = String::from_utf8_lossy(&previous.value_raw).into_owned();
            } else {
                entries.push(Tag {
                    key_raw: key.to_vec(),
                    value_raw: value.to_vec(),
                    key: key_string,
                    value: String::from_utf8_lossy(value).into_owned(),
                });
            }
            line_start = line_start.saturating_add(line.len() + 1);
        }
        Ok(Self { entries })
    }

    /// Returns all ordered entries.
    #[must_use]
    pub fn entries(&self) -> &[Tag] {
        &self.entries
    }

    /// Returns the first value whose key matches case-insensitively.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.key.eq_ignore_ascii_case(key))
            .map(Tag::value)
    }

    /// Reports whether a key exists case-insensitively.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
}

fn trim_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(|byte| (1..=0x20).contains(byte)) {
        value = &value[1..];
    }
    while value.last().is_some_and(|byte| (1..=0x20).contains(byte)) {
        value = &value[..value.len() - 1];
    }
    value
}

fn valid_identifier(value: &[u8]) -> bool {
    let Some(first) = value.first() else {
        return false;
    };
    (first.is_ascii_alphabetic() || *first == b'_')
        && value[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::Tags;

    #[test]
    fn parses_whitespace_case_and_adjacent_multiline_values() {
        let tags = Tags::parse(
            "memory.psf",
            b"  TITLE = One \r\nTitle=Two\nartist=Someone\ncustom=\xff\n",
            20,
        )
        .unwrap();
        assert_eq!(tags.entries().len(), 3);
        assert_eq!(tags.get("title"), Some("One\nTwo"));
        assert_eq!(tags.entries()[0].value_raw(), b"One\nTwo");
        assert_eq!(tags.get("ARTIST"), Some("Someone"));
        assert_eq!(tags.entries()[2].value(), "�");
        assert_eq!(tags.entries()[2].value_raw(), b"\xff");
    }

    #[test]
    fn nonadjacent_duplicates_remain_ordered() {
        let tags = Tags::parse("x", b"comment=a\ntitle=x\ncomment=b", 0).unwrap();
        assert_eq!(tags.entries().len(), 3);
        assert_eq!(tags.get("comment"), Some("a"));
    }
}
