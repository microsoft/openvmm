// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serial output for debugging.

use core::{arch::asm, fmt};
use super::io;

use sync_nostd::Mutex;

/// Serial port addresses.
/// These are the standard COM ports used in x86 systems.
#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SerialPort {
    COM1,
    COM2,
    COM3,
    COM4,
}

impl SerialPort {
    /// Convert the SerialPort enum to its u16 representation.
    pub fn value(self) -> u16 {
        match self {
            SerialPort::COM1 => 0x3F8,
            SerialPort::COM2 => 0x2F8,
            SerialPort::COM3 => 0x3E8,
            SerialPort::COM4 => 0x2E8,
        }
    }
}

/// A trait to access io ports used by the serial device.
pub trait IoAccess {
    /// Issue an in byte instruction.
    ///
    /// # Safety
    ///
    /// The caller must be sure that the given port is safe to read from.
    unsafe fn inb(&self, port: u16) -> u8;
    /// Issue an out byte instruction.
    ///
    /// # Safety
    ///
    /// The caller must be sure that the given port is safe to write to, and that the
    /// given value is safe for it.
    unsafe fn outb(&self, port: u16, data: u8);
}

/// A struct to access io ports using in/out instructions.
pub struct InstrIoAccess;

impl IoAccess for InstrIoAccess {
    unsafe fn inb(&self, port: u16) -> u8 {
        io::inb(port)
    }

    unsafe fn outb(&self, port: u16, data: u8) {
        io::outb(port, data)
    }
}

impl Default for InstrIoAccess {
    fn default() -> Self {
        InstrIoAccess
    }
}

/// A writer for the UART COM Ports.
pub struct Serial<T: IoAccess> {
    io: T,
    serial_port: SerialPort,
    mutex: Mutex<()>,
}

impl<T: IoAccess + Default> Serial<T> {
    /// Initialize the serial port.
    pub fn new(serial_port: SerialPort) -> Self {
        let io = T::default();
        
        // SAFETY: Writing these values to the serial device is safe.
        unsafe {
            io.outb(serial_port.value() + 1, 0x00); // Disable all interrupts
            io.outb(serial_port.value() + 2, 0xC7); // Enable FIFO, clear them, with 14-byte threshold
            io.outb(serial_port.value() + 4, 0x0F);
        }

        Self { io, serial_port, mutex: Mutex::new(()) }
    }

    fn write_byte(&self, b: u8) {
        // SAFETY: Reading and writing text to the serial device is safe.
        unsafe {
            while self.io.inb(self.serial_port.value() + 5) & 0x20 == 0 {}
            self.io.outb(self.serial_port.value(), b);
        }
    }
}

impl<T: IoAccess + Default> fmt::Write for Serial<T> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let _guard = self.mutex.lock();
        for &b in s.as_bytes() {
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
        Ok(())
    }
}
