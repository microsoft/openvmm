// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Various "legacy" chipset devices that collectively implement the Hyper-V
//! Generation 1 VM chipset.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod i440bx_host_pci_bridge;
pub mod piix4_cmos_rtc;
pub mod piix4_pci_bus;
pub mod piix4_pci_isa_bridge;
pub mod piix4_pm;
pub mod piix4_uhci;
pub mod winbond83977_sio;
