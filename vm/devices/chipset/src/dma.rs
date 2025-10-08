// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements dual 8237 ISA DMA controllers.
//!
//! Tested working with Floppy, but may require additional improvements to work
//! properly with other legacy hardware (e.g: Sound Blaster)
//!
//! Rather than having the DMA controller device be some big, complicated
//! behemoth that asynchronously resolves DMA requests itself (i.e: how the DMA
//! controller works in actual hardware), our virtual DMA controller takes a
//! simpler approach.
//!
//! This device is essentially just a big bundle-of-registers, which DMA capable
//! devices can query via the specialized
//! [`vmcore::isa_dma_channel::IsaDmaChannel`] trait in order to get the info
//! they need to fulfil DMA requests themselves.

#![warn(missing_docs)]

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::PortIoIntercept;
use inspect::Inspect;
use inspect::InspectMut;
use open_enum::open_enum;
use std::ops::RangeInclusive;
use vmcore::device_state::ChangeDeviceState;
use vmcore::isa_dma_channel::IsaDmaBuffer;
use vmcore::isa_dma_channel::IsaDmaDirection;

// Skip registering page port 0x80 so that the PCAT BIOS can handle
// it for debugging purposes.
const DMA_PAGE_REGISTER_PORT_RANGE: u16 = 0x80;
const PAGE_PORTS: RangeInclusive<u16> =
    (DMA_PAGE_REGISTER_PORT_RANGE + 0x1)..=(DMA_PAGE_REGISTER_PORT_RANGE + 0xf);
const CONTROLLER0_PORTS: RangeInclusive<u16> = 0x00..=0x0f;
/// Only the even ports, e.g., 0xc0, 0xc2, ..., 0xde, due to the 16
/// bit channel size that the second DMA controller supports.
const CONTROLLER1_PORTS: RangeInclusive<u16> = 0xc0..=0xdf;

const CHANNELS_PER_CONTROLLER: usize = 4;

const PAGE_PORTS_FOR_CHANNEL: [usize; 8] = [0x7, 0x3, 0x1, 0x2, 0xF, 0xB, 0x9, 0xA];

open_enum! {
    // Starts at offset 8 from the register bank.
    enum ControlRegister: u16 {
        STATUS = 0,          // port 0x08 (controller 0), port 0xd0 (controller 1). Read-only
        COMMAND = 0,         // port 0x08 (controller 0), port 0xd0 (controller 1). Write-only
        REQUEST = 1,         // port 0x09 (controller 0), port 0xd2 (controller 1)
        MASK = 2,            // port 0x0a (controller 0), port 0xd4 (controller 1)
        MODE = 3,            // port 0x0b (controller 0), port 0xd6 (controller 1)
        CLEAR_FLIP_FLOP = 4, // port 0x0c (controller 0), port 0xd8 (controller 1)
        INTERMEDIATE = 5,    // port 0x0d (controller 0), port 0xda (controller 1). Read-only
        RESET = 5,           // port 0x0d (controller 0), port 0xda (controller 1). Write-only
        CLEAR_MASK = 6,      // port 0x0e (controller 0), port 0xdc (controller 1)
        WRITE_MASK = 7,      // port 0x0f (controller 0), port 0xde (controller 1)
    }
}

/// Dual 8237 DMA controllers.
#[derive(Debug, InspectMut)]
pub struct DmaController {
    // Volatile state
    state: DmaControllerState,
}

#[derive(Debug, Default, Clone, Inspect)]
struct DmaControllerState {
    #[inspect(iter_by_index)]
    page_registers: [u8; 16],
    controller0: Controller,
    controller1: Controller,
}

impl DmaController {
    /// Returns a new controller.
    pub fn new() -> Self {
        Self {
            state: DmaControllerState::default(),
        }
    }

    fn get_controller(&mut self, channel_number: usize) -> Option<&mut Controller> {
        if channel_number < CHANNELS_PER_CONTROLLER {
            Some(&mut self.state.controller0)
        } else if channel_number < CHANNELS_PER_CONTROLLER * 2 {
            Some(&mut self.state.controller1)
        } else {
            None
        }
    }

    /// Checks the value of the DMA channel's configured transfer size.
    ///
    /// Corresponds to the `check_transfer_size` function in the `IsaDmaChannel`
    /// trait.
    pub fn check_transfer_size(&mut self, channel_number: usize) -> u16 {
        let Some(controller) = self.get_controller(channel_number) else {
            tracelimit::error_ratelimited!(?channel_number, "invalid channel number");
            return 0;
        };

        controller.channels[channel_number % CHANNELS_PER_CONTROLLER].count
    }

    /// Requests an access to ISA DMA channel buffer.
    ///
    /// Corresponds to the `request` function in the `IsaDmaChannel` trait.
    pub fn request(
        &mut self,
        channel_number: usize,
        direction: IsaDmaDirection,
    ) -> Option<IsaDmaBuffer> {
        if channel_number >= CHANNELS_PER_CONTROLLER * 2 {
            tracelimit::error_ratelimited!(?channel_number, "invalid channel number");
            return None;
        }

        let page = self.state.page_registers[PAGE_PORTS_FOR_CHANNEL[channel_number]];

        let controller = self.get_controller(channel_number).unwrap();
        if controller.disabled {
            tracelimit::warn_ratelimited!(?channel_number, "channel is disabled");
            return None;
        }

        let channel_index = channel_number % CHANNELS_PER_CONTROLLER;

        let channel = &controller.channels[channel_index];
        if !channel.enabled {
            tracing::warn!(
                ?channel_number,
                ?channel_index,
                "channel currently disabled"
            );
            return None;
        }

        let transfer_type = match (channel.mode >> 2) & 0x3 {
            0 => {
                tracing::error!(?channel_number, "invalid request: mode is self-test");
                return None;
            }
            1 => IsaDmaDirection::Write,
            2 => IsaDmaDirection::Read,
            _ => {
                tracing::error!(?channel_number, "invalid request: mode is invalid");
                return None;
            }
        };

        if transfer_type != direction {
            tracing::warn!(
                ?channel_number,
                ?channel_index,
                "mismatch between programmed and requested transfer directions"
            );
            return None;
        }

        let address = channel.address as u64 | (page as u64) << 16;

        // Report the channel as being active.
        controller.status &= !(1 << channel_index);

        let buffer = IsaDmaBuffer {
            address,
            size: channel.count as usize,
        };

        Some(buffer)
    }

    /// Signals to the DMA controller that the transfer is concluded.
    ///
    /// Corresponds to the `complete` function in the `IsaDmaChannel` trait.
    pub fn complete(&mut self, channel_number: usize) {
        let Some(controller) = self.get_controller(channel_number) else {
            tracing::error!(?channel_number, "invalid channel number");
            return;
        };

        let channel_index = channel_number % CHANNELS_PER_CONTROLLER;

        if (controller.status & (1 << channel_index)) != 0 {
            tracing::warn!(?channel_number, "channel was not active");
        }

        // Report the channel as being inactive.
        controller.status |= 1 << channel_index;
    }
}

impl ChangeDeviceState for DmaController {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.state = Default::default();
    }
}

impl ChipsetDevice for DmaController {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }
}

impl PortIoIntercept for DmaController {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        data.fill(0);
        data[0] = if PAGE_PORTS.contains(&io_port) {
            self.state.page_registers[io_port as usize & 0xf]
        } else if CONTROLLER0_PORTS.contains(&io_port) {
            match self.state.controller0.read(io_port) {
                Ok(val) => val,
                Err(e) => return IoResult::Err(e),
            }
        } else if CONTROLLER1_PORTS.contains(&io_port) {
            // The secondary controller registers are 16 bits wide (but still have only 8 bytes of data).
            match self.state.controller1.read((io_port / 2) & 0xf) {
                Ok(val) => val,
                Err(e) => return IoResult::Err(e),
            }
        } else {
            return IoResult::Err(IoError::InvalidRegister);
        };

        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if PAGE_PORTS.contains(&io_port) {
            self.state.page_registers[io_port as usize & 0xf] = data[0];
            IoResult::Ok
        } else if CONTROLLER0_PORTS.contains(&io_port) {
            self.state.controller0.write(io_port, data[0])
        } else if CONTROLLER1_PORTS.contains(&io_port) {
            // The secondary controller registers are 16 bits wide (but still have only 8 bytes of data).
            self.state.controller1.write((io_port / 2) & 0xf, data[0])
        } else {
            IoResult::Err(IoError::InvalidRegister)
        }
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        &[
            ("page", PAGE_PORTS),
            ("controller0", CONTROLLER0_PORTS),
            ("controller1", CONTROLLER1_PORTS),
        ]
    }
}

/// A single DMA controller.
#[derive(Debug, Default, Clone, Inspect)]
struct Controller {
    #[inspect(iter_by_index)]
    channels: [Channel; CHANNELS_PER_CONTROLLER],
    flip_flop_high: bool,
    latched_address: u16,
    latched_count: u16,
    disabled: bool,
    status: u8,
}

#[derive(Debug, Default, Clone, Inspect)]
struct Channel {
    #[inspect(hex)]
    address: u16,
    #[inspect(hex)]
    count: u16,
    #[inspect(hex)]
    mode: u8,
    enabled: bool,
}

impl Controller {
    fn read(&mut self, reg: u16) -> Result<u8, IoError> {
        let res = if reg < 8 {
            let channel = reg as usize / 2;
            let data = if reg.is_multiple_of(2) {
                // Address port.
                if !self.flip_flop_high {
                    self.latched_address = self.channels[channel].address;
                }
                self.latched_address
            } else {
                // Word count port.
                if !self.flip_flop_high {
                    self.latched_count = self.channels[channel].count;
                }
                self.latched_count
            };

            // Extract the high or low byte depending on the flip-flop state.
            self.flip_flop_high = !self.flip_flop_high;
            if !self.flip_flop_high {
                (data >> 8) as u8
            } else {
                data as u8
            }
        } else {
            match ControlRegister(reg - 8) {
                ControlRegister::STATUS => std::mem::take(&mut self.status),
                ControlRegister::INTERMEDIATE => 0,
                ControlRegister::WRITE_MASK => {
                    let mut data = 0xf0;
                    for (n, channel) in self.channels.iter().enumerate() {
                        if channel.enabled {
                            // should this be `!channel.enabled`?
                            data |= 1 << n;
                        }
                    }
                    data
                }
                _ => return Err(IoError::InvalidRegister),
            }
        };

        Ok(res)
    }

    fn write(&mut self, reg: u16, data: u8) -> IoResult {
        if reg < 8 {
            let channel = reg as usize / 2;
            let mem = if reg.is_multiple_of(2) {
                // Address port.
                &mut self.channels[channel].address
            } else {
                &mut self.channels[channel].count
            };
            if self.flip_flop_high {
                *mem = (*mem & 0xff) | (data as u16) << 8
            } else {
                *mem = (*mem & 0xff00) | data as u16
            }
            self.flip_flop_high = !self.flip_flop_high;
        } else {
            match ControlRegister(reg - 8) {
                ControlRegister::COMMAND => {
                    self.disabled = data != 0;
                }
                ControlRegister::REQUEST => {
                    // Our emulation doesn't support software-initiated DMA
                    // transfers. Specify that the channel has reached its
                    // terminal count.
                    self.status |= 1 << (data & 3);
                }
                ControlRegister::MASK => {
                    self.channels[data as usize & 3].enabled = data & 4 == 0;
                }
                ControlRegister::MODE => self.channels[data as usize & 3].mode = data,
                ControlRegister::RESET => {
                    *self = Default::default();
                }
                ControlRegister::CLEAR_MASK => {
                    for channel in &mut self.channels {
                        channel.enabled = true;
                    }
                }
                ControlRegister::WRITE_MASK => {
                    for (n, channel) in self.channels.iter_mut().enumerate() {
                        channel.enabled = data & (1 << n) == 0;
                    }
                }
                ControlRegister::CLEAR_FLIP_FLOP => self.flip_flop_high = false,
                _ => return IoResult::Err(IoError::InvalidRegister),
            }
        }

        IoResult::Ok
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.dma")]
        pub struct SavedState {
            #[mesh(1)]
            pub page_registers: [u8; 16],
            #[mesh(2)]
            pub controller0: SavedController,
            #[mesh(3)]
            pub controller1: SavedController,
        }

        #[derive(Protobuf)]
        #[mesh(package = "chipset.dma")]
        pub struct SavedController {
            #[mesh(1)]
            pub channels: [SavedChannel; 4],
            #[mesh(2)]
            pub flip_flop_high: bool,
            #[mesh(3)]
            pub latched_address: u16,
            #[mesh(4)]
            pub latched_count: u16,
            #[mesh(5)]
            pub status: u8,
            #[mesh(6)]
            pub disabled: bool,
        }

        #[derive(Protobuf)]
        #[mesh(package = "chipset.dma")]
        pub struct SavedChannel {
            #[mesh(1)]
            pub address: u16,
            #[mesh(2)]
            pub count: u16,
            #[mesh(3)]
            pub mode: u8,
            #[mesh(4)]
            pub enabled: bool,
        }
    }

    impl SaveRestore for DmaController {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let DmaControllerState {
                page_registers,
                controller0,
                controller1,
            } = self.state.clone();

            let [controller0, controller1] = [controller0, controller1].map(|con| {
                let Controller {
                    channels,
                    flip_flop_high,
                    latched_address,
                    latched_count,
                    status,
                    disabled,
                } = con;

                state::SavedController {
                    channels: channels.map(|chan| {
                        let Channel {
                            address,
                            count,
                            mode,
                            enabled,
                        } = chan;

                        state::SavedChannel {
                            address,
                            count,
                            mode,
                            enabled,
                        }
                    }),
                    flip_flop_high,
                    latched_address,
                    latched_count,
                    status,
                    disabled,
                }
            });

            let saved_state = state::SavedState {
                page_registers,
                controller0,
                controller1,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                page_registers,
                controller0,
                controller1,
            } = state;

            let [controller0, controller1] = [controller0, controller1].map(|con| {
                let state::SavedController {
                    channels,
                    flip_flop_high,
                    latched_address,
                    latched_count,
                    status,
                    disabled,
                } = con;

                Controller {
                    channels: channels.map(|chan| {
                        let state::SavedChannel {
                            address,
                            count,
                            mode,
                            enabled,
                        } = chan;

                        Channel {
                            address,
                            count,
                            mode,
                            enabled,
                        }
                    }),
                    flip_flop_high,
                    latched_address,
                    latched_count,
                    status,
                    disabled,
                }
            });

            self.state = DmaControllerState {
                page_registers,
                controller0,
                controller1,
            };

            Ok(())
        }
    }
}
