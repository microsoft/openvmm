// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Azure policy: only the settings from the device policy file.
#[derive(mesh_protobuf::Protobuf, Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "manifest", serde(deny_unknown_fields))]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[mesh(package = "openhcl.product_policy")]
pub struct AzurePolicy {
    /// From the device policy file: the VPCI relay settings. Unset means no
    /// allowed devices and [`crate::VpciRelayMode::DefaultOff`].
    #[mesh(1)]
    #[cfg_attr(feature = "manifest", serde(skip))]
    pub vpci_relay: Option<crate::VpciRelayPolicy>,
}
