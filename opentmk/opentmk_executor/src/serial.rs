// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(not(target_arch = "x86_64"), expect(unused_variables))]

pub use opentmk_core::arch::serial::SerialPort;
#[cfg(target_arch = "x86_64")]
use {
    opentmk_core::arch::serial::SERIAL_CONFIG, uart_16550::Uart16550,
    uart_16550::backend::PioBackend,
};

pub(crate) trait SerialIo {
    fn init(&mut self);
    fn drain(&mut self);
    fn write_byte(&mut self, byte: u8);
    fn read_byte(&mut self) -> u8;
}

pub(crate) struct OpenTmkSerialIo {
    #[cfg(target_arch = "x86_64")]
    handle: Uart16550<PioBackend>,
}

impl OpenTmkSerialIo {
    pub fn new(port: SerialPort) -> Self {
        log::info!("creating serial port");
        Self {
            #[cfg(target_arch = "x86_64")]
            handle: port.uart(),
        }
    }
}

impl SerialIo for OpenTmkSerialIo {
    fn init(&mut self) {
        #[cfg(target_arch = "x86_64")]
        self.handle
            .init(SERIAL_CONFIG)
            .expect("serial port should be present");
    }

    fn drain(&mut self) {
        #[cfg(target_arch = "x86_64")]
        while self.handle.try_receive_byte().is_ok() {}
    }

    fn write_byte(&mut self, byte: u8) {
        #[cfg(target_arch = "x86_64")]
        self.handle.send_bytes_exact(&[byte]);
    }

    #[cfg(target_arch = "x86_64")]
    fn read_byte(&mut self) -> u8 {
        let mut byte = [0];
        self.handle.receive_bytes_exact(&mut byte);
        byte[0]
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn read_byte(&mut self) -> u8 {
        0xFF
    }
}
