// SPDX-License-Identifier: LGPL-2.1-or-later
//! Named IOP import services for firmware-free PSF2 playback.
//!
//! The dispatcher owns only format-facing state such as read-only descriptors,
//! TTY output, and the small SIF/SSBUS register surfaces. Machine and kernel
//! mutations cross the narrow [`BiosServices`] boundary.

mod context;
mod error;
mod ioman;
mod matrix;
mod services;
mod sysclib;

pub use context::{GuestAddressRange, ServiceContext, ServiceMemory, ServiceMemoryError};
pub use error::{BackendError, ServiceError};
pub use ioman::ReadOnlyFileSystem;
pub use matrix::{ServiceDescription, ServiceFamily, SupportLevel, describe_import};
pub use services::{
    BackendPayload, BackendRequest, BackendResponse, BiosServices, ImportRequest, IopServices,
    ServiceAction, ServiceOutcome,
};
