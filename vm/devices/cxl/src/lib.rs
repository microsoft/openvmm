// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

//! CXL device and register emulation.

pub mod component_registers;
pub mod pci_registers;
pub mod test;

pub use component_registers::CxlComponentRegister;
pub use component_registers::CxlComponentRegisters;
pub use pci_registers::CxlDeviceDevsecExtendedCapability;
pub use pci_registers::CxlFlexBusPortDvsecExtendedCapability;
pub use pci_registers::CxlPortDvsecExtendedCapability;
pub use pci_registers::CxlRegisterLocatorDvsecExtendedCapability;
