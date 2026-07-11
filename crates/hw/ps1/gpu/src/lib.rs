// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS1 GPU control and status registers for audio-oriented machine composition.
//!
//! The component models command acceptance, display/DMA control state, and GPU
//! information latches. It intentionally has no framebuffer or rasterizer:
//! PSF playback needs the control-plane behavior used by game initialization,
//! while rendered video has no observable audio effect.

use thiserror::Error;

/// GP0 command and GPU read-data register.
pub const GP0: u32 = 0x1f80_1810;
/// GP1 control and GPU status register.
pub const GP1: u32 = 0x1f80_1814;

const RESET_STATUS: u32 = 0x1480_2000;
const STATUS_IRQ: u32 = 1 << 24;
const STATUS_DMA_REQUEST: u32 = 1 << 25;
const STATUS_READY_COMMAND: u32 = 1 << 26;
const STATUS_READY_VRAM_READ: u32 = 1 << 27;
const STATUS_READY_DMA: u32 = 1 << 28;
const STATUS_DMA_DIRECTION: u32 = 3 << 29;

/// Invalid GPU register access.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum GpuError {
    /// Address is not one of the two 32-bit GPU ports.
    #[error("invalid PS1 GPU register address {address:#010x}")]
    InvalidRegister {
        /// Physical register address.
        address: u32,
    },
}

/// Instance-owned PS1 GPU control-plane state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Gpu {
    status: u32,
    read_latch: u32,
    texture_window: u32,
    draw_area_top_left: u32,
    draw_area_bottom_right: u32,
    draw_offset: u32,
}

impl Gpu {
    /// Constructs the post-reset GPU state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            status: RESET_STATUS,
            read_latch: 0,
            texture_window: 0,
            draw_area_top_left: 0,
            draw_area_bottom_right: 0,
            draw_offset: 0,
        }
    }

    /// Reads GPUREAD or GPUSTAT.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::InvalidRegister`] for any other address.
    pub const fn read_register(&self, address: u32) -> Result<u32, GpuError> {
        match address {
            GP0 => Ok(self.read_latch),
            GP1 => Ok(self.status),
            _ => Err(GpuError::InvalidRegister { address }),
        }
    }

    /// Writes a GP0 or GP1 command word.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::InvalidRegister`] for any other address.
    pub fn write_register(&mut self, address: u32, value: u32) -> Result<(), GpuError> {
        match address {
            GP0 => self.write_gp0(value),
            GP1 => self.write_gp1(value),
            _ => return Err(GpuError::InvalidRegister { address }),
        }
        Ok(())
    }

    fn write_gp0(&mut self, value: u32) {
        match value >> 24 {
            0x1f => self.status |= STATUS_IRQ,
            0xe1 => {
                self.status = (self.status & !0x0000_87ff)
                    | (value & 0x0000_07ff)
                    | ((value & 0x0000_0800) << 4);
            }
            0xe2 => self.texture_window = value & 0x000f_ffff,
            0xe3 => self.draw_area_top_left = value & 0x0007_ffff,
            0xe4 => self.draw_area_bottom_right = value & 0x0007_ffff,
            0xe5 => self.draw_offset = value & 0x003f_ffff,
            0xe6 => self.status = (self.status & !(3 << 11)) | ((value & 3) << 11),
            _ => {}
        }
        self.status |= STATUS_READY_COMMAND | STATUS_READY_DMA;
    }

    fn write_gp1(&mut self, value: u32) {
        match value >> 24 {
            0x00 => *self = Self::new(),
            0x01 => {
                self.status |= STATUS_READY_COMMAND | STATUS_READY_DMA;
                self.status &= !STATUS_READY_VRAM_READ;
            }
            0x02 => self.status &= !STATUS_IRQ,
            0x03 => {
                self.status = (self.status & !(1 << 23)) | ((value & 1) << 23);
            }
            0x04 => self.set_dma_direction(value & 3),
            0x08 => self.set_display_mode(value),
            0x10..=0x1f => self.read_latch = self.info(value & 7),
            _ => {}
        }
    }

    fn set_dma_direction(&mut self, direction: u32) {
        self.status =
            (self.status & !(STATUS_DMA_DIRECTION | STATUS_DMA_REQUEST)) | (direction << 29);
        let request = match direction {
            1 => true,
            2 => self.status & STATUS_READY_DMA != 0,
            3 => self.status & STATUS_READY_VRAM_READ != 0,
            _ => false,
        };
        if request {
            self.status |= STATUS_DMA_REQUEST;
        }
    }

    fn set_display_mode(&mut self, value: u32) {
        const DISPLAY_BITS: u32 = (1 << 14) | (1 << 16) | (3 << 17) | (0xf << 19);
        self.status &= !DISPLAY_BITS;
        self.status |= ((value >> 6) & 1) << 16;
        self.status |= (value & 3) << 17;
        self.status |= ((value >> 2) & 0x0f) << 19;
        self.status |= ((value >> 7) & 1) << 14;
        if value & (1 << 5) == 0 {
            self.status |= 1 << 13;
        }
    }

    const fn info(&self, index: u32) -> u32 {
        match index {
            2 => self.texture_window,
            3 => self.draw_area_top_left,
            4 => self.draw_area_bottom_right,
            5 => self.draw_offset,
            7 => 2,
            _ => self.read_latch,
        }
    }
}

impl Default for Gpu {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{GP0, GP1, Gpu, RESET_STATUS, STATUS_DMA_REQUEST, STATUS_IRQ};

    #[test]
    fn reset_status_and_gp1_reset_are_exact() {
        let mut gpu = Gpu::new();
        assert_eq!(gpu.read_register(GP1).unwrap(), RESET_STATUS);
        gpu.write_register(GP1, 0x0300_0000).unwrap();
        gpu.write_register(GP1, 0x0400_0002).unwrap();
        gpu.write_register(GP1, 0).unwrap();
        assert_eq!(gpu.read_register(GP1).unwrap(), RESET_STATUS);
    }

    #[test]
    fn display_dma_and_irq_commands_update_status() {
        let mut gpu = Gpu::new();
        gpu.write_register(GP1, 0x0300_0000).unwrap();
        assert_eq!(gpu.read_register(GP1).unwrap() & (1 << 23), 0);
        gpu.write_register(GP1, 0x0400_0002).unwrap();
        let status = gpu.read_register(GP1).unwrap();
        assert_eq!((status >> 29) & 3, 2);
        assert_ne!(status & STATUS_DMA_REQUEST, 0);
        gpu.write_register(GP0, 0x1f00_0000).unwrap();
        assert_ne!(gpu.read_register(GP1).unwrap() & STATUS_IRQ, 0);
        gpu.write_register(GP1, 0x0200_0000).unwrap();
        assert_eq!(gpu.read_register(GP1).unwrap() & STATUS_IRQ, 0);
    }

    #[test]
    fn drawing_attributes_are_available_through_info_queries() {
        let mut gpu = Gpu::new();
        gpu.write_register(GP0, 0xe200_1234).unwrap();
        gpu.write_register(GP0, 0xe300_5678).unwrap();
        gpu.write_register(GP1, 0x1000_0002).unwrap();
        assert_eq!(gpu.read_register(GP0).unwrap(), 0x1234);
        gpu.write_register(GP1, 0x1000_0003).unwrap();
        assert_eq!(gpu.read_register(GP0).unwrap(), 0x5678);
    }
}
