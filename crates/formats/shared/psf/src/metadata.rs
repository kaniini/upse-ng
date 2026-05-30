// SPDX-License-Identifier: LGPL-2.1-or-later
use thiserror::Error;

use crate::{Duration, DurationError, Tag, Tags};

/// Supported video refresh rates used by PSF timing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshRate {
    /// PAL-like 50 Hz refresh.
    Hz50,
    /// NTSC-like 60 Hz refresh.
    Hz60,
}

impl RefreshRate {
    /// Returns the integer refresh rate.
    #[must_use]
    pub const fn hz(self) -> u8 {
        match self {
            Self::Hz50 => 50,
            Self::Hz60 => 60,
        }
    }
}

/// Parsed playback and descriptive metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackMetadata {
    /// Track title.
    pub title: Option<String>,
    /// Track artist.
    pub artist: Option<String>,
    /// Source game.
    pub game: Option<String>,
    /// Release year text.
    pub year: Option<String>,
    /// Genre text.
    pub genre: Option<String>,
    /// Free-form comment.
    pub comment: Option<String>,
    /// Copyright text.
    pub copyright: Option<String>,
    /// Person responsible for the PSF rip.
    pub psfby: Option<String>,
    /// Relative post-mix amplitude, with no clipping implied.
    pub volume: f64,
    /// Declared play length, or no automatic ending when absent.
    pub length: Option<Duration>,
    /// Declared linear fade duration.
    pub fade: Duration,
    /// Optional explicit refresh override.
    pub refresh: Option<RefreshRate>,
    raw_tags: Vec<Tag>,
}

impl PlaybackMetadata {
    /// Parses known tags while retaining all ordered raw tags.
    ///
    /// # Errors
    ///
    /// Returns [`MetadataError`] when a known volume, duration, or refresh tag
    /// is invalid.
    pub fn parse(tags: &Tags) -> Result<Self, MetadataError> {
        let volume = match tags.get("volume") {
            None => 1.0,
            Some(value) => {
                let parsed: f64 = value
                    .replace(',', ".")
                    .parse()
                    .map_err(|_| MetadataError::InvalidVolume)?;
                if !parsed.is_finite() {
                    return Err(MetadataError::InvalidVolume);
                }
                parsed
            }
        };
        let length = tags
            .get("length")
            .map(Duration::parse)
            .transpose()
            .map_err(MetadataError::InvalidDuration)?;
        let fade = tags
            .get("fade")
            .map(Duration::parse)
            .transpose()
            .map_err(MetadataError::InvalidDuration)?
            .unwrap_or(Duration::ZERO);
        let refresh = match tags.get("_refresh") {
            None => None,
            Some("50") => Some(RefreshRate::Hz50),
            Some("60") => Some(RefreshRate::Hz60),
            Some(_) => return Err(MetadataError::InvalidRefresh),
        };
        Ok(Self {
            title: owned(tags, "title"),
            artist: owned(tags, "artist"),
            game: owned(tags, "game"),
            year: owned(tags, "year"),
            genre: owned(tags, "genre"),
            comment: owned(tags, "comment"),
            copyright: owned(tags, "copyright"),
            psfby: owned(tags, "psfby"),
            volume,
            length,
            fade,
            refresh,
            raw_tags: tags.entries().to_vec(),
        })
    }

    /// Returns all tags in source order, including unknown keys.
    #[must_use]
    pub fn raw_tags(&self) -> &[Tag] {
        &self.raw_tags
    }
}

/// Invalid known metadata.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum MetadataError {
    /// A volume value was not a finite real number.
    #[error("invalid volume tag")]
    InvalidVolume,
    /// A duration value was invalid.
    #[error("invalid duration tag: {0}")]
    InvalidDuration(DurationError),
    /// `_refresh` was present but not exactly 50 or 60.
    #[error("invalid _refresh tag")]
    InvalidRefresh,
}

fn owned(tags: &Tags, key: &str) -> Option<String> {
    tags.get(key).map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use crate::{PlaybackMetadata, PsfBuilder, PsfContainer, PsfVersion, RefreshRate};

    #[test]
    fn defaults_and_known_values_are_explicit() {
        let bytes = PsfBuilder::new(PsfVersion::Psf2).build();
        let container = PsfContainer::parse("no-extension", &bytes).unwrap();
        let metadata = PlaybackMetadata::parse(container.tags()).unwrap();
        assert!((metadata.volume - 1.0).abs() < f64::EPSILON);
        assert_eq!(metadata.length, None);
        assert_eq!(metadata.fade.numerator(), 0);

        let bytes = PsfBuilder::new(PsfVersion::Psf1)
            .program(b"program")
            .tag("TITLE", "Synthetic")
            .tag("volume", "-1.25")
            .tag("length", "1:02.5")
            .tag("fade", "3")
            .tag("_refresh", "50")
            .tag("unknown", "kept")
            .build();
        let container = PsfContainer::parse("looks.psf2", &bytes).unwrap();
        let metadata = PlaybackMetadata::parse(container.tags()).unwrap();
        assert_eq!(metadata.title.as_deref(), Some("Synthetic"));
        assert!((metadata.volume + 1.25).abs() < f64::EPSILON);
        assert_eq!(
            metadata.length.unwrap().to_frames_floor(44_100).unwrap(),
            2_756_250
        );
        assert_eq!(metadata.refresh, Some(RefreshRate::Hz50));
        assert_eq!(metadata.raw_tags().len(), 6);
    }
}
