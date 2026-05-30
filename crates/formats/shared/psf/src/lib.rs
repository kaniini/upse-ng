// SPDX-License-Identifier: LGPL-2.1-or-later
//! Safe, bounded parsing and dependency planning for PSF containers.
//!
//! Parsing is selected exclusively by the version byte. This crate never maps
//! executables into emulated memory and never executes guest code.

mod container;
mod duration;
mod fixture;
mod load;
mod metadata;
mod tags;

pub use container::{
    ByteRange, ParseError, ParseErrorKind, ParseLimits, ParseStage, PsfContainer, PsfVersion,
};
pub use duration::{Duration, DurationError};
pub use fixture::PsfBuilder;
pub use load::{
    DependencyLimits, FileResolver, LoadError, LoadPlan, MemoryResolver, PlanLayer, Psf1LoadPlan,
    Psf2LoadPlan, ResolvedFile, Resolver, ResolverError, load_plan,
};
pub use metadata::{MetadataError, PlaybackMetadata, RefreshRate};
pub use tags::{Tag, Tags};
