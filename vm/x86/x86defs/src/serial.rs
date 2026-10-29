// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixed resources of standard PC COM ports.

/// IO register bases, ordered COM1 through COM4.
pub const COM_BASES: [u16; 4] = [0x3f8, 0x2f8, 0x3e8, 0x2e8];
/// Interrupt lines, ordered COM1 through COM4.
pub const COM_IRQS: [u8; 4] = [4, 3, 4, 3];
/// Number of registers in a 16550 UART. Standard COM registers are one byte wide.
pub const COM_REGISTER_COUNT: u8 = 8;
