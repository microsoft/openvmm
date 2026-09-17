// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the ARM PL011 serial port.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::kind::SerialBackendHandle;

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
