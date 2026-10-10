// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UART identities to describe in the device tree.

#![forbid(unsafe_code)]

use mesh::payload::Protobuf;
use serial_16550_resources::ComPort;

/// A UART at its standard platform location.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Protobuf)]
pub enum UartId {
    /// A PC COM port.
    Com(ComPort),
    /// The first ARM64 PL011 UART.
    Pl0110,
    /// The second ARM64 PL011 UART.
    Pl0111,
}
