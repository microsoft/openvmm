// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serial ports.

use uart_16550::BaudRate;
use uart_16550::Config;
use uart_16550::Uart16550;
use uart_16550::backend::PioBackend;

/// The configuration used for all serial ports.
pub const SERIAL_CONFIG: Config = Config {
    baud_rate: BaudRate::Baud115200,
    ..Config::DEFAULT
};

/// Serial port addresses.
/// These are the standard COM ports used in x86 systems.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SerialPort {
    /// COM1 serial port
    COM1,
    /// COM2 serial port
    COM2,
    /// COM3 serial port
    COM3,
    /// COM4 serial port
    COM4,
}

impl SerialPort {
    /// Returns the base I/O port.
    pub const fn base(self) -> u16 {
        match self {
            SerialPort::COM1 => 0x3F8,
            SerialPort::COM2 => 0x2F8,
            SerialPort::COM3 => 0x3E8,
            SerialPort::COM4 => 0x2E8,
        }
    }

    /// Returns the UART behind this port, without initializing it.
    pub const fn uart(self) -> Uart16550<PioBackend> {
        // SAFETY: The standard COM ports are valid I/O ports, and OpenTMK uses
        // each of them for a single purpose only.
        match unsafe { Uart16550::new_port(self.base()) } {
            Ok(uart) => uart,
            Err(_) => panic!("standard COM ports should be valid"),
        }
    }
}
