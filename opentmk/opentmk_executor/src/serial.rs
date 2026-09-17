// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(not(target_arch = "x86_64"), expect(unused_variables))]

#[cfg(target_arch = "x86_64")]
use opentmk_core::arch::serial::InstrIoAccess;
#[cfg(target_arch = "x86_64")]
use opentmk_core::arch::serial::Serial;
pub use opentmk_core::arch::serial::SerialPort;

pub(crate) trait SerialIo {
    fn init(&mut self);
    fn drain(&mut self);
    fn write_byte(&mut self, byte: u8);
    fn read_byte(&mut self) -> u8;
}

pub(crate) struct OpenTmkSerialIo {
    #[cfg(target_arch = "x86_64")]
    handle: Serial<InstrIoAccess>,
}

impl OpenTmkSerialIo {
    pub fn new(port: SerialPort) -> Self {
        log::info!("creating serial port");
        Self {
            #[cfg(target_arch = "x86_64")]
            handle: Serial::new(port, InstrIoAccess),
        }
    }
}

impl SerialIo for OpenTmkSerialIo {
    fn init(&mut self) {
        #[cfg(target_arch = "x86_64")]
        self.handle.init();
    }

    fn drain(&mut self) {
        #[cfg(target_arch = "x86_64")]
        self.handle.drain();
    }

    fn write_byte(&mut self, byte: u8) {
        #[cfg(target_arch = "x86_64")]
        self.handle.write_byte(byte);
    }

    #[cfg(target_arch = "x86_64")]
    fn read_byte(&mut self) -> u8 {
        self.handle.read_byte()
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn read_byte(&mut self) -> u8 {
        0xFF
    }
}
