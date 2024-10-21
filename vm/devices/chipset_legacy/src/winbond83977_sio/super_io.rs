// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This file implements the emulation of the Winbond 83977 super I/O (SIO)
//! chipset.
//!
//! The chipset is accessed via I/O ports that are normally used for the floppy
//! controller. As such, there is a dependency between the floppy controller
//! device and this device. This module doesn't install its own I/O port
//! callbacks. Rather, it relies on the floppy controller module to call it when
//! the I/O ports are accessed.
//!
//! # Emulation accuracy
//!
//! There is a universe where we went off the deep-end and actually supported
//! each and every little bit and bob of configuration data exposed by the
//! chipset, which includes wacky stuff like relocating the address of
//! fundamental devices (like the RTC), changing IRQ modes, etc...
//!
//! Thankfully, we don't live in that universe.
//!
//! Instead, we just hard code some sane defaults, sanity check some writes, and
//! hope that no-one tried to do anything particularly exotic.
//!
//! And hey, it's been working great since '02, so why stop now?
//!
//! The alternative would be a _lot_ of effort, and a _lot_ of cross-device
//! plumbing, which isn't really something we'd like to be in the business of
//! doing (let alone for an archaic device this this).

use chipset_device::ChipsetDevice;
use inspect::Inspect;
use inspect::InspectMut;
use open_enum::open_enum;
use thiserror::Error;
use vmcore::device_state::ChangeDeviceState;

const NUM_SIO_DEVICES: usize = 9;

#[derive(Debug, Error)]
pub enum SioConfigError {
    #[error("sio controller not in config mode: {0:?} state")]
    NotInConfigMode(ConfigIdxState),
}

#[derive(Default, Debug, Inspect, Copy, Clone)]
#[inspect(debug)]
pub enum ConfigIdxState {
    /// Waiting for first byte of handshake.
    #[default]
    Idle,
    /// Waiting for second byte of handshake.
    Handshake,
    /// Handshake complete.
    Ready,
}

open_enum! {
    /// Configuration Registers - See Section 10. of the spec.
    #[derive(Default, Inspect)]
    #[inspect(debug)]
    enum ConfigRegister: u8 {
        // Card-specific registers
        LOGICAL_DEVICE_NUMBER = 0x07,
        DEVICE_ID             = 0x20,
        REVISION_NUMBER       = 0x21,
        POWER_DOWN_CONTROL    = 0x22,
        PNP_CONTROL           = 0x24,

        // Device-specific registers.
        ENABLE_DEVICE = 0x30,
        IO_BASE_MSB0  = 0x60,
        IO_BASE_LSB0  = 0x61,
        IO_BASE_MSB1  = 0x62,
        IO_BASE_LSB1  = 0x63,

        IRQ_SELECT1 = 0x70,
        IRQ_TYPE1   = 0x71,
        IRQ_SELECT2 = 0x72,
        IRQ_TYPE2   = 0x73,
        DMA_CONFIG1 = 0x74,
        DMA_CONFIG2 = 0x75,

        ADDRESS_UNDOCUMENTED = 0xBA,

        DEVICE_BIT_CONFIG0 = 0xE8,
        DEVICE_BIT_CONFIG1 = 0xE9,
        DEVICE_BIT_CONFIG2 = 0xEA,
        DEVICE_BIT_CONFIG3 = 0xEB,
        DEVICE_BIT_CONFIG4 = 0xEC,
        DEVICE_BIT_CONFIG5 = 0xED,
        DEVICE_BIT_CONFIG6 = 0xEE,
        DEVICE_BIT_CONFIG7 = 0xEF,

        DEVICE_CONFIG0 = 0xF0,
        DEVICE_CONFIG1 = 0xF1,
        DEVICE_CONFIG2 = 0xF2,
        DEVICE_CONFIG3 = 0xF3,
        DEVICE_CONFIG4 = 0xF4,
        DEVICE_CONFIG5 = 0xF5,
    }
}

impl ConfigRegister {
    /// Check if this register contains device-specific data.
    fn is_device_specific(&self) -> bool {
        self.0 >= 0x30
    }
}

open_enum! {
    #[derive(Default, Inspect)]
    #[inspect(debug)]
    enum LogicalDeviceIndex: u8 {
        FLOPPY_CONTROLLER   = 0,
        PARALLEL_PORT       = 1,
        COM1_PORT           = 2,
        COM2_PORT           = 3,
        RTC                 = 4,
        KEYBOARD_CONTROLLER = 5,
        INFRARED_PORT       = 6,
        AUX_IO_CONTROL1     = 7,
        AUX_IO_CONTROL2     = 8,
    }
}

#[derive(Debug, Default, Copy, Clone, Inspect)]
struct LogicalDeviceData {
    enabled: bool,
    #[inspect(with = "|x| inspect::iter_by_index(x).map_value(inspect::AsHex)")]
    io_port_base: [u16; 2],
    #[inspect(bytes)]
    irq_vector: [u8; 2],
    #[inspect(bytes)]
    dma_channel: [u8; 2],
    // DEVNOTE: For all intents and purposes, you can consider these values
    // "magic". If you're really interested in what they do (on a per-device
    // level), feel free to whip out the spec.
    #[inspect(bytes)]
    config_data: [u8; 8],
}

impl LogicalDeviceData {
    // DEVNOTE: From a "code correctness" POV, all this config data _should_ be
    // plumbed through via the device's constructor, and reflect the reality of
    // how the system topology was set up in the top level VMM init code.
    //
    // ...but given that this device is only really here to support compatibility
    // with legacy Hyper-V Generation 1 VMs (which have a rigid system topology
    // least wrt these sorts of base chipset devices), we'll take the pragmatic
    // approach of hard-coding these values to "known good" values, and assume the
    // top-level VMM code hasn't decided to move things around.
    fn default_data() -> [Self; NUM_SIO_DEVICES] {
        let mut defaults: [Self; NUM_SIO_DEVICES] = [Self::default(); NUM_SIO_DEVICES];

        defaults[LogicalDeviceIndex::FLOPPY_CONTROLLER.0 as usize] = Self {
            enabled: true,
            io_port_base: [0x3F0, 0x370], // [primary, secondary]
            irq_vector: [6, 0],
            dma_channel: [2, 0],
            config_data: [0x0E, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00],
        };

        // This must exist for compatibility sake, even though we don't actually
        // have an emulated parallel port.
        defaults[LogicalDeviceIndex::PARALLEL_PORT.0 as usize] = Self {
            enabled: false,
            io_port_base: [0x0000; 2],
            irq_vector: [0, 0],
            dma_channel: [4, 0],
            config_data: [0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        };

        defaults[LogicalDeviceIndex::COM1_PORT.0 as usize] = Self {
            enabled: true,
            io_port_base: [0x3F8, 0],
            irq_vector: [3, 0],
            dma_channel: [4, 0],
            config_data: [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        };

        defaults[LogicalDeviceIndex::COM2_PORT.0 as usize] = Self {
            enabled: true,
            io_port_base: [0x2F8, 0],
            irq_vector: [4, 0],
            dma_channel: [4, 0],
            config_data: [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        };

        defaults[LogicalDeviceIndex::RTC.0 as usize] = Self {
            enabled: true,
            io_port_base: [0x70, 0],
            irq_vector: [8, 0],
            dma_channel: [4, 0],
            config_data: [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        };

        defaults[LogicalDeviceIndex::KEYBOARD_CONTROLLER.0 as usize] = Self {
            enabled: true,
            io_port_base: [0x60, 0x64], // [keyboard, mouse]
            irq_vector: [1, 12],        // [keyboard, mouse]
            dma_channel: [4, 0],
            config_data: [0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        };

        defaults[LogicalDeviceIndex::INFRARED_PORT.0 as usize] = Self {
            enabled: false,
            io_port_base: [0x0000; 2],
            irq_vector: [0, 0],
            dma_channel: [4, 4],
            config_data: [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        };

        defaults[LogicalDeviceIndex::AUX_IO_CONTROL1.0 as usize] = Self {
            enabled: false,
            io_port_base: [0x0000; 2],
            irq_vector: [0, 0],
            dma_channel: [4, 0],
            config_data: [0xEF, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00],
        };

        defaults[LogicalDeviceIndex::AUX_IO_CONTROL2.0 as usize] = Self {
            enabled: false,
            io_port_base: [0x0000; 2],
            irq_vector: [0, 0],
            dma_channel: [4, 4],
            config_data: [0xEF, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00],
        };

        defaults
    }
}

#[derive(Debug, Inspect)]
struct SioControllerState {
    config_idx_state: ConfigIdxState,
    config_idx: ConfigRegister,
    device_idx: LogicalDeviceIndex,
    #[inspect(iter_by_index)]
    device_data: [LogicalDeviceData; NUM_SIO_DEVICES],
}

#[derive(Debug, InspectMut)]
pub struct SioController {
    // Volatile state
    state: SioControllerState,
}

impl Default for SioController {
    fn default() -> Self {
        Self {
            state: SioControllerState {
                config_idx_state: ConfigIdxState::default(),
                config_idx: ConfigRegister::default(),
                device_idx: LogicalDeviceIndex::default(),
                device_data: LogicalDeviceData::default_data(),
            },
        }
    }
}

impl SioController {
    pub fn update_config_state(&mut self, value: u8) {
        let prev_state = self.state.config_idx_state;

        // This port provides a handshake for entering SIO config mode.
        // First check for the secret handshake to enter config mode.

        // 0x87h must be written twice to the Extended Functions Enable Register
        // (EFER, I/O port address 3F0h or 370h) in order to read or write the
        // config registers. After programming of config registers is finished
        // 0xAA should be written to EFER to exit config mode.

        self.state.config_idx_state = match (self.state.config_idx_state, value) {
            // If in config mode, a write of 0xAA exits config mode.
            (ConfigIdxState::Ready, 0xAA) => ConfigIdxState::Idle,
            // Okay, we're in config mode. Pass through the value.
            (ConfigIdxState::Ready, _) => {
                // If in config mode, writing to this port sets which config register
                // gets accessed via `config_{read,write}`.
                self.state.config_idx = ConfigRegister(value);
                // remain in the ready state
                ConfigIdxState::Ready
            }
            // If outside of config mode, 0x87 must be written twice in a row to enter config mode.
            (ConfigIdxState::Idle, 0x87) => ConfigIdxState::Handshake,
            (ConfigIdxState::Handshake, 0x87) => ConfigIdxState::Ready,
            // Any other values reset the handshake back to it's default state.
            (_, _) => ConfigIdxState::Idle,
        };

        tracing::trace!(
            ?value,
            ?prev_state,
            cur_state = ?self.state.config_idx_state,
            register = ?self.state.config_idx,
            "update sio config state"
        );
    }

    pub fn config_read(&mut self) -> Result<u8, SioConfigError> {
        // If not in config mode, reads will abort config handshake.
        if !matches!(self.state.config_idx_state, ConfigIdxState::Ready) {
            let state = self.state.config_idx_state;
            self.state.config_idx_state = ConfigIdxState::Idle;
            return Err(SioConfigError::NotInConfigMode(state));
        }

        // Check if this is a read from a non-device-specific register.
        if !self.state.config_idx.is_device_specific() {
            let value = match self.state.config_idx {
                ConfigRegister::DEVICE_ID => 0x97,
                ConfigRegister::REVISION_NUMBER => 0x71,
                _ => {
                    tracelimit::warn_ratelimited!(
                        ?self.state.config_idx,
                        "unexpected config register read"
                    );
                    0x00
                }
            };
            return Ok(value);
        }

        let dev = match self
            .state
            .device_data
            .get_mut(self.state.device_idx.0 as usize)
        {
            Some(dev) => dev,
            None => {
                tracelimit::warn_ratelimited!(
                    logical_device_number = self.state.device_idx.0,
                    "invalid logical device index"
                );

                return Ok(0);
            }
        };

        // Handle reads to device-specific registers.
        let value = match self.state.config_idx {
            ConfigRegister::ENABLE_DEVICE => dev.enabled as u8,
            ConfigRegister::IO_BASE_MSB0 => (dev.io_port_base[0] >> 8) as u8,
            ConfigRegister::IO_BASE_LSB0 => dev.io_port_base[0] as u8,
            ConfigRegister::IO_BASE_MSB1 => (dev.io_port_base[1] >> 8) as u8,
            ConfigRegister::IO_BASE_LSB1 => dev.io_port_base[1] as u8,
            ConfigRegister::IRQ_SELECT1 => dev.irq_vector[0],
            ConfigRegister::IRQ_TYPE1 => 0b10, // high, edge triggered
            ConfigRegister::IRQ_SELECT2 => dev.irq_vector[1],
            ConfigRegister::IRQ_TYPE2 => 0b10, // high, edge triggered
            ConfigRegister::DMA_CONFIG1 => dev.dma_channel[0],
            ConfigRegister::DMA_CONFIG2 => dev.dma_channel[1],
            ConfigRegister::DEVICE_CONFIG0 => dev.config_data[0],
            ConfigRegister::DEVICE_CONFIG1 => dev.config_data[1],
            ConfigRegister::DEVICE_CONFIG2 => dev.config_data[2],
            ConfigRegister::DEVICE_CONFIG3 => dev.config_data[3],
            ConfigRegister::DEVICE_CONFIG4 => dev.config_data[4],
            ConfigRegister::DEVICE_CONFIG5 => dev.config_data[5],
            _ => {
                tracelimit::warn_ratelimited!(
                    ?self.state.config_idx,
                    "unexpected config register read"
                );
                0x00
            }
        };

        tracing::trace!(
            config_reg = ?self.state.config_idx,
            device_idx = ?self.state.device_idx,
            ?value,
            "sio config read"
        );

        Ok(value)
    }

    pub fn config_write(&mut self, value: u8) {
        // If not in config mode, writes will abort config handshake.
        if !matches!(self.state.config_idx_state, ConfigIdxState::Ready) {
            self.state.config_idx_state = ConfigIdxState::Idle;
            return;
        }

        tracing::trace!(
            config_reg = ?self.state.config_idx,
            device_idx = ?self.state.device_idx,
            ?value,
            "sio config write"
        );

        // Check if this is a write to a non-device-specific register.
        if !self.state.config_idx.is_device_specific() {
            match self.state.config_idx {
                ConfigRegister::LOGICAL_DEVICE_NUMBER => {
                    self.state.device_idx = LogicalDeviceIndex(value);
                }
                ConfigRegister::POWER_DOWN_CONTROL => {
                    if value != 0xFF {
                        tracelimit::warn_ratelimited!(
                            value = value,
                            "invalid value written to POWER_DOWN_CONTROL register"
                        )
                    }
                }
                ConfigRegister::PNP_CONTROL => {
                    if value != 0xC4 {
                        tracelimit::warn_ratelimited!(
                            value = value,
                            "invalid value written to PNP_CONTROL register"
                        )
                    }
                }
                _ => {
                    tracelimit::warn_ratelimited!(
                        ?self.state.config_idx,
                        ?value,
                        "unexpected config register write"
                    )
                }
            }
            return;
        }

        let dev = match self
            .state
            .device_data
            .get_mut(self.state.device_idx.0 as usize)
        {
            Some(dev) => dev,
            None => {
                tracelimit::warn_ratelimited!(
                    logical_device_number = self.state.device_idx.0,
                    "invalid logical device index"
                );

                return;
            }
        };

        // Handle writes to device-specific registers.
        match self.state.config_idx {
            ConfigRegister::ENABLE_DEVICE => {
                // Disallow enabling parallel port.
                if self.state.device_idx != LogicalDeviceIndex::PARALLEL_PORT {
                    dev.enabled = value & 0x1 == 1;
                } else {
                    tracing::debug!("attempted to enable parallel port")
                }
            }
            ConfigRegister::IO_BASE_MSB0 => {
                dev.io_port_base[0] = (dev.io_port_base[0] & !0xFF00) | (value as u16) << 8;
            }
            ConfigRegister::IO_BASE_LSB0 => {
                dev.io_port_base[0] = (dev.io_port_base[0] & !0x00FF) | (value as u16);
            }
            ConfigRegister::IO_BASE_MSB1 => {
                dev.io_port_base[1] = (dev.io_port_base[1] & !0xFF00) | (value as u16) << 8;
            }
            ConfigRegister::IO_BASE_LSB1 => {
                dev.io_port_base[1] = (dev.io_port_base[1] & !0x00FF) | (value as u16);
            }
            ConfigRegister::IRQ_SELECT1 => dev.irq_vector[0] = value,
            ConfigRegister::IRQ_SELECT2 => dev.irq_vector[1] = value,
            ConfigRegister::DMA_CONFIG1 => dev.dma_channel[0] = value,
            ConfigRegister::DMA_CONFIG2 => dev.dma_channel[1] = value,
            ConfigRegister::DEVICE_CONFIG0 => dev.config_data[0] = value,
            ConfigRegister::DEVICE_CONFIG1 => dev.config_data[1] = value,
            ConfigRegister::DEVICE_CONFIG2 => dev.config_data[2] = value,
            ConfigRegister::DEVICE_CONFIG3 => dev.config_data[3] = value,
            ConfigRegister::DEVICE_CONFIG4 => dev.config_data[4] = value,
            ConfigRegister::DEVICE_CONFIG5 => dev.config_data[5] = value,
            _ => {
                // sanity-check the guest is writing values we support to the
                // `AUX_IO_CONTROL2`-specific registers
                if self.state.device_idx == LogicalDeviceIndex::AUX_IO_CONTROL2 {
                    let expected = match self.state.config_idx {
                        ConfigRegister::DEVICE_BIT_CONFIG0 => value == 0x10 || value == 0x12, // Keyboard reset functionality
                        ConfigRegister::DEVICE_BIT_CONFIG5 => value == 0x08, //  A20 Gate functionality
                        ConfigRegister::ADDRESS_UNDOCUMENTED => value == 0xf0, // Keyboard functionality
                        _ => false,
                    };

                    if !expected {
                        tracelimit::warn_ratelimited!(?self.state.config_idx, ?value, "wrote an unexpected value");
                    }
                } else {
                    tracelimit::warn_ratelimited!(
                        ?self.state.config_idx,
                        ?value,
                        "unexpected config register write"
                    )
                }
            }
        }
    }
}

impl ChangeDeviceState for SioController {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.state.config_idx_state = ConfigIdxState::default();
        self.state.config_idx = ConfigRegister::default();
        self.state.device_idx = LogicalDeviceIndex::default();
        self.state.device_data = LogicalDeviceData::default_data();
    }
}

// Sio is an interesting chipset device, since it doesn't *directly* interact
// with any chipset services. Rather, it is a sub-component of another chipset
// device (the winbond83977_sio).
//
// Nonetheless, we implement ChipsetDevice for consistency, as it is "logically"
// a chipset device.
impl ChipsetDevice for SioController {}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        const SAVED_NUM_SIO_DEVICES: usize = 9;

        #[derive(Protobuf)]
        #[mesh(package = "chipset.superio")]
        pub enum SavedConfigIdxState {
            #[mesh(1)]
            Idle,
            #[mesh(2)]
            Handshake,
            #[mesh(3)]
            Ready,
        }

        #[derive(Protobuf)]
        #[mesh(package = "chipset.superio")]
        pub struct SavedLogicalDeviceData {
            #[mesh(1)]
            pub enabled: bool,
            #[mesh(2)]
            pub io_port_base: [u16; 2],
            #[mesh(3)]
            pub irq_vector: [u8; 2],
            #[mesh(4)]
            pub dma_channel: [u8; 2],
            #[mesh(5)]
            pub config_data: [u8; 8],
        }

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.superio")]
        pub struct SavedState {
            #[mesh(1)]
            pub config_idx_state: SavedConfigIdxState,
            #[mesh(2)]
            pub config_idx: u8,
            #[mesh(3)]
            pub device_idx: u8,
            #[mesh(4)]
            pub device_data: [SavedLogicalDeviceData; SAVED_NUM_SIO_DEVICES],
        }
    }

    impl SaveRestore for SioController {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let SioControllerState {
                config_idx_state,
                config_idx,
                device_idx,
                device_data,
            } = self.state;

            let saved_state = state::SavedState {
                config_idx_state: match config_idx_state {
                    ConfigIdxState::Idle => state::SavedConfigIdxState::Idle,
                    ConfigIdxState::Handshake => state::SavedConfigIdxState::Handshake,
                    ConfigIdxState::Ready => state::SavedConfigIdxState::Ready,
                },
                config_idx: config_idx.0,
                device_idx: device_idx.0,
                device_data: device_data.map(|data| {
                    let LogicalDeviceData {
                        enabled,
                        io_port_base,
                        irq_vector,
                        dma_channel,
                        config_data,
                    } = data;

                    state::SavedLogicalDeviceData {
                        enabled,
                        io_port_base,
                        irq_vector,
                        dma_channel,
                        config_data,
                    }
                }),
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                config_idx_state,
                config_idx,
                device_idx,
                device_data,
            } = state;

            self.state = SioControllerState {
                config_idx_state: match config_idx_state {
                    state::SavedConfigIdxState::Idle => ConfigIdxState::Idle,
                    state::SavedConfigIdxState::Handshake => ConfigIdxState::Handshake,
                    state::SavedConfigIdxState::Ready => ConfigIdxState::Ready,
                },
                config_idx: ConfigRegister(config_idx),
                device_idx: LogicalDeviceIndex(device_idx),
                device_data: device_data.map(|data| {
                    let state::SavedLogicalDeviceData {
                        enabled,
                        io_port_base,
                        irq_vector,
                        dma_channel,
                        config_data,
                    } = data;

                    LogicalDeviceData {
                        enabled,
                        io_port_base,
                        irq_vector,
                        dma_channel,
                        config_data,
                    }
                }),
            };

            Ok(())
        }
    }
}
