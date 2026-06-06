// SPDX-License-Identifier: LGPL-2.1-or-later
//! Register-free integer primitives shared by PS1 SPU and PS2 SPU2 devices.

mod adpcm;
mod adsr;
mod fixed;
mod interpolation;
mod noise;
mod pitch;
mod reverb;

pub use adpcm::{AdpcmError, AdpcmFlags, AdpcmHistory, DecodedBlock, decode_block};
pub use adsr::{Envelope, EnvelopeConfig, EnvelopePhase, EnvelopeRate};
pub use fixed::{clamp_i16, mac_q15, multiply_q15};
pub use interpolation::{GAUSSIAN_WEIGHTS, GaussianInterpolator};
pub use noise::NoiseGenerator;
pub use pitch::{PitchCounter, PitchStep};
pub use reverb::RingBufferAddress;
