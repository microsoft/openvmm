// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the 16550A UART.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::kind::SerialBackendHandle;

/// A handle to a 16550A serial device.
#[derive(MeshPayload)]
pub struct Serial16550DeviceHandle {
    /// The base address for the device registers.
    pub base: MmioOrIoPort,
    /// The width of the device registers, in bytes.
    pub register_width: u8,
    /// The IRQ line for interrupts.
    pub irq: u32,
    /// The IO backend.
    pub io: Resource<SerialBackendHandle>,
    /// If true, wait for the guest to set DTR+RTS modem bits before
    /// transmitting data to it. Otherwise, relay data from `io` even if
    /// the guest does not appear to be ready.
    pub wait_for_rts: bool,
}

impl ResourceId<ChipsetDeviceHandleKind> for Serial16550DeviceHandle {
    const ID: &'static str = "serial_16550";
}

/// A PC standard COM port.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ComPort {
    /// COM1, at 0x3f8/IRQ4.
    Com1,
    /// COM2, at 0x2f8/IRQ3.
    Com2,
    /// COM3, at 0x3e8/IRQ4.
    Com3,
    /// COM4, at 0x2e8/IRQ3.
    Com4,
}

impl ComPort {
    /// The IO port for the COM port.
    pub const fn io_port(&self) -> u16 {
        match *self {
            ComPort::Com1 => 0x3f8,
            ComPort::Com2 => 0x2f8,
            ComPort::Com3 => 0x3e8,
            ComPort::Com4 => 0x2e8,
        }
    }

    /// The IRQ line for the COM port.
    pub const fn irq(&self) -> u8 {
        match *self {
            ComPort::Com1 => 4,
            ComPort::Com2 => 3,
            ComPort::Com3 => 4,
            ComPort::Com4 => 3,
        }
    }
}

impl Serial16550DeviceHandle {
    /// Helper function to construct a standard PC COM port.
    pub fn com_port(com_port: ComPort, io: Resource<SerialBackendHandle>) -> Self {
        Self {
            base: MmioOrIoPort::IoPort(com_port.io_port()),
            register_width: 1,
            irq: com_port.irq().into(),
            io,
            wait_for_rts: false,
        }
    }

    /// Helper function to construct the four standard PC COM ports.
    pub fn com_ports(io: [Resource<SerialBackendHandle>; 4]) -> [Self; 4] {
        let [com1, com2, com3, com4] = io;
        [
            Self::com_port(ComPort::Com1, com1),
            Self::com_port(ComPort::Com2, com2),
            Self::com_port(ComPort::Com3, com3),
            Self::com_port(ComPort::Com4, com4),
        ]
    }
}

/// The base address for the serial controller, either an MMIO address or an IO
/// port.
#[derive(MeshPayload)]
pub enum MmioOrIoPort {
    /// The physical MMIO address.
    Mmio(u64),
    /// The IO port.
    IoPort(u16),
}
