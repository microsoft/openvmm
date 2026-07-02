// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Microsoft-allocated PCI identifiers used by OpenVMM's emulated devices.
//!
//! Unlike the values in [`crate::spec`], these are *not* defined by the PCI
//! specification. They are allocations under Microsoft's PCI vendor ID, and so
//! live in their own module to keep [`crate::spec`] free of vendor-specific
//! constants.

/// Microsoft's PCI vendor ID.
///
/// First-party OpenVMM devices report this as their vendor ID. Devices that
/// emulate standardized hardware (and therefore report the real hardware
/// vendor ID) instead report this as their *subsystem* vendor ID, to identify
/// OpenVMM as the hosting environment.
pub const VENDOR_ID: u16 = 0x1414;

open_enum::open_enum! {
    /// Device IDs allocated under [`VENDOR_ID`] for OpenVMM's first-party
    /// emulated devices.
    ///
    /// These are centrally managed by Microsoft: a device ID must be formally
    /// assigned before use so that it does not collide with any other Microsoft
    /// product that shares this vendor ID.
    pub enum DeviceId: u16 {
        /// MANA/GDMA network adapter.
        GDMA = 0x00BA,
        /// VGA adapter.
        VGA = 0x5353,
        /// AZIHSM (Azure Integrated HSM) device.
        AZIHSM = 0xC003,
        /// PCIe root port.
        PCIE_ROOT_PORT = 0xC030,
        /// PCIe upstream switch port.
        PCIE_UPSTREAM_SWITCH_PORT = 0xC031,
        /// PCIe downstream switch port.
        PCIE_DOWNSTREAM_SWITCH_PORT = 0xC032,
        /// NVMe controller.
        NVME = 0xC03E,
    }
}

/// Default PCI subsystem ID reported by OpenVMM's emulated devices.
///
/// This is a generic marker, carved out of Microsoft's vendor-ID space, that
/// identifies a device as being emulated by OpenVMM.
///
/// A device that must emulate a very specific piece of hardware (where the
/// guest inspects the subsystem ID to identify the exact board) may override
/// this with its own value.
pub const DEFAULT_SUBSYSTEM_ID: u16 = 0x2000;
