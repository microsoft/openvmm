// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serial output for debugging.

use uart_16550::BaudRate;
use uart_16550::Config;
use uart_16550::Uart16550;
use uart_16550::backend::PioBackend;

/// The base I/O port of the UART used for debug output.
pub const COM3: u16 = 0x3e8;

/// The configuration of the UART used for debug output.
pub const SERIAL_CONFIG: Config = Config {
    baud_rate: BaudRate::Baud115200,
    ..Config::DEFAULT
};

/// Returns the COM3 UART, without initializing the device.
pub const fn com3() -> Uart16550<PioBackend> {
    // SAFETY: COM3 is a valid I/O port, which is used for debug output only.
    match unsafe { Uart16550::new_port(COM3) } {
        Ok(uart) => uart,
        Err(_) => panic!("COM3 should be a valid port"),
    }
}
