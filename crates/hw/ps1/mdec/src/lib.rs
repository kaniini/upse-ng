// SPDX-License-Identifier: LGPL-2.1-or-later
//! PS1 MDEC control and status registers for audio-oriented machine composition.
//!
//! The component models reset, DMA request control, and initialization-table
//! command consumption. Macroblock decoding is intentionally unavailable
//! because decoded video has no observable effect on PSF playback.

use thiserror::Error;
use upse_ps1_dma::{EndpointError, MdecDmaEndpoint};

/// MDEC command, parameter, and output-data port.
pub const MDEC_DATA: u32 = 0x1f80_1820;
/// MDEC control-write and status-read port.
pub const MDEC_CONTROL_STATUS: u32 = 0x1f80_1824;
/// First physical MDEC register address.
pub const MDEC_BASE: u32 = MDEC_DATA;
/// Final physical MDEC register address, inclusive.
pub const MDEC_END: u32 = MDEC_CONTROL_STATUS + 3;

const CONTROL_RESET: u32 = 1 << 31;
const CONTROL_DMA_IN_ENABLE: u32 = 1 << 30;
const CONTROL_DMA_OUT_ENABLE: u32 = 1 << 29;
const STATUS_DATA_OUT_EMPTY: u32 = 1 << 31;
const STATUS_COMMAND_BUSY: u32 = 1 << 29;
const STATUS_DATA_IN_REQUEST: u32 = 1 << 28;
const STATUS_RESET_BLOCK: u32 = 4 << 16;
const RESET_STATUS: u32 = STATUS_DATA_OUT_EMPTY | STATUS_RESET_BLOCK | 0xffff;

/// Invalid or unavailable MDEC operation.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MdecError {
    /// Address is outside the two 32-bit MDEC ports.
    #[error("invalid PS1 MDEC register address {address:#010x}")]
    InvalidRegister {
        /// Physical register address.
        address: u32,
    },
    /// Audio-oriented composition cannot produce decoded macroblock data.
    #[error("PS1 MDEC macroblock output is unavailable")]
    MacroblockOutputUnavailable,
    /// Audio-oriented composition cannot execute a video decode command.
    #[error("PS1 MDEC macroblock decode command {command:#010x} is unavailable")]
    MacroblockDecodeUnavailable {
        /// Rejected command word.
        command: u32,
    },
    /// Command selector is not defined by the MDEC hardware.
    #[error("unsupported PS1 MDEC command {command:#010x}")]
    UnsupportedCommand {
        /// Rejected command word.
        command: u32,
    },
}

/// Instance-owned PS1 MDEC control-plane state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mdec {
    dma_in_enabled: bool,
    dma_out_enabled: bool,
    parameter_words_remaining: u16,
}

impl Mdec {
    /// Constructs the post-reset MDEC state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            dma_in_enabled: false,
            dma_out_enabled: false,
            parameter_words_remaining: 0,
        }
    }

    /// Reads the MDEC status register.
    ///
    /// # Errors
    ///
    /// Returns [`MdecError::MacroblockOutputUnavailable`] for the macroblock
    /// data port or [`MdecError::InvalidRegister`] for any other address.
    pub const fn read_register(&self, address: u32) -> Result<u32, MdecError> {
        match address {
            MDEC_CONTROL_STATUS => Ok(self.status()),
            MDEC_DATA => Err(MdecError::MacroblockOutputUnavailable),
            _ => Err(MdecError::InvalidRegister { address }),
        }
    }

    /// Writes the MDEC command/data or control register.
    ///
    /// Reset clears prior state before the DMA-enable bits from the same word
    /// are applied, matching the hardware register ordering.
    ///
    /// # Errors
    ///
    /// Returns a structured diagnostic for macroblock decode and undefined
    /// commands, or [`MdecError::InvalidRegister`] for any other address.
    pub fn write_register(&mut self, address: u32, value: u32) -> Result<(), MdecError> {
        match address {
            MDEC_CONTROL_STATUS => {
                if value & CONTROL_RESET != 0 {
                    *self = Self::new();
                }
                self.dma_in_enabled = value & CONTROL_DMA_IN_ENABLE != 0;
                self.dma_out_enabled = value & CONTROL_DMA_OUT_ENABLE != 0;
                Ok(())
            }
            MDEC_DATA => self.write_data(value),
            _ => Err(MdecError::InvalidRegister { address }),
        }
    }

    /// Reports whether MDEC input DMA requests are enabled.
    #[must_use]
    pub const fn dma_in_enabled(&self) -> bool {
        self.dma_in_enabled
    }

    /// Reports whether MDEC output DMA requests are enabled.
    #[must_use]
    pub const fn dma_out_enabled(&self) -> bool {
        self.dma_out_enabled
    }

    /// Returns the number of initialization-table parameter words still needed.
    #[must_use]
    pub const fn parameter_words_remaining(&self) -> u16 {
        self.parameter_words_remaining
    }

    fn write_data(&mut self, value: u32) -> Result<(), MdecError> {
        if self.parameter_words_remaining != 0 {
            self.parameter_words_remaining -= 1;
            return Ok(());
        }
        match value >> 29 {
            1 => Err(MdecError::MacroblockDecodeUnavailable { command: value }),
            2 => {
                self.parameter_words_remaining = if value & 1 == 0 { 16 } else { 32 };
                Ok(())
            }
            3 => {
                self.parameter_words_remaining = 32;
                Ok(())
            }
            _ => Err(MdecError::UnsupportedCommand { command: value }),
        }
    }

    const fn status(self) -> u32 {
        let base = if self.parameter_words_remaining == 0 {
            RESET_STATUS
        } else {
            STATUS_DATA_OUT_EMPTY
                | STATUS_COMMAND_BUSY
                | STATUS_RESET_BLOCK
                | (self.parameter_words_remaining - 1) as u32
        };
        base | if self.dma_in_enabled {
            STATUS_DATA_IN_REQUEST
        } else {
            0
        }
    }
}

impl Default for Mdec {
    fn default() -> Self {
        Self::new()
    }
}

impl MdecDmaEndpoint for Mdec {
    fn write_word(&mut self, value: u32) -> Result<(), EndpointError> {
        self.write_register(MDEC_DATA, value)
            .map_err(|error| EndpointError::new(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MDEC_CONTROL_STATUS, MDEC_DATA, Mdec, MdecError, RESET_STATUS, STATUS_COMMAND_BUSY,
        STATUS_DATA_IN_REQUEST,
    };

    #[test]
    fn reset_status_and_dma_requests_are_exact() {
        let mut mdec = Mdec::new();
        assert_eq!(mdec.read_register(MDEC_CONTROL_STATUS), Ok(RESET_STATUS));

        mdec.write_register(MDEC_CONTROL_STATUS, 0x6000_0000)
            .unwrap();
        assert!(mdec.dma_in_enabled());
        assert!(mdec.dma_out_enabled());
        assert_eq!(
            mdec.read_register(MDEC_CONTROL_STATUS),
            Ok(RESET_STATUS | STATUS_DATA_IN_REQUEST)
        );

        mdec.write_register(MDEC_CONTROL_STATUS, 0x8000_0000)
            .unwrap();
        assert!(!mdec.dma_in_enabled());
        assert!(!mdec.dma_out_enabled());
        assert_eq!(mdec.read_register(MDEC_CONTROL_STATUS), Ok(RESET_STATUS));
    }

    #[test]
    fn macroblock_data_and_invalid_addresses_remain_diagnostic() {
        let mut mdec = Mdec::new();
        assert_eq!(
            mdec.read_register(MDEC_DATA),
            Err(MdecError::MacroblockOutputUnavailable)
        );
        assert_eq!(
            mdec.write_register(MDEC_DATA, 0x2000_0001),
            Err(MdecError::MacroblockDecodeUnavailable {
                command: 0x2000_0001
            })
        );
        assert_eq!(
            mdec.read_register(MDEC_CONTROL_STATUS + 4),
            Err(MdecError::InvalidRegister {
                address: MDEC_CONTROL_STATUS + 4
            })
        );
    }

    #[test]
    fn initialization_table_commands_consume_exact_word_counts() {
        let mut mdec = Mdec::new();
        mdec.write_register(MDEC_DATA, 0x4000_0001).unwrap();
        assert_eq!(mdec.parameter_words_remaining(), 32);
        assert_eq!(
            mdec.read_register(MDEC_CONTROL_STATUS).unwrap() & (STATUS_COMMAND_BUSY | 0xffff),
            STATUS_COMMAND_BUSY | 31
        );
        for word in 0..32 {
            mdec.write_register(MDEC_DATA, word).unwrap();
        }
        assert_eq!(mdec.parameter_words_remaining(), 0);
        assert_eq!(mdec.read_register(MDEC_CONTROL_STATUS), Ok(RESET_STATUS));

        mdec.write_register(MDEC_DATA, 0x6000_0000).unwrap();
        assert_eq!(mdec.parameter_words_remaining(), 32);
        for word in 0..32 {
            mdec.write_register(MDEC_DATA, word).unwrap();
        }
        assert_eq!(mdec.read_register(MDEC_CONTROL_STATUS), Ok(RESET_STATUS));
    }
}
