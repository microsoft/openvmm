// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serial output for debugging.

use core::fmt;

use bitfield_struct::bitfield;
use spin::Mutex;

use super::io;

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
    /// Convert the SerialPort enum to its u16 representation.
    pub fn value(self, off: u16) -> u16 {
        off + match self {
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
        unsafe {
            // SAFETY: Caller assured that this port is safe to be read from
            io::inb(port)
        }
    }

    unsafe fn outb(&self, port: u16, data: u8) {
        unsafe {
            // SAFETY: Caller assured that this port is safe to be written to
            io::outb(port, data)
        }
    }
}

// Offsets to all the registers

/// Receive buffer (in)
pub const RX_BUFFER: u16 = 0;
/// Transmit buffer (out)
pub const TX_BUFFER: u16 = 0;
/// Interrupt enable register (in/out)
pub const INTERUPT_ENABLE: u16 = 1;
/// When DLAB in [`LINE_CONTROL`] is set, this is LSB of the divisor value for setting baud rate
pub const DLAB_LSB_BAUD: u16 = 0;
/// When DLAB in [`LINE_CONTROL`] is set, this is MSB of the divisor value for setting baud rate
pub const DLAB_MSB_BAUD: u16 = 1;
/// FIFO control registers (out). Use FIFO_* to construct flags
pub const FIFO_CONTROL: u16 = 2;
/// Line control registers (in/out). Use `LINE_*` to construct flags
pub const LINE_CONTROL: u16 = 3;
/// Modem control register (in/out).
pub const MODEM_CONTROL: u16 = 4;
/// Line status register (in).
pub const LINE_STATUS: u16 = 5;
/// Modem status register (in).
pub const MODEM_STATUS: u16 = 6;
/// Scratch register (in/out).
pub const SCRATCH: u16 = 7;

/// Represents the number of bits stored in a character. Fewer bits is faster
/// but store less information. By default the length is 8-bits
/// ([`LineDataBits::Len8`])
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum LineDataBits {
    /// Length is 5 bits
    #[default]
    Len5,
    /// Length is 6 bits
    Len6,
    /// Length is 7 bits
    Len7,
    /// Length is 8 bits
    Len8,
}

impl LineDataBits {
    const fn into_bits(self) -> u8 {
        self as _
    }
    const fn from_bits(value: u8) -> Self {
        match value {
            0 => Self::Len5,
            1 => Self::Len6,
            2 => Self::Len7,
            _ => Self::Len8,
        }
    }
}

/// Line control bits.
///
/// This configures the line protocol in terms of how bits are transitted to the
/// other end. In general, the sending and receiving device should have the same
/// protocol parameter values in order for communication to be successful.
///
/// These days you can consider 8N1 (8 bits, no parity, one stop bit) as pretty
/// much the default.
#[bitfield(u8)]
#[derive(PartialEq, Eq)]
pub struct LineControl {
    /// Determines the number of bits stored in a character.
    #[bits(2)]
    pub data_bits: LineDataBits,
    /// The number of bits to send after each character of data. false means
    /// there is 1 stop bits, true means 1.5 bits if character length is
    /// specifically 5 bits, else 2 bits.
    pub stop_bits: bool,
    /// The parity bit to send at the end of each character of data transmitted.
    #[bits(3)]
    pub parity_bits: u8,
    /// Break enable bit
    pub break_enable: bool,
    /// Set the DLAB or divisor latch access bit to set the baud rate divisor.
    /// The resulting baud rate is then computed as the formula (`115200 / (lsb | (msb << 8))`)
    pub dlab: bool,
}

/// Line status bits. This is read by reading [`LINE_STATUS`] offset of the
/// serial port.
#[bitfield(u8)]
#[derive(PartialEq, Eq)]
pub struct LineStatus {
    /// Data ready: line status flag is set when there is data that can be read
    pub dr: bool,
    /// Overrun error: line status flag is set when there has been data loss
    pub oe: bool,
    /// Parity error: line status flag is set when there was an error in the
    /// transmission detected by parity
    pub pe: bool,
    /// Framing error: line status flag is set when a stop bit was missing
    pub fe: bool,
    /// Break idicator: line status flag is set when there is a break in data input
    pub bi: bool,
    /// Transmitter holding register empty: line status flag is set when the
    /// transmission buffer is empty (i.e. data can be sent)
    pub thre: bool,
    /// Transmitter empty: line status flag is set when the transmitter is not
    /// doing anything
    pub temt: bool,
    /// Impending Error: line status flag is set when there is an error with a word
    /// in the input buffer
    pub err: bool,
}

/// FIFO control bits. This controls the FIFO buffers, accessed by reading the
/// [`FIFO_CONTROL`] offset.
#[bitfield(u8)]
#[derive(PartialEq, Eq)]
pub struct FifoControl {
    /// Enable FIFO
    pub enable: bool,
    /// Clear receive FIFO buffer.
    pub clear_rx: bool,
    /// Clear transmit FIFO buffer.
    pub clear_tx: bool,
    #[bits(5)]
    _reserved: u8,
}

/// A writer for the UART COM Ports.
pub struct Serial<T: IoAccess> {
    io: T,
    serial_port: SerialPort,
    mutex: Mutex<()>,
}

impl<T: IoAccess> Serial<T> {
    /// Initialize the serial port.
    pub const fn new(serial_port: SerialPort, io: T) -> Self {
        Self {
            io,
            serial_port,
            mutex: Mutex::new(()),
        }
    }

    /// Initialize the serial port.
    pub fn init(&self) {
        // SAFETY: Initializing the serial port is safe and well-defined with
        // these parameters.
        unsafe {
            // Disable all interrupts
            self.io.outb(self.serial_port.value(INTERUPT_ENABLE), 0);

            // Set baud
            self.line_control(LineControl::new().with_dlab(true));
            self.io.outb(self.serial_port.value(DLAB_LSB_BAUD), 1); // Low byte divisor
            self.io.outb(self.serial_port.value(DLAB_MSB_BAUD), 0); // High byte divisor

            self.line_control(LineControl::new().with_data_bits(LineDataBits::Len8));
            self.fifo_control(FifoControl::new().with_enable(true).with_clear_rx(true).with_clear_tx(true));
        }
    }

    fn line_status(&self) -> LineStatus {
        unsafe {
            // SAFETY: it is safe to read from the line status register
            LineStatus::from_bits(self.io.inb(self.serial_port.value(LINE_STATUS)))
        }
    }

    /// # Safety
    ///
    /// The caller must be sure that the control values set here leaves the serial
    /// port in a defined state
    unsafe fn fifo_control(&self, value: FifoControl) {
        unsafe {
            // SAFETY: caller guarantees that setting the FIFO_CONTROL to these values
            // is safe
            self.io.outb(self.serial_port.value(LINE_CONTROL), value.into_bits());
        }
    }

    /// # Safety
    ///
    /// The caller must be sure that the control values set here leaves the serial
    /// port in a defined state
    unsafe fn line_control(&self, value: LineControl) {
        unsafe {
            // SAFETY: caller guarantees that setting the LINE_CONTROL to these values
            // is safe
            self.io.outb(self.serial_port.value(LINE_CONTROL), value.into_bits());
        }
    }

    /// Write a single byte to the serial port, blocking until the transmit
    /// holding register is empty.
    pub fn write_byte(&self, b: u8) {
        let _guard = self.mutex.lock();
        self.write_byte_unlocked(b);
    }

    fn write_byte_unlocked(&self, b: u8) {
        while !self.line_status().thre() {
            core::hint::spin_loop();
        }
        unsafe {
            // SAFETY: Reading and writing bytes to the serial device is safe.
            self.io.outb(self.serial_port.value(TX_BUFFER), b);
        }
    }

    /// Read a single byte from the serial port, blocking until one is
    /// available.
    pub fn read_byte(&self) -> u8 {
        while !self.line_status().dr() {
            core::hint::spin_loop();
        }
        unsafe {
            // SAFETY: Reading and writing bytes to the serial device is safe.
            self.io.inb(self.serial_port.value(RX_BUFFER))
        }
    }

    /// Drain any bytes currently pending in the receive FIFO.
    pub fn drain(&self) {
        unsafe {
            // SAFETY: reading bytes from the serial device is safe
            while self.line_status().dr() {
                self.io.inb(self.serial_port.value(RX_BUFFER));
                core::hint::spin_loop();
            }
        }
    }
}

impl<T: IoAccess> fmt::Write for Serial<T> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let _guard = self.mutex.lock();
        for &b in s.as_bytes() {
            if b == b'\n' {
                self.write_byte_unlocked(b'\r');
            }
            self.write_byte_unlocked(b);
        }
        Ok(())
    }
}
