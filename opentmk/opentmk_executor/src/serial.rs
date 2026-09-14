// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use opentmk_core::arch::serial::InstrIoAccess;
use opentmk_core::arch::serial::Serial;
pub use opentmk_core::arch::serial::SerialPort;

pub(crate) trait SerialIo {
    fn init(&mut self);
    fn drain(&mut self);
    fn write_byte(&mut self, byte: u8);
    fn read_byte(&mut self) -> u8;
}

pub(crate) struct OpenTmkSerialIo {
    handle: Serial<InstrIoAccess>,
}

impl OpenTmkSerialIo {
    pub fn new(port: SerialPort) -> Self {
        log::info!("creating serial port");
        Self {
            handle: Serial::new(port, InstrIoAccess),
        }
    }
}

impl SerialIo for OpenTmkSerialIo {
    fn init(&mut self) {
        self.handle.init();
    }

    fn drain(&mut self) {
        self.handle.drain();
    }

    fn write_byte(&mut self, byte: u8) {
        self.handle.write_byte(byte);
    }

    fn read_byte(&mut self) -> u8 {
        self.handle.read_byte()
    }
}
