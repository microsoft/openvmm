// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the ARM PL011 serial port.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::kind::SerialBackendHandle;

/// MMIO base of the first standard ARM64 UART.
pub const PL011_SERIAL0_BASE: u64 = 0xEFFE_C000;
/// MMIO base of the second standard ARM64 UART.
pub const PL011_SERIAL1_BASE: u64 = 0xEFFE_B000;
/// MMIO register aperture of each standard ARM64 UART.
pub const PL011_SERIAL_SIZE: u64 = 0x1000;
/// GIC SPI index of the first UART (not an absolute interrupt ID).
pub const PL011_SERIAL0_SPI: u32 = 1;
/// GIC SPI index of the second UART (not an absolute interrupt ID).
pub const PL011_SERIAL1_SPI: u32 = 2;

/// A handle for a PL011 device.
#[derive(MeshPayload)]
pub struct SerialPl011DeviceHandle {
    /// The base address for MMIO.
    pub base: u64,
    /// IRQ line for interrupts.
    pub irq: u32,
    /// The IO backend.
    pub io: Resource<SerialBackendHandle>,
    /// If true, insert a debugger-mode relay between the emulator and the
    /// backend that keeps the backend drained, dropping bytes instead of
    /// applying backpressure. Intended for WinDbg / KD-over-serial.
    pub debugger_mode: bool,
}

impl ResourceId<ChipsetDeviceHandleKind> for SerialPl011DeviceHandle {
    const ID: &'static str = "serial_pl011";
}
