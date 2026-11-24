// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serial output for debugging.

/// Serial port addresses.
/// These are the standard COM ports.
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
