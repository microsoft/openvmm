// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

extern crate alloc;

use alloc::vec::Vec;

/// Azure CVM policy: only the settings from the device policy file.
#[derive(mesh_protobuf::Protobuf, Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "manifest", serde(deny_unknown_fields))]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[mesh(package = "openhcl.product_policy")]
pub struct AzureCvmPolicy {
    /// From the device policy file: the PCI devices that the VPCI relay may
    /// expose to VTL0.
    #[mesh(1)]
    #[cfg_attr(feature = "manifest", serde(skip))]
    #[cfg_attr(feature = "inspect", inspect(skip))]
    pub allowed_pci_devices: Vec<crate::AllowedPciDevice>,

    /// From the device policy file: fail the boot when the VPCI relay can't
    /// run, instead of starting without the allowed devices.
    #[mesh(2)]
    #[cfg_attr(feature = "manifest", serde(skip))]
    #[cfg_attr(feature = "inspect", inspect(skip))]
    pub require_vpci_relay: bool,

    /// From the device policy file: the most VPCI devices the relay makes
    /// room for.
    #[mesh(3)]
    #[cfg_attr(feature = "manifest", serde(skip))]
    #[cfg_attr(feature = "inspect", inspect(skip))]
    pub max_vpci_devices: Option<u16>,
}
