// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub use opentmk_core::arch::serial::SerialPort;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use opentmk_core::arch::serial::{InstrIoAccess, Serial};

/// Copy of the x86 serial ports, used as a polyfill for those architectures
/// that are not currently supported yet
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
#[expect(unused)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SerialPort {
    COM1,
    COM2,
    COM3,
    COM4,
}

pub(crate) trait SerialIo {
    fn init(&mut self);
    fn drain(&mut self);
    fn write_byte(&mut self, byte: u8);
    fn read_byte(&mut self) -> u8;
}

pub(crate) struct OpenTmkSerialIo {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    handle: Serial<InstrIoAccess>,
}

impl OpenTmkSerialIo {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn new(port: SerialPort) -> Self {
        log::info!("creating serial port");
        Self {
            handle: Serial::new(port, InstrIoAccess),
        }
    }

    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    pub fn new(_port: SerialPort) -> Self {
        log::info!("creating serial port (dummy)");
        Self {}
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
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

// Dummy transport for non x86 serial specifically to allow compilation to
// take place. Crashes so that we flag this issue early on.
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
impl SerialIo for OpenTmkSerialIo {
    fn init(&mut self) {
        todo!()
    }

    fn drain(&mut self) {
        todo!()
    }

    fn write_byte(&mut self, _byte: u8) {
        todo!()
    }

    fn read_byte(&mut self) -> u8 {
        todo!()
    }
}
